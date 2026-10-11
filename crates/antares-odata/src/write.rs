// SPDX-License-Identifier: EUPL-1.2
//! Writes of one Entity (odata-linkml-mapping.md section 6): `POST {Set}`
//! as Create Entity with single-valued binds (W1, W2), `PATCH` and `PUT`
//! of an Entity as Merge and Replace Entity (W4, W6), `DELETE` as Delete
//! Entity (W9). Each is one operation of the view through `ViewOps::run`,
//! so the policy gate fires and the view's rule 7 holds in the handler; a
//! bind reads its target first with Retrieve Entity under the same view,
//! and a target the view hides answers like an absent one (D7, O8). The
//! body is written in the normalized form, so no kind is guessed from a
//! value. The other write forms answer `501` or `405` before any operation.

use crate::edm::{Set, Shape};
use crate::filter::Translated;
use crate::read::{
    encode, entity, not_found, not_implemented, refused, resource, run, run_request, set_of,
    written, Reads,
};
use antares_ops::{Caller, OperationId, OpsRequest};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, IF_MATCH, IF_NONE_MATCH, LOCATION};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use odata_axum::{json_level, ODataErrorResponse, ODataRequest};
use odata_json::{
    read_entity_body, Bind, BodyError, BodyLimits, Ctx, ODataError, ODataVersion, WriteKind,
};
use odata_url::{Key, KeyValue, Literal, ODataUri, QueryOptions, Segment};
use serde_json::{json, Map, Value};

/// The NGSI-LD Null (8.4.3): a merge that names it deletes the Attribute.
const NGSI_LD_NULL: &str = "urn:ngsi-ld:null";

/// One write request below the service root.
pub(crate) async fn serve(
    reads: &Reads,
    method: &Method,
    caller: &Caller,
    request: &ODataRequest,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    let version = request.version;
    let ODataUri::Resource { path, options } = &request.uri else {
        return written(method, &request.uri, version);
    };
    let form = match (method, path.as_slice()) {
        (&Method::POST, [Segment::EntitySet(set)]) => Form::Create(set),
        (&Method::PATCH, [Segment::EntitySet(set), Segment::Key(key)]) => {
            Form::Change(set, key, WriteKind::Update)
        }
        (&Method::PUT, [Segment::EntitySet(set), Segment::Key(key)]) => {
            Form::Change(set, key, WriteKind::Replace)
        }
        (&Method::DELETE, [Segment::EntitySet(set), Segment::Key(key)]) => Form::Delete(set, key),
        _ => return written(method, &request.uri, version),
    };
    // Rule 4: a write the view does not offer is refused before its body
    // is read, whatever the body holds.
    if !reads.view.allows(form.op()) {
        return refused(StatusCode::METHOD_NOT_ALLOWED, version).into_response();
    }
    // W32: the broker has no conditional write; options shape a read's
    // answer, and a write answers no representation but a create's.
    if headers.contains_key(IF_MATCH)
        || headers.contains_key(IF_NONE_MATCH)
        || *options != QueryOptions::default()
    {
        return not_implemented(version).into_response();
    }
    let level = match json_level(request, headers, reads.state.max_header_len) {
        Ok(level) => level,
        Err(refused) => return refused.into_response(),
    };
    let ctx = Ctx {
        service_root: &reads.state.service_root,
        level,
        version,
        metadata_etag: reads.state.metadata_etag.as_deref(),
    };
    let answer = match form {
        Form::Create(set) => create(reads, caller, &ctx, set, headers, body).await,
        Form::Change(set, key, kind) => change(reads, caller, &ctx, (set, key), kind, body).await,
        Form::Delete(set, key) => delete(reads, caller, set, key, version).await,
    };
    answer.unwrap_or_else(IntoResponse::into_response)
}

/// The write a method and a path stand for.
enum Form<'u> {
    Create(&'u str),
    Change(&'u str, &'u Key, WriteKind),
    Delete(&'u str, &'u Key),
}

impl Form<'_> {
    /// The operation the write is.
    fn op(&self) -> OperationId {
        match self {
            Self::Create(_) => OperationId::CreateEntity,
            Self::Change(.., WriteKind::Replace) => OperationId::ReplaceEntity,
            Self::Change(..) => OperationId::MergeEntity,
            Self::Delete(..) => OperationId::DeleteEntity,
        }
    }
}

