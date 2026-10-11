// SPDX-License-Identifier: EUPL-1.2
//! The view table (ADR-0023 decision block and rules 8, 10 and 11,
//! surfaces-threat-model 9.6 rule M4 and V23).
//!
//! A [`View`] is configuration: written to `/q/views/{id}`, validated before
//! it is stored, kept as a document under the reserved tenant, and loaded
//! into the table every pod serves from. The table is rebuilt from the
//! store after every write on the pod that took it, and on every pod each
//! [`RELOAD_ENV`] interval, then swapped whole; no restart. A stored view
//! that no longer validates (written by another version, or by hand) is not
//! served and `/q/health` names it with the reason. A surface that serves
//! through views is closed while no view is loaded.
//!
//! No code path builds a view that serves everything: `View` has no
//! `Default`, a view selects at least one type and shows its Entities
//! through at least one rule, and no environment variable makes one.
//!
//! ```compile_fail
//! let everything = antares_ops::views::View::default();
//! ```

use crate::OperationId;
use antares_api::policy::Rule;
use antares_api::views::{CompiledView, Schema, ViewEntry, SCHEMA_ID_PREFIX};
use antares_api::AppState;
use antares_jsonld::Context;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Weak};

mod preview;
pub use preview::MAX_PREVIEW_LIMIT;
pub mod rule_documents;

/// How often, in milliseconds, every pod rebuilds its table from the store
/// (default 5000): the bound on how long a view written on one pod takes to
/// open, change or close on another.
pub const RELOAD_ENV: &str = "ANTARES_VIEWS_RELOAD_MS";

/// The largest `LinkML` schema document `/q/schemas/{tenant}` accepts, in
/// bytes. No default: unset, the broker takes no schema (T-3628).
pub const SCHEMA_MAX_ENV: &str = "ANTARES_SCHEMA_MAX_BYTES";

/// Every variable the view table reads. The broker accepts these as config
/// keys from here, so a new one is added in this crate alone (T-3766).
pub const ENV_KEYS: &[&str] = &[RELOAD_ENV, SCHEMA_MAX_ENV];

/// The largest view document `/q/views` accepts, in bytes.
pub const MAX_VIEW_BYTES: usize = 64 * 1024;

/// The most rules one view holds (V23: the predicate is bounded at load;
/// every request through the view evaluates each of them).
pub const MAX_RULES: usize = 64;

/// The one name that is no name: a view of every type and every
/// Attribute writes it alone (ADR-0023 rule 12).
pub const EVERY: &str = "*";

/// The most names in one list of a view: its selector, a rule's types or
/// Attributes, its operations.
pub const MAX_NAMES: usize = 256;

/// The longest `q` or `scopeQ` a view or a rule carries, in bytes.
pub const MAX_QUERY_BYTES: usize = 4096;

/// A view as written to `/q/views/{id}` (ADR-0023 decision block). Names
/// in `selector`, `rules[].types` and `rules[].attributes` are expanded
/// through the view's own `@context` (an inline object, over the core
/// context) at load. Every `q` is read against the core context: a
/// condition is the policy's, and a caller's `@context` must not change
/// what it means (ADR-0023 rule 11).
///
/// ```
/// let view = antares_ops::views::View::parse(br#"{
///   "id": "cars", "tenant": "city", "selector": ["Car"],
///   "rules": [{"types": ["Car"], "attributes": ["speed"]}],
///   "operations": ["retrieve_entity", "query_entities"]
/// }"#).expect("a view");
/// assert!(view.load().is_ok());
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct View {
    /// Its name, `[a-z0-9-]{1,63}`, which is also its path segment.
    pub id: String,
    /// The one Tenant it serves.
    pub tenant: String,
    /// Expands the view's type and Attribute names.
    #[serde(rename = "@context", default)]
    pub context: Option<serde_json::Map<String, Value>>,
    /// The Entity types it selects, at least one.
    pub selector: Vec<String>,
    /// A query every Entity it serves satisfies (4.9).
    #[serde(default)]
    pub q: Option<String>,
    /// A scope query every Entity it serves satisfies (4.19).
    #[serde(default)]
    pub scope_q: Option<String>,
    /// An area every Entity it serves lies in (4.10).
    #[serde(default)]
    pub geo_q: Option<GeoQ>,
    /// What each Entity shows, at least one rule (ADR-0023 rule 11).
    pub rules: Vec<ViewRule>,
    /// The operations it allows, as `operation-matrix.yaml` ids.
    pub operations: Vec<String>,
    /// The compiled Mapping IR of a view of another model (gap G18,
    /// `docs/analysis/view-mapping.md` section 3): the view serves its
    /// target class, selects its source class alone, and allows only
    /// [`crate::mapping::READS`].
    #[serde(default)]
    pub mapping: Option<Value>,
}

