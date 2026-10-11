// SPDX-License-Identifier: EUPL-1.2
//! The OGC API - Features surface of a view (`docs/analysis/ogc-features-mapping.md`,
//! ADR-0029, T-3744): `/x/views/{view}/ogc/features` serves the view's Entities as Features
//! of OGC API - Features Part 1 Core with `GeoJSON` and `OpenAPI` 3.0, Part 2 `CRS84` (`crs.rs`),
//! the Part 4 draft's writes (`write.rs`, T-3746) and the Part 5 draft's schemas, sortables
//! and feature references (`schema.rs`, T-3754). A collection is a class
//! of the Tenant's `LinkML` schema whose type the view shows (`model.rs`); its Features are the
//! Entities a Query Entities through the view returns for the caller, and a write one Create,
//! Replace, Merge or Delete Entity through it, so the operation's own gate fires and the view narrows the answer as on the NGSI-LD view root.
//!
//! A view whose Tenant has no schema, or one this surface cannot read, has no OGC endpoint:
//! every path answers 404 as an unknown view does, as the `OData` surface does.
//!
//! ```
//! use antares_ops::Surface;
//! let ogc = antares_ogc::Ogc;
//! assert_eq!(ogc.prefix(), "/x/views");
//! assert_eq!(antares_ops::mount(ogc).name(), "ogc");
//! ```

#![forbid(unsafe_code)]

mod api;
mod crs;
mod items;
mod model;
mod schema;
mod write;

use antares_ops::{Caller, Ops, Surface, ViewOps};
use axum::body::Body;
use axum::extract::{Query, Request};
use axum::http::header::{ACCEPT, ALLOW, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use axum::Router;
use model::Collection;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Where the surface is mounted; the view id and `ogc/features` follow.
pub const PREFIX: &str = "/x/views";

/// The bounds a Tenant's schema is read under (threat LK7), as the `OData` surface reads it.
pub const SCHEMA_LIMITS: linkml_model::Limits = linkml_model::Limits {
    max_bytes: 16 << 20,
    max_depth: 64,
    max_elements: 100_000,
};

/// The conformance classes `/conformance` declares (mapping rows C3, R1 to R5, W1 to W5, S1 to
/// S3, S5, S7; Parts 4 and 5 are the drafts OGC 20-002r1 and 23-058r1). A view that offers no
/// write answers each write 405 with the methods it offers (W4).
pub const CONFORMANCE: [&str; 14] = [
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/oas30",
    "http://www.opengis.net/spec/ogcapi-features-5/1.0/conf/schemas",
    "http://www.opengis.net/spec/ogcapi-features-5/1.0/conf/core-roles-features",
    "http://www.opengis.net/spec/ogcapi-features-5/1.0/conf/returnables-and-receivables",
    "http://www.opengis.net/spec/ogcapi-features-5/1.0/conf/sortables",
    "http://www.opengis.net/spec/ogcapi-features-5/1.0/conf/feature-references",
    "http://www.opengis.net/spec/ogcapi-features-5/1.0/conf/profile-parameter",
    "http://www.opengis.net/spec/ogcapi-features-5/1.0/conf/profile-references",
    "http://www.opengis.net/spec/ogcapi-features-2/1.0/conf/crs",
    "http://www.opengis.net/spec/ogcapi-features-4/1.0/conf/create-replace-delete",
    "http://www.opengis.net/spec/ogcapi-features-4/1.0/conf/update",
    "http://www.opengis.net/spec/ogcapi-features-4/1.0/conf/features",
];

const JSON: &str = "application/json";
const GEOJSON: &str = "application/geo+json";
const OPENAPI: &str = "application/vnd.oai.openapi+json;version=3.0";
const SCHEMA: &str = "application/schema+json";

/// The OGC API - Features surface.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ogc;

impl Surface for Ogc {
    fn name(&self) -> &'static str {
        "ogc"
    }

    fn prefix(&self) -> &'static str {
        PREFIX
    }

    fn version_info(&self) -> Value {
        json!({"endpoint": format!("{PREFIX}/{{view}}/ogc/features"), "conformsTo": CONFORMANCE})
    }

    fn router(&self, ops: Ops) -> Router {
        let handler = any(move |caller: Caller, req: Request| {
            let ops = ops.clone();
            async move { serve(&ops, &caller, req).await }
        });
        Router::new()
            .route("/{view}/ogc/features", handler.clone())
            .route("/{view}/ogc/features/", handler.clone())
            .route("/{view}/ogc/features/{*rest}", handler)
    }
}

