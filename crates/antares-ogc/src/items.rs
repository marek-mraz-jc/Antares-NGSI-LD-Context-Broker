// SPDX-License-Identifier: EUPL-1.2
//! `/collections/{c}/items` and `/collections/{c}/items/{featureId}` (mapping rows C8 to C20):
//! the query parameters, the Query Entities requests they become, and the Features of the
//! answers.

use crate::model::{Collection, Time};
use crate::Problem;
use antares_ops::{Caller, OperationId, OpsRequest, ViewOps};
use axum::http::{Method, StatusCode};
use serde_json::{json, Map, Value};

/// Features per page when the request names no `limit` (Part 1 `fc-limit-definition`).
pub(crate) const DEFAULT_LIMIT: u64 = 10;
/// The largest page: the broker's default `max_limit` (`limit` above it is clamped, C10).
pub(crate) const MAX_LIMIT: u64 = 1000;
/// The largest `limit` Part 1 lets a client name; above it the request is 400.
const MAX_NAMED_LIMIT: u64 = 10_000;

/// The query parameters of `items`, read and checked.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Params {
    pub limit: u64,
    pub bbox: Option<[f64; 4]>,
    pub datetime: Option<(Option<String>, Option<String>)>,
    pub next: Option<String>,
    /// `sortby` (S5): Attribute IRIs, `true` descending.
    pub sortby: Vec<(String, bool)>,
    /// `properties` (S6): the Attribute IRIs to keep, all when `None`.
    pub properties: Option<Vec<String>>,
    /// `profile` (S7).
    pub profile: Profile,
    /// `crs` (R2): the CRS the answer's coordinates are in, CRS84 by default.
    pub crs: &'static str,
}

/// How a Feature writes a Relationship to a collection the view shows (Part 5
/// `profiles-for-feature-references`); one to a class the view does not show stays its URN.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Profile {
    /// The target's `featureId`, its URN.
    #[default]
    Key,
    /// The target Feature's URI.
    Uri,
    /// A link object: `href` the target Feature's URI, `title` its URN.
    Link,
}

/// The feature-reference profiles by id.
const PROFILES: [(&str, Profile); 3] = [
    ("rel-as-key", Profile::Key),
    ("rel-as-uri", Profile::Uri),
    ("rel-as-link", Profile::Link),
];

/// Where the profile URIs live; Part 5 writes `OGC` and `ogc` both, so it is matched in any case.
const PROFILE_BASE: &str = "http://www.opengis.net/def/profile/OGC/0/";

/// A profile as Part 5 names it: its id, a safe CURIE `[ogc-profile:id]`, or its URI.
fn profile(text: &str) -> Option<Profile> {
    let id = text
        .strip_prefix("[ogc-profile:")
        .and_then(|t| t.strip_suffix(']'))
        .or_else(|| {
            text.get(..PROFILE_BASE.len())
                .filter(|head| head.eq_ignore_ascii_case(PROFILE_BASE))
                .and_then(|_| text.get(PROFILE_BASE.len()..))
        })
        .unwrap_or(text);
    PROFILES
        .iter()
        .find(|(name, _)| *name == id)
        .map(|(_, p)| *p)
}

/// The URI of a profile, as a `profile` link names it.
pub(crate) fn profile_uri(p: Profile) -> String {
    let id = PROFILES
        .iter()
        .find(|(_, q)| *q == p)
        .map_or("rel-as-key", |(id, _)| id);
    format!("{PROFILE_BASE}{id}")
}

/// `sortby` (OGC API Records and Features Part 8 drafts): `[+|-]name` by commas; a `+` the
/// form decoding turned into a space is ascending too. Only sortable properties (S5).
fn sortby(c: &Collection, text: &str) -> Option<Vec<(String, bool)>> {
    text.split(',')
        .map(|key| {
            let (name, desc) = match key.strip_prefix('-') {
                Some(name) => (name, true),
                None => (key.trim_start_matches([' ', '+']), false),
            };
            let p = c.properties.iter().find(|p| p.name == name && p.sortable)?;
            Some((p.iri.clone(), desc))
        })
        .collect()
}