/// One allowlist of a [`View`]: the Entities of `types` for which the
/// condition holds show `attributes`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ViewRule {
    /// Entity types, at least one.
    pub types: Vec<String>,
    /// A condition on the Entity's own members.
    #[serde(default)]
    pub q: Option<String>,
    /// A scope condition.
    #[serde(default)]
    pub scope_q: Option<String>,
    /// An area condition.
    #[serde(default)]
    pub geo_q: Option<GeoQ>,
    /// The Attributes an Entity this rule holds for shows.
    pub attributes: Vec<String>,
}

/// A geoquery (4.10) as the request parameters of 7.2.5 name it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeoQ {
    /// `near;maxDistance==…`, `within`, `intersects` and the others.
    pub georel: String,
    /// `Point`, `Polygon` and the other `GeoJSON` geometry types.
    pub geometry: String,
    /// The coordinates, as a JSON array or as its text.
    pub coordinates: Value,
    /// The `GeoProperty` compared, `location` when absent.
    #[serde(default)]
    pub geoproperty: Option<String>,
}

/// A view that passed validation.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The narrowing every operation through the view is composed with.
    pub view: Arc<CompiledView>,
    /// The one Tenant it serves.
    pub tenant: String,
    /// The operations it allows.
    pub operations: Vec<OperationId>,
    /// What an operator should know: rules whose condition reads an
    /// Attribute they do not show, a temporal view keyed on a mutable
    /// member.
    pub notes: Vec<String>,
    /// The compiled Mapping of a view of another model.
    pub mapping: Option<Arc<crate::mapping::Mapping>>,
}

impl View {
    /// A view from the bytes of its document: at most [`MAX_VIEW_BYTES`],
    /// and no member the view does not define.
    ///
    /// ```
    /// assert!(antares_ops::views::View::parse(b"{}").is_err());
    /// ```
    ///
    /// # Errors
    /// Why the bytes are not a view.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_VIEW_BYTES {
            return Err(format!("a view document is at most {MAX_VIEW_BYTES} bytes"));
        }
        serde_json::from_slice(bytes).map_err(|e| format!("not a view: {e}"))
    }

    /// Whether this is ADR-0023 rule 12's view of every type and every
    /// Attribute: `selector ["*"]` and the one rule `{types ["*"],
    /// attributes ["*"]}` with no condition of its own. `*` anywhere else is
    /// an error, before any name is expanded, so no `@context` turns it into
    /// a name.
    fn is_every(&self) -> Result<bool, String> {
        let star = |names: &[String]| names.iter().any(|n| n == EVERY);
        let only = |names: &[String]| names.len() == 1 && star(names);
        if !star(&self.selector)
            && !self
                .rules
                .iter()
                .any(|r| star(&r.types) || star(&r.attributes))
        {
            return Ok(false);
        }
        match self.rules.as_slice() {
            [r] if only(&self.selector)
                && only(&r.types)
                && only(&r.attributes)
                && r.q.is_none()
                && r.scope_q.is_none()
                && r.geo_q.is_none() =>
            {
                Ok(true)
            }
            _ => Err(format!(
                "`{EVERY}` stands alone (ADR-0023 rule 12): selector [\"{EVERY}\"] and the one \
                 rule {{\"types\": [\"{EVERY}\"], \"attributes\": [\"{EVERY}\"]}}"
            )),
        }
    }

    /// Validate the view and compile it (ADR-0023 rule 10): its id, its
    /// tenant, its names expanded, every query parsed within its bounds,
    /// every operation a row of the matrix.
    ///
    /// ```
    /// let view = antares_ops::views::View::parse(br#"{
    ///   "id": "cars", "tenant": "city", "selector": [],
    ///   "rules": [{"types": ["Car"], "attributes": ["speed"]}],
    ///   "operations": ["query_entities"]
    /// }"#).expect("a view document");
    /// assert!(view.load().is_err(), "a view selects at least one type");
    /// ```
    ///
    /// # Errors
    /// The first reason the view cannot be served.
    pub fn load(&self) -> Result<Loaded, String> {
        if !valid_id(&self.id) {
            return Err("a view id is 1 to 63 of [a-z0-9-]".into());
        }
        let tenant = antares_model::TenantId::new(&self.tenant)
            .map_err(|_| format!("{:?} is not a tenant a client can name", self.tenant))?;
        let mut ctx = antares_jsonld::core_context();
        if let Some(local) = &self.context {
            ctx.merge_object(local)
                .map_err(|e| format!("the view's @context: {e}"))?;
        }
        let (q, scope, geo) = (
            parse_q("q", self.q.as_deref())?,
            scope_q("scopeQ", self.scope_q.as_deref())?,
            geo_q("geoQ", self.geo_q.as_ref())?,
        );
        let operations = operations(&self.operations)?;
        let (view, notes, selector) = if self.is_every()? {
            (
                CompiledView::every(q, scope, geo),
                Vec::new(),
                vec![EVERY.to_owned()],
            )
        } else {
            let selector = expand_all("selector", &self.selector, &ctx)?;
            if self.rules.len() > MAX_RULES {
                return Err(format!("a view holds at most {MAX_RULES} rules"));
            }
            let rules = self
                .rules
                .iter()
                .enumerate()
                .map(|(i, r)| compile_rule(i, r, &ctx))
                .collect::<Result<Vec<_>, _>>()?;
            let notes = disclosures(&rules);
            (
                CompiledView::new(&selector, q, scope, geo, rules)?,
                notes,
                selector,
            )
        };
        let mapping = match &self.mapping {
            None => None,
            Some(ir) => Some(Arc::new(mapped(
                ir,
                &ctx,
                self.context.as_ref(),
                &selector,
                &operations,
            )?)),
        };
        let view = view.named(&self.id);
        let mut loaded = Loaded {
            view: Arc::new(view),
            tenant: tenant.as_str().to_owned(),
            operations,
            notes,
            mapping,
        };
        if self.q.is_some()
            && loaded
                .operations
                .iter()
                .any(|op| op.id().starts_with("temporal"))
        {
            // ADR-0023 rule 8: membership is the current state, so an Entity
            // that enters the view brings its earlier history along.
            loaded.notes.push(
                "the view serves history and its q reads Attributes that change: an Entity \
                 entering the view brings its earlier history along (ADR-0023 rule 8)"
                    .into(),
            );
        }
        Ok(loaded)
    }
}

