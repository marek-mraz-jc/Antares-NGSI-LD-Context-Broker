// SPDX-License-Identifier: EUPL-1.2
//! The documents of one narrowed schema (rows L1 to L9) under the
//! platform's names and media types (`gateway-to-broker-map.md` M11: the
//! Portal, the SDK and the App generator read them), and the cache that
//! renders them once per view, schema and visible set.

use crate::narrow::{narrow, LIMITS};
use antares_ops::Visible;
use linkml_model::{Model, Range};
use serde_json::{json, Map, Value};
use sha2::{Digest as _, Sha256};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

/// The media type of the index and of a class or slot.
pub(crate) const JSON: &str = "application/json";

/// One document a schema major publishes (L2 to L6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Artifact {
    /// LinkML YAML.
    LinkMl,
    /// JSON Schema.
    JsonSchema,
    /// The JSON-LD `@context`.
    Context,
    /// SHACL shapes.
    Shacl,
    /// OWL.
    Owl,
    /// RDFS.
    Rdf,
    /// Markdown.
    Markdown,
}

impl Artifact {
    /// Every document, in the order an agent should read them.
    pub const ALL: [Self; 7] = [
        Self::LinkMl,
        Self::JsonSchema,
        Self::Context,
        Self::Shacl,
        Self::Owl,
        Self::Rdf,
        Self::Markdown,
    ];

    /// The file name it is published under.
    ///
    /// ```
    /// assert_eq!(antares_linkml::Artifact::Context.file_name(), "context.jsonld");
    /// ```
    #[must_use]
    pub fn file_name(self) -> &'static str {
        match self {
            Self::LinkMl => "model.linkml.yaml",
            Self::JsonSchema => "model.schema.json",
            Self::Context => "context.jsonld",
            Self::Shacl => "model.shacl.ttl",
            Self::Owl => "model.owl.ttl",
            Self::Rdf => "model.rdf.ttl",
            Self::Markdown => "model.md",
        }
    }

    /// Its media type.
    ///
    /// ```
    /// assert_eq!(antares_linkml::Artifact::LinkMl.media_type(), "text/yaml");
    /// ```
    #[must_use]
    pub fn media_type(self) -> &'static str {
        match self {
            Self::LinkMl => "text/yaml",
            Self::JsonSchema => "application/schema+json",
            Self::Context => "application/ld+json",
            Self::Shacl | Self::Rdf => "text/turtle",
            Self::Owl => "text/turtle; profile=\"owl\"",
            Self::Markdown => "text/markdown",
        }
    }

    /// The document one path segment names: a file name, its short name, or
    /// `model`, which `accept` picks the formalism of (L7). `model.qb.ttl`
    /// and `qb` name nothing: the subset holds no Data Structure Definition
    /// (L6).
    ///
    /// ```
    /// use antares_linkml::Artifact;
    /// assert_eq!(Artifact::of("shacl", ""), Some(Artifact::Shacl));
    /// assert_eq!(Artifact::of("model", "text/yaml"), Some(Artifact::LinkMl));
    /// assert_eq!(Artifact::of("qb", ""), None);
    /// ```
    #[must_use]
    pub fn of(segment: &str, accept: &str) -> Option<Self> {
        match segment {
            "model.linkml.yaml" | "linkml" => Some(Self::LinkMl),
            "model.schema.json" | "json-schema" => Some(Self::JsonSchema),
            "context.jsonld" | "context" => Some(Self::Context),
            "model.shacl.ttl" | "shacl" => Some(Self::Shacl),
            "model.owl.ttl" | "owl" => Some(Self::Owl),
            "model.rdf.ttl" | "rdf" => Some(Self::Rdf),
            "model.md" | "docs" => Some(Self::Markdown),
            "model" => Some(Self::negotiated(accept)),
            _ => None,
        }
    }

    /// The formalism `accept` asks `model` for, as the platform picks it.
    fn negotiated(accept: &str) -> Self {
        if accept.contains("turtle") {
            if accept.contains("owl") {
                Self::Owl
            } else if accept.contains("rdf") {
                Self::Rdf
            } else {
                Self::Shacl
            }
        } else if accept.contains("yaml") {
            Self::LinkMl
        } else if accept.contains("markdown") {
            Self::Markdown
        } else if accept.contains("ld+json") {
            Self::Context
        } else {
            Self::JsonSchema
        }
    }

    fn render(self, model: &Model) -> String {
        match self {
            Self::LinkMl => linkml_model::yaml(model),
            Self::JsonSchema => linkml_model::json_schema(model),
            Self::Context => linkml_model::context(model),
            Self::Shacl => linkml_model::shacl(model),
            Self::Owl => linkml_model::owl(model),
            Self::Rdf => linkml_model::rdfs(model),
            Self::Markdown => linkml_model::markdown(model),
        }
    }
}

