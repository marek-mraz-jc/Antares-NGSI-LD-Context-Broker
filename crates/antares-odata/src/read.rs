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
use crate::paths;
use antares_ops::{core_name, Caller, OperationId, OpsRequest, Refusal, ViewOps};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use odata_axum::{json_level, json_response, ODataErrorResponse, ODataRequest, ODataState};
use odata_json::{
    write_collection, write_entity, write_entity_references, Ctx, EntityValue, ODataVersion, Page,
    Resource,
};
use odata_url::{
    EdmModel, ExpandKind, ExpandPath, Key, KeyValue, Literal, ODataUri, QueryOptions, Segment,
    SelectItem,
};
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
    body: &[u8],
) -> Response {
    let version = request.version;
    if method != Method::GET {
        return crate::write::serve(reads, method, caller, request, headers, body).await;
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
    if let ODataUri::Entity { options } = &request.uri {
        return dereference(reads, caller, &ctx, options)
            .await
            .unwrap_or_else(IntoResponse::into_response);
    }
    let (ODataUri::Resource { path, options }, None) = (&request.uri, later(&request.uri)) else {
        return not_implemented(version).into_response();
    };
    let answer = match path.as_slice() {
        [_, Segment::Ref] | [_, _, Segment::Ref] if shaped(options) => Err(no_properties(version)),
        [Segment::EntitySet(set)] | [Segment::EntitySet(set), Segment::Ref] => {
            collection(reads, caller, &ctx, set, path, options, headers).await
        }
        [Segment::EntitySet(set), Segment::Count] => {
            count(reads, caller, set, options, version).await
        }
        [Segment::EntitySet(set), Segment::Key(key)] => {
            entity(reads, caller, &ctx, set, key, options).await
        }
        [Segment::EntitySet(set), Segment::Key(key), Segment::Ref] if only_select(options) => {
            paths::reference(reads, caller, &ctx, (set, key), options).await
        }
        // Below one Entity only `$select` applies (Q31 to Q33).
        [_, _, ..] if !only_select(options) => Err(not_implemented(version)),
        [Segment::EntitySet(set), Segment::Key(key), Segment::Property(name)] => {
            paths::property(reads, caller, &ctx, (set, key, name), false, options).await
        }
        [Segment::EntitySet(set), Segment::Key(key), Segment::Property(name), Segment::Value] => {
            paths::property(reads, caller, &ctx, (set, key, name), true, options).await
        }
        [Segment::EntitySet(set), Segment::Key(key), Segment::Navigation(nav)] => {
            paths::related(reads, caller, &ctx, (set, key, nav), false, options).await
        }
        [Segment::EntitySet(set), Segment::Key(key), Segment::Navigation(nav), Segment::Ref] => {
            paths::related(reads, caller, &ctx, (set, key, nav), true, options).await
        }
        _ => Err(not_implemented(version)),
    };
    answer.unwrap_or_else(IntoResponse::into_response)
}

/// `$entity?$id=` (Q23): the id, percent-decoded, is a URL of this service,
/// absolute under its root or relative to it, that names one Entity by key
/// and nothing else; it is read as that URL is. A missing `$id`, a broken
/// escape or an id with query options of its own is `400`; any other id
/// names no Entity here and is `404`, like a hidden one (O5). A foreign URL
/// is never fetched.
async fn dereference(
    reads: &Reads,
    caller: &Caller,
    ctx: &Ctx<'_>,
    options: &QueryOptions,
) -> Result<Response, ODataErrorResponse> {
    let id = options
        .id
        .as_deref()
        .and_then(percent_decoded)
        .ok_or_else(|| {
            ODataErrorResponse::new(
                StatusCode::BAD_REQUEST,
                "BadRequest",
                "$id is missing or not percent-encoded",
            )
            .with_version(ctx.version)
        })?;
    let relative = id.strip_prefix(ctx.service_root).unwrap_or(&id);
    let model = EdmModel(&reads.state.model);
    let parsed = (!relative.starts_with('/') && !relative.contains("://"))
        .then(|| odata_url::parse(relative, &model, &reads.state.url_limits));
    match parsed {
        Some(Ok(ODataUri::Resource { options, .. })) if options != QueryOptions::default() => {
            Err(ODataErrorResponse::new(
                StatusCode::BAD_REQUEST,
                "BadRequest",
                "$id has query options of its own",
            )
            .with_version(ctx.version))
        }
        Some(Ok(ODataUri::Resource { path, options })) => match path.as_slice() {
            [Segment::EntitySet(set), Segment::Key(key)] => {
                entity(reads, caller, ctx, set, key, &options).await
            }
            _ => Err(not_found(ctx.version)),
        },
        _ => Err(not_found(ctx.version)),
    }
}

/// `text` with each `%XX` decoded; `None` for a broken escape or bytes that
/// are no UTF-8.
fn percent_decoded(text: &str) -> Option<String> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(b) = bytes.next() {
        out.push(if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?
        } else {
            b
        });
    }
    String::from_utf8(out).ok()
}