/// Section 3 and 7: the IR compiles, the view selects its source class
/// alone (any other type would be served rebuilt as the target class), and
/// it allows only the reads a mapping inverts.
fn mapped(
    ir: &Value,
    ctx: &Context,
    own: Option<&serde_json::Map<String, Value>>,
    selector: &[String],
    operations: &[OperationId],
) -> Result<crate::mapping::Mapping, String> {
    let mapping = crate::mapping::Mapping::compile(ir, ctx, own)?;
    if selector != [mapping.source_class()] {
        return Err("a mapped view selects its mapping's sourceClass and nothing else".into());
    }
    if let Some(op) = operations
        .iter()
        .find(|op| !crate::mapping::READS.contains(op))
    {
        return Err(format!(
            "a mapped view is read only: {:?} is not one of the reads a mapping inverts",
            op.id()
        ));
    }
    Ok(mapping)
}

fn valid_id(id: &str) -> bool {
    (1..=63).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn expand_all(what: &str, names: &[String], ctx: &Context) -> Result<Vec<String>, String> {
    if names.is_empty() {
        return Err(format!("{what} names at least one"));
    }
    if names.len() > MAX_NAMES {
        return Err(format!("{what} names at most {MAX_NAMES}"));
    }
    names
        .iter()
        .map(|n| {
            let iri = ctx.expand_key(n);
            if antares_jsonld::context::is_absolute_iri(&iri) {
                Ok(iri)
            } else {
                Err(format!("{what}: {n:?} does not expand to an IRI"))
            }
        })
        .collect()
}

fn compile_rule(i: usize, r: &ViewRule, ctx: &Context) -> Result<Rule, String> {
    let at = |what: &str| format!("rules[{i}].{what}");
    let attributes = if r.attributes.is_empty() {
        Vec::new()
    } else {
        expand_all(&at("attributes"), &r.attributes, ctx)?
    };
    Ok(Rule {
        types: expand_all(&at("types"), &r.types, ctx)?,
        q: parse_q(&at("q"), r.q.as_deref())?.map(Box::new),
        scope_q: scope_q(&at("scopeQ"), r.scope_q.as_deref())?,
        geo_q: geo_q(&at("geoQ"), r.geo_q.as_ref())?.map(Box::new),
        attributes,
        // a view rule has one area, names no ids and no windows; an engine's
        // narrow temporal reads
        ..Rule::default()
    })
}

fn parse_q(what: &str, q: Option<&str>) -> Result<Option<antares_ql::QNode>, String> {
    let Some(q) = q else { return Ok(None) };
    if q.len() > MAX_QUERY_BYTES {
        return Err(format!("{what} is at most {MAX_QUERY_BYTES} bytes"));
    }
    antares_ql::parse_q(q)
        .map(Some)
        .map_err(|e| format!("{what}: {e}"))
}

fn scope_q(what: &str, s: Option<&str>) -> Result<Option<String>, String> {
    match s {
        Some(s) if s.is_empty() || s.len() > MAX_QUERY_BYTES => {
            Err(format!("{what} is 1 to {MAX_QUERY_BYTES} bytes"))
        }
        other => Ok(other.map(str::to_owned)),
    }
}

fn geo_q(what: &str, g: Option<&GeoQ>) -> Result<Option<antares_ql::geo::GeoQuery>, String> {
    let Some(g) = g else { return Ok(None) };
    let coordinates = match &g.coordinates {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut params = HashMap::from([
        ("georel".to_owned(), g.georel.clone()),
        ("geometry".to_owned(), g.geometry.clone()),
        ("coordinates".to_owned(), coordinates),
    ]);
    if let Some(p) = &g.geoproperty {
        params.insert("geoproperty".to_owned(), p.clone());
    }
    antares_ql::geo::GeoQuery::from_params(&params).map_err(|e| format!("{what}: {e}"))
}

fn operations(ids: &[String]) -> Result<Vec<OperationId>, String> {
    if ids.is_empty() || ids.len() > MAX_NAMES {
        return Err(format!("operations names 1 to {MAX_NAMES} operations"));
    }
    let mut out = Vec::new();
    for id in ids {
        let op = OperationId::from_id(id)
            .ok_or_else(|| format!("operations: {id:?} is not a row of the operation matrix"))?;
        if !out.contains(&op) {
            out.push(op);
        }
    }
    Ok(out)
}

/// ADR-0023 rule 11: the attribute set reveals which rule matched, so the
/// loader names every rule whose condition reads what it does not show.
fn disclosures(rules: &[Rule]) -> Vec<String> {
    let core = antares_jsonld::core_context();
    rules
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let read = r.discloses(&core);
            (!read.is_empty()).then(|| {
                format!(
                    "rules[{i}] reads {} and does not show it: the Attributes it shows \
                     disclose that its condition held",
                    read.join(", ")
                )
            })
        })
        .collect()
}

