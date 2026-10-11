// SPDX-License-Identifier: EUPL-1.2
//! Reads (odata-linkml-mapping.md 5.1): an EntitySet (Q29) as Query
//! Entities, one Entity by key (Q30) as Retrieve Entity, `/$count` (Q13),
//! with `$filter` and `$orderby` ([`crate::filter`]), `$select`, `$top`,
//! `$skip` and `$count`. Each request is one
//! operation through `ViewOps::run`; the NGSI-LD answer is mapped back
//! through the model, so a member the model does not hold is never written.
//! The options and forms of later rounds answer `501` before any operation
//! runs.

use crate::edm::{base_member, Edm, Set, Shape};
use crate::filter::{order, translate, Refused, Translated};
use antares_ops::{core_name, Caller, OperationId, OpsRequest, Refusal, ViewOps};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use odata_axum::{json_level, json_response, ODataErrorResponse, ODataRequest, ODataState};
use odata_json::{write_collection, write_entity, Ctx, EntityValue, ODataVersion, Page, Resource};
use odata_url::{Key, KeyValue, Literal, ODataUri, QueryOptions, Segment, SelectItem};
use serde_json::Value;
use std::sync::Arc;

/// The page size when neither `$top` nor `odata.maxpagesize` is smaller.
pub const DEFAULT_PAGE: u64 = 100;

/// The largest page: the NGSI-LD `limit` bound of the broker.
pub const MAX_PAGE: u64 = 1000;

/// What a read needs besides the request.
pub(crate) struct Reads {
    pub(crate) view: ViewOps,
    pub(crate) edm: Arc<Edm>,
    pub(crate) state: Arc<ODataState>,
}

/// One request below the service root that is not the service or metadata
/// document.
pub(crate) async fn serve(
    reads: &Reads,
    method: &Method,
    caller: &Caller,
    request: &ODataRequest,
    headers: &HeaderMap,
) -> Response {
    let version = request.version;
    if method != Method::GET {
        return not_allowed(version);
    }
    let level = match json_level(request, headers, reads.state.max_header_len) {
        Ok(level) => level,
        Err(refused) => return refused.into_response(),
    };
    let (ODataUri::Resource { path, options }, None) = (&request.uri, later(&request.uri)) else {
        return not_implemented(version).into_response();
    };
    let ctx = Ctx {
        service_root: &reads.state.service_root,
        level,
        version,
    };
    let answer = match path.as_slice() {
        [Segment::EntitySet(set)] => {
            collection(reads, caller, &ctx, set, path, options, headers).await
        }
        [Segment::EntitySet(set), Segment::Count] => {
            count(reads, caller, set, options, version).await
        }
        [Segment::EntitySet(set), Segment::Key(key)] => {
            entity(reads, caller, &ctx, set, key, options).await
        }
        _ => Err(not_implemented(version)),
    };
    answer.unwrap_or_else(IntoResponse::into_response)
}

/// Why a parsed URL is answered `501` in this round: a form or an option a
/// later round maps. `None` for what this round serves.
fn later(uri: &ODataUri) -> Option<&'static str> {
    let ODataUri::Resource { options, .. } = uri else {
        return Some("this resource is not served");
    };
    let refused = [
        (options.expand.is_some(), "$expand"),
        (options.skiptoken.is_some(), "$skiptoken"),
        (options.id.is_some(), "$id"),
        (
            options.schemaversion.as_deref().is_some_and(|v| v != "*"),
            "$schemaversion",
        ),
    ];
    refused
        .iter()
        .find(|(asked, _)| *asked)
        .map(|(_, name)| *name)
}

/// The members `$select` names, `None` for every member. A path through a
/// complex property, a navigation property or an annotation is a later
/// round's.
fn selected<'s>(set: &'s Set, options: &'s QueryOptions) -> Result<Option<Vec<&'s str>>, ()> {
    let Some(items) = &options.select else {
        return Ok(None);
    };
    let mut names = Vec::new();
    for item in items {
        match item {
            SelectItem::Star => return Ok(None),
            SelectItem::Path(path) => match path.as_slice() {
                [name] if base_member(name) => names.push(name.as_str()),
                [name]
                    if set
                        .members
                        .get(name)
                        .is_some_and(|m| !matches!(m.shape, Shape::Nav { .. })) =>
                {
                    names.push(name.as_str());
                }
                _ => return Err(()),
            },
            SelectItem::Annotation(_) => return Err(()),
        }
    }
    Ok(Some(names))
}