/// `properties` (S6): property names by commas, none when empty.
fn properties(c: &Collection, text: &str) -> Option<Vec<String>> {
    text.split(',')
        .filter(|name| !text.is_empty() || !name.is_empty())
        .map(|name| Some(c.properties.iter().find(|p| p.name == name)?.iri.clone()))
        .collect()
}

fn bad(detail: &'static str) -> Problem {
    Problem(StatusCode::BAD_REQUEST, detail)
}

/// Reads the query of an `items/{featureId}` request: `properties` and `profile` only.
pub(crate) fn item_params(c: &Collection, pairs: &[(String, String)]) -> Result<Params, Problem> {
    if pairs
        .iter()
        .all(|(name, _)| matches!(name.as_str(), "properties" | "profile" | "crs"))
    {
        params(c, pairs)
    } else {
        Err(bad(
            "the request names a query parameter this resource does not define",
        ))
    }
}

/// Reads the query of an `items` request (C9 to C13, S5 to S7): each parameter once, none
/// unknown; a property the view hides reads as one the collection does not have.
pub(crate) fn params(c: &Collection, pairs: &[(String, String)]) -> Result<Params, Problem> {
    let mut p = Params {
        limit: DEFAULT_LIMIT,
        crs: crate::crs::CRS84,
        ..Params::default()
    };
    let mut seen: Vec<&str> = Vec::new();
    for (name, value) in pairs {
        if seen.contains(&name.as_str()) {
            return Err(bad("a query parameter is repeated"));
        }
        seen.push(name);
        match name.as_str() {
            "limit" => {
                let n: u64 = value.parse().map_err(|_| bad("limit is an integer from 1 to 10000"))?;
                if !(1..=MAX_NAMED_LIMIT).contains(&n) {
                    return Err(bad("limit is an integer from 1 to 10000"));
                }
                p.limit = n.min(MAX_LIMIT);
            }
            "bbox" => p.bbox = Some(bbox(value).ok_or_else(|| bad("bbox is 4 or 6 numbers in CRS84: lower corner, upper corner"))?),
            "datetime" => p.datetime = Some(datetime(value).ok_or_else(|| bad("datetime is an RFC 3339 date-time or an interval start/end, '..' or empty for an open end"))?),
            "next" => p.next = Some(value.clone()),
            "crs" => p.crs = crate::crs::read(value).ok_or(bad(crate::crs::UNSUPPORTED))?,
            // R3: both CRS name the same axes, so the box reads the same.
            "bbox-crs" => {
                crate::crs::read(value).ok_or(bad(crate::crs::UNSUPPORTED))?;
            }
            "sortby" => p.sortby = sortby(c, value).ok_or_else(|| bad("sortby names properties of the collection's sortables, each [+|-]name"))?,
            "properties" => p.properties = Some(properties(c, value).ok_or_else(|| bad("properties names properties of the collection's schema"))?),
            "profile" => p.profile = profile(value).ok_or_else(|| bad("profile is one of rel-as-key, rel-as-uri and rel-as-link"))?,
            _ => return Err(bad("the request names a query parameter this resource does not define")),
        }
    }
    Ok(p)
}

/// `minx,miny,maxx,maxy` or `minx,miny,minz,maxx,maxy,maxz` in CRS84; heights are dropped.
fn bbox(text: &str) -> Option<[f64; 4]> {
    let numbers: Vec<f64> = text
        .split(',')
        .map(|v| v.trim().parse().ok().filter(|v: &f64| v.is_finite()))
        .collect::<Option<_>>()?;
    let [west, south, east, north] = match numbers.as_slice() {
        [west, south, east, north] => [*west, *south, *east, *north],
        [west, south, low, east, north, high] if low <= high => [*west, *south, *east, *north],
        _ => return None,
    };
    let lon = |x: f64| (-180.0..=180.0).contains(&x);
    let lat = |y: f64| (-90.0..=90.0).contains(&y);
    (lon(west) && lon(east) && lat(south) && lat(north) && south <= north)
        .then_some([west, south, east, north])
}