/// The table entry of one stored document.
fn entry(doc: &Value) -> ViewEntry {
    let loaded = serde_json::from_value::<View>(doc.clone())
        .map_err(|e| format!("not a view: {e}"))
        .and_then(|v| v.load());
    match loaded {
        Ok(l) => ViewEntry::Loaded {
            view: l.view,
            tenant: l.tenant,
            operations: l.operations.iter().map(|op| op.id().to_owned()).collect(),
            notes: l.notes,
            mapping: l
                .mapping
                .map(|m| antares_api::views::Attached(m as Arc<dyn std::any::Any + Send + Sync>)),
        },
        Err(reason) => ViewEntry::Invalid { reason },
    }
}

/// Rebuilds one pod's table from the store. One reload at a time, so a
/// slow reload that read the store before a write cannot swap in its older
/// table after the write's own reload.
struct Reloader {
    state: AppState,
    /// Each stored document and its entry, so an unchanged view is not
    /// validated (and its notes not logged) again on every reload.
    cache: tokio::sync::Mutex<HashMap<String, (Value, ViewEntry)>>,
    /// The same for the policy engine's rule documents, per Tenant.
    policy: tokio::sync::Mutex<rule_documents::Cache>,
    /// [`SCHEMA_MAX_ENV`]; `None` refuses every schema write.
    schema_max: Option<usize>,
}

impl Reloader {
    async fn reload(&self) -> Result<(), String> {
        let mut cache = self.cache.lock().await;
        let docs = self.state.view_docs().await?;
        let mut next = HashMap::with_capacity(docs.len());
        let mut schemas = BTreeMap::new();
        for doc in docs {
            let Some(id) = doc.get("id").and_then(Value::as_str).map(str::to_owned) else {
                tracing::warn!("a stored view document has no id; skipped");
                continue;
            };
            if let Some(tenant) = id.strip_prefix(SCHEMA_ID_PREFIX) {
                if let Some(text) = doc.get("text").and_then(Value::as_str) {
                    schemas.insert(tenant.to_owned(), Schema::new(text.as_bytes()));
                } else {
                    tracing::warn!(tenant, "a stored schema document has no text; skipped");
                }
                continue;
            }
            let entry = match cache.remove(&id) {
                Some((old, e)) if old == doc => e,
                _ => {
                    let e = entry(&doc);
                    log_entry(&id, &e);
                    e
                }
            };
            next.insert(id, (doc, entry));
        }
        let table: BTreeMap<_, _> = next.iter().collect();
        self.state.views.replace(
            table
                .iter()
                .map(|(id, (_, e))| ((*id).clone(), e.clone()))
                .collect(),
            revision(table.iter().map(|(id, (doc, _))| (id.as_str(), doc))),
        );
        self.state.views.replace_schemas(schemas);
        *cache = next;
        drop(cache);
        rule_documents::reload(&self.state, &mut *self.policy.lock().await).await
    }
}