/// `POST {Set}` (W1, W2): Create Entity of the set's type with the body's
/// id; `201` with the Entity as the view reads it and its `Location`, or
/// `204` under `Prefer: return=minimal`.
async fn create(
    reads: &Reads,
    caller: &Caller,
    ctx: &Ctx<'_>,
    set_name: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, ODataErrorResponse> {
    let version = ctx.version;
    let set = set_of(reads, set_name, version)?;
    let resource = resource(reads, set_name, version)?;
    let payload = read_entity_body(
        ctx,
        &resource,
        WriteKind::Create,
        body,
        &BodyLimits::strict(),
    )
    .map_err(|e| body_error(&e, version))?;
    // The reader requires the key and types it `Edm.String`.
    let id = payload
        .key
        .first()
        .and_then(|(_, id)| id.as_str())
        .unwrap_or_default()
        .to_owned();
    let mut doc = attributes(set, &payload.properties, WriteKind::Create);
    bind(reads, caller, set, &payload.binds, &mut doc, version).await?;
    doc.insert("id".into(), Value::from(id.as_str()));
    doc.insert("type".into(), Value::from(set.type_iri.as_str()));
    send(
        reads,
        caller,
        OperationId::CreateEntity,
        Method::POST,
        "/ngsi-ld/v1/entities".into(),
        Some(doc),
        version,
    )
    .await?;
    let key = Key::Single(KeyValue::Literal(Literal::String(id)));
    let location = format!(
        "{}{set_name}{}",
        ctx.service_root,
        Segment::Key(key.clone())
    );
    let minimal = headers
        .get_all("Prefer")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.contains("return=minimal"));
    let mut answer = if minimal {
        no_content(version)
    } else {
        // The view reads it as any other Entity: what it hides stays out.
        let mut read = entity(reads, caller, ctx, set_name, &key, &QueryOptions::default()).await?;
        *read.status_mut() = StatusCode::CREATED;
        read
    };
    // A key is a URL segment: always a header value.
    let location = HeaderValue::from_str(&location).unwrap_or(HeaderValue::from_static("/"));
    if minimal {
        answer
            .headers_mut()
            .insert("OData-EntityId", location.clone());
    }
    answer.headers_mut().insert(LOCATION, location);
    Ok(answer)
}

/// `PATCH` (W4) and `PUT` (W6) of an Entity: Merge or Replace Entity of
/// the set's type, `204`. A key in the body must be the URL's; an Entity
/// that is absent, of another type or hidden is `404`, never created
/// (W7).
async fn change(
    reads: &Reads,
    caller: &Caller,
    ctx: &Ctx<'_>,
    (set_name, key): (&str, &Key),
    kind: WriteKind,
    body: &[u8],
) -> Result<Response, ODataErrorResponse> {
    let version = ctx.version;
    let set = set_of(reads, set_name, version)?;
    let id = id_of(key, version)?;
    let resource = resource(reads, set_name, version)?;
    let payload = read_entity_body(ctx, &resource, kind, body, &BodyLimits::strict())
        .map_err(|e| body_error(&e, version))?;
    if payload
        .key
        .iter()
        .any(|(_, value)| value.as_str() != Some(id.as_str()))
    {
        return Err(body_error(&invalid_key(), version));
    }
    let mut doc = attributes(set, &payload.properties, kind);
    bind(reads, caller, set, &payload.binds, &mut doc, version).await?;
    let (op, method) = if kind == WriteKind::Replace {
        doc.insert("id".into(), Value::from(id.as_str()));
        doc.insert("type".into(), Value::from(set.type_iri.as_str()));
        (OperationId::ReplaceEntity, Method::PUT)
    } else {
        (OperationId::MergeEntity, Method::PATCH)
    };
    send(
        reads,
        caller,
        op,
        method,
        typed(&format!("/ngsi-ld/v1/entities/{}", encode(&id)), set),
        Some(doc),
        version,
    )
    .await?;
    Ok(no_content(version))
}

/// `DELETE {Set}('{id}')` (W9): Delete Entity of the set's type, `204`.
async fn delete(
    reads: &Reads,
    caller: &Caller,
    set_name: &str,
    key: &Key,
    version: ODataVersion,
) -> Result<Response, ODataErrorResponse> {
    let set = set_of(reads, set_name, version)?;
    let id = id_of(key, version)?;
    send(
        reads,
        caller,
        OperationId::DeleteEntity,
        Method::DELETE,
        typed(&format!("/ngsi-ld/v1/entities/{}", encode(&id)), set),
        None,
        version,
    )
    .await?;
    Ok(no_content(version))
}