/// An instant (both ends the same) or `start/end` with `..` or empty for an open end; both
/// ends open is refused (Part 1 Req 26 F).
fn datetime(text: &str) -> Option<(Option<String>, Option<String>)> {
    let end = |s: &str| -> Option<Option<String>> {
        match s {
            "" | ".." => Some(None),
            s if is_date_time(s) => Some(Some(s.to_owned())),
            _ => None,
        }
    };
    match text.split_once('/') {
        None if is_date_time(text) => Some((Some(text.to_owned()), Some(text.to_owned()))),
        None => None,
        Some((a, b)) => match (end(a)?, end(b)?) {
            (None, None) => None,
            (from, to) => Some((from, to)),
        },
    }
}

/// RFC 3339 `date-time`: `YYYY-MM-DDThh:mm:ss[.frac](Z|±hh:mm)`, `T` and `Z` in either case.
fn is_date_time(s: &str) -> bool {
    let digits = |from: usize, to: usize| {
        s.get(from..to)
            .is_some_and(|d| d.bytes().all(|b| b.is_ascii_digit()))
    };
    let at = |i: usize, c: u8| {
        s.as_bytes()
            .get(i)
            .is_some_and(|x| x.eq_ignore_ascii_case(&c))
    };
    let head = digits(0, 4)
        && at(4, b'-')
        && digits(5, 7)
        && at(7, b'-')
        && digits(8, 10)
        && at(10, b'T')
        && digits(11, 13)
        && at(13, b':')
        && digits(14, 16)
        && at(16, b':')
        && digits(17, 19);
    let rest = s.get(19..).unwrap_or_default();
    let zone = match rest.strip_prefix('.') {
        Some(fraction) => {
            let zone = fraction.trim_start_matches(|c: char| c.is_ascii_digit());
            (zone.len() < fraction.len()).then_some(zone)
        }
        None => Some(rest),
    };
    let offset = |z: &str| {
        let b = z.as_bytes();
        b.len() == 6
            && matches!(b.first(), Some(b'+' | b'-'))
            && b.get(3) == Some(&b':')
            && [1, 2, 4, 5]
                .iter()
                .all(|&i| b.get(i).is_some_and(u8::is_ascii_digit))
    };
    head && zone.is_some_and(|z| z.eq_ignore_ascii_case("z") || offset(z))
}

/// Percent-encodes everything but the unreserved characters.
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

/// The terms one collection's requests use: core-context names as they are, every other
/// Attribute `p{n}` in the request's own `@context`, so the answer's keys are known.
pub(crate) struct Terms {
    context: Map<String, Value>,
    by_iri: Vec<(String, String)>,
}

impl Terms {
    pub(crate) fn of(c: &Collection) -> Self {
        let mut t = Self {
            context: Map::new(),
            by_iri: Vec::new(),
        };
        for iri in c.geometry.iter().chain(c.properties.iter().map(|p| &p.iri)) {
            let core = antares_ops::core_name(iri);
            let term = if core == *iri {
                let term = format!("p{}", t.context.len());
                t.context.insert(term.clone(), Value::from(iri.as_str()));
                term
            } else {
                core
            };
            t.by_iri.push((iri.clone(), term));
        }
        t
    }

    /// The term of an Attribute of the collection.
    pub(crate) fn term<'a>(&'a self, iri: &'a str) -> &'a str {
        self.by_iri
            .iter()
            .find(|(i, _)| i == iri)
            .map_or(iri, |(_, t)| t.as_str())
    }

    fn request(&self, target: String) -> OpsRequest {
        let req = OpsRequest::new(Method::GET, target);
        if self.context.is_empty() {
            req
        } else {
            req.context(Value::Object(self.context.clone()))
        }
    }
}