fn log_entry(id: &str, e: &ViewEntry) {
    match e {
        ViewEntry::Loaded { notes, .. } => {
            for note in notes {
                tracing::warn!(view = id, note = note.as_str(), "view loaded with a note");
            }
        }
        ViewEntry::Invalid { reason } => {
            tracing::warn!(view = id, reason = reason.as_str(), "view not served");
        }
    }
}

/// The digest of the view documents, in id order (threat V26): a pure
/// function of what the store holds, so pods over one store agree without
/// coordination.
fn revision<'a>(docs: impl Iterator<Item = (&'a str, &'a Value)>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (id, doc) in docs {
        id.hash(&mut h);
        doc.to_string().hash(&mut h);
    }
    h.finish()
}

/// [`SCHEMA_MAX_ENV`]: `None` unset; a value that is no positive byte count
/// is an error, so a typo never silently refuses every schema.
fn schema_max(raw: Option<String>) -> Result<Option<usize>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    match raw.parse::<usize>() {
        Ok(n) if n > 0 => Ok(Some(n)),
        _ => Err(format!(
            "{SCHEMA_MAX_ENV}={raw:?} is not a positive byte count"
        )),
    }
}

/// Load the stored views into `state`'s table, serve `/q/views`,
/// `/q/schemas` and `/q/policy` on its admin surface, and reload the table
/// every [`RELOAD_ENV`] milliseconds. Call it once, after the policy engine
/// is attached (the rule documents go to the engine of the `state` given
/// here) and before the router is built, inside a tokio runtime.
///
/// # Errors
/// The store's, or routes set already.
pub async fn install(state: &AppState) -> Result<Reload, String> {
    let reloader = Arc::new(Reloader {
        state: state.clone(),
        cache: tokio::sync::Mutex::new(HashMap::new()),
        policy: tokio::sync::Mutex::new(rule_documents::Cache::new()),
        schema_max: schema_max(std::env::var(SCHEMA_MAX_ENV).ok())?,
    });
    reloader.reload().await?;
    // The routes live in the table the reloader fills; a strong handle there
    // would keep the state alive through itself.
    state.views.set_admin(admin(&Arc::downgrade(&reloader)))?;
    let every = std::env::var(RELOAD_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(5000);
    let looping = reloader.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(every));
        tick.tick().await;
        loop {
            tick.tick().await;
            looping.reload_or_log().await;
        }
    });
    Ok(Reload(reloader))
}

impl Reloader {
    async fn reload_or_log(&self) {
        if let Err(e) = self.reload().await {
            tracing::warn!(
                error = e.as_str(),
                "view table reload failed; the table is unchanged"
            );
        }
    }
}

/// Rebuilds the table [`install`] loaded, on demand: the interval and every
/// `/q/views` write already do; this is for whoever changed the store
/// another way.
pub struct Reload(Arc<Reloader>);

impl Reload {
    /// Rebuild the table from the store now.
    ///
    /// # Errors
    /// The store's; the table is then unchanged.
    pub async fn now(&self) -> Result<(), String> {
        self.0.reload().await
    }
}