/// The NGSI-LD query of an EntitySet: its type, the Attributes selected
/// (`attrs`, which takes IRIs; the core members always come), the system
/// members only when selected (E6).
fn query(set: &Set, picked: Option<&[&str]>, filter: &Translated) -> String {
    let mut q = format!("type={}", encode(&set.type_iri));
    if let Some(ids) = &filter.ids {
        let ids: Vec<String> = ids.iter().map(|id| encode(id)).collect();
        q.push_str("&id=");
        q.push_str(&ids.join(","));
    }
    if let Some(text) = &filter.q {
        q.push_str("&q=");
        q.push_str(&encode(text));
    }
    if let Some(keys) = &filter.order {
        q.push_str("&orderBy=");
        q.push_str(&encode(keys));
    }
    if let Some(names) = picked {
        let attrs: Vec<String> = names
            .iter()
            .filter_map(|name| set.members.get(*name))
            .map(|m| encode(&m.iri))
            .collect();
        if !attrs.is_empty() {
            q.push_str("&attrs=");
            q.push_str(&attrs.join(","));
        }
        if names
            .iter()
            .any(|m| matches!(*m, "createdAt" | "modifiedAt"))
        {
            q.push_str("&options=sysAttrs");
        }
    }
    q
}

async fn collection(
    reads: &Reads,
    caller: &Caller,
    ctx: &Ctx<'_>,
    set_name: &str,
    path: &[Segment],
    options: &QueryOptions,
    headers: &HeaderMap,
) -> Result<Response, ODataErrorResponse> {
    let version = ctx.version;
    let set = set_of(reads, set_name, version)?;
    let picked = selected(set, options).map_err(|()| not_implemented(version))?;
    let filter = filtered(set, options, version)?;
    let skip = options.skip.unwrap_or(0);
    let page = options
        .top
        .unwrap_or(u64::MAX)
        .min(max_page(headers, reads.state.max_header_len))
        .min(MAX_PAGE);
    let target = format!(
        "/ngsi-ld/v1/entities?{}&limit={page}&offset={skip}&count=true",
        query(set, picked.as_deref(), &filter),
    );
    let (docs, total) = run(
        reads,
        caller,
        OperationId::QueryEntities,
        target,
        &filter,
        version,
    )
    .await?;
    let entities: Vec<EntityValue> = docs
        .iter()
        .map(|doc| entity_value(reads, set_name, set, doc, picked.as_deref(), &filter))
        .collect();
    let served = u64::try_from(entities.len()).unwrap_or(u64::MAX);
    let next = skip.saturating_add(served);
    let end = options
        .top
        .map_or(total, |top| total.min(skip.saturating_add(top)));
    let next_link = (served > 0 && next < end).then(|| {
        let rest = QueryOptions {
            skip: Some(next),
            top: options.top.map(|top| top.saturating_sub(served)),
            ..options.clone()
        };
        let uri = ODataUri::Resource {
            path: path.to_vec(),
            options: rest,
        };
        format!("{}{uri}", reads.state.service_root)
    });
    let names = picked.unwrap_or_default();
    let resource = resource(reads, set_name, version)?;
    let resource = if options.select.is_some() && !names.is_empty() {
        resource.select(&names)
    } else {
        resource
    };
    let page = Page {
        count: (options.count == Some(true)).then_some(total),
        next_link: next_link.as_deref(),
    };
    let mut body = Vec::new();
    // A Vec takes every byte: the write cannot fail.
    let _ = write_collection(ctx, &resource, &entities, &page, &mut body);
    Ok(json_response(version, ctx.level, body))
}