/// The `q` the `datetime` parameter adds (C13, C14): the temporal geometry intersects the
/// interval, or the Feature has none.
fn time_q(
    time: Option<&Time>,
    terms: &Terms,
    (from, to): &(Option<String>, Option<String>),
) -> Option<String> {
    let cmp = |iri: &str, op: &str, at: &Option<String>| {
        at.as_ref().map(|at| format!("{}{op}{at}", terms.term(iri)))
    };
    let either = |iri: &str, conds: Vec<String>| {
        let t = terms.term(iri);
        if conds.is_empty() {
            None
        } else {
            Some(format!("(({})|!{t})", conds.join(";")))
        }
    };
    let parts: Vec<String> = match time? {
        Time::Instant(t) => either(
            t,
            cmp(t, ">=", from)
                .into_iter()
                .chain(cmp(t, "<=", to))
                .collect(),
        )
        .into_iter()
        .collect(),
        Time::Interval(start, end) => [
            start
                .as_ref()
                .and_then(|s| either(s, cmp(s, "<=", to).into_iter().collect())),
            end.as_ref()
                .and_then(|e| either(e, cmp(e, ">=", from).into_iter().collect())),
        ]
        .into_iter()
        .flatten()
        .collect(),
    };
    (!parts.is_empty()).then(|| parts.join(";"))
}

/// The `orderBy` (S5) and `pick` (S6) a request adds; `pick` keeps `id`, `type` and the
/// geometry, which every Feature carries.
fn shape(c: &Collection, terms: &Terms, p: &Params) -> String {
    let order: Vec<String> = p
        .sortby
        .iter()
        .map(|(iri, desc)| format!("{};{}", terms.term(iri), if *desc { "desc" } else { "asc" }))
        .collect();
    let order = (!order.is_empty()).then(|| format!("&orderBy={}", encode(&order.join(","))));
    let pick = p.properties.as_ref().map(|keep| {
        let names: Vec<&str> = ["id", "type"]
            .into_iter()
            .chain(c.geometry.iter().map(|g| terms.term(g)))
            .chain(keep.iter().map(|iri| terms.term(iri)))
            .collect();
        format!("&pick={}", encode(&names.join(",")))
    });
    order.into_iter().chain(pick).collect()
}

/// The polygon(s) of a CRS84 box; one that crosses the antimeridian (its west edge east of
/// its east edge) is two.
fn bbox_geometry([west, south, east, north]: [f64; 4]) -> (&'static str, String) {
    let ring = |from: f64, to: f64| {
        json!([[
            [from, south],
            [to, south],
            [to, north],
            [from, north],
            [from, south]
        ]])
    };
    if west <= east {
        ("Polygon", ring(west, east).to_string())
    } else {
        (
            "MultiPolygon",
            json!([ring(west, 180.0), ring(-180.0, east)]).to_string(),
        )
    }
}

/// The Query Entities requests of one `items` request: one, or two when `bbox` must also
/// keep the Features without a geometry (C12). Disjoint, served in this order.
pub(crate) fn targets(c: &Collection, terms: &Terms, p: &Params) -> Vec<String> {
    let base = format!(
        "/ngsi-ld/v1/entities?type={}&format=simplified&count=true{}",
        encode(&c.type_iri),
        shape(c, terms, p)
    );
    let time = p
        .datetime
        .as_ref()
        .and_then(|d| time_q(c.time.as_ref(), terms, d));
    let with_q = |extra: Option<String>| {
        let q: Vec<String> = time.iter().cloned().chain(extra).collect();
        if q.is_empty() {
            base.clone()
        } else {
            format!("{base}&q={}", encode(&q.join(";")))
        }
    };
    match (p.bbox, c.geometry.as_deref()) {
        (Some(bbox), Some(geometry)) => {
            let (kind, coordinates) = bbox_geometry(bbox);
            vec![
                format!(
                    "{}&georel=intersects&geometry={kind}&coordinates={}&geoproperty={}",
                    with_q(None),
                    encode(&coordinates),
                    encode(geometry)
                ),
                with_q(Some(format!("!{}", terms.term(geometry)))),
            ]
        }
        _ => vec![with_q(None)],
    }
}