/// Whether no option but `$select` (and `$format`, `$schemaversion=*`,
/// read before) is asked.
/// Whether `$select` or `$expand` asks for properties.
fn shaped(options: &QueryOptions) -> bool {
    options.select.is_some() || options.expand.is_some()
}

/// A reference has no properties to select or expand (URL 4.4).
fn no_properties(version: ODataVersion) -> ODataErrorResponse {
    ODataErrorResponse::new(
        StatusCode::BAD_REQUEST,
        "BadRequest",
        "an entity reference has no properties to $select or $expand",
    )
    .with_version(version)
}

fn only_select(options: &QueryOptions) -> bool {
    options.filter.is_none()
        && options.expand.is_none()
        && options.orderby.is_none()
        && options.top.is_none()
        && options.skip.is_none()
        && options.count.is_none()
        && options.skiptoken.is_none()
}

/// Why a parsed URL is answered `501` in this round: a form or an option a
/// later round maps. `None` for what this round serves.
fn later(uri: &ODataUri) -> Option<&'static str> {
    let ODataUri::Resource { options, .. } = uri else {
        return Some("this resource is not served");
    };
    let refused = [
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

/// The members a `$select` list names, `None` for every member. A path
/// through a complex property, a navigation property or an annotation is a
/// later round's.
pub(crate) fn selected<'s>(
    set: &'s Set,
    select: Option<&'s Vec<SelectItem>>,
) -> Result<Option<Vec<&'s str>>, ()> {
    let Some(items) = select else {
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

/// What one `$expand` item writes.
enum Writes<'s> {
    /// `/$ref`: an entity reference per target, under this id member.
    Refs(&'static str),
    /// The targets, projected by the nested `$select` (`None` for all).
    Entities(Option<Vec<&'s str>>),
}

/// One navigation member `$expand` names, one level deep (Q3, Q6 to Q8).
pub(crate) struct Expansion<'s> {
    name: &'s str,
    iri: &'s str,
    target_name: &'s str,
    target: &'s Set,
    collection: bool,
    writes: Writes<'s>,
}

/// The `$expand` items of a request. `*` names every navigation member,
/// a named item overrides it. A nested `$expand`, `$levels` or a path
/// through a complex property is a later round's.
fn expanded<'s>(
    edm: &'s Edm,
    set: &'s Set,
    options: &'s QueryOptions,
    version: ODataVersion,
) -> Result<Vec<Expansion<'s>>, ()> {
    let mut out: Vec<Expansion<'s>> = Vec::new();
    for item in options.expand.iter().flatten() {
        let named = match &item.path {
            ExpandPath::Star => None,
            ExpandPath::Path(path) => match path.as_slice() {
                [name] => Some(name.as_str()),
                _ => return Err(()),
            },
        };
        let select = match &item.kind {
            ExpandKind::Ref => None,
            ExpandKind::Entities {
                select,
                expand: None,
                levels: None,
            } => select.as_ref(),
            ExpandKind::Entities { .. } => return Err(()),
        };
        for (name, member) in &set.members {
            let Shape::Nav { target, collection } = &member.shape else {
                continue;
            };
            if named.is_some_and(|n| n != name) {
                continue;
            }
            if named.is_none() && out.iter().any(|e| e.name == name) {
                continue;
            }
            out.retain(|e| e.name != name);
            let target_set = edm.sets.get(target).ok_or(())?;
            let writes = match item.kind {
                ExpandKind::Ref => Writes::Refs(match version {
                    ODataVersion::V4_0 => "@odata.id",
                    ODataVersion::V4_01 => "@id",
                }),
                ExpandKind::Entities { .. } => Writes::Entities(selected(target_set, select)?),
            };
            out.push(Expansion {
                name,
                iri: &member.iri,
                target_name: target,
                target: target_set,
                collection: *collection,
                writes,
            });
        }
    }
    Ok(out)
}

/// The names the context URL lists (JSON Format 10.9): the selected ones
/// and each expanded member, `owner()` or `owner(name)`; 4.0 lists an
/// expansion only with a nested `$select`.
fn listed(picked: Option<&[&str]>, expansions: &[Expansion], version: ODataVersion) -> Vec<String> {
    let mut names: Vec<String> = picked
        .into_iter()
        .flatten()
        .map(|n| (*n).to_owned())
        .collect();
    for e in expansions {
        match (&e.writes, version) {
            (Writes::Entities(Some(nested)), _) => {
                names.push(format!("{}({})", e.name, nested.join(",")));
            }
            (Writes::Entities(None), ODataVersion::V4_01) => names.push(format!("{}()", e.name)),
            _ => {}
        }
    }
    names
}

