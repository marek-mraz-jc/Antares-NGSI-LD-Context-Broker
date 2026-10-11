// SPDX-License-Identifier: EUPL-1.2
//! `/collections/{c}/schema` and `/collections/{c}/sortables` (mapping rows S1 to S3, S5,
//! OGC 23-058r1 Part 5): JSON Schema 2020-12 of the Features a collection serves, from the
//! Tenant's `LinkML` schema intersected with what the view shows, so no hidden Attribute is
//! named.

use crate::model::{Collection, Prop};
use serde_json::{Map, Value};

/// The JSON Schema dialect Part 5 requires (`/req/schemas/json-schema`).
const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// The schema of `c` served at `id`: its `id`, its geometry and every property the view shows
/// (`sortables`: only those `sortby` may name), in Feature order (`x-ogc-propertySeq`).
pub(crate) fn document(c: &Collection, id: &str, sortables: bool) -> Value {
    let identity = fields([
        ("title", Some("id".into())),
        ("type", Some("string".into())),
        ("format", Some("uri".into())),
        ("x-ogc-role", Some("id".into())),
    ]);
    let geometry = c.geometry.as_ref().map(|iri| {
        fields([
            ("title", Some("geometry".into())),
            ("format", Some("geometry-any".into())),
            ("x-ogc-role", Some("primary-geometry".into())),
            ("x-ogc-definition", Some(iri.as_str().into())),
        ])
    });
    let head = [("id".to_owned(), identity)]
        .into_iter()
        .chain(geometry.map(|g| ("geometry".to_owned(), g)))
        .filter(|_| !sortables);
    let props = c
        .properties
        .iter()
        .filter(|p| !sortables || p.sortable)
        .map(|p| (p.name.clone(), property(p)));
    let properties: Map<String, Value> = (1_u64..)
        .zip(head.chain(props))
        .map(|(seq, (name, mut schema))| {
            schema.insert("x-ogc-propertySeq".into(), seq.into());
            (name, Value::Object(schema))
        })
        .collect();
    Value::Object(fields([
        ("$schema", Some(DIALECT.into())),
        ("$id", Some(id.into())),
        ("type", Some("object".into())),
        ("title", Some(c.id.as_str().into())),
        ("description", c.description.as_deref().map(Value::from)),
        ("properties", Some(Value::Object(properties))),
    ]))
}

/// The members that are present, in order.
fn fields<const N: usize>(members: [(&str, Option<Value>); N]) -> Map<String, Value> {
    members
        .into_iter()
        .filter_map(|(key, value)| Some((key.to_owned(), value?)))
        .collect()
}

/// One property: its value's schema with its title, role, unit, definition and description.
fn property(p: &Prop) -> Map<String, Value> {
    let role = if p.reference {
        Some("reference")
    } else {
        p.role.as_deref()
    };
    let mut out = p.schema.as_object().cloned().unwrap_or_default();
    out.extend(fields([
        ("title", Some(p.name.as_str().into())),
        ("description", p.description.as_deref().map(Value::from)),
        ("x-ogc-role", role.map(Value::from)),
        ("x-ogc-unit", p.unit.as_deref().map(Value::from)),
        ("x-ogc-collectionId", p.target.as_deref().map(Value::from)),
        ("x-ogc-definition", Some(p.iri.as_str().into())),
    ]));
    out
}