/// The body's structural members as normalized NGSI-LD Attributes keyed by
/// IRI, `scope` as the Entity's. A `null` in a merge is the NGSI-LD Null
/// in the Attribute's own member (8.4.3: it deletes the Attribute); in a
/// create or a replace it is an absent member. A member no write may name ([`Shape::writable`]) was refused by
/// the body reader through the Capabilities that list it.
fn attributes(set: &Set, properties: &[(String, Value)], kind: WriteKind) -> Map<String, Value> {
    let mut doc = Map::new();
    for (name, value) in properties {
        let value = match (value, kind) {
            (Value::Null, WriteKind::Update) => Value::from(NGSI_LD_NULL),
            (Value::Null, _) => continue,
            (value, _) => value.clone(),
        };
        if name == "scope" {
            doc.insert("scope".into(), value);
            continue;
        }
        let Some(member) = set.members.get(name) else {
            continue;
        };
        let attribute = match (&member.shape, value) {
            (Shape::Value { .. }, value) => json!({"type": "Property", "value": value}),
            (Shape::Geo, value) => json!({"type": "GeoProperty", "value": value}),
            (Shape::Json, value) => json!({"type": "JsonProperty", "json": value}),
            (Shape::Vocab, value) => json!({"type": "VocabProperty", "vocab": value}),
            (Shape::Language | Shape::Url { .. } | Shape::Nav { .. }, _) => continue,
        };
        doc.insert(member.iri.clone(), attribute);
    }
    doc
}

/// Each bind as a Relationship, after its target is read under the view
/// (W2): a target the view or the engine hides, an absent one and one of
/// another type are the same `404`, and nothing is written.
async fn bind(
    reads: &Reads,
    caller: &Caller,
    set: &Set,
    binds: &[Bind],
    doc: &mut Map<String, Value>,
    version: ODataVersion,
) -> Result<(), ODataErrorResponse> {
    for bind in binds {
        // The body reader resolved both against the same model.
        let member = set
            .members
            .get(&bind.navigation)
            .ok_or(not_found(version))?;
        let target = set_of(reads, &bind.target.set, version)?;
        let urn = id_of(&bind.target.key, version)?;
        run(
            reads,
            caller,
            OperationId::RetrieveEntity,
            typed(&format!("/ngsi-ld/v1/entities/{}", encode(&urn)), target),
            &Translated::default(),
            version,
        )
        .await?;
        doc.insert(
            member.iri.clone(),
            json!({"type": "Relationship", "object": urn}),
        );
    }
    Ok(())
}

/// Runs one write operation; its status alone counts.
async fn send(
    reads: &Reads,
    caller: &Caller,
    op: OperationId,
    method: Method,
    target: String,
    doc: Option<Map<String, Value>>,
    version: ODataVersion,
) -> Result<(), ODataErrorResponse> {
    let mut req = OpsRequest::new(method, target);
    if let Some(doc) = doc {
        let body = Value::Object(doc).to_string();
        req = req
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(CONTENT_LENGTH, HeaderValue::from(body.len()))
            .body(body);
    }
    run_request(reads, caller, op, req, version).await.map(drop)
}

/// `target` narrowed to the set's type: an Entity of another type is not
/// known to the operation (5.6.6.4), tested under the store's row lock.
fn typed(target: &str, set: &Set) -> String {
    format!("{target}?type={}", encode(&set.type_iri))
}

/// The id a key names; another key form names no Entity.
fn id_of(key: &Key, version: ODataVersion) -> Result<String, ODataErrorResponse> {
    crate::read::key_id(key, &QueryOptions::default()).ok_or_else(|| not_found(version))
}

fn invalid_key() -> BodyError {
    BodyError::InvalidValue {
        property: "id".into(),
    }
}

fn body_error(error: &BodyError, version: ODataVersion) -> ODataErrorResponse {
    ODataErrorResponse {
        status: StatusCode::from_u16(error.status()).unwrap_or(StatusCode::BAD_REQUEST),
        error: ODataError::from(error),
        version: Some(version),
    }
}

fn no_content(version: ODataVersion) -> Response {
    (
        StatusCode::NO_CONTENT,
        [("odata-version", HeaderValue::from_static(version.as_str()))],
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edm::Member;
    use std::collections::BTreeMap;

    #[test]
    fn a_member_no_write_may_name_is_never_written() {
        let member = |iri: &str, shape| Member {
            iri: iri.into(),
            shape,
        };
        let set = Set {
            type_iri: "https://e.org/T".into(),
            members: BTreeMap::from([
                (
                    "url".into(),
                    member("https://e.org/u", Shape::Url { collection: false }),
                ),
                ("lang".into(), member("https://e.org/l", Shape::Language)),
                (
                    "nav".into(),
                    member(
                        "https://e.org/n",
                        Shape::Nav {
                            target: "T".into(),
                            collection: false,
                        },
                    ),
                ),
            ]),
        };
        let named = |n: &str| (n.to_owned(), json!("x"));
        let doc = attributes(
            &set,
            &[named("url"), named("lang"), named("nav"), named("unknown")],
            WriteKind::Create,
        );
        assert!(doc.is_empty(), "{doc:?}");
    }
}