/// The lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Every document of one narrowed schema, rendered once.
#[derive(Debug)]
pub(crate) struct Rendered {
    model: Model,
    /// The schema's major version, the `v{major}` of its paths.
    pub(crate) major: String,
    documents: Vec<(Artifact, Arc<[u8]>)>,
    /// `schema/index.json` (L1).
    pub(crate) index: Arc<[u8]>,
}

impl Rendered {
    fn new(model: Model, view: &str, left_out: bool) -> Self {
        let major = major(model.schema().version.as_deref());
        let documents: Vec<(Artifact, Arc<[u8]>)> = Artifact::ALL
            .iter()
            .map(|a| (*a, Arc::from(a.render(&model).into_bytes())))
            .collect();
        let artifacts: Map<String, Value> = documents
            .iter()
            .map(|(a, bytes)| {
                let described =
                    json!({"type": a.media_type(), "bytes": bytes.len(), "sha256": sha256(bytes)});
                (a.file_name().to_owned(), described)
            })
            .collect();
        let s = model.schema();
        let mut described = json!({
            "name": s.name,
            "version": major,
            "semver": s.version,
            "types": s.classes.keys().collect::<Vec<_>>(),
            "artifacts": artifacts,
        });
        if left_out {
            // Whether anything was left out, never what (the platform's EP-47).
            insert(&mut described, "redacted", Value::Bool(true));
        }
        let index = json!({"endpoint": view, "models": [described]});
        Self {
            index: Arc::from(index.to_string().into_bytes()),
            model,
            major,
            documents,
        }
    }

    pub(crate) fn document(&self, artifact: Artifact) -> Option<Arc<[u8]>> {
        self.documents
            .iter()
            .find(|(a, _)| *a == artifact)
            .map(|(_, b)| b.clone())
    }

    /// One visible class with its slots, its own and inherited (L8).
    pub(crate) fn class(&self, name: &str) -> Option<Arc<[u8]>> {
        let m = &self.model;
        let class = m.schema().classes.get(name)?;
        let slots: Vec<Value> = m
            .class_slots(name)
            .iter()
            .map(|(n, s)| self.slot_json(n, s))
            .collect();
        let mut doc = json!({"name": name, "iri": m.class_iri(name), "slots": slots});
        if let Some(parent) = &class.is_a {
            insert(&mut doc, "is_a", json!(parent));
        }
        if let Some(d) = &class.description {
            insert(&mut doc, "description", json!(d));
        }
        Some(Arc::from(doc.to_string().into_bytes()))
    }

    /// One visible slot with the visible classes that use it (L9).
    pub(crate) fn slot(&self, name: &str) -> Option<Arc<[u8]>> {
        let m = &self.model;
        let slot = m.schema().slots.get(name)?;
        let classes: Vec<&String> = m
            .schema()
            .classes
            .keys()
            .filter(|c| m.class_slots(c).iter().any(|(n, _)| *n == name))
            .collect();
        let mut doc = self.slot_json(name, slot);
        insert(&mut doc, "classes", json!(classes));
        Some(Arc::from(doc.to_string().into_bytes()))
    }

