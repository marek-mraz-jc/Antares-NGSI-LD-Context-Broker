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

use crate::below;
use crate::edm::{Member, Set, Shape};
use crate::filter::Translated;
use crate::read::{
    body_id, canonical_key, encode, entity, key_id, not_found, not_implemented, refused, resource,
    run, run_request, set_of, written, Reads,
};
use antares_ops::{Caller, OperationId, OpsRequest};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, IF_MATCH, IF_NONE_MATCH, LOCATION};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use odata_axum::{json_level, ODataErrorResponse, ODataRequest};
use odata_json::{
    read_entity_body, Bind, BodyError, BodyLimits, Ctx, ODataError, ODataVersion, Target, WriteKind,
};
use odata_url::{Key, ODataUri, QueryOptions, Segment};
use serde_json::{json, Map, Value};

/// The NGSI-LD Null (8.4.2, 8.4.3): an update or a merge that names it
/// deletes the Attribute.
pub(crate) const NGSI_LD_NULL: &str = "urn:ngsi-ld:null";

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
        (
            &Method::PUT | &Method::PATCH | &Method::DELETE,
            [Segment::EntitySet(set), Segment::Key(key), Segment::Property(name)],
        )
        | (
            &Method::DELETE,
            [Segment::EntitySet(set), Segment::Key(key), Segment::Property(name), Segment::Value],
        ) => Form::Attribute(set, key, name, method.clone()),
        (
            &Method::PUT | &Method::DELETE,
            [Segment::EntitySet(set), Segment::Key(key), Segment::Navigation(nav), Segment::Ref],
        ) => Form::Reference(set, key, nav, method == Method::DELETE),
        _ => return written(method, &request.uri, version),
    };
    let Some(op) = form.op(reads) else {
        // W19: no operation removes one object of a Relationship.
        return not_implemented(version).into_response();
    };
    // Rule 4: a write the view does not offer is refused before its body
    // is read, whatever the body holds.
    if !reads.view.allows(op) || !form.taken(reads) {
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
        Form::Attribute(set, key, name, method) => {
            below::property(reads, caller, &ctx, (set, key, name), &method, body).await
        }
        Form::Reference(set, key, nav, _) => {
            below::reference(reads, caller, &ctx, (set, key, nav), op, body).await
        }
    };
    answer.unwrap_or_else(IntoResponse::into_response)
}