/// A refusal: its status and a fixed text that never names what the view hides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Problem(pub StatusCode, pub &'static str);

impl Problem {
    fn reply(self) -> Reply {
        problem(self.0, self.1)
    }
}

/// An answer: built before it is sent, so every refusal is one too.
type Reply = Response;

/// An RFC 7807 problem (Part 1 7.5.1); `detail` never names what the view hides.
fn problem(status: StatusCode, detail: &str) -> Reply {
    let body = json!({
        "type": "about:blank",
        "title": status.canonical_reason().unwrap_or_default(),
        "status": status.as_u16(),
        "detail": detail,
    });
    (
        status,
        [(CONTENT_TYPE, "application/problem+json")],
        body.to_string(),
    )
        .into_response()
}

fn document(media: &'static str, body: &Value) -> Reply {
    (StatusCode::OK, [(CONTENT_TYPE, media)], body.to_string()).into_response()
}

/// `HEAD` is the `GET` without its body: status, headers and the one `Content-Length` the
/// `GET` sends (Part 1 `/req/core/http`, RFC 9110 9.3.2).
async fn serve(ops: &Ops, caller: &Caller, req: Request) -> Response {
    let head = req.method() == Method::HEAD;
    let (mut parts, body) = req.into_parts();
    if head {
        parts.method = Method::GET;
    }
    let answer = respond(ops, caller, &parts, body).await;
    if !head {
        return answer;
    }
    let (mut parts, body) = answer.into_parts();
    let read = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    parts.headers.insert(CONTENT_LENGTH, read.len().into());
    Response::from_parts(parts, axum::body::Body::empty())
}

/// One request below `/{view}/ogc/features`, read as sent: a view id is `[a-z0-9-]`, so its
/// raw segment is its id.
async fn respond(ops: &Ops, caller: &Caller, parts: &Parts, body: Body) -> Reply {
    let (method, uri, headers) = (&parts.method, &parts.uri, &parts.headers);
    let mut segments = uri.path().split('/').skip(1);
    let id = segments.next().unwrap_or_default();
    // `ogc`, `features`: the route matched them.
    let rest: Vec<&str> = segments.skip(2).collect();
    let not_found = || problem(StatusCode::NOT_FOUND, "no such resource");
    let Some(view) = ops.view(id) else {
        return not_found();
    };
    let Some(collections) = collections(&view) else {
        return not_found();
    };
    // W4: a resource's methods are the reads and the writes the view offers on it.
    let item = match rest.as_slice() {
        ["collections", c, "items"] => Some((*c, None)),
        ["collections", c, "items", feature] => Some((*c, Some(*feature))),
        _ => None,
    };
    if let Some(reply) = offered(&view, method, item) {
        return reply;
    }
    let base = format!(
        "{}{PREFIX}/{id}/ogc/features",
        antares_ops::public_base(headers)
    );
    if let Some(item) = item.filter(|_| method != Method::GET) {
        return write_to(&view, caller, &collections, &base, parts, item, body).await;
    }
    if !acceptable(headers) {
        return problem(
            StatusCode::NOT_ACCEPTABLE,
            "this surface writes JSON, GeoJSON, JSON Schema and OpenAPI JSON",
        );
    }
    // Form decoding is lossy (an undecodable escape stays as written), so this never fails.
    let pairs = Query::<Vec<(String, String)>>::try_from_uri(uri)
        .map(|Query(pairs)| pairs)
        .unwrap_or_default();
    let at = Here {
        view: &view,
        id,
        base: &base,
        caller,
        collections: &collections,
    };
    match (rest.as_slice(), pairs.is_empty()) {
        ([] | [""], true) => document(JSON, &at.landing()),
        (["conformance"], true) => document(JSON, &json!({"conformsTo": CONFORMANCE})),
        (["api"], true) => document(
            OPENAPI,
            &api::document(&base, &collections, &|op| view.allows(op)),
        ),
        (["collections"], true) => document(JSON, &at.collections()),
        (["collections", c], true) => match collections.get(*c) {
            Some(c) => document(JSON, &at.collection(c)),
            None => not_found(),
        },
        (["collections", c, which @ ("schema" | "sortables")], true) => match collections.get(*c) {
            Some(c) => {
                let id = format!("{base}/collections/{}/{which}", c.id);
                document(SCHEMA, &schema::document(c, &id, *which == "sortables"))
            }
            None => not_found(),
        },
        (["collections", c, "items"], _) => match collections.get(*c) {
            Some(c) => at.items(c, &pairs, uri.query()).await,
            None => not_found(),
        },
        (["collections", c, "items", feature], _) => match (collections.get(*c), decode(feature)) {
            (Some(c), Some(feature)) => at.item(c, &feature, &pairs).await,
            _ => not_found(),
        },
        (
            []
            | ["" | "conformance" | "api" | "collections"]
            | ["collections", _]
            | ["collections", _, "schema" | "sortables"],
            false,
        ) => problem(
            StatusCode::BAD_REQUEST,
            "the request names a query parameter this resource does not define",
        ),
        _ => not_found(),
    }
}