async fn count(
    reads: &Reads,
    caller: &Caller,
    set_name: &str,
    options: &QueryOptions,
    version: ODataVersion,
) -> Result<Response, ODataErrorResponse> {
    let set = set_of(reads, set_name, version)?;
    let filter = filtered(set, options, version)?;
    let target = format!(
        "/ngsi-ld/v1/entities?{}&limit=0&count=true",
        query(set, None, &filter)
    );
    let (_, total) = run(
        reads,
        caller,
        OperationId::QueryEntities,
        target,
        &filter,
        version,
    )
    .await?;
    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain")],
        total.to_string(),
    )
        .into_response())
}

async fn entity(
    reads: &Reads,
    caller: &Caller,
    ctx: &Ctx<'_>,
    set_name: &str,
    key: &Key,
    options: &QueryOptions,
) -> Result<Response, ODataErrorResponse> {
    let version = ctx.version;
    let set = set_of(reads, set_name, version)?;
    if options.filter.is_some() {
        return Err(not_implemented(version));
    }
    let picked = selected(set, options).map_err(|()| not_implemented(version))?;
    let id = key_id(key, options).ok_or_else(|| not_found(version))?;
    let target = format!(
        "/ngsi-ld/v1/entities/{}?{}",
        encode(&id),
        query(set, picked.as_deref(), &Translated::default()),
    );
    let none = Translated::default();
    let (docs, _) = run(
        reads,
        caller,
        OperationId::RetrieveEntity,
        target,
        &none,
        version,
    )
    .await?;
    let doc = docs.first().ok_or_else(|| not_found(version))?;
    let value = entity_value(reads, set_name, set, doc, picked.as_deref(), &none);
    let names = picked.unwrap_or_default();
    let resource = resource(reads, set_name, version)?;
    let resource = if names.is_empty() {
        resource
    } else {
        resource.select(&names)
    };
    let mut body = Vec::new();
    // A Vec takes every byte: the write cannot fail.
    let _ = write_entity(ctx, &resource, &value, &mut body);
    Ok(json_response(version, ctx.level, body))
}

/// The request's `$filter` as `q` and `id` (5.2) and its `$orderby` as
/// `orderBy` (Q9), empty without them.
fn filtered(
    set: &Set,
    options: &QueryOptions,
    version: ODataVersion,
) -> Result<Translated, ODataErrorResponse> {
    let translated = options
        .filter
        .as_ref()
        .map_or_else(
            || Ok(Translated::default()),
            |f| translate(f, set, &options.aliases),
        )
        .and_then(|mut t| match &options.orderby {
            Some(items) => order(items, set, &mut t).map(|()| t),
            None => Ok(t),
        });
    translated.map_err(|refused| match refused {
        Refused::Later => not_implemented(version),
        Refused::Bad => ODataErrorResponse::new(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            "the query options cannot hold for this model",
        )
        .with_version(version),
        Refused::TooComplex => ODataErrorResponse::new(
            StatusCode::BAD_REQUEST,
            "TooComplexQuery",
            "the filter is too complex",
        )
        .with_version(version),
    })
}