/// `/q/views`: the table (GET), one view's document (GET, PUT, DELETE), and
/// what a draft would serve (`POST /q/views/{id}/preview`);
/// `/q/schemas/{tenant}`: one Tenant's `LinkML` schema (GET, PUT, DELETE);
/// and `/q/policy` beside them.
fn admin(r: &Weak<Reloader>) -> Router {
    let (list, one, put, del) = (r.clone(), r.clone(), r.clone(), r.clone());
    let (s_get, s_put, s_del) = (r.clone(), r.clone(), r.clone());
    // Bytes past the bound are refused unread; one byte more reaches
    // `put_schema`, which answers with a problem document.
    let limit = r
        .upgrade()
        .and_then(|r| r.schema_max)
        .map_or(MAX_VIEW_BYTES, |n| n.saturating_add(1));
    Router::new()
        .route(
            "/schemas/{tenant}",
            get(move |Path(t): Path<String>| serve(s_get.clone(), |r| get_schema(r, t)))
                .put(move |Path(t): Path<String>, body: Bytes| {
                    serve(s_put.clone(), |r| put_schema(r, t, body))
                })
                .delete(move |Path(t): Path<String>| serve(s_del.clone(), |r| delete_schema(r, t)))
                .layer(DefaultBodyLimit::max(limit)),
        )
        .route(
            "/views",
            get(move || {
                serve(list.clone(), |r| async move {
                    Ok(axum::Json(r.state.views.report()).into_response())
                })
            }),
        )
        .route(
            "/views/{id}",
            get(move |Path(id): Path<String>| serve(one.clone(), |r| get_one(r, id)))
                .put(move |Path(id): Path<String>, body: Bytes| {
                    serve(put.clone(), |r| put_one(r, id, body))
                })
                .delete(move |Path(id): Path<String>| serve(del.clone(), |r| delete_one(r, id))),
        )
        .merge(preview::routes(r))
        .merge(rule_documents::routes(r))
}

/// Run `f` with the reloader, or answer 503 once the runtime that ran
/// [`install`] has stopped and taken the reloader with it.
async fn serve<F, Fut>(r: Weak<Reloader>, f: F) -> Response
where
    F: FnOnce(Arc<Reloader>) -> Fut,
    Fut: std::future::Future<Output = Result<Response, String>>,
{
    let Some(r) = r.upgrade() else {
        return problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "InternalError",
            "the view table is not running",
        );
    };
    f(r).await.unwrap_or_else(|e| inner(&e))
}

async fn get_one(r: Arc<Reloader>, id: String) -> Result<Response, String> {
    // A schema shares the store with the views; only a view id reads here.
    if !valid_id(&id) {
        return Ok(not_found());
    }
    Ok(match r.state.view_doc(&id).await? {
        Some(doc) => axum::Json(doc).into_response(),
        None => not_found(),
    })
}

async fn delete_one(r: Arc<Reloader>, id: String) -> Result<Response, String> {
    if !valid_id(&id) || !r.state.delete_view_doc(&id).await? {
        return Ok(not_found());
    }
    r.reload().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The document of view `id` and the view it loads, or the first reason it
/// cannot be served.
fn candidate(id: &str, body: &[u8]) -> Result<(Value, Loaded), String> {
    let doc = serde_json::from_slice::<Value>(body).map_err(|e| format!("not a view: {e}"))?;
    let view: View = serde_json::from_value(doc.clone()).map_err(|e| format!("not a view: {e}"))?;
    if view.id != id {
        return Err("the document's id names another view than the path".into());
    }
    view.load().map(|l| (doc, l))
}

/// Validate, store, and answer once this pod's table holds the write.
async fn put_one(r: Arc<Reloader>, id: String, body: Bytes) -> Result<Response, String> {
    if body.len() > MAX_VIEW_BYTES {
        return Ok(problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            "BadRequestData",
            &format!("a view document is at most {MAX_VIEW_BYTES} bytes"),
        ));
    }
    let doc = match candidate(&id, &body) {
        Ok((doc, _)) => doc,
        Err(why) => return Ok(problem(StatusCode::BAD_REQUEST, "BadRequestData", &why)),
    };
    let replaced = r.state.put_view_doc(&id, doc).await?;
    r.reload().await?;
    Ok(if replaced {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    }
    .into_response())
}

/// The key of `tenant`'s schema, `None` when the path names no Tenant a
/// client can.
fn schema_key(tenant: &str) -> Option<String> {
    antares_model::TenantId::new(tenant)
        .ok()
        .map(|t| format!("{SCHEMA_ID_PREFIX}{}", t.as_str()))
}

fn bad_tenant() -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        "BadRequestData",
        "the path names no tenant a client can",
    )
}

async fn get_schema(r: Arc<Reloader>, tenant: String) -> Result<Response, String> {
    let Some(key) = schema_key(&tenant) else {
        return Ok(bad_tenant());
    };
    Ok(
        match r
            .state
            .view_doc(&key)
            .await?
            .as_ref()
            .and_then(|d| d.get("text"))
            .and_then(Value::as_str)
        {
            Some(text) => (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                text.to_owned(),
            )
                .into_response(),
            None => no_schema(),
        },
    )
}