/// W4: whether the resource offers `method`: its reads, and the writes the view offers on it
/// (`item`: the collection and the Feature of an `items` path). A method it does not offer is
/// 405 before the body is read (W1); `OPTIONS` answers the list. `None`: serve it.
fn offered(view: &ViewOps, method: &Method, item: Option<(&str, Option<&str>)>) -> Option<Reply> {
    let writes = [
        (Method::POST, "POST"),
        (Method::PUT, "PUT"),
        (Method::PATCH, "PATCH"),
        (Method::DELETE, "DELETE"),
    ];
    let allow: Vec<&str> = ["GET", "HEAD", "OPTIONS"]
        .into_iter()
        .chain(
            writes
                .iter()
                .filter(|(m, _)| {
                    item.and_then(|(_, feature)| write::Write::of(m, feature))
                        .is_some_and(|w| view.allows(w.operation().0))
                })
                .map(|(_, name)| *name),
        )
        .collect();
    // Method names joined by commas are a header value.
    let allow_header =
        HeaderValue::from_str(&allow.join(", ")).unwrap_or(HeaderValue::from_static("GET"));
    if !allow.contains(&method.as_str()) {
        let mut refused = problem(
            StatusCode::METHOD_NOT_ALLOWED,
            "this view does not offer the method on this resource",
        );
        refused.headers_mut().insert(ALLOW, allow_header);
        return Some(refused);
    }
    (method == Method::OPTIONS).then(|| (StatusCode::OK, [(ALLOW, allow_header)]).into_response())
}

/// A write on an `items` path the view offers (W1 to W7).
async fn write_to(
    view: &ViewOps,
    caller: &Caller,
    collections: &BTreeMap<String, Collection>,
    base: &str,
    parts: &Parts,
    (c, feature): (&str, Option<&str>),
    body: Body,
) -> Reply {
    let decoded = feature.map(decode);
    let write = match &decoded {
        None => write::Write::of(&parts.method, None),
        Some(f) => f
            .as_deref()
            .and_then(|f| write::Write::of(&parts.method, Some(f))),
    };
    let (Some(c), Some(write)) = (collections.get(c), write) else {
        return problem(StatusCode::NOT_FOUND, "no such resource");
    };
    write::run(view, caller, (c, base), write, &parts.headers, body)
        .await
        .unwrap_or_else(Problem::reply)
}

/// The collections of the view, if its Tenant has a schema this surface reads.
/// ponytail: parsed per request; a per-revision cache as `antares-odata` keeps when a profile
/// shows the parse.
fn collections(view: &ViewOps) -> Option<BTreeMap<String, Collection>> {
    let (_, bytes) = view.schema()?;
    let schema = linkml_model::Model::read(&bytes, &SCHEMA_LIMITS).ok()?;
    Some(model::build(&schema, &view.shown_by_type()))
}