    fn slot_json(&self, name: &str, slot: &linkml_model::Slot) -> Value {
        let m = &self.model;
        let range = match m.range(slot) {
            Range::Type(b) => b.name(),
            Range::Class(c) | Range::Enum(c) => c,
        };
        let mut doc = json!({
            "name": name,
            "iri": m.slot_iri(name),
            "range": range,
            "kind": m.kind(slot),
            "required": slot.required.unwrap_or(false),
            "multivalued": slot.multivalued.unwrap_or(false),
        });
        if let Some(d) = &slot.description {
            insert(&mut doc, "description", json!(d));
        }
        doc
    }
}

/// `value` as member `key` of the object `doc`.
fn insert(doc: &mut Value, key: &str, value: Value) {
    if let Some(o) = doc.as_object_mut() {
        o.insert(key.to_owned(), value);
    }
}

/// The leading number of a semantic version, `1` for none.
fn major(version: Option<&str>) -> String {
    version
        .and_then(|v| v.split('.').next())
        .filter(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or("1")
        .to_owned()
}

/// A stored schema's bytes and the model read from them.
type Source = (Arc<[u8]>, Arc<Model>);

struct Entry {
    view: String,
    schema: Arc<[u8]>,
    visible: Visible,
    rendered: Arc<Rendered>,
}

/// The rendered schemas of this pod, keyed by view id, the schema's bytes
/// and the visible set (LK2: a visible set is part of the key, so no
/// caller is answered from another's narrowing), at most
/// [`CACHE_ENTRIES`](crate::CACHE_ENTRIES), the oldest dropped first.
#[derive(Default)]
pub(crate) struct Cache {
    entries: Mutex<VecDeque<Entry>>,
    /// Each stored schema as read, so a request that names a class or a
    /// slot finds its IRI without reading the schema again.
    sources: Mutex<VecDeque<Source>>,
}

impl Cache {
    /// The stored schema `schema` of `view` as read; `None` when it does not
    /// load (LK4: a metaslot outside the subset closes the view's LinkML
    /// surface, the reason goes to the log).
    pub(crate) fn source(&self, view: &str, schema: &Arc<[u8]>) -> Option<Arc<Model>> {
        {
            let sources = self.sources.lock().unwrap_or_else(PoisonError::into_inner);
            let hit = sources
                .iter()
                .find(|(bytes, _)| Arc::ptr_eq(bytes, schema) || bytes == schema);
            if let Some((_, model)) = hit {
                return Some(model.clone());
            }
        }
        let model = match Model::read(schema, &LIMITS) {
            Ok(model) => Arc::new(model),
            Err(e) => {
                tracing::warn!(view, error = %e, "the view's LinkML schema does not load; its LinkML surface is closed");
                return None;
            }
        };
        let mut sources = self.sources.lock().unwrap_or_else(PoisonError::into_inner);
        if sources.len() >= crate::CACHE_ENTRIES {
            sources.pop_front();
        }
        sources.push_back((schema.clone(), model.clone()));
        Some(model)
    }

    /// The rendered schema of `view` for `visible`, rendered on a miss;
    /// `None` when the schema does not load (see [`Cache::source`]).
    pub(crate) fn get(
        &self,
        view: &str,
        schema: &Arc<[u8]>,
        visible: &Visible,
    ) -> Option<Arc<Rendered>> {
        {
            let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            let hit = entries.iter().find(|e| {
                e.view == view
                    && (Arc::ptr_eq(&e.schema, schema) || e.schema == *schema)
                    && e.visible == *visible
            });
            if let Some(e) = hit {
                return Some(e.rendered.clone());
            }
        }
        let source = self.source(view, schema)?;
        let rendered = match narrow(&source, visible, view) {
            Ok((model, left_out)) => Arc::new(Rendered::new(model, view, left_out)),
            Err(e) => {
                tracing::warn!(view, error = %e, "the view's narrowed LinkML schema does not load");
                return None;
            }
        };
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= crate::CACHE_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(Entry {
            view: view.to_owned(),
            schema: schema.clone(),
            visible: visible.clone(),
            rendered: rendered.clone(),
        });
        Some(rendered)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antares_ops::policy::{Filter, Rule};

    const SCHEMA: &[u8] = br#"{"id": "https://e.org/s", "name": "s", "version": "3.0.1",
        "classes": {"Room": {"slots": ["temperature", "secret"]}},
        "slots": {"temperature": {"range": "float"}, "secret": {}}}"#;

    fn visible(attrs: &[&str]) -> Visible {
        Visible::from_filter(&Filter {
            rules: vec![Rule {
                types: vec!["https://e.org/s/Room".into()],
                attributes: attrs
                    .iter()
                    .map(|a| format!("https://e.org/s/{a}"))
                    .collect(),
                ..Rule::default()
            }],
            ..Filter::default()
        })
    }

    #[test]
    fn the_major_is_the_leading_number_or_1() {
        assert_eq!(major(Some("3.0.1")), "3");
        assert_eq!(major(Some("12")), "12");
        assert_eq!(major(Some("v3")), "1");
        assert_eq!(major(None), "1");
    }

    #[test]
    fn linkml_schema_is_built_once_per_cache_key() {
        let cache = Cache::default();
        let schema: Arc<[u8]> = Arc::from(SCHEMA);
        let a = cache
            .get("v", &schema, &visible(&["temperature"]))
            .expect("renders");
        let again = cache
            .get("v", &Arc::from(SCHEMA), &visible(&["temperature"]))
            .expect("hit");
        assert!(
            Arc::ptr_eq(&a, &again),
            "same bytes, same visible set: one render"
        );
        let wider = cache
            .get("v", &schema, &visible(&["temperature", "secret"]))
            .expect("renders");
        assert!(!Arc::ptr_eq(&a, &wider));
        let other_view = cache
            .get("w", &schema, &visible(&["temperature"]))
            .expect("renders");
        assert!(!Arc::ptr_eq(&a, &other_view));
        assert_eq!(cache.len(), 3);
        let yaml = a.document(Artifact::LinkMl).expect("yaml");
        assert!(!String::from_utf8_lossy(&yaml).contains("secret"));
    }

    #[test]
    fn the_cache_holds_at_most_its_bound() {
        let cache = Cache::default();
        let schema: Arc<[u8]> = Arc::from(SCHEMA);
        for i in 0..=crate::CACHE_ENTRIES {
            cache.get(&format!("v{i}"), &schema, &visible(&[]));
        }
        assert_eq!(cache.len(), crate::CACHE_ENTRIES);
    }

    #[test]
    fn a_schema_outside_the_subset_renders_nothing() {
        let bad: Arc<[u8]> =
            Arc::from(&br#"{"id": "https://e.org/s", "name": "s", "imports": []}"#[..]);
        assert!(Cache::default().get("v", &bad, &visible(&[])).is_none());
    }

    #[test]
    fn the_index_lists_each_artifact_with_its_size_and_digest() {
        let r = Cache::default()
            .get("v", &Arc::from(SCHEMA), &visible(&["temperature"]))
            .expect("renders");
        let index: Value = serde_json::from_slice(&r.index).expect("json");
        let model = &index["models"][0];
        assert_eq!(index["endpoint"], "v");
        assert_eq!(
            (model["version"].as_str(), model["redacted"].as_bool()),
            (Some("3"), Some(true))
        );
        let yaml = r.document(Artifact::LinkMl).expect("yaml");
        assert_eq!(
            model["artifacts"]["model.linkml.yaml"]["sha256"],
            sha256(&yaml)
        );
        assert_eq!(model["artifacts"]["model.linkml.yaml"]["bytes"], yaml.len());
        assert_eq!(model["artifacts"].as_object().map(Map::len), Some(7));
    }
}
