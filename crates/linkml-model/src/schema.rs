// SPDX-License-Identifier: EUPL-1.2
//! The LinkML subset as plain data (odata-linkml-mapping.md section 9): the
//! metaslots the surfaces read, in the shape of LinkML's JSON serialisation.
//! Every struct denies unknown fields, so a metaslot outside the subset fails
//! the load instead of being dropped (threat LK4).

use std::collections::BTreeMap;

use serde::Deserialize;

/// A schema: `id`, `name`, `version`, `prefixes`, `default_prefix`, and its
/// classes, slots and enums keyed by name. The two declarations the subset
/// implies, [`IMPORTS`] and [`DEFAULT_RANGE`], are read when they are
/// exactly those and refused otherwise (LK4); the writer declares them, so
/// a LinkML tool reads the schema the broker means.
///
/// ```
/// let s: linkml_model::Schema =
///     serde_json::from_str(r#"{"id": "https://example.org/s", "name": "s"}"#).unwrap();
/// assert!(s.classes.is_empty());
/// let declared: Result<linkml_model::Schema, _> = serde_json::from_str(
///     r#"{"id": "https://e.org/s", "name": "s", "imports": ["linkml:types"], "default_range": "string"}"#);
/// assert!(declared.is_ok());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawSchema")]
pub struct Schema {
    /// The schema IRI.
    pub id: String,
    /// The schema name.
    pub name: String,
    /// The schema version.
    #[serde(default)]
    pub version: Option<String>,
    /// CURIE prefixes, keyed by prefix.
    #[serde(default)]
    pub prefixes: BTreeMap<String, Prefix>,
    /// The prefix of every element without its own URI.
    #[serde(default)]
    pub default_prefix: Option<String>,
    /// Classes, keyed by name.
    #[serde(default)]
    pub classes: BTreeMap<String, Class>,
    /// Slots, keyed by name.
    #[serde(default)]
    pub slots: BTreeMap<String, Slot>,
    /// Enums, keyed by name.
    #[serde(default)]
    pub enums: BTreeMap<String, Enum>,
}

/// The built-in types every range outside the schema's own names resolves
/// to: the one import the subset reads.
pub const IMPORTS: &str = "linkml:types";

/// The range of a slot that names none.
pub const DEFAULT_RANGE: &str = "string";

/// [`Schema`] as read, with the two declarations the subset implies.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    /// The schema IRI.
    pub id: String,
    /// The schema name.
    pub name: String,
    /// The schema version.
    #[serde(default)]
    pub version: Option<String>,
    /// CURIE prefixes, keyed by prefix.
    #[serde(default)]
    pub prefixes: BTreeMap<String, Prefix>,
    /// The prefix of every element without its own URI.
    #[serde(default)]
    pub default_prefix: Option<String>,
    /// Classes, keyed by name.
    #[serde(default)]
    pub classes: BTreeMap<String, Class>,
    /// Slots, keyed by name.
    #[serde(default)]
    pub slots: BTreeMap<String, Slot>,
    /// Enums, keyed by name.
    #[serde(default)]
    pub enums: BTreeMap<String, Enum>,
    #[serde(default)]
    imports: Option<serde_json::Value>,
    #[serde(default)]
    default_range: Option<serde_json::Value>,
}

impl TryFrom<RawSchema> for Schema {
    type Error = String;

    fn try_from(raw: RawSchema) -> Result<Self, String> {
        // worded as serde words an unknown field, which the load reports
        // as outside the subset (`Error::NotImplemented`)
        if raw
            .imports
            .is_some_and(|i| i != serde_json::json!([IMPORTS]))
        {
            return Err(format!(
                "unknown field `imports`: the subset reads [{IMPORTS:?}] alone"
            ));
        }
        if raw.default_range.is_some_and(|r| r != DEFAULT_RANGE) {
            return Err(format!(
                "unknown field `default_range`: the subset reads {DEFAULT_RANGE:?} alone"
            ));
        }
        Ok(Self {
            id: raw.id,
            name: raw.name,
            version: raw.version,
            prefixes: raw.prefixes,
            default_prefix: raw.default_prefix,
            classes: raw.classes,
            slots: raw.slots,
            enums: raw.enums,
        })
    }
}

/// One prefix in LinkML's JSON form.
///
/// ```
/// let p: linkml_model::Prefix = serde_json::from_str(
///     r#"{"prefix_prefix": "schema", "prefix_reference": "https://schema.org/"}"#,
/// ).unwrap();
/// assert_eq!(p.prefix_reference, "https://schema.org/");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prefix {
    /// The prefix; equal to its key.
    pub prefix_prefix: String,
    /// The IRI the prefix stands for.
    pub prefix_reference: String,
}