/// The token of the next page: the part and the offset in it, and a digest of the request
/// without its `next` and of the view, so a token read under another view or query does not
/// match (C16). ponytail: std `SipHash`; a forged token does no more than an offset, every
/// page is narrowed by the store again.
pub(crate) fn digest(view: &str, collection: &str, pairs: &[(String, String)]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (view, collection).hash(&mut hasher);
    for pair in pairs.iter().filter(|(n, _)| n != "next") {
        pair.hash(&mut hasher);
    }
    hasher.finish()
}

fn token(part: usize, offset: u64, digest: u64) -> String {
    format!("{part}.{offset}.{digest:016x}")
}

fn read_token(text: &str, digest: u64, parts: usize) -> Option<(usize, u64)> {
    let mut it = text.split('.');
    let (Some(part), Some(offset), Some(tag), None) = (it.next(), it.next(), it.next(), it.next())
    else {
        return None;
    };
    let part: usize = part.parse().ok()?;
    (tag == format!("{digest:016x}") && part < parts).then_some(())?;
    Some((part, offset.parse().ok()?))
}

/// One Query Entities run through the view: its Entities and its count.
async fn query(
    view: &ViewOps,
    caller: &Caller,
    terms: &Terms,
    target: String,
) -> Result<(Vec<Value>, u64), Problem> {
    let refused = |status| Problem(status, "the view refused the query");
    let answer = view
        .run(OperationId::QueryEntities, caller, terms.request(target))
        .await
        .map_err(|r| refused(r.status()))?;
    if answer.status() == StatusCode::NOT_FOUND {
        // A Tenant that holds nothing yet answers 404 (5.5.10): an empty collection.
        return Ok((Vec::new(), 0));
    }
    if !answer.status().is_success() {
        return Err(refused(answer.status()));
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
    let docs = serde_json::from_slice::<Vec<Value>>(&bytes).unwrap_or_default();
    Ok((docs, total))
}

/// One page of Features (C8 to C18).
pub(crate) struct Page {
    pub features: Vec<Value>,
    pub matched: u64,
    pub next: Option<String>,
}

pub(crate) async fn page(
    view: &ViewOps,
    caller: &Caller,
    c: &Collection,
    p: &Params,
    digest: u64,
    base: &str,
) -> Result<Page, Problem> {
    let refs = Refs {
        profile: p.profile,
        base,
    };
    let terms = Terms::of(c);
    let targets = targets(c, &terms, p);
    let (start, mut offset) = match &p.next {
        None => (0, 0),
        Some(t) => read_token(t, digest, targets.len())
            .ok_or_else(|| bad("the next token does not belong to this request"))?,
    };
    let mut out = Page {
        features: Vec::new(),
        matched: 0,
        next: None,
    };
    for (part, target) in targets.into_iter().enumerate() {
        let room = if part < start {
            0
        } else {
            p.limit
                .saturating_sub(u64::try_from(out.features.len()).unwrap_or(u64::MAX))
        };
        let at = if part == start { offset } else { 0 };
        let (docs, count) = query(
            view,
            caller,
            &terms,
            format!("{target}&limit={room}&offset={at}"),
        )
        .await?;
        out.matched = out.matched.saturating_add(count);
        let got = u64::try_from(docs.len()).unwrap_or(u64::MAX);
        out.features.extend(
            docs.iter()
                .map(|d| Value::Object(feature(c, &terms, d, refs))),
        );
        if part >= start && out.next.is_none() && at.saturating_add(got) < count {
            out.next = Some(token(part, at.saturating_add(got), digest));
        }
        offset = 0;
    }
    Ok(out)
}

/// One Entity of `c` as a `GeoJSON` Feature (M2 to M5): `id` the URN, `geometry` the primary
/// geometry or null, `properties` the other Attributes the view shows, by slot name.
pub(crate) fn feature(
    c: &Collection,
    terms: &Terms,
    doc: &Value,
    refs: Refs,
) -> Map<String, Value> {
    let geometry = c
        .geometry
        .as_ref()
        .and_then(|g| doc.get(terms.term(g)))
        .cloned()
        .unwrap_or(Value::Null);
    let properties: Map<String, Value> = c
        .properties
        .iter()
        .filter_map(|p| {
            let value = doc.get(terms.term(&p.iri))?;
            let value = match (&p.target, refs.profile) {
                (Some(target), Profile::Uri | Profile::Link) => {
                    refer(value, &|urn| refs.render(target, urn))
                }
                _ => value.clone(),
            };
            Some((p.name.clone(), value))
        })
        .collect();
    Map::from_iter([
        ("type".to_owned(), json!("Feature")),
        (
            "id".to_owned(),
            doc.get("id").cloned().unwrap_or(Value::Null),
        ),
        ("geometry".to_owned(), geometry),
        ("properties".to_owned(), Value::Object(properties)),
    ])
}

/// How one request writes references (S7): the profile and the endpoint's URI.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Refs<'a> {
    pub profile: Profile,
    pub base: &'a str,
}

