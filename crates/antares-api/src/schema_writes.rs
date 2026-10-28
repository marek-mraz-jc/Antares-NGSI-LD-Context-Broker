// SPDX-License-Identifier: EUPL-1.2
//! Every NGSI-LD write held to its Tenant's `LinkML` schema (T-5912).
//!
//! The check itself lives in a protocol crate (`linkml-model`), which the
//! vanilla build never links (ADR-0027): a surface that reads schemas
//! registers it here with [`Hook::set`], and every write route asks
//! `admits` after its policy gate. The `OData`, OGC and STA surfaces write
//! through these routes, so a value that breaks the model is refused with
//! the same 400 whichever surface sent it.
//!
//! [`SCHEMA_WRITES_ENV`] picks what a refusal does: `enforce` answers it,
//! `report` (the default) logs it and writes anyway, so a deployment lists
//! what its sources send against the model before it turns enforcement on;
//! `off` checks nothing. A Tenant without a schema, or with one the
//! surfaces refuse, is written as before.

use std::sync::{Arc, OnceLock};

use antares_model::{NgsiError, TenantId};
use serde_json::Value;

/// The environment key of [`Mode`]: `enforce`, `report` (default) or `off`.
pub const SCHEMA_WRITES_ENV: &str = "ANTARES_SCHEMA_WRITES";

/// Whether the expanded Entity or fragment (third argument) keeps to the
/// schema bytes (second) of the revision (first); the fourth is `true` for
/// a write that sets the whole Entity (create, replace). `Err` is the
/// refusal's text, which names the Attribute and the rule, never the value.
pub type SchemaCheck = fn(u64, &Arc<[u8]>, &Value, bool) -> Result<(), String>;

/// What a refusal does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Nothing is checked.
    Off,
    /// A refusal is logged and the write goes on.
    Report,
    /// A refusal answers 400 Bad Request Data.
    Enforce,
}

impl Mode {
    /// The mode `value` names; `None` for any other text.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "report" => Some(Self::Report),
            "enforce" => Some(Self::Enforce),
            _ => None,
        }
    }
}

/// The check a surface registered and the mode, held by the view table.
#[derive(Debug, Default)]
pub struct Hook {
    check: OnceLock<SchemaCheck>,
    mode: OnceLock<Mode>,
}

impl Hook {
    /// Register the check; the first registration holds (every surface
    /// registers the same one).
    pub fn set(&self, check: SchemaCheck) {
        let _ = self.check.set(check);
    }

    /// Fix the mode: the views loader sets [`SCHEMA_WRITES_ENV`]'s at
    /// startup; the first call holds, so a test sets its own before.
    pub fn set_mode(&self, mode: Mode) {
        let _ = self.mode.set(mode);
    }

    /// The mode; `report` until one is set.
    pub fn mode(&self) -> Mode {
        self.mode.get().copied().unwrap_or(Mode::Report)
    }
}

/// [`admits`] for a document whose `type` names the Entity's types (a
/// create, a replace, a batch member): no store read.
pub(crate) fn admits_typed(
    st: &crate::state::AppState,
    tenant: &TenantId,
    doc: &Value,
    whole: bool,
) -> Result<(), NgsiError> {
    let hook = &st.views.schema_writes;
    let Some(check) = hook.check.get() else {
        return Ok(());
    };
    let mode = hook.mode();
    if mode == Mode::Off {
        return Ok(());
    }
    let Some(schema) = st.views.schema(tenant.as_str()) else {
        return Ok(());
    };
    match check(schema.revision, &schema.bytes, doc, whole) {
        Ok(()) => Ok(()),
        Err(why) if mode == Mode::Enforce => Err(NgsiError::BadRequestData(why)),
        Err(why) => {
            tracing::warn!(tenant = %tenant, refusal = %why, "a write breaks the Tenant's schema (reported, not refused)");
            Ok(())
        }
    }
}

/// Rule T-5912 for a write to the Entity `id`: `doc` (expanded) keeps to
/// the Tenant's schema. A fragment that names no `type` is judged by
/// `types` (the stored types the policy gate read) or, when the gate read
/// none, by the stored Entity's, read only when a check and a schema are
/// there to use them.
pub(crate) async fn admits(
    st: &crate::state::AppState,
    tenant: &TenantId,
    id: &str,
    doc: &Value,
    types: &[String],
) -> Result<(), NgsiError> {
    if doc.get("type").is_some() {
        return admits_typed(st, tenant, doc, false);
    }
    let hook = &st.views.schema_writes;
    if hook.check.get().is_none()
        || hook.mode() == Mode::Off
        || st.views.schema(tenant.as_str()).is_none()
    {
        return Ok(());
    }
    let types: Vec<Value> = if types.is_empty() {
        let stored = st
            .store
            .get(tenant, antares_store::Kind::Entity, id)
            .await?;
        stored
            .as_ref()
            .and_then(|d| d.get("type"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else {
        types.iter().cloned().map(Value::String).collect()
    };
    if types.is_empty() {
        return Ok(());
    }
    let mut typed = doc.clone();
    if let Some(o) = typed.as_object_mut() {
        o.insert("type".to_owned(), Value::Array(types));
    }
    admits_typed(st, tenant, &typed, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_parses_only_from_its_name() {
        assert_eq!(Mode::parse("enforce"), Some(Mode::Enforce));
        assert_eq!(Mode::parse("report"), Some(Mode::Report));
        assert_eq!(Mode::parse("off"), Some(Mode::Off));
        assert_eq!(Mode::parse("Enforce"), None);
    }

    #[test]
    fn the_first_mode_and_check_hold() {
        fn refuse(_: u64, _: &Arc<[u8]>, _: &Value, _: bool) -> Result<(), String> {
            Err("no".into())
        }
        let hook = Hook::default();
        hook.set_mode(Mode::Enforce);
        hook.set_mode(Mode::Off);
        assert_eq!(hook.mode(), Mode::Enforce);
        hook.set(refuse);
        assert!(hook.check.get().is_some());
    }
}