/// A class: `class_uri`, `is_a`, `slots`, `slot_usage`, `description`.
///
/// ```
/// let c: linkml_model::Class =
///     serde_json::from_str(r#"{"is_a": "Thing", "slots": ["name"]}"#).unwrap();
/// assert_eq!(c.is_a.as_deref(), Some("Thing"));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Class {
    /// The class name; equal to its key when present.
    #[serde(default)]
    pub name: Option<String>,
    /// The class IRI or CURIE.
    #[serde(default)]
    pub class_uri: Option<String>,
    /// The parent class.
    #[serde(default)]
    pub is_a: Option<String>,
    /// The slots the class declares, in order.
    #[serde(default)]
    pub slots: Vec<String>,
    /// Per-class refinements of slots, keyed by slot name.
    #[serde(default)]
    pub slot_usage: BTreeMap<String, Slot>,
    /// Tagged values, keyed by tag; `odata_insertable`, `odata_updatable`
    /// and `odata_deletable` are read (T-3722).
    #[serde(default)]
    pub annotations: BTreeMap<String, Annotation>,
    /// Free text.
    #[serde(default)]
    pub description: Option<String>,
}

/// A slot, or a `slot_usage` refinement of one: every metaslot optional.
///
/// ```
/// let s: linkml_model::Slot =
///     serde_json::from_str(r#"{"range": "integer", "required": true}"#).unwrap();
/// assert_eq!(s.required, Some(true));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    /// The slot name; equal to its key when present.
    #[serde(default)]
    pub name: Option<String>,
    /// The slot IRI or CURIE.
    #[serde(default)]
    pub slot_uri: Option<String>,
    /// A class, an enum or a built-in type; `string` when absent.
    #[serde(default)]
    pub range: Option<String>,
    /// Whether every instance carries it.
    #[serde(default)]
    pub required: Option<bool>,
    /// Whether it holds several values.
    #[serde(default)]
    pub multivalued: Option<bool>,
    /// Whether its value identifies the instance among its class's (T-3720).
    #[serde(default)]
    pub identifier: Option<bool>,
    /// Whether a class-ranged value is nested instead of referenced.
    #[serde(default)]
    pub inlined: Option<bool>,
    /// The slot that states the same relation from the other end.
    #[serde(default)]
    pub inverse: Option<String>,
    /// The unit of a quantity.
    #[serde(default)]
    pub unit: Option<Unit>,
    /// Tagged values, keyed by tag; `ngsi_ld_kind` (DM-05), `edm_type` (T-3720),
    /// `odata_computed` and `odata_immutable` (T-3722) are read, and `summary`,
    /// `label` (JSON text of a language map) and `hidden` (DM-99…DM-101, T-4672).
    #[serde(default)]
    pub annotations: BTreeMap<String, Annotation>,
    /// Free text.
    #[serde(default)]
    pub description: Option<String>,
    /// Where the slot stands in its class's display order, lower first (DM-98).
    #[serde(default)]
    pub rank: Option<i64>,
    /// The slot's human-readable name.
    #[serde(default)]
    pub title: Option<String>,
}

/// A unit: `ucum_code`, `symbol`, `exact_mappings` (a `unece:` CURIE among them).
///
/// ```
/// let u: linkml_model::Unit = serde_json::from_str(r#"{"ucum_code": "Cel"}"#).unwrap();
/// assert_eq!(u.ucum_code.as_deref(), Some("Cel"));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unit {
    /// The UCUM code.
    #[serde(default)]
    pub ucum_code: Option<String>,
    /// The printed symbol.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Equivalent unit IRIs or CURIEs.
    #[serde(default)]
    pub exact_mappings: Vec<String>,
}

/// One annotation in LinkML's JSON form.
///
/// ```
/// let a: linkml_model::Annotation =
///     serde_json::from_str(r#"{"tag": "ngsi_ld_kind", "value": "Property"}"#).unwrap();
/// assert_eq!(a.value, "Property");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Annotation {
    /// The tag; equal to its key.
    pub tag: String,
    /// The value, a string in this subset.
    pub value: String,
}

/// An enum: `description` and `permissible_values`.
///
/// ```
/// let e: linkml_model::Enum =
///     serde_json::from_str(r#"{"permissible_values": {"red": {}}}"#).unwrap();
/// assert!(e.permissible_values.contains_key("red"));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enum {
    /// The enum name; equal to its key when present.
    #[serde(default)]
    pub name: Option<String>,
    /// Free text.
    #[serde(default)]
    pub description: Option<String>,
    /// The values, keyed by text.
    #[serde(default)]
    pub permissible_values: BTreeMap<String, PermissibleValue>,
}

/// One permissible value: `text`, `description`, `meaning` and its `annotations`, such as the
/// `label` that names it in each language (T-4803).
///
/// ```
/// let v: linkml_model::PermissibleValue =
///     serde_json::from_str(r#"{"meaning": "https://example.org/Red"}"#).unwrap();
/// assert!(v.text.is_none());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissibleValue {
    /// The value; equal to its key when present.
    #[serde(default)]
    pub text: Option<String>,
    /// Free text.
    #[serde(default)]
    pub description: Option<String>,
    /// The IRI or CURIE the value stands for.
    #[serde(default)]
    pub meaning: Option<String>,
    /// Tagged values, keyed by tag.
    #[serde(default)]
    pub annotations: BTreeMap<String, Annotation>,
}