/// No `Accept`, or one that takes some JSON or anything.
fn acceptable(headers: &HeaderMap) -> bool {
    headers.get_all(ACCEPT).iter().next().is_none()
        || headers.get_all(ACCEPT).iter().any(|v| {
            v.to_str().is_ok_and(|v| {
                v.split(',')
                    .map(|range| range.split(';').next().unwrap_or_default().trim())
                    .any(|m| m.contains("json") || m == "*/*" || m == "application/*")
            })
        })
}

/// A path segment percent-decoded, if it decodes to UTF-8.
fn decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = segment.get(i.saturating_add(1)..i.saturating_add(3))?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i = i.saturating_add(3);
        } else {
            out.push(b);
            i = i.saturating_add(1);
        }
    }
    String::from_utf8(out).ok()
}

/// The link object of Part 1 (`rel`, `type`, `href`, Req 29).
fn link(rel: &str, media: &str, href: &str) -> Value {
    json!({"rel": rel, "type": media, "href": href})
}

/// The link relation of a collection's schema (Part 5 `/req/returnables-and-receivables`).
const SCHEMA_REL: &str = "http://www.opengis.net/def/rel/ogc/1.0/schema";
/// The link relation of a collection's sortables (Part 5 `/req/sortables`).
const SORTABLES_REL: &str = "http://www.opengis.net/def/rel/ogc/1.0/sortables";

/// The `profile` link of a response whose collection holds references (Part 5
/// `/req/profile-references`): the profile its references are written in.
fn profile_link(c: &Collection, profile: items::Profile) -> Option<Value> {
    c.properties
        .iter()
        .any(|p| p.reference)
        .then(|| json!({"rel": "profile", "href": items::profile_uri(profile)}))
}

/// A Feature answer with the `Content-Crs` of its coordinates (R4).
fn in_crs(crs: &str, mut reply: Reply) -> Reply {
    // A CRS URI is ASCII: always a header value.
    let value = HeaderValue::from_str(&format!("<{crs}>")).unwrap_or(HeaderValue::from_static(""));
    reply.headers_mut().insert("content-crs", value);
    reply
}

/// What one request reads under.
struct Here<'a> {
    view: &'a ViewOps,
    id: &'a str,
    base: &'a str,
    caller: &'a Caller,
    collections: &'a BTreeMap<String, Collection>,
}

