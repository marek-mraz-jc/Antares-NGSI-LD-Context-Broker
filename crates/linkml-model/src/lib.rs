// SPDX-License-Identifier: EUPL-1.2
//! The LinkML subset the broker's surfaces use (odata-linkml-mapping.md
//! sections 8 and 9, ADR-0024): read from LinkML's resolved JSON
//! serialisation, never from YAML, with every metaslot outside the subset
//! refused (threat LK4); written as LinkML YAML, JSON Schema, a JSON-LD
//! `@context`, SHACL, OWL, RDFS and Markdown by deterministic writers that
//! quote and escape every name they print (threat LK3).
//!
//! ```
//! use linkml_model::{Limits, Model};
//!
//! let json = br#"{
//!   "id": "https://example.org/building", "name": "building",
//!   "classes": {"Room": {"slots": ["temperature"]}},
//!   "slots": {"temperature": {"range": "float", "required": true}}
//! }"#;
//! let limits = Limits { max_bytes: 1 << 20, max_depth: 32, max_elements: 10_000 };
//! let model = Model::read(json, &limits).unwrap();
//! assert!(linkml_model::yaml(&model).contains("\"temperature\":"));
//! ```
#![forbid(unsafe_code)]

mod error;
mod model;
mod schema;
mod write;

pub use error::Error;
pub use model::{Builtin, Limits, Model, Range, EDM_TYPE_TAG, KIND_TAG};
pub use schema::{Annotation, Class, Enum, PermissibleValue, Prefix, Schema, Slot, Unit};
pub use write::{context, json_schema, markdown, owl, rdfs, shacl, yaml};