/// The Entity id a key predicate names: `('urn:…')` or `(id='urn:…')`,
/// either through a parameter alias bound to a string.
fn key_id(key: &Key, options: &QueryOptions) -> Option<String> {
    let value = match key {
        Key::Single(v) => v,
        Key::Compound(pairs) => &pairs.iter().find(|(name, _)| name == "id")?.1,
    };
    let literal = match value {
        KeyValue::Literal(l) => l,
        KeyValue::Alias(name) => &options.aliases.iter().find(|(n, _)| n == name)?.1,
    };
    match literal {
        Literal::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Runs one operation; the Entities of its answer and the result count.
async fn run(
    reads: &Reads,
    caller: &Caller,
    op: OperationId,
    target: String,
    filter: &Translated,
    version: ODataVersion,
) -> Result<(Vec<Value>, u64), ODataErrorResponse> {
    let mut req = OpsRequest::new(Method::GET, target);
    if !filter.context.is_empty() {
        req = req.context(Value::Object(filter.context.clone()));
    }
    let answer = reads
        .view
        .run(op, caller, req)
        .await
        .map_err(|refusal: Refusal| refused(refusal.status(), version))?;
    if answer.status() == StatusCode::NOT_FOUND {
        return Err(not_found(version));
    }
    if !answer.status().is_success() {
        return Err(refused(answer.status(), version));
    }
    let total = answer
        .headers()
        .get("NGSILD-Results-Count")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let docs = serde_json::from_slice::<Value>(&bytes)
        .into_iter()
        .flat_map(|doc| match doc {
            Value::Array(docs) => docs,
            one => vec![one],
        })
        .collect();
    Ok((docs, total))
}

/// One NGSI-LD Entity, as normalized JSON keyed by the core context and the
/// filter's terms, as
/// the members of `set` (or the `picked` ones).
fn entity_value(
    reads: &Reads,
    set_name: &str,
    set: &Set,
    doc: &Value,
    picked: Option<&[&str]>,
    filter: &Translated,
) -> EntityValue {
    let id = doc.get("id").and_then(Value::as_str).unwrap_or_default();
    let wanted = |name: &str| picked.is_none_or(|p| p.contains(&name));
    let mut properties = vec![("id".to_owned(), Value::from(id))];
    if wanted("_types") {
        properties.push(("_types".into(), Value::from(visible_types(&reads.edm, doc))));
    }
    for system in ["scope", "createdAt", "modifiedAt"] {
        if let Some(value) = doc.get(system).filter(|_| wanted(system)) {
            let value = match (system, value) {
                ("scope", Value::String(_)) => Value::Array(vec![value.clone()]),
                _ => value.clone(),
            };
            properties.push((system.into(), value));
        }
    }
    for (name, member) in &set.members {
        if !wanted(name) || matches!(member.shape, Shape::Nav { .. }) {
            continue;
        }
        // The answer is compacted under the request's context: a term the
        // filter defined names its Attribute.
        let key = filter
            .context
            .iter()
            .find(|(_, iri)| iri.as_str() == Some(member.iri.as_str()))
            .map_or_else(|| core_name(&member.iri), |(term, _)| term.clone());
        let instance = default_instance(doc.get(&key));
        properties.push((name.clone(), member_value(&member.shape, instance)));
    }
    let key = Segment::Key(Key::Single(KeyValue::Literal(Literal::String(
        id.to_owned(),
    ))));
    EntityValue {
        id: format!("{set_name}{key}"),
        properties,
    }
}

/// The sets of the Entity's types the model holds, by name.
fn visible_types(edm: &Edm, doc: &Value) -> Vec<String> {
    let types: Vec<&str> = match doc.get("type") {
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        Some(one) => one.as_str().into_iter().collect(),
        None => Vec::new(),
    };
    edm.sets
        .iter()
        .filter(|(_, s)| types.contains(&core_name(&s.type_iri).as_str()))
        .map(|(name, _)| name.clone())
        .collect()
}

/// The instance without a `datasetId` (S3), else the first.
fn default_instance(attr: Option<&Value>) -> Option<&Value> {
    match attr? {
        Value::Array(instances) => instances
            .iter()
            .find(|i| i.get("datasetId").is_none())
            .or_else(|| instances.first()),
        one => Some(one),
    }
}

/// The OData value of one Attribute instance (rows P1 to P7, R5).
fn member_value(shape: &Shape, instance: Option<&Value>) -> Value {
    let member = |names: &[&str]| {
        names
            .iter()
            .find_map(|n| instance.and_then(|i| i.get(*n)))
            .cloned()
    };
    let value = match shape {
        Shape::Value { .. } => member(&["value", "valueList"]).map(plain),
        Shape::Geo | Shape::Json => member(&["value", "json"]),
        Shape::Language => member(&["value"])
            .or_else(|| member(&["languageMap"]).and_then(|m| m.get("@none").cloned())),
        Shape::Vocab => member(&["vocab"]),
        Shape::Url { .. } | Shape::Nav { .. } => member(&["object", "objectList"]),
    };
    let collection = matches!(
        shape,
        Shape::Value {
            collection: true,
            ..
        } | Shape::Url { collection: true }
    );
    match value {
        Some(Value::Array(items)) if !collection => items.into_iter().next().unwrap_or(Value::Null),
        Some(one) if collection && !one.is_array() => Value::Array(vec![one]),
        Some(v) => v,
        None if collection => Value::Array(Vec::new()),
        None => Value::Null,
    }
}

/// A typed literal (`{"@type": "DateTime", "@value": …}`) as its value,
/// item by item in a list.
fn plain(value: Value) -> Value {
    match value {
        Value::Object(mut o) if o.contains_key("@value") => {
            o.remove("@value").unwrap_or(Value::Null)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(plain).collect()),
        other => other,
    }
}

fn set_of<'r>(
    reads: &'r Reads,
    name: &str,
    version: ODataVersion,
) -> Result<&'r Set, ODataErrorResponse> {
    // The URL resolved against the same model, so the set is there.
    reads.edm.sets.get(name).ok_or_else(|| not_found(version))
}