/// The NGSI-LD query of an EntitySet: its type, the Attributes selected
/// (`attrs`, which takes IRIs; the core members always come, and the
/// expanded Relationships), the system members only when selected (E6),
/// `join=inline` one level deep for an expansion of entities or a `q` that
/// follows a Relationship (5.7.2.4). A join
/// follows every Relationship it is given (4.5.23), so it names the
/// structural members and the expanded ones: a navigation not expanded
/// spends none of the walk's lookups.
pub(crate) fn query(
    set: &Set,
    picked: Option<&[&str]>,
    expansions: &[Expansion],
    filter: &Translated,
) -> String {
    let nested = expansions.iter().filter_map(|e| match &e.writes {
        Writes::Entities(picked) => Some(picked.as_deref()),
        Writes::Refs(_) => None,
    });
    let joined = filter.linked || nested.clone().count() > 0;
    let structural: Vec<&str>;
    let picked = match picked {
        None if joined => {
            structural = set
                .members
                .iter()
                .filter(|(_, m)| !matches!(m.shape, Shape::Nav { .. }))
                .map(|(name, _)| name.as_str())
                .collect();
            Some(structural.as_slice())
        }
        picked => picked,
    };
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
    if let Some((property, coordinates)) = &filter.geo {
        q.push_str("&georel=intersects&geometry=Polygon&coordinates=");
        q.push_str(&encode(coordinates));
        q.push_str("&geoproperty=");
        q.push_str(&encode(property));
    }
    if !filter.expand_values.is_empty() {
        q.push_str("&expandValues=");
        q.push_str(&encode(&filter.expand_values.join(",")));
    }
    if let Some(keys) = &filter.order {
        q.push_str("&orderBy=");
        q.push_str(&encode(keys));
    }
    if let Some(names) = picked {
        let attrs: Vec<String> = names
            .iter()
            .filter_map(|name| set.members.get(*name))
            .map(|m| m.iri.as_str())
            .chain(expansions.iter().map(|e| e.iri))
            .map(encode)
            .collect();
        if !attrs.is_empty() {
            q.push_str("&attrs=");
            q.push_str(&attrs.join(","));
        }
    }
    let system = |names: Option<&[&str]>| {
        names.is_some_and(|n| n.iter().any(|m| matches!(*m, "createdAt" | "modifiedAt")))
    };
    if system(picked) || nested.clone().any(system) {
        q.push_str("&options=sysAttrs");
    }
    if joined {
        q.push_str("&join=inline&joinLevel=1");
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
    let picked = selected(set, options.select.as_ref()).map_err(|()| not_implemented(version))?;
    let expansions =
        expanded(&reads.edm, set, options, version).map_err(|()| not_implemented(version))?;
    let filter = filtered(set, &reads.edm.sets, options, version)?;
    let digest = digest(&reads.state.service_root, path, options);
    let (skip, top) = match &options.skiptoken {
        None => (options.skip.unwrap_or(0), options.top),
        // The token carries both: a $skip or $top beside it is an edit.
        Some(_) if options.skip.is_some() || options.top.is_some() => {
            return Err(bad_token(version))
        }
        Some(text) => read_token(text, digest).ok_or_else(|| bad_token(version))?,
    };
    let page = top
        .unwrap_or(u64::MAX)
        .min(max_page(headers, reads.state.max_header_len))
        .min(MAX_PAGE);
    let target = format!(
        "/ngsi-ld/v1/entities?{}&limit={page}&offset={skip}&count=true",
        query(set, picked.as_deref(), &expansions, &filter),
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
        .map(|doc| {
            entity_value(
                reads,
                set_name,
                set,
                doc,
                picked.as_deref(),
                &expansions,
                &filter,
            )
        })
        .collect();
    let served = u64::try_from(entities.len()).unwrap_or(u64::MAX);
    let next = skip.saturating_add(served);
    let end = top.map_or(total, |top| total.min(skip.saturating_add(top)));
    let next_link = (served > 0 && next < end).then(|| {
        let rest = QueryOptions {
            skip: None,
            top: None,
            skiptoken: Some(token(
                next,
                top.map(|top| top.saturating_sub(served)),
                digest,
            )),
            ..options.clone()
        };
        let uri = ODataUri::Resource {
            path: path.to_vec(),
            options: rest,
        };
        format!("{}{uri}", reads.state.service_root)
    });
    let names = listed(picked.as_deref(), &expansions, version);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let resource = resource(reads, set_name, version)?;
    let resource = if names.is_empty() {
        resource
    } else {
        resource.select(&names)
    };
    let page = Page {
        count: (options.count == Some(true)).then_some(total),
        next_link: next_link.as_deref(),
    };
    let mut body = Vec::new();
    // A Vec takes every byte: the writes cannot fail.
    if path.last() == Some(&Segment::Ref) {
        let ids: Vec<String> = docs
            .iter()
            .filter_map(|doc| doc.get("id")?.as_str())
            .map(|urn| format!("{set_name}{}", key_segment(set, urn)))
            .collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let _ = write_entity_references(ctx, &ids, &page, &mut body);
        return Ok(json_response(version, ctx.level, body));
    }
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
    let filter = filtered(set, &reads.edm.sets, options, version)?;
    let target = format!(
        "/ngsi-ld/v1/entities?{}&limit=0&count=true",
        query(set, None, &[], &filter)
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

pub(crate) async fn entity(
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
    let picked = selected(set, options.select.as_ref()).map_err(|()| not_implemented(version))?;
    let expansions =
        expanded(&reads.edm, set, options, version).map_err(|()| not_implemented(version))?;
    let id = key_id(set, key, options).ok_or_else(|| not_found(version))?;
    let target = format!(
        "/ngsi-ld/v1/entities/{}?{}",
        encode(&id),
        query(set, picked.as_deref(), &expansions, &Translated::default()),
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
    let value = entity_value(
        reads,
        set_name,
        set,
        doc,
        picked.as_deref(),
        &expansions,
        &none,
    );
    let names = listed(picked.as_deref(), &expansions, version);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
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
    sets: &std::collections::BTreeMap<String, Set>,
    options: &QueryOptions,
    version: ODataVersion,
) -> Result<Translated, ODataErrorResponse> {
    let translated = options
        .filter
        .as_ref()
        .map_or_else(
            || Ok(Translated::default()),
            |f| translate(f, set, sets, &options.aliases),
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

/// The digest a `$skiptoken` carries (S5.2): the view's service root and
/// the request without its paging, so a token read under another view or
/// another query does not match. ponytail: std `SipHash`; a forged token
/// does no more than `$skip`, every page is narrowed by the store again.
fn digest(root: &str, path: &[Segment], options: &QueryOptions) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let normal = ODataUri::Resource {
        path: path.to_vec(),
        options: QueryOptions {
            skip: None,
            top: None,
            skiptoken: None,
            ..options.clone()
        },
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    format!("{root}{normal}").hash(&mut hasher);
    hasher.finish()
}

/// `{offset}.{remaining $top or -}.{digest}`.
fn token(offset: u64, top: Option<u64>, digest: u64) -> String {
    let top = top.map_or_else(|| "-".to_owned(), |t| t.to_string());
    format!("{offset}.{top}.{digest:016x}")
}

/// The offset and remaining `$top` of a token issued for `digest`.
fn read_token(text: &str, digest: u64) -> Option<(u64, Option<u64>)> {
    let mut parts = text.split('.');
    let (Some(offset), Some(top), Some(tag), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if tag != format!("{digest:016x}") {
        return None;
    }
    let top = match top {
        "-" => None,
        n => Some(n.parse().ok()?),
    };
    Some((offset.parse().ok()?, top))
}

fn bad_token(version: ODataVersion) -> ODataErrorResponse {
    ODataErrorResponse::new(
        StatusCode::BAD_REQUEST,
        "BadRequest",
        "the skip token does not belong to this request",
    )
    .with_version(version)
}

/// The Entity id a key predicate names, either through a parameter alias:
/// `('urn:…')` or `(id='urn:…')` (E2); on a set with an identifier (E2a)
/// its value `(v)` or `(ID=v)` names `{prefix}{v}`, and `('urn:…')` stays
/// a URN where the identifier is no string.
pub(crate) fn key_id(set: &Set, key: &Key, options: &QueryOptions) -> Option<String> {
    let own = set.identifier.as_ref();
    let (value, by_id) = match key {
        Key::Single(v) => (v, false),
        Key::Compound(pairs) => pairs
            .iter()
            .find(|(name, _)| name == "id")
            .map(|(_, v)| (v, true))
            .or_else(|| {
                let i = own?;
                pairs
                    .iter()
                    .find(|(name, _)| *name == i.member)
                    .map(|(_, v)| (v, false))
            })?,
    };
    let literal = match value {
        KeyValue::Literal(l) => l,
        KeyValue::Alias(name) => &options.aliases.iter().find(|(n, _)| n == name)?.1,
    };
    match (own.filter(|_| !by_id), literal) {
        (Some(i), Literal::Guid(g)) if i.edm == "Edm.Guid" => Some(format!("{}{g}", i.prefix)),
        (Some(i), Literal::Int(n)) if integer(i.edm) => Some(format!("{}{n}", i.prefix)),
        (Some(i), Literal::String(s)) if i.edm == "Edm.String" => Some(format!("{}{s}", i.prefix)),
        (_, Literal::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// The canonical key predicate of Entity `urn` in `set`: its identifier's
/// value when the URN follows `{prefix}{v}` and `v` reads back as the
/// same URN (E2a), else `(id='urn:…')`; `('urn:…')` without an identifier.
pub(crate) fn key_segment(set: &Set, urn: &str) -> Segment {
    Segment::Key(canonical_key(set, urn))
}

/// The key of [`key_segment`].
pub(crate) fn canonical_key(set: &Set, urn: &str) -> Key {
    let string = |s: &str| KeyValue::Literal(Literal::String(s.to_owned()));
    let Some(own) = &set.identifier else {
        return Key::Single(string(urn));
    };
    let value = urn
        .strip_prefix(own.prefix.as_str())
        .and_then(|v| match own.edm {
            "Edm.Guid" => is_guid(v).then(|| Literal::Guid(v.to_owned())),
            edm if integer(edm) => v
                .parse()
                .ok()
                .filter(|n: &i64| n.to_string() == v)
                .map(Literal::Int),
            _ => Some(Literal::String(v.to_owned())),
        });
    match value {
        Some(v) => Key::Single(KeyValue::Literal(v)),
        None => Key::Compound(vec![("id".into(), string(urn))]),
    }
}

/// One of the integer Edm types a key may have (T-3722).
fn integer(edm: &str) -> bool {
    matches!(
        edm,
        "Edm.Byte" | "Edm.SByte" | "Edm.Int16" | "Edm.Int32" | "Edm.Int64"
    )
}

/// `8-4-4-4-12` lower-case hex digits, as a `guid` literal reads back.
fn is_guid(v: &str) -> bool {
    v.len() == 36
        && v.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => matches!(c, '0'..='9' | 'a'..='f'),
        })
}

/// The Entity id a key member of a write body names: the URN itself (E2),
/// or the identifier's value as in [`key_id`] (E2a).
pub(crate) fn body_id(set: &Set, value: &Value) -> Option<String> {
    match (&set.identifier, value) {
        (None, Value::String(s)) => Some(s.clone()),
        (Some(i), Value::String(s)) if i.edm == "Edm.Guid" => {
            Some(format!("{}{}", i.prefix, s.to_ascii_lowercase()))
        }
        (Some(i), Value::String(s)) => Some(format!("{}{s}", i.prefix)),
        (Some(i), Value::Number(n)) => Some(format!("{}{n}", i.prefix)),
        _ => None,
    }
}

/// Runs one operation; the Entities of its answer and the result count.
pub(crate) async fn run(
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
    run_request(reads, caller, op, req, version).await
}

/// Runs one operation of any method; the Entities of its answer, if it
/// has a body, and the result count.
pub(crate) async fn run_request(
    reads: &Reads,
    caller: &Caller,
    op: OperationId,
    req: OpsRequest,
    version: ODataVersion,
) -> Result<(Vec<Value>, u64), ODataErrorResponse> {
    let answer = reads
        .view
        .run(op, caller, req)
        .await
        .map_err(|refusal: Refusal| refused(refusal.status(), version))?;
    if answer.status() == StatusCode::NOT_FOUND {
        // A query answers 404 for a Tenant that holds nothing yet (5.5.10);
        // its EntitySet is empty.
        return match op {
            OperationId::QueryEntities => Ok((Vec::new(), 0)),
            _ => Err(not_found(version)),
        };
    }
    if !answer.status().is_success() {
        return Err(refused(answer.status(), version));
    }
    // The join walk stopped at the broker's lookup budget (4.5.23.1): an
    // OData client cannot see the NGSI-LD warning, so a partial expansion
    // is refused (S1, O6, D9).
    let truncated = answer
        .headers()
        .get_all("NGSILD-Warning")
        .iter()
        .any(|w| w.as_bytes().windows(9).any(|at| at == b"truncated"));
    if truncated {
        return Err(ODataErrorResponse::new(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            "the expansion reaches more Entities than one answer embeds: narrow it with $top or $filter",
        )
        .with_version(version));
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
/// filter's terms, as the members of `set` (or the `picked` ones) and the
/// expanded navigation members.
pub(crate) fn entity_value(
    reads: &Reads,
    set_name: &str,
    set: &Set,
    doc: &Value,
    picked: Option<&[&str]>,
    expansions: &[Expansion],
    filter: &Translated,
) -> EntityValue {
    let id = doc.get("id").and_then(Value::as_str).unwrap_or_default();
    let wanted = |name: &str| picked.is_none_or(|p| p.contains(&name));
    let mut properties = vec![("id".to_owned(), Value::from(id))];
    if wanted("_types") {
        properties.push(("_types".into(), Value::from(visible_types(&reads.edm, doc))));
    }
    for system in ["scope", "createdAt", "modifiedAt"] {
        // The temporal members only when named (E6): a nested $select may
        // have asked the broker for them.
        let named = system == "scope" || picked.is_some_and(|p| p.contains(&system));
        if let Some(value) = doc.get(system).filter(|_| wanted(system) && named) {
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
        let instance = default_instance(doc.get(key_of(&member.iri, filter)));
        properties.push((name.clone(), member_value(&member.shape, instance)));
    }
    for e in expansions {
        let instance = default_instance(doc.get(key_of(e.iri, filter)));
        properties.push((e.name.to_owned(), expansion(reads, e, instance, filter)));
    }
    EntityValue {
        id: format!("{set_name}{}", key_segment(set, id)),
        properties,
    }
}

/// The key of an Attribute in an answer compacted under the request's
/// context: a term the filter defined, else the core name.
fn key_of(iri: &str, filter: &Translated) -> String {
    filter
        .context
        .iter()
        .find(|(_, defined)| defined.as_str() == Some(iri))
        .map_or_else(|| core_name(iri), |(term, _)| term.clone())
}

/// An expanded navigation member: references to the Relationship's
/// objects, or the targets the join embedded. A target the view or the
/// policy hides is not embedded (T-3470), nor one of another type: it reads
/// like an absent target, `null` or left out of the collection (O5).
fn expansion(reads: &Reads, e: &Expansion, instance: Option<&Value>, filter: &Translated) -> Value {
    let found: Vec<Value> = match &e.writes {
        Writes::Refs(id) => objects(instance)
            .into_iter()
            .map(|urn| {
                let key = key_segment(e.target, &urn);
                serde_json::json!({ *id: format!("{}{key}", e.target_name) })
            })
            .collect(),
        Writes::Entities(picked) => {
            let wanted = core_name(&e.target.type_iri);
            ["entity", "entityList"]
                .iter()
                .filter_map(|m| instance.and_then(|i| i.get(*m)))
                .flat_map(|v| match v {
                    Value::Array(docs) => docs.clone(),
                    one => vec![one.clone()],
                })
                .filter(|doc| types_of(doc).contains(&wanted.as_str()))
                .map(|doc| {
                    let value = entity_value(
                        reads,
                        e.target_name,
                        e.target,
                        &doc,
                        picked.as_deref(),
                        &[],
                        filter,
                    );
                    Value::Object(value.properties.into_iter().collect())
                })
                .collect()
        }
    };
    if e.collection {
        Value::Array(found)
    } else {
        found.into_iter().next().unwrap_or(Value::Null)
    }
}

/// The object URIs of a Relationship instance: `object` (one or many) or a
/// ListRelationship's `objectList`.
pub(crate) fn objects(instance: Option<&Value>) -> Vec<String> {
    let Some(instance) = instance else {
        return Vec::new();
    };
    let items = |v: &Value| match v {
        Value::Array(items) => items.clone(),
        one => vec![one.clone()],
    };
    instance
        .get("object")
        .map(items)
        .into_iter()
        .chain(instance.get("objectList").map(items))
        .flatten()
        .filter_map(|v| match v {
            Value::String(urn) => Some(urn),
            Value::Object(o) => o.get("object").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// An Entity's types as its answer names them.
fn types_of(doc: &Value) -> Vec<&str> {
    match doc.get("type") {
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        Some(one) => one.as_str().into_iter().collect(),
        None => Vec::new(),
    }
}

/// The sets of the Entity's types the model holds, by name.
fn visible_types(edm: &Edm, doc: &Value) -> Vec<String> {
    let types = types_of(doc);
    edm.sets
        .iter()
        .filter(|(_, s)| types.contains(&core_name(&s.type_iri).as_str()))
        .map(|(name, _)| name.clone())
        .collect()
}

/// The instance without a `datasetId` (S3), else the first.
pub(crate) fn default_instance(attr: Option<&Value>) -> Option<&Value> {
    match attr? {
        Value::Array(instances) => instances
            .iter()
            .find(|i| i.get("datasetId").is_none())
            .or_else(|| instances.first()),
        one => Some(one),
    }
}

/// The OData value of one Attribute instance (rows P1 to P7, R5).
pub(crate) fn member_value(shape: &Shape, instance: Option<&Value>) -> Value {
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

pub(crate) fn set_of<'r>(
    reads: &'r Reads,
    name: &str,
    version: ODataVersion,
) -> Result<&'r Set, ODataErrorResponse> {
    // The URL resolved against the same model, so the set is there.
    reads.edm.sets.get(name).ok_or_else(|| not_found(version))
}

pub(crate) fn resource<'r>(
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
pub(crate) fn encode(raw: &str) -> String {
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
pub(crate) fn refused(status: StatusCode, version: ODataVersion) -> ODataErrorResponse {
    let code = match status {
        StatusCode::NOT_FOUND => "NotFound",
        StatusCode::CONFLICT => "Conflict",
        StatusCode::FORBIDDEN => "Forbidden",
        StatusCode::METHOD_NOT_ALLOWED => "MethodNotAllowed",
        StatusCode::BAD_REQUEST => "BadRequest",
        StatusCode::SERVICE_UNAVAILABLE => "ServiceUnavailable",
        _ => "InternalError",
    };
    ODataErrorResponse::new(status, code, "the request was refused").with_version(version)
}

pub(crate) fn not_found(version: ODataVersion) -> ODataErrorResponse {
    ODataErrorResponse::new(StatusCode::NOT_FOUND, "NotFound", "no such resource")
        .with_version(version)
}

pub(crate) fn not_implemented(version: ODataVersion) -> ODataErrorResponse {
    ODataErrorResponse::new(
        StatusCode::NOT_IMPLEMENTED,
        "NotImplemented",
        "this request is not supported",
    )
    .with_version(version)
}

/// A write this surface does not serve: `405` where the method has no
/// meaning (matrix `odata_write_readonly`, `odata_delete_collection`, a
/// `POST` to one Entity), `501` for the forms a later round maps.
pub(crate) fn written(method: &Method, uri: &ODataUri, version: ODataVersion) -> Response {
    let read_only = match uri {
        ODataUri::Resource { path, .. } => match path.as_slice() {
            [Segment::EntitySet(_), Segment::Count] => true,
            [Segment::EntitySet(_), Segment::Query] => method != Method::POST,
            [Segment::EntitySet(_)] => method == Method::DELETE,
            [Segment::EntitySet(_), Segment::Key(_)] => method == Method::POST,
            _ => false,
        },
        // `POST $batch` is served before any other request.
        ODataUri::Batch { .. } => true,
        _ => false,
    };
    if read_only {
        odata_axum::read_only(version)
    } else {
        not_implemented(version).into_response()
    }
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
            list: false,
            written: crate::edm::Written::Always,
        };
        Set {
            type_iri: "https://e.org/T".into(),
            identifier: None,
            writable: crate::edm::Writable::default(),
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
    fn expand_takes_one_level_and_the_objects_of_a_relationship() {
        let edm = Edm {
            csdl: odata_csdl::Model {
                version: odata_csdl::CsdlVersion::V4_01,
                references: vec![],
                schemas: vec![],
            },
            namespace: "n".into(),
            sets: BTreeMap::new(),
        };
        let item = |path: &[&str]| QueryOptions {
            expand: Some(vec![odata_url::ExpandItem {
                path: ExpandPath::Path(path.iter().map(|s| (*s).to_owned()).collect()),
                kind: ExpandKind::Ref,
            }]),
            ..QueryOptions::default()
        };
        let set = set();
        assert!(expanded(&edm, &set, &item(&["c", "nav"]), ODataVersion::V4_0).is_err());
        assert!(
            expanded(&edm, &set, &item(&["nav"]), ODataVersion::V4_0).is_err(),
            "a target set the model lacks"
        );
        assert_eq!(objects(None), Vec::<String>::new());
        let relationship = json!({"object": ["a", 1], "objectList": [{"object": "b"}, "c", 2]});
        assert_eq!(objects(Some(&relationship)), ["a", "b", "c"]);
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
        assert_eq!(selected(&set, star.select.as_ref()), Ok(None));
        let plain = with(vec![path(&["geo"]), path(&["scope"])]);
        assert_eq!(
            selected(&set, plain.select.as_ref()),
            Ok(Some(vec!["geo", "scope"]))
        );
        for refused in [
            with(vec![path(&["nav"])]),
            with(vec![path(&["geo", "x"])]),
            with(vec![SelectItem::Annotation("@Core.Description".into())]),
        ] {
            assert_eq!(
                selected(&set, refused.select.as_ref()),
                Err(()),
                "{refused:?}"
            );
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
    fn a_token_reads_back_only_under_its_digest() {
        assert_eq!(read_token(&token(5, Some(3), 7), 7), Some((5, Some(3))));
        assert_eq!(read_token(&token(5, None, 7), 7), Some((5, None)));
        for bad in [
            token(5, None, 8),
            "5.-".to_owned(),
            format!("5.-.{:016x}.x", 7),
            format!("x.-.{:016x}", 7),
            format!("5.x.{:016x}", 7),
            "5.-.7".to_owned(),
        ] {
            assert_eq!(read_token(&bad, 7), None, "{bad}");
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

    #[test]
    fn a_key_maps_to_its_urn_and_back() {
        // E2a, T-3720: each identifier type both ways, and a URN outside
        // the pattern by `id`.
        let keyed = |edm| Set {
            identifier: Some(crate::edm::Identifier {
                member: "ID".into(),
                edm,
                prefix: "urn:ngsi-ld:P:".into(),
            }),
            ..set()
        };
        let none = QueryOptions::default();
        let plain = set();
        let single = |l| Key::Single(KeyValue::Literal(l));
        let named = |n: &str, l| Key::Compound(vec![(n.into(), KeyValue::Literal(l))]);
        let guid = "01234567-89ab-cdef-0123-456789abcdef";
        let cases = [
            (
                "Edm.Guid",
                single(Literal::Guid(guid.into())),
                format!("urn:ngsi-ld:P:{guid}"),
            ),
            (
                "Edm.Int64",
                single(Literal::Int(-7)),
                "urn:ngsi-ld:P:-7".to_owned(),
            ),
            (
                "Edm.String",
                single(Literal::String("a b".into())),
                "urn:ngsi-ld:P:a b".to_owned(),
            ),
            (
                "Edm.String",
                named("ID", Literal::String("x".into())),
                "urn:ngsi-ld:P:x".to_owned(),
            ),
        ];
        for (edm, key, urn) in cases {
            let set = keyed(edm);
            assert_eq!(
                key_id(&set, &key, &none).as_deref(),
                Some(urn.as_str()),
                "{edm}"
            );
            assert_eq!(canonical_key(&set, &urn), single_or(&key), "{edm}");
        }
        let by_id = |urn: &str| named("id", Literal::String(urn.into()));
        for (edm, urn) in [
            (
                "Edm.Guid",
                "urn:ngsi-ld:P:01234567-89AB-cdef-0123-456789abcdef",
            ),
            ("Edm.Guid", "urn:ngsi-ld:P:x"),
            ("Edm.Int64", "urn:ngsi-ld:P:07"),
            ("Edm.Int64", "urn:ngsi-ld:P:99999999999999999999"),
            ("Edm.String", "urn:ngsi-ld:Q:1"),
        ] {
            let set = keyed(edm);
            assert_eq!(canonical_key(&set, urn), by_id(urn), "{edm} {urn}");
            assert_eq!(key_id(&set, &by_id(urn), &none).as_deref(), Some(urn));
        }
        // A literal of another type names nothing; a string URN stays one
        // where the key is no string.
        let set = keyed("Edm.Guid");
        assert_eq!(key_id(&set, &single(Literal::Int(1)), &none), None);
        assert_eq!(
            key_id(&set, &named("other", Literal::Guid(guid.into())), &none),
            None
        );
        assert_eq!(
            key_id(&set, &single(Literal::String("urn:x".into())), &none).as_deref(),
            Some("urn:x")
        );
        assert_eq!(
            key_id(
                &keyed("Edm.Int64"),
                &single(Literal::Guid(guid.into())),
                &none
            ),
            None
        );
        // Without an identifier the key is the URN (E2).
        assert_eq!(
            canonical_key(&plain, "urn:x"),
            single(Literal::String("urn:x".into()))
        );
        assert_eq!(
            key_id(&plain, &single(Literal::Guid(guid.into())), &none),
            None
        );
        // A body names the key as JSON.
        assert_eq!(
            body_id(&set, &json!(guid.to_ascii_uppercase())),
            Some(format!("urn:ngsi-ld:P:{guid}"))
        );
        assert_eq!(
            body_id(&keyed("Edm.Int64"), &json!(5)),
            Some("urn:ngsi-ld:P:5".into())
        );
        assert_eq!(body_id(&plain, &json!("urn:x")), Some("urn:x".into()));
        assert_eq!(body_id(&plain, &json!(5)), None);
        assert_eq!(body_id(&set, &json!(null)), None);
    }

    #[test]
    fn a_string_key_keeps_its_case_and_a_broken_escape_decodes_to_nothing() {
        let codes = Set {
            identifier: Some(crate::edm::Identifier {
                member: "code".into(),
                edm: "Edm.String",
                prefix: "urn:ngsi-ld:P:".into(),
            }),
            ..set()
        };
        assert_eq!(
            body_id(&codes, &json!("Ab")),
            Some("urn:ngsi-ld:P:Ab".into())
        );
        assert_eq!(percent_decoded("P%28a%29"), Some("P(a)".into()));
        for broken in ["P%zz", "P%2", "%FF"] {
            assert_eq!(percent_decoded(broken), None, "{broken}");
        }
    }

    fn single_or(key: &Key) -> Key {
        // The canonical form of a named key is the single one.
        match key {
            Key::Compound(pairs) => pairs
                .first()
                .map_or_else(|| key.clone(), |(_, v)| Key::Single(v.clone())),
            Key::Single(_) => key.clone(),
        }
    }
}