async fn delete_schema(r: Arc<Reloader>, tenant: String) -> Result<Response, String> {
    let Some(key) = schema_key(&tenant) else {
        return Ok(bad_tenant());
    };
    if !r.state.delete_view_doc(&key).await? {
        return Ok(no_schema());
    }
    r.reload().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Bound, check, store, and answer once this pod's table holds the write.
async fn put_schema(r: Arc<Reloader>, tenant: String, body: Bytes) -> Result<Response, String> {
    let Some(max) = r.schema_max else {
        return Ok(problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "OperationNotSupported",
            &format!("{SCHEMA_MAX_ENV} is not set: this broker takes no schema"),
        ));
    };
    if body.len() > max {
        return Ok(problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            "BadRequestData",
            &format!("a schema document is at most {max} bytes"),
        ));
    }
    let Some(key) = schema_key(&tenant) else {
        return Ok(bad_tenant());
    };
    let text = match std::str::from_utf8(&body)
        .map_err(|e| e.to_string())
        .and_then(|t| {
            serde_json::from_str::<serde::de::IgnoredAny>(t)
                .map(|_| t)
                .map_err(|e| e.to_string())
        }) {
        Ok(t) => t,
        Err(e) => {
            return Ok(problem(
                StatusCode::BAD_REQUEST,
                "BadRequestData",
                &format!("not JSON: {e}"),
            ))
        }
    };
    let doc = serde_json::json!({"id": key, "text": text});
    let replaced = r.state.put_view_doc(&key, doc).await?;
    r.reload().await?;
    Ok(if replaced {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    }
    .into_response())
}

fn no_schema() -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "ResourceNotFound",
        "no schema for this tenant",
    )
}

fn not_found() -> Response {
    problem(StatusCode::NOT_FOUND, "ResourceNotFound", "no such view")
}

fn inner(e: &str) -> Response {
    tracing::warn!(error = e, "the view store failed");
    problem(
        StatusCode::INTERNAL_SERVER_ERROR,
        "InternalError",
        "the view store failed",
    )
}