/// The write a method and a path stand for.
enum Form<'u> {
    Create(&'u str),
    Change(&'u str, &'u Key, WriteKind),
    Delete(&'u str, &'u Key),
    /// A structural property: `PUT`, `PATCH` or `DELETE` (W10, W11, W13).
    Attribute(&'u str, &'u Key, &'u str, Method),
    /// A navigation's `$ref`: set (`false`) or removed (W16, W17, W20).
    Reference(&'u str, &'u Key, &'u str, bool),
}

impl Form<'_> {
    /// Whether the set's class takes this write (C10a): a create, a delete,
    /// or an update of the Entity, a property or a reference.
    fn taken(&self, reads: &Reads) -> bool {
        let (Self::Create(set)
        | Self::Change(set, ..)
        | Self::Delete(set, _)
        | Self::Attribute(set, ..)
        | Self::Reference(set, ..)) = self;
        // An unknown set is the write's own 404 (the path resolves against
        // the same model first, so it does not get here).
        reads.edm.sets.get(*set).is_none_or(|s| match self {
            Self::Create(_) => s.writable.insert,
            Self::Delete(..) => s.writable.delete,
            _ => s.writable.update,
        })
    }

    /// The operation the write is; `None` for a form none is (W19).
    fn op(&self, reads: &Reads) -> Option<OperationId> {
        Some(match self {
            Self::Create(_) => OperationId::CreateEntity,
            Self::Change(.., WriteKind::Replace) => OperationId::ReplaceEntity,
            Self::Change(..) => OperationId::MergeEntity,
            Self::Delete(..) => OperationId::DeleteEntity,
            Self::Attribute(.., method) if method == Method::PUT => OperationId::ReplaceAttribute,
            Self::Attribute(.., method) if method == Method::PATCH => {
                OperationId::PartialAttributeUpdate
            }
            Self::Attribute(..) => OperationId::DeleteAttribute,
            Self::Reference(set, _, nav, remove) => {
                let collection = reads
                    .edm
                    .sets
                    .get(*set)
                    .is_some_and(|s| crate::paths::nav_of(s, nav).2);
                match (collection, remove) {
                    (true, true) => return None,
                    (_, true) => OperationId::DeleteAttribute,
                    (_, false) => OperationId::AppendAttributes,
                }
            }
        })
    }
}

/// `POST {Set}` (W1, W2): Create Entity of the set's type with the body's
/// id, or a minted one when the body leaves out the key; `201` with the Entity as the view reads it and its `Location`, or
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
    // The reader checks a key against its Edm type, and leaves one out
    // only where `$metadata` says the service computes it (W1).
    let (id, own) = match payload.key.first() {
        Some((_, value)) => (
            body_id(set, value).ok_or_else(|| body_error(&invalid_key(), version))?,
            value.clone(),
        ),
        None => minted(set, set_name),
    };
    let mut doc = attributes(set, &payload.properties, WriteKind::Create);
    // E2a: the identifier is an Attribute of its own too.
    let identifier = set
        .identifier
        .as_ref()
        .and_then(|i| set.members.get(&i.member));
    doc.extend(identifier.and_then(|m| Some((m.iri.clone(), attribute(m, &own)?))));
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
    let key = canonical_key(set, &id);
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

/// W1: the URN of a create that names no key and the key value it is
/// under: `urn:ngsi-ld:{Set}:{uuid}`, or the E2a identifier's prefix and
/// the uuid as its value.
fn minted(set: &Set, set_name: &str) -> (String, Value) {
    let uuid = uuid::Uuid::new_v4().to_string();
    let prefix = set
        .identifier
        .as_ref()
        .map_or_else(|| format!("urn:ngsi-ld:{set_name}:"), |i| i.prefix.clone());
    (format!("{prefix}{uuid}"), Value::from(uuid))
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
    let id = id_of(set, key, version)?;
    let resource = resource(reads, set_name, version)?;
    let payload = read_entity_body(ctx, &resource, kind, body, &BodyLimits::strict())
        .map_err(|e| body_error(&e, version))?;
    if payload
        .key
        .iter()
        .any(|(_, value)| body_id(set, value).as_deref() != Some(id.as_str()))
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
    let id = id_of(set, key, version)?;
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
/// as the member's value (8.4.3: it deletes the Attribute, whatever its
/// kind); in a create or a replace it is an absent member. A member no
/// write may name ([`Shape::writable`]) was refused by the body reader
/// through the Capabilities that list it.
fn attributes(set: &Set, properties: &[(String, Value)], kind: WriteKind) -> Map<String, Value> {
    let mut doc = Map::new();
    for (name, value) in properties {
        let member = set.members.get(name);
        let iri = match (member, name.as_str()) {
            (Some(member), _) => member.iri.as_str(),
            (None, "scope") => "scope",
            (None, _) => continue,
        };
        let written = match (value, kind, member) {
            (Value::Null, WriteKind::Update, _) => Some(Value::from(NGSI_LD_NULL)),
            (Value::Null, ..) => None,
            (value, _, None) => Some(value.clone()),
            (value, _, Some(member)) => attribute(member, value),
        };
        doc.extend(written.map(|v| (iri.to_owned(), v)));
    }
    doc
}

/// One member's value as its normalized NGSI-LD Attribute, of the kind
/// the schema names; `None` for a member no write may name.
pub(crate) fn attribute(member: &Member, value: &Value) -> Option<Value> {
    Some(match (&member.shape, member.list) {
        (Shape::Value { .. }, true) => json!({"type": "ListProperty", "valueList": value}),
        (Shape::Value { .. }, false) => json!({"type": "Property", "value": value}),
        (Shape::Geo, _) => json!({"type": "GeoProperty", "value": value}),
        (Shape::Json, _) => json!({"type": "JsonProperty", "json": value}),
        (Shape::Vocab, _) => json!({"type": "VocabProperty", "vocab": value}),
        (Shape::Language | Shape::Url { .. } | Shape::Nav { .. }, _) => return None,
    })
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
        let urn = visible(reads, caller, &bind.target, version).await?;
        doc.insert(
            member.iri.clone(),
            json!({"type": "Relationship", "object": urn}),
        );
    }
    Ok(())
}

/// The id of a bound Entity, after Retrieve Entity of the target's set
/// reads it under the view: hidden, absent and of another type are the
/// same `404` (T-3441 section 6, O15).
pub(crate) async fn visible(
    reads: &Reads,
    caller: &Caller,
    target: &Target,
    version: ODataVersion,
) -> Result<String, ODataErrorResponse> {
    let set = set_of(reads, &target.set, version)?;
    let urn = id_of(set, &target.key, version)?;
    run(
        reads,
        caller,
        OperationId::RetrieveEntity,
        typed(&format!("/ngsi-ld/v1/entities/{}", encode(&urn)), set),
        &Translated::default(),
        version,
    )
    .await?;
    Ok(urn)
}

/// Runs one write operation; its status alone counts.
pub(crate) async fn send(
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
pub(crate) fn typed(target: &str, set: &Set) -> String {
    format!("{target}?type={}", encode(&set.type_iri))
}

/// The id a key names in `set`; another key form names no Entity.
pub(crate) fn id_of(
    set: &Set,
    key: &Key,
    version: ODataVersion,
) -> Result<String, ODataErrorResponse> {
    key_id(set, key, &QueryOptions::default()).ok_or_else(|| not_found(version))
}

fn invalid_key() -> BodyError {
    BodyError::InvalidValue {
        property: "id".into(),
    }
}

pub(crate) fn body_error(error: &BodyError, version: ODataVersion) -> ODataErrorResponse {
    ODataErrorResponse {
        status: StatusCode::from_u16(error.status()).unwrap_or(StatusCode::BAD_REQUEST),
        error: ODataError::from(error),
        version: Some(version),
    }
}

pub(crate) fn no_content(version: ODataVersion) -> Response {
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
            list: false,
            written: crate::edm::Written::Always,
        };
        let set = Set {
            type_iri: "https://e.org/T".into(),
            identifier: None,
            writable: crate::edm::Writable::default(),
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