fn resource<'r>(
    reads: &'r Reads,
    name: &str,
    version: ODataVersion,
) -> Result<Resource<'r>, ODataErrorResponse> {
    Resource::new(&reads.state.model, name).ok_or_else(|| not_found(version))
}

/// `Prefer: odata.maxpagesize`, else [`DEFAULT_PAGE`].
fn max_page(headers: &HeaderMap, max_len: usize) -> u64 {
    headers
        .get("Prefer")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| odata_json::parse_prefer(v, max_len).ok())
        .and_then(|p| p.maxpagesize)
        .unwrap_or(DEFAULT_PAGE)
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
fn encode(raw: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// A refused or failed operation as an OData error with the same status.
/// The text is fixed: no detail of the broker's reaches the caller.
fn refused(status: StatusCode, version: ODataVersion) -> ODataErrorResponse {
    let code = match status {
        StatusCode::NOT_FOUND => "NotFound",
        StatusCode::FORBIDDEN => "Forbidden",
        StatusCode::BAD_REQUEST => "BadRequest",
        StatusCode::SERVICE_UNAVAILABLE => "ServiceUnavailable",
        _ => "InternalError",
    };
    ODataErrorResponse::new(status, code, "the request was refused").with_version(version)
}

fn not_found(version: ODataVersion) -> ODataErrorResponse {
    ODataErrorResponse::new(StatusCode::NOT_FOUND, "NotFound", "no such resource")
        .with_version(version)
}

fn not_implemented(version: ODataVersion) -> ODataErrorResponse {
    ODataErrorResponse::new(
        StatusCode::NOT_IMPLEMENTED,
        "NotImplemented",
        "this request is not supported",
    )
    .with_version(version)
}

fn not_allowed(version: ODataVersion) -> Response {
    let mut answer = ODataErrorResponse::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "MethodNotAllowed",
        "this resource is read only",
    )
    .with_version(version)
    .into_response();
    answer.headers_mut().insert(
        axum::http::header::ALLOW,
        axum::http::HeaderValue::from_static("GET"),
    );
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edm::Member;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn set() -> Set {
        let member = |shape| Member {
            iri: "https://e.org/m".into(),
            shape,
        };
        Set {
            type_iri: "https://e.org/T".into(),
            members: BTreeMap::from([
                ("geo".into(), member(Shape::Geo)),
                (
                    "nav".into(),
                    member(Shape::Nav {
                        target: "T".into(),
                        collection: false,
                    }),
                ),
            ]),
        }
    }

    #[test]
    fn select_takes_plain_names_and_leaves_paths_and_annotations_to_later() {
        let set = set();
        let with = |items| QueryOptions {
            select: Some(items),
            ..QueryOptions::default()
        };
        let path = |p: &[&str]| SelectItem::Path(p.iter().map(|s| (*s).to_owned()).collect());
        let star = with(vec![path(&["geo"]), SelectItem::Star]);
        assert_eq!(selected(&set, &star), Ok(None));
        let plain = with(vec![path(&["geo"]), path(&["scope"])]);
        assert_eq!(selected(&set, &plain), Ok(Some(vec!["geo", "scope"])));
        for refused in [
            with(vec![path(&["nav"])]),
            with(vec![path(&["geo", "x"])]),
            with(vec![SelectItem::Annotation("@Core.Description".into())]),
        ] {
            assert_eq!(selected(&set, &refused), Err(()), "{refused:?}");
        }
    }

    #[test]
    fn each_shape_reads_its_member_of_the_instance() {
        let value = |collection| Shape::Value {
            edm: "Edm.String",
            collection,
        };
        for (shape, instance, want) in [
            (
                Shape::Geo,
                json!({"value": {"type": "Point"}}),
                json!({"type": "Point"}),
            ),
            (Shape::Json, json!({"json": {"a": 1}}), json!({"a": 1})),
            (
                Shape::Language,
                json!({"languageMap": {"@none": "x", "en": "y"}}),
                json!("x"),
            ),
            (
                Shape::Language,
                json!({"languageMap": {"en": "y"}}),
                Value::Null,
            ),
            (Shape::Vocab, json!({"vocab": "Red"}), json!("Red")),
            (
                value(true),
                json!({"value": [{"@value": "a"}, "b"]}),
                json!(["a", "b"]),
            ),
            (value(true), json!({"value": "a"}), json!(["a"])),
            (value(false), json!({"value": ["a", "b"]}), json!("a")),
            (value(false), json!({"value": []}), Value::Null),
            (value(false), json!({"value": {"a": 1}}), json!({"a": 1})),
            (Shape::Url { collection: true }, json!({}), json!([])),
        ] {
            assert_eq!(
                member_value(&shape, Some(&instance)),
                want,
                "{shape:?} {instance}"
            );
        }
        assert_eq!(member_value(&value(false), None), Value::Null);
    }

    #[test]
    fn the_default_instance_is_the_one_without_a_dataset_id() {
        let many = json!([{"datasetId": "urn:d:1", "value": 1}, {"value": 2}]);
        assert_eq!(default_instance(Some(&many)), Some(&json!({"value": 2})));
        let only_named = json!([{"datasetId": "urn:d:1", "value": 1}]);
        assert_eq!(default_instance(Some(&only_named)), Some(&only_named[0]));
        assert_eq!(default_instance(Some(&json!([]))), None);
        assert_eq!(default_instance(None), None);
    }

    #[test]
    fn an_entity_names_the_visible_sets_of_its_types() {
        let edm = Edm {
            csdl: odata_csdl::Model {
                version: odata_csdl::CsdlVersion::V4_01,
                references: vec![],
                schemas: vec![],
            },
            namespace: "v".into(),
            sets: BTreeMap::from([("T".into(), set())]),
        };
        let both = json!({"type": ["https://e.org/T", "https://e.org/Hidden"]});
        assert_eq!(visible_types(&edm, &both), vec!["T".to_owned()]);
        assert_eq!(
            visible_types(&edm, &json!({"type": "https://e.org/T"})),
            vec!["T".to_owned()]
        );
        assert_eq!(visible_types(&edm, &json!({})), Vec::<String>::new());
    }

    #[test]
    fn a_refusal_keeps_its_status_and_never_its_detail() {
        for (status, code) in [
            (StatusCode::NOT_FOUND, "NotFound"),
            (StatusCode::FORBIDDEN, "Forbidden"),
            (StatusCode::BAD_REQUEST, "BadRequest"),
            (StatusCode::SERVICE_UNAVAILABLE, "ServiceUnavailable"),
            (StatusCode::INTERNAL_SERVER_ERROR, "InternalError"),
        ] {
            let answer = refused(status, ODataVersion::V4_01).into_response();
            assert_eq!(answer.status(), status, "{code}");
        }
    }

    #[test]
    fn encode_leaves_only_unreserved_characters() {
        assert_eq!(
            encode("https://e.org/a#b c"),
            "https%3A%2F%2Fe.org%2Fa%23b%20c"
        );
        assert_eq!(encode("a-b_c.d~e"), "a-b_c.d~e");
    }
}