fn problem(status: StatusCode, title: &str, detail: &str) -> Response {
    let body = serde_json::json!({
        "type": format!("https://uri.etsi.org/ngsi-ld/errors/{title}"),
        "title": title,
        "status": status.as_u16(),
        "detail": detail,
    });
    (status, axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Value {
        json!({
            "id": "v", "tenant": "city", "selector": ["Car"],
            "rules": [{"types": ["Car"], "attributes": ["speed"]}],
            "operations": ["query_entities"]
        })
    }

    fn load(v: &Value) -> Result<Loaded, String> {
        View::parse(v.to_string().as_bytes()).and_then(|v| v.load())
    }

    fn with(member: &str, value: Value) -> Value {
        let mut v = base();
        v[member] = value;
        v
    }

    fn mapped() -> Value {
        let mut v = with(
            "mapping",
            json!({"version": 2, "sourceClass": "Car", "targetClass": "Vehicle",
                   "slots": [{"target": "velocity", "source": "speed", "kind": "rename"}]}),
        );
        v["operations"] = json!([
            "retrieve_entity",
            "query_entities",
            "retrieve_attribute_value"
        ]);
        v
    }

    #[test]
    fn a_mapped_view_serves_the_temporal_and_discovery_reads() {
        let mut v = mapped();
        v["operations"] = json!(crate::mapping::READS
            .iter()
            .map(|op| op.id())
            .collect::<Vec<_>>());
        let loaded = load(&v).map(|l| l.operations.len());
        assert_eq!(loaded, Ok(11));
    }

    #[test]
    fn a_mapped_view_loads_with_its_mapping() {
        let loaded = load(&mapped());
        assert_eq!(
            loaded
                .ok()
                .and_then(|l| l.mapping.map(|m| m.source_class().to_owned())),
            Some("https://uri.etsi.org/ngsi-ld/default-context/Car".to_owned())
        );
        assert!(load(&base()).is_ok_and(|l| l.mapping.is_none()));
    }

    #[test]
    fn a_mapped_view_is_read_only_selects_its_source_class_alone_and_runs_a_valid_ir() {
        for op in [
            "create_entity",
            "create_subscription",
            "batch_upsert",
            "query_entities_post",
            "temporal_query_post",
            "temporal_upsert",
            "create_temporal_entity_map",
        ] {
            let mut v = mapped();
            v["operations"] = json!(["query_entities", op]);
            let e = load(&v).err().unwrap_or_default();
            assert!(e.contains("read only"), "{op}: {e}");
        }
        for selector in [json!(["Truck"]), json!(["Car", "Truck"])] {
            let mut v = mapped();
            v["selector"] = selector.clone();
            v["rules"] = json!([{"types": ["Car", "Truck"], "attributes": ["speed"]}]);
            let e = load(&v).err().unwrap_or_default();
            assert!(
                e.contains("sourceClass and nothing else"),
                "{selector}: {e}"
            );
        }
        let mut v = mapped();
        v["mapping"]["version"] = json!(1);
        assert!(load(&v).err().unwrap_or_default().contains("version 1"));
    }

    #[test]
    fn a_view_of_everything_takes_no_mapping() {
        let mut v = mapped();
        v["selector"] = json!([EVERY]);
        v["rules"] = json!([{"types": [EVERY], "attributes": [EVERY]}]);
        let e = load(&v).err().unwrap_or_default();
        assert!(e.contains("sourceClass and nothing else"), "{e}");
    }

    #[test]
    fn an_id_is_one_to_63_of_lowercase_digits_and_dashes() {
        for ok in ["a", "a-1", &"z".repeat(63)] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in ["", &"z".repeat(64), "A", "a_b", "a.b", "a/b", "é"] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn every_bound_holds_at_load() {
        let many = |n: usize| Value::Array(vec![json!("Car"); n]);
        assert!(load(&with("selector", many(MAX_NAMES))).is_ok());
        let e = load(&with("selector", many(MAX_NAMES + 1)))
            .err()
            .unwrap_or_default();
        assert!(e.contains("at most 256"), "{e}");
        let ops = Value::Array(vec![json!("query_entities"); MAX_NAMES + 1]);
        assert!(load(&with("operations", ops)).is_err());
        let long = "/a".repeat(MAX_QUERY_BYTES);
        assert!(load(&with("scopeQ", json!(long))).is_err());
        assert!(load(&with("scopeQ", json!("/a/#"))).is_ok());
        let big = vec![b' '; MAX_VIEW_BYTES + 1];
        assert!(View::parse(&big)
            .err()
            .unwrap_or_default()
            .contains("at most"));
    }

    #[test]
    fn an_operation_named_twice_is_allowed_once() {
        let loaded = load(&with(
            "operations",
            json!(["query_entities", "query_entities"]),
        ));
        assert_eq!(
            loaded.map(|l| l.operations),
            Ok(vec![OperationId::QueryEntities])
        );
    }

    #[test]
    fn a_geoq_takes_its_coordinates_as_json_or_text_and_names_its_geoproperty() {
        for coordinates in [
            json!([[[0, 0], [1, 0], [1, 1], [0, 0]]]),
            json!("[[[0,0],[1,0],[1,1],[0,0]]]"),
        ] {
            let g = json!({"georel": "within", "geometry": "Polygon",
                           "coordinates": coordinates, "geoproperty": "position"});
            assert!(load(&with("geoQ", g)).is_ok());
        }
    }

    #[test]
    fn a_scoped_context_in_the_view_is_refused() {
        let ctx = json!({"Car": {"@id": "https://x.example/Car", "@context": {}}});
        let e = load(&with("@context", ctx)).err().unwrap_or_default();
        assert!(e.starts_with("the view's @context"), "{e}");
    }

    #[test]
    fn the_schema_bound_is_a_positive_byte_count_or_unset() {
        assert_eq!(schema_max(None), Ok(None));
        assert_eq!(schema_max(Some("64".into())), Ok(Some(64)));
        for bad in ["0", "-1", "64k", ""] {
            let e = schema_max(Some(bad.into())).err().unwrap_or_default();
            assert!(e.starts_with(SCHEMA_MAX_ENV), "{bad}: {e}");
        }
    }

    #[tokio::test]
    async fn without_a_bound_no_schema_is_taken_or_stored() {
        let r = Arc::new(Reloader {
            state: AppState::new("antares-test".into()),
            cache: tokio::sync::Mutex::new(HashMap::new()),
            policy: tokio::sync::Mutex::new(rule_documents::Cache::new()),
            schema_max: None,
        });
        let put = put_schema(r.clone(), "city".into(), Bytes::from_static(b"{}")).await;
        assert_eq!(put.map(|p| p.status()), Ok(StatusCode::SERVICE_UNAVAILABLE));
        assert_eq!(r.state.view_doc("schema:city").await, Ok(None));
    }

    #[test]
    fn a_stored_document_that_is_no_view_is_invalid_with_the_reason() {
        let mut doc = base();
        doc["allowAll"] = json!(true);
        let shown = format!("{:?}", entry(&doc));
        assert!(
            shown.starts_with("Invalid { reason: \"not a view: unknown field `allowAll`"),
            "{shown}"
        );
    }
}
