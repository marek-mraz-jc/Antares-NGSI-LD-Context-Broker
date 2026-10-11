// SPDX-License-Identifier: EUPL-1.2
//! `/api` (mapping row C2, Part 1 `/req/oas30/*`): the `OpenAPI` 3.0 document of exactly what
//! this view serves, one path per collection the view shows, so no hidden type is named.

use crate::model::Collection;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The answers every operation may give besides its 200 (`/req/oas30/completeness`).
fn refusals() -> Map<String, Value> {
    let problem = |text: &str| json!({"description": text, "content": {"application/problem+json": {"schema": {"$ref": "#/components/schemas/problem"}}}});
    Map::from_iter(
        [
            ("400", "the query is not one this resource defines"),
            ("404", "no such resource in this view"),
            ("406", "the client accepts no JSON"),
            (
                "500",
                "the broker failed; the detail is logged under a request id",
            ),
        ]
        .map(|(code, text)| (code.to_owned(), problem(text))),
    )
}

fn get(id: &str, summary: &str, media: &str, parameters: &[Value]) -> Value {
    let mut responses = refusals();
    responses.insert(
        "200".into(),
        json!({"description": summary, "content": {media: {"schema": {"type": "object"}}}}),
    );
    json!({"get": {"operationId": id, "summary": summary, "parameters": parameters, "responses": responses}})
}

/// The document for the endpoint at `base` serving `collections`.
pub(crate) fn document(base: &str, collections: &BTreeMap<String, Collection>) -> Value {
    let mut paths = Map::new();
    paths.insert(
        "/".into(),
        get("landing", "the landing page", "application/json", &[]),
    );
    paths.insert(
        "/conformance".into(),
        get(
            "conformance",
            "the conformance classes",
            "application/json",
            &[],
        ),
    );
    paths.insert(
        "/api".into(),
        get(
            "api",
            "this document",
            "application/vnd.oai.openapi+json;version=3.0",
            &[],
        ),
    );
    paths.insert(
        "/collections".into(),
        get("collections", "the collections", "application/json", &[]),
    );
    let items_params = [
        json!({"$ref": "#/components/parameters/limit"}),
        json!({"$ref": "#/components/parameters/bbox"}),
        json!({"$ref": "#/components/parameters/datetime"}),
        json!({"$ref": "#/components/parameters/next"}),
    ];
    let feature_id = [
        json!({"name": "featureId", "in": "path", "required": true, "schema": {"type": "string"}}),
    ];
    for c in collections.values() {
        let at = format!("/collections/{}", c.id);
        paths.insert(
            at.clone(),
            get(
                &format!("collection-{}", c.id),
                "the collection",
                "application/json",
                &[],
            ),
        );
        paths.insert(
            format!("{at}/items"),
            get(
                &format!("items-{}", c.id),
                "a page of Features",
                "application/geo+json",
                &items_params,
            ),
        );
        paths.insert(
            format!("{at}/items/{{featureId}}"),
            get(
                &format!("item-{}", c.id),
                "one Feature",
                "application/geo+json",
                &feature_id,
            ),
        );
    }
    json!({
        "openapi": "3.0.3",
        "info": {"title": "OGC API - Features", "version": "1.0.0"},
        "servers": [{"url": base}],
        "paths": paths,
        "components": {
            "schemas": {"problem": {"type": "object", "properties": {"type": {"type": "string"}, "title": {"type": "string"}, "status": {"type": "integer"}, "detail": {"type": "string"}}}},
            "parameters": {
                "limit": {"name": "limit", "in": "query", "required": false, "style": "form", "explode": false, "schema": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 10}},
                "bbox": {"name": "bbox", "in": "query", "required": false, "style": "form", "explode": false, "schema": {"type": "array", "oneOf": [{"minItems": 4, "maxItems": 4}, {"minItems": 6, "maxItems": 6}], "items": {"type": "number"}}},
                "datetime": {"name": "datetime", "in": "query", "required": false, "style": "form", "explode": false, "schema": {"type": "string"}},
                "next": {"name": "next", "in": "query", "required": false, "description": "the token of a next link", "schema": {"type": "string"}},
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_documents_its_refusals_and_only_shown_collections_appear() {
        let car = Collection {
            id: "Car".into(),
            description: None,
            type_iri: "https://e.org/Car".into(),
            geometry: None,
            time: None,
            properties: Vec::new(),
        };
        let doc = document("http://b", &BTreeMap::from([("Car".to_owned(), car)]));
        let paths = doc
            .get("paths")
            .unwrap_or(&Value::Null)
            .as_object()
            .map(|p| p.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        assert_eq!(
            paths,
            [
                "/",
                "/api",
                "/collections",
                "/collections/Car",
                "/collections/Car/items",
                "/collections/Car/items/{featureId}",
                "/conformance"
            ]
        );
        for (_, op) in doc
            .get("paths")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            for code in ["200", "400", "404", "406", "500"] {
                assert!(
                    op.pointer(&format!("/get/responses/{code}")).is_some(),
                    "{op}"
                );
            }
        }
        assert_eq!(doc.pointer("/servers/0/url"), Some(&json!("http://b")));
    }
}