impl Refs<'_> {
    /// A reference to `urn` in the collection `target` the view shows, as `rel-as-uri` or
    /// `rel-as-link` write it (`rel-as-key` keeps the URN and never calls this).
    fn render(self, target: &str, urn: &str) -> Value {
        let href = format!("{}/collections/{target}/items/{}", self.base, encode(urn));
        if self.profile == Profile::Link {
            json!({"href": href, "title": urn})
        } else {
            Value::from(href)
        }
    }
}

/// A Relationship's object (a URN, or an array of them) with each URN rendered.
fn refer(value: &Value, render: &dyn Fn(&str) -> Value) -> Value {
    match value {
        Value::String(urn) => render(urn),
        Value::Array(objects) => Value::Array(objects.iter().map(|o| refer(o, render)).collect()),
        other => other.clone(),
    }
}

/// One Feature by its id, if the view shows it and it is of the collection's type (C19): a
/// Query Entities by id and type, so the view and its engine narrow it like a page. An id that
/// is no URI names no Feature either.
pub(crate) async fn one(
    view: &ViewOps,
    caller: &Caller,
    c: &Collection,
    id: &str,
    p: &Params,
    refs: Refs<'_>,
) -> Result<Map<String, Value>, Problem> {
    let terms = Terms::of(c);
    let target = format!(
        "/ngsi-ld/v1/entities?id={}&type={}&format=simplified{}",
        encode(id),
        encode(&c.type_iri),
        shape(c, &terms, p)
    );
    match query(view, caller, &terms, target).await {
        Ok((docs, _)) => docs.first().map(|d| feature(c, &terms, d, refs)),
        Err(Problem(StatusCode::BAD_REQUEST, _)) => None,
        Err(refused) => return Err(refused),
    }
    .ok_or(Problem(
        StatusCode::NOT_FOUND,
        "no such Feature in this collection",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(name: &str, sortable: bool) -> crate::model::Prop {
        crate::model::Prop {
            name: name.into(),
            iri: format!("https://e.org/{name}"),
            schema: json!({"type": "number"}),
            role: None,
            unit: None,
            description: None,
            sortable,
            kind: "Property".into(),
            reference: false,
            target: None,
        }
    }

    fn car() -> Collection {
        Collection {
            id: "Car".into(),
            description: None,
            type_iri: "https://e.org/Car".into(),
            geometry: Some(crate::model::LOCATION.into()),
            properties: vec![prop("speed", true), prop("tags", false)],
            time: None,
        }
    }

    fn pairs(q: &[(&str, &str)]) -> Vec<(String, String)> {
        q.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    #[test]
    fn parameters_are_read_and_checked() {
        assert_eq!(
            params(&car(), &[]).map(|p| p.limit).ok(),
            Some(DEFAULT_LIMIT)
        );
        assert_eq!(
            params(&car(), &pairs(&[("limit", "10000")]))
                .map(|p| p.limit)
                .ok(),
            Some(MAX_LIMIT),
            "clamped"
        );
        for bad in [
            vec![("limit", "0")],
            vec![("limit", "10001")],
            vec![("limit", "x")],
            vec![("limit", "1"), ("limit", "2")],
            vec![("crs", "x")],
            vec![("bbox", "1,2,3")],
            vec![("bbox", "1,2,3,x")],
            vec![("bbox", "0,1,1,0")],
            vec![("bbox", "0,0,181,1")],
            vec![("bbox", "0,0,5,1,1,4")],
            vec![("bbox", "-181,0,1,1")],
            vec![("bbox", "0,-91,1,0")],
            vec![("bbox", "0,0,1,91")],
            vec![("datetime", "yesterday")],
            vec![("datetime", "../..")],
            vec![("datetime", "/")],
            vec![("datetime", "2026-01-01")],
            vec![("datetime", "2026-01-01T00:00:00Z/tomorrow")],
        ] {
            assert!(params(&car(), &pairs(&bad)).is_err(), "{bad:?}");
        }
        let p = params(
            &car(),
            &pairs(&[
                ("bbox", "170,-10,-170,10"),
                ("datetime", "2026-01-01T00:00:00Z/.."),
                ("next", "t"),
            ]),
        )
        .unwrap_or_default();
        assert_eq!(p.bbox, Some([170.0, -10.0, -170.0, 10.0]));
        assert_eq!(
            p.datetime,
            Some((Some("2026-01-01T00:00:00Z".into()), None))
        );
        assert_eq!(p.next.as_deref(), Some("t"));
        assert_eq!(
            params(&car(), &pairs(&[("bbox", "0,0,-5,1,1,5")]))
                .map(|p| p.bbox)
                .ok(),
            Some(Some([0.0, 0.0, 1.0, 1.0]))
        );
        assert_eq!(
            params(
                &car(),
                &pairs(&[("datetime", "/2026-01-01T00:00:00.5+01:00")])
            )
            .map(|p| p.datetime)
            .ok(),
            Some(Some((None, Some("2026-01-01T00:00:00.5+01:00".into()))))
        );
    }

    #[test]
    fn date_times_follow_rfc_3339() {
        for ok in [
            "2026-01-01T00:00:00Z",
            "2026-01-01t00:00:00z",
            "2026-01-01T00:00:00.123Z",
            "2026-01-01T00:00:00-05:30",
        ] {
            assert!(is_date_time(ok), "{ok}");
        }
        for no in [
            "2026-01-01",
            "2026-01-01T00:00:00",
            "2026-01-01T00:00:00.Z",
            "2026-01-01T00:00:00+0100",
            "2026-01-01T00:00:00+01:0x",
            "2026-01-01T00:00:00+0x:00",
            "2026-1-01T00:00:00Z",
            "x026-01-01T00:00:00Z",
            "2026x01-01T00:00:00Z",
            "2026-01-0xT00:00:00Z",
            "2026-01-01T0x:00:00Z",
            "2026-01-01T00:0x:00Z",
            "2026-01x01T00:00:00Z",
            "2026-01-01x00:00:00Z",
            "2026-01-01T00x00:00Z",
            "2026-01-01T00:00x00Z",
            "2026-01-01T00:00:0xZ",
            "2026-01-01T00:00:00*01:00",
            "2026-01-01T00:00:00+01x00",
            "2026-01-01T00:00:00+01:000",
        ] {
            assert!(!is_date_time(no), "{no}");
        }
    }

    #[test]
    fn a_box_across_the_antimeridian_is_two_polygons() {
        assert_eq!(bbox_geometry([0.0, 0.0, 1.0, 1.0]).0, "Polygon");
        let (kind, coordinates) = bbox_geometry([170.0, -1.0, -170.0, 1.0]);
        assert_eq!(kind, "MultiPolygon");
        assert!(
            coordinates.starts_with("[[[[170.0,-1.0],[180.0,-1.0]"),
            "{coordinates}"
        );
    }

    #[test]
    fn a_token_belongs_to_its_digest_and_names_a_part() {
        let t = token(1, 20, 7);
        assert_eq!(read_token(&t, 7, 2), Some((1, 20)));
        assert_eq!(read_token(&t, 8, 2), None, "another request");
        assert_eq!(read_token(&t, 7, 1), None, "no such part");
        for bad in [
            "",
            "1.2",
            "1.2.3.4",
            "x.2.0000000000000007",
            "1.x.0000000000000007",
        ] {
            assert_eq!(read_token(bad, 7, 2), None, "{bad}");
        }
        assert_ne!(digest("a", "Car", &[]), digest("b", "Car", &[]));
        assert_eq!(
            digest("a", "Car", &pairs(&[("limit", "1")])),
            digest("a", "Car", &pairs(&[("limit", "1"), ("next", "x")]))
        );
    }

    #[test]
    fn the_time_query_keeps_features_without_time() {
        let terms = Terms {
            context: Map::new(),
            by_iri: vec![("s".into(), "s".into()), ("e".into(), "e".into())],
        };
        let range = (Some("A".to_owned()), Some("B".to_owned()));
        assert_eq!(
            time_q(Some(&Time::Instant("s".into())), &terms, &range).as_deref(),
            Some("((s>=A;s<=B)|!s)")
        );
        assert_eq!(
            time_q(
                Some(&Time::Interval(Some("s".into()), Some("e".into()))),
                &terms,
                &range
            )
            .as_deref(),
            Some("((s<=B)|!s);((e>=A)|!e)")
        );
        assert_eq!(
            time_q(
                Some(&Time::Interval(Some("s".into()), None)),
                &terms,
                &(Some("A".into()), None)
            ),
            None,
            "an open end constrains nothing"
        );
        assert_eq!(time_q(None, &terms, &range), None);
    }

    #[test]
    fn references_render_each_urn_of_an_object() {
        let refs = Refs {
            profile: Profile::Uri,
            base: "http://b",
        };
        let render = |urn: &str| refs.render("Stop", urn);
        assert_eq!(
            refer(&json!(["urn:a", "urn:b"]), &render),
            json!([
                "http://b/collections/Stop/items/urn%3Aa",
                "http://b/collections/Stop/items/urn%3Ab"
            ])
        );
        assert_eq!(refer(&json!(7), &render), json!(7), "no URN, kept");
        assert_eq!(profile("rel-as-nothing"), None);
        assert_eq!(profile("[ogc-profile:rel-as-link"), None);
        assert_eq!(
            profile_uri(Profile::Uri),
            "http://www.opengis.net/def/profile/OGC/0/rel-as-uri"
        );
    }

    #[test]
    fn core_attributes_keep_their_name_and_others_get_a_context_term() {
        let c = Collection {
            id: "Car".into(),
            description: None,
            type_iri: "https://e.org/Car".into(),
            geometry: Some(crate::model::LOCATION.into()),
            properties: vec![prop("speed", true)],
            time: None,
        };
        let t = Terms::of(&c);
        assert_eq!(t.term(crate::model::LOCATION), "location");
        assert_eq!(t.term("https://e.org/speed"), "p0");
        assert_eq!(t.term("https://e.org/other"), "https://e.org/other");
        assert_eq!(t.context.get("p0"), Some(&json!("https://e.org/speed")));
        let _ = t.request("/ngsi-ld/v1/entities".into());
        let only_core = Terms::of(&Collection {
            properties: Vec::new(),
            ..c
        });
        assert!(only_core.context.is_empty());
        let _ = only_core.request("/ngsi-ld/v1/entities".into());
    }
}
