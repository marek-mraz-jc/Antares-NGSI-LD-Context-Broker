// SPDX-License-Identifier: EUPL-1.2
//! Part 4 (draft OGC 20-002r1, mapping rows W1 to W7): `POST /items`, `PUT`, `PATCH` and
//! `DELETE /items/{featureId}` as Create, Replace, Merge and Delete Entity through the view,
//! so the view's operations and the Policy decide each write and nothing bypasses `ViewOps`.

use crate::items::encode;
use crate::model::{Collection, Prop};
use crate::{crs, Problem};
use antares_ops::{Caller, OperationId, OpsRequest, ViewOps};
use axum::body::Body;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use serde_json::{json, Map, Value};

/// The largest body a write reads (threat O-class bound): above it, 413 before it is parsed.
pub(crate) const MAX_BODY: usize = 1 << 20;

/// The NGSI-LD Null a merge writes for an RFC 7396 `null` (8.4.3: it deletes the Attribute).
const NGSI_LD_NULL: &str = "urn:ngsi-ld:null";

fn bad(detail: &'static str) -> Problem {
    Problem(StatusCode::BAD_REQUEST, detail)
}

/// One write and the Feature it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Write<'a> {
    /// `POST /items` (W1).
    Create,
    /// `PUT /items/{featureId}` (W2).
    Replace(&'a str),
    /// `PATCH /items/{featureId}` (W5).
    Merge(&'a str),
    /// `DELETE /items/{featureId}` (W3).
    Delete(&'a str),
}

impl<'a> Write<'a> {
    /// The write a method is on `items` (`feature` `None`) or on one Feature.
    pub(crate) fn of(method: &Method, feature: Option<&'a str>) -> Option<Self> {
        match (method.as_str(), feature) {
            ("POST", None) => Some(Self::Create),
            ("PUT", Some(id)) => Some(Self::Replace(id)),
            ("PATCH", Some(id)) => Some(Self::Merge(id)),
            ("DELETE", Some(id)) => Some(Self::Delete(id)),
            _ => None,
        }
    }

    /// The NGSI-LD operation it runs and that operation's method.
    pub(crate) fn operation(self) -> (OperationId, Method) {
        match self {
            Self::Create => (OperationId::CreateEntity, Method::POST),
            Self::Replace(_) => (OperationId::ReplaceEntity, Method::PUT),
            Self::Merge(_) => (OperationId::MergeEntity, Method::PATCH),
            Self::Delete(_) => (OperationId::DeleteEntity, Method::DELETE),
        }
    }
}

/// One write through the view: the view's operations and the Policy decide it (W7).
pub(crate) async fn run(
    view: &ViewOps,
    caller: &Caller,
    (c, base): (&Collection, &str),
    write: Write<'_>,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, Problem> {
    if headers.contains_key("if-match") || headers.contains_key("if-unmodified-since") {
        // W6: the broker has no atomic conditional write, and an ignored precondition can lose
        // an update.
        return Err(Problem(
            StatusCode::NOT_IMPLEMENTED,
            "conditional writes are not supported",
        ));
    }
    let content_crs = headers.get("content-crs").map(|v| {
        v.to_str()
            .ok()
            .and_then(|v| crs::read(v.trim().trim_start_matches('<').trim_end_matches('>')))
    });
    if content_crs == Some(None) {
        return Err(bad(crs::UNSUPPORTED));
    }
    let path = |id: &str| {
        format!(
            "/ngsi-ld/v1/entities/{}?type={}",
            encode(id),
            encode(&c.type_iri)
        )
    };
    let typed = |mut doc: Map<String, Value>, id: &str| {
        doc.insert("id".into(), Value::from(id));
        doc.insert("type".into(), Value::from(c.type_iri.as_str()));
        doc
    };
    let (target, doc, created) = match write {
        Write::Delete(id) => (path(id), None, None),
        Write::Create => {
            let feature = read(headers, body, false).await?;
            let doc = attributes(c, &feature, false)?;
            let id = match feature.get("id") {
                None | Some(Value::Null) => format!(
                    "urn:ngsi-ld:{}:{}",
                    antares_ops::core_name(&c.type_iri),
                    uuid::Uuid::new_v4()
                ),
                Some(Value::String(id)) => id.clone(),
                Some(_) => return Err(bad("a Feature id is a URI")),
            };
            let doc = typed(doc, &id);
            ("/ngsi-ld/v1/entities".to_owned(), Some(doc), Some(id))
        }
        Write::Replace(id) | Write::Merge(id) => {
            let merge = matches!(write, Write::Merge(_));
            let feature = read(headers, body, merge).await?;
            if feature.get("id").is_some_and(|v| v != id) {
                return Err(bad("the body's id is not the Feature's"));
            }
            let doc = attributes(c, &feature, merge)?;
            let doc = if merge { doc } else { typed(doc, id) };
            (path(id), Some(doc), None)
        }
    };
    let (op, method) = write.operation();
    let mut req = OpsRequest::new(method, target);
    if let Some(doc) = doc {
        let body = Value::Object(doc).to_string();
        req = req
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(CONTENT_LENGTH, HeaderValue::from(body.len()))
            .body(body);
    }
    let refused = |status: StatusCode| {
        if status == StatusCode::NOT_FOUND {
            Problem(status, "no such Feature in this collection")
        } else {
            Problem(status, "the view refused the write")
        }
    };
    let status = match view.run(op, caller, req).await {
        Ok(answer) => answer.status(),
        Err(refusal) => refusal.status(),
    };
    if !status.is_success() {
        return Err(refused(status));
    }
    Ok(match created {
        Some(id) => {
            let at = format!("{base}/collections/{}/items/{}", c.id, encode(&id));
            // Percent-encoded ASCII is always a header value.
            let at = HeaderValue::from_str(&at).unwrap_or(HeaderValue::from_static("/"));
            (StatusCode::CREATED, [(LOCATION, at)]).into_response()
        }
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

/// The body of a write: its media type checked (415), its size bounded (413), its JSON read
/// (400). A merge takes `application/merge-patch+json` (W5), the others `GeoJSON` or JSON.
async fn read(headers: &HeaderMap, body: Body, merge: bool) -> Result<Value, Problem> {
    let media = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let fits = if merge {
        media == "application/merge-patch+json"
    } else {
        media == "application/geo+json" || media == "application/json"
    };
    if !fits {
        return Err(Problem(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "a write is application/geo+json, a PATCH application/merge-patch+json",
        ));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| Problem(StatusCode::PAYLOAD_TOO_LARGE, "the body is above 1 MiB"))?;
    serde_json::from_slice(&bytes).map_err(|_| bad("the body is no JSON"))
}

/// A Feature (or a merge patch of one) as the NGSI-LD Attributes of the Entity, by IRI: the
/// geometry as the collection's `GeoProperty`, each property by its kind. A property the
/// collection does not have, hidden or misspelt, is the same 400. A `null` deletes in a merge
/// and is absent otherwise.
fn attributes(c: &Collection, feature: &Value, merge: bool) -> Result<Map<String, Value>, Problem> {
    let Value::Object(feature) = feature else {
        return Err(bad("the body is a GeoJSON Feature"));
    };
    if !merge && feature.get("type") != Some(&json!("Feature")) {
        return Err(bad("the body is a GeoJSON Feature"));
    }
    let gone = || Value::from(NGSI_LD_NULL);
    let mut doc = Map::new();
    match (feature.get("geometry"), &c.geometry) {
        (None, _) | (Some(Value::Null), None) => {}
        (Some(Value::Null), Some(g)) => {
            if merge {
                doc.insert(g.clone(), gone());
            }
        }
        (Some(geometry), Some(g)) if geometry.get("type").is_some_and(Value::is_string) => {
            doc.insert(g.clone(), json!({"type": "GeoProperty", "value": geometry}));
        }
        _ => return Err(bad("geometry is a GeoJSON geometry or null")),
    }
    let properties = match feature.get("properties") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(p)) => p.clone(),
        Some(_) => return Err(bad("properties is an object")),
    };
    for (name, value) in properties {
        let Some(p) = c.properties.iter().find(|p| p.name == name) else {
            return Err(bad(
                "the body names a property this collection does not have",
            ));
        };
        match (value, merge) {
            (Value::Null, true) => {
                doc.insert(p.iri.clone(), gone());
            }
            (Value::Null, false) => {}
            (value, _) => {
                doc.insert(p.iri.clone(), attribute(p, &value));
            }
        }
    }
    Ok(doc)
}

/// One property's value as its normalized NGSI-LD Attribute, of the kind the schema names.
fn attribute(p: &Prop, value: &Value) -> Value {
    let (kind, member) = match p.kind.as_str() {
        "Relationship" => ("Relationship", "object"),
        "ListRelationship" => ("ListRelationship", "objectList"),
        "GeoProperty" => ("GeoProperty", "value"),
        "JsonProperty" => ("JsonProperty", "json"),
        "VocabProperty" => ("VocabProperty", "vocab"),
        "LanguageProperty" => ("LanguageProperty", "languageMap"),
        "ListProperty" => ("ListProperty", "valueList"),
        _ => ("Property", "value"),
    };
    json!({"type": kind, member: value})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_writes_its_own_member() {
        let kinds = [
            ("Relationship", "object"),
            ("ListRelationship", "objectList"),
            ("GeoProperty", "value"),
            ("JsonProperty", "json"),
            ("VocabProperty", "vocab"),
            ("LanguageProperty", "languageMap"),
            ("ListProperty", "valueList"),
            ("Property", "value"),
        ];
        for (kind, member) in kinds {
            let p = Prop {
                name: "p".into(),
                iri: "https://e.org/p".into(),
                schema: json!({}),
                role: None,
                unit: None,
                description: None,
                sortable: false,
                kind: kind.into(),
                reference: false,
                target: None,
            };
            assert_eq!(
                attribute(&p, &json!(1)),
                json!({"type": kind, member: 1}),
                "{kind}"
            );
        }
    }
}