impl Here<'_> {
    /// C1: the landing page.
    fn landing(&self) -> Value {
        let b = self.base;
        json!({
            "title": format!("OGC API - Features of view {}", self.id),
            "links": [
                link("self", JSON, b),
                link("service-desc", OPENAPI, &format!("{b}/api")),
                link("conformance", JSON, &format!("{b}/conformance")),
                link("data", JSON, &format!("{b}/collections")),
            ],
        })
    }

    /// C5: every collection, with its links.
    fn collections(&self) -> Value {
        let all: Vec<Value> = self
            .collections
            .values()
            .map(|c| self.collection(c))
            .collect();
        json!({
            "links": [link("self", JSON, &format!("{}/collections", self.base))],
            "crs": [crs::CRS84, crs::CRS84H],
            "collections": all,
        })
    }

    /// C7: one collection; no `extent` (M8).
    fn collection(&self, c: &Collection) -> Value {
        let at = format!("{}/collections/{}", self.base, c.id);
        let links = json!([
            link("self", JSON, &at),
            link("items", GEOJSON, &format!("{at}/items")),
            link(SCHEMA_REL, SCHEMA, &format!("{at}/schema")),
            link(SORTABLES_REL, SCHEMA, &format!("{at}/sortables")),
        ]);
        let fields = [
            ("id", Some(Value::from(c.id.as_str()))),
            ("title", Some(Value::from(c.id.as_str()))),
            ("description", c.description.as_deref().map(Value::from)),
            ("itemType", Some(Value::from("feature"))),
            ("crs", Some(json!([crs::CRS84, crs::CRS84H]))),
            ("storageCrs", Some(Value::from(crs::CRS84))),
            ("links", Some(links)),
        ];
        Value::Object(
            fields
                .into_iter()
                .filter_map(|(k, v)| Some((k.to_owned(), v?)))
                .collect(),
        )
    }

    /// C8 to C18: one page of Features.
    async fn items(
        &self,
        c: &Collection,
        pairs: &[(String, String)],
        query: Option<&str>,
    ) -> Reply {
        let p = match items::params(c, pairs) {
            Ok(p) => p,
            Err(refused) => return refused.reply(),
        };
        let digest = items::digest(self.id, &c.id, pairs);
        let page = match items::page(self.view, self.caller, c, &p, digest, self.base).await {
            Ok(page) => page,
            Err(refused) => return refused.reply(),
        };
        let at = format!("{}/collections/{}/items", self.base, c.id);
        let this = query.map_or_else(|| at.clone(), |q| format!("{at}?{q}"));
        let mut links = vec![
            link("self", GEOJSON, &this),
            link(
                "collection",
                JSON,
                &format!("{}/collections/{}", self.base, c.id),
            ),
        ];
        if let Some(token) = &page.next {
            let kept = pairs
                .iter()
                .filter(|(n, _)| n != "next")
                .map(|(n, v)| format!("{}={}", items::encode(n), items::encode(v)));
            let query: Vec<String> = kept.chain([format!("next={token}")]).collect();
            links.push(link("next", GEOJSON, &format!("{at}?{}", query.join("&"))));
        }
        links.extend(profile_link(c, p.profile));
        let returned = page.features.len();
        let mut body = json!({
            "type": "FeatureCollection",
            "features": page.features,
            "numberReturned": returned,
            "links": links,
        });
        // Optional in Part 1: omitted when the count would need more than the budget reads.
        if let (Some(matched), Some(members)) = (page.matched, body.as_object_mut()) {
            members.insert("numberMatched".into(), json!(matched));
        }
        in_crs(p.crs, document(GEOJSON, &body))
    }

    /// C19: one Feature; `properties` and `profile` are its only parameters (S6, S7).
    async fn item(&self, c: &Collection, id: &str, pairs: &[(String, String)]) -> Reply {
        let p = match items::item_params(c, pairs) {
            Ok(p) => p,
            Err(refused) => return refused.reply(),
        };
        let refs = items::Refs {
            profile: p.profile,
            base: self.base,
        };
        match items::one(self.view, self.caller, c, id, &p, refs).await {
            Ok(mut feature) => {
                let at = format!("{}/collections/{}", self.base, c.id);
                let own = format!("{at}/items/{}", items::encode(id));
                let links: Vec<Value> =
                    [link("self", GEOJSON, &own), link("collection", JSON, &at)]
                        .into_iter()
                        .chain(profile_link(c, p.profile))
                        .collect();
                feature.insert("links".into(), Value::from(links));
                in_crs(p.crs, document(GEOJSON, &Value::Object(feature)))
            }
            Err(refused) => refused.reply(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_surface_names_its_endpoint() {
        assert_eq!(Ogc.name(), "ogc");
        assert_eq!(
            Ogc.version_info().pointer("/endpoint"),
            Some(&json!("/x/views/{view}/ogc/features"))
        );
    }

    #[test]
    fn segments_decode_or_are_refused() {
        assert_eq!(
            decode("urn%3Angsi-ld%3ACar%3A1").as_deref(),
            Some("urn:ngsi-ld:Car:1")
        );
        assert_eq!(
            decode("urn:ngsi-ld:Car:1").as_deref(),
            Some("urn:ngsi-ld:Car:1")
        );
        assert_eq!(decode("a%2"), None);
        assert_eq!(decode("a%zz"), None);
        assert_eq!(decode("%ff"), None, "not UTF-8");
    }

    #[test]
    fn accept_takes_json_or_anything() {
        let with = |v: &'static str| {
            let mut h = HeaderMap::new();
            h.insert(ACCEPT, HeaderValue::from_static(v));
            acceptable(&h)
        };
        assert!(acceptable(&HeaderMap::new()));
        assert!(with("application/geo+json"));
        assert!(with("text/html;q=1, */*;q=0.1"));
        assert!(with("application/*"));
        assert!(!with("text/html"));
        assert!(!with("application/gml+xml; profile=x"));
    }
}
