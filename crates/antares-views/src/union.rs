// SPDX-License-Identifier: EUPL-1.2
//! One Query Entities across many views (T-4494,
//! `docs/analysis/view-union.md`): `GET /x/views/_union/ngsi-ld/v1/entities?views=a,b`
//! runs the read through each named view as `/x/views/{view}` runs it, and
//! merges the pages by Entity id under one cursor. `Antares-Entity-Views` names
//! the view of each Entity in body order; the body stays what the views
//! answered. A view the caller may not read contributes nothing.

use antares_ops::{Caller, OperationId, Ops, OpsRequest, Refusal, ViewOps};
use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, StreamExt};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::LazyLock;

/// Where the union answers, below the view root. `_` is no view-id
/// character, so no view shadows it.
pub const ROUTE: &str = "/_union/ngsi-ld/v1/entities";
/// The response header naming the view of each Entity, in body order.
pub const VIEWS_HEADER: &str = "antares-entity-views";
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;
/// Entities buffered across every view at once; each view is asked for its
/// share of it per page.
const ROWS_HELD: usize = 2000;
/// Views asked at the same time.
const WIDTH: usize = 16;
/// The largest page one view may answer.
const PAGE_BYTES: usize = 32 * 1024 * 1024;
/// The parameters the union reads itself; every other one goes to each view.
const OWN: [&str; 5] = ["views", "cursor", "limit", "offset", "count"];
const LD: &str = "application/ld+json";

/// The variable naming the most views one union read may name (200).
pub const MAX_VIEWS_ENV: &str = "ANTARES_UNION_MAX_VIEWS";
static MAX_VIEWS: LazyLock<usize> = LazyLock::new(|| {
    std::env::var(MAX_VIEWS_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(200)
});

/// The union read of `req` as `caller`.
pub async fn serve(ops: &Ops, caller: &Caller, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    if parts.method != Method::GET && parts.method != Method::HEAD {
        return Refusal::NotAllowed {
            allow: vec![Method::GET, Method::HEAD],
        }
        .ngsi_ld();
    }
    let ask = match Ask::of(&parts) {
        Ok(ask) => ask,
        Err(refusal) => return refusal.ngsi_ld(),
    };
    let Some(json) = wants_json(&parts.headers) else {
        return not_acceptable();
    };
    match run(ops, caller, &parts.headers, &ask).await {
        Ok(page) => page.answer(&ask, json),
        Err(answer) => *answer,
    }
}

/// The union's own parameters and what goes on to each view.
struct Ask {
    views: Vec<String>,
    offsets: Vec<u64>,
    limit: usize,
    count: bool,
    /// The caller's other parameters, as sent.
    rest: Vec<String>,
}

impl Ask {
    fn of(parts: &Parts) -> Result<Self, Refusal> {
        if parts.headers.contains_key("ngsild-tenant") {
            return Err(Refusal::Malformed(
                "a union names no tenant: each view reads its own",
            ));
        }
        if parts.headers.contains_key("ngsild-entitymap") {
            return Err(Refusal::Malformed(crate::NO_MAPS));
        }
        let decoded = axum::extract::Query::<Vec<(String, String)>>::try_from_uri(&parts.uri)
            .map_err(|_| Refusal::Malformed("the query is no form encoding"))?
            .0;
        for (key, _) in &decoded {
            match key.as_str() {
                "orderBy" | "collation" => {
                    return Err(Refusal::Malformed(
                        "a read across views is never ordered by orderBy (4.23.1): it is in Entity id order",
                    ))
                }
                "entityMap" => return Err(Refusal::Malformed(crate::NO_MAPS)),
                _ => {}
            }
        }
        let mut rest = Vec::new();
        let mut own_sent = 0usize;
        for pair in parts.uri.query().unwrap_or_default().split('&') {
            let key = pair.split_once('=').map_or(pair, |(k, _)| k);
            if OWN.contains(&key) {
                own_sent = own_sent.saturating_add(1);
            } else if !pair.is_empty() {
                rest.push(pair.to_owned());
            }
        }
        let own: Vec<&(String, String)> = decoded
            .iter()
            .filter(|(k, _)| OWN.contains(&k.as_str()))
            .collect();
        // an encoded `views` would reach each view as its own parameter
        if own.len() != own_sent {
            return Err(Refusal::Malformed("a union parameter is percent-encoded"));
        }
        let get = |name: &str| -> Result<Option<&str>, Refusal> {
            let mut all = own.iter().filter(|(k, _)| k == name);
            match (all.next(), all.next()) {
                (one, None) => Ok(one.map(|(_, v)| v.as_str())),
                _ => Err(Refusal::Malformed("a union parameter is given twice")),
            }
        };
        let mut views: Vec<String> = Vec::new();
        for id in get("views")?.unwrap_or_default().split(',').map(str::trim) {
            if !id.is_empty() && !views.iter().any(|v| v == id) {
                views.push(id.to_owned());
            }
        }
        if views.is_empty() {
            return Err(Refusal::Malformed("views names no view"));
        }
        if views.len() > *MAX_VIEWS {
            return Err(Refusal::Limit(
                "views names more views than ANTARES_UNION_MAX_VIEWS",
            ));
        }
        let limit = match get("limit")? {
            None => DEFAULT_LIMIT,
            Some(v) => v
                .parse()
                .map_err(|_| Refusal::Malformed("limit is no count"))?,
        };
        if limit > MAX_LIMIT {
            return Err(Refusal::Limit(
                "limit is above 1000 for a read across views",
            ));
        }
        if get("offset")?.is_some_and(|o| o != "0") {
            return Err(Refusal::Malformed(
                "a read across views pages with the cursor of its next link, never an offset",
            ));
        }
        let offsets = match get("cursor")? {
            None => vec![0; views.len()],
            Some(c) => c
                .split('.')
                .map(str::parse)
                .collect::<Result<Vec<u64>, _>>()
                .ok()
                .filter(|o| o.len() == views.len())
                .ok_or(Refusal::Malformed("cursor does not fit views"))?,
        };
        Ok(Self {
            views,
            offsets,
            limit,
            count: get("count")? == Some("true"),
            rest,
        })
    }

    /// The query of one view's page.
    fn target(&self, offset: u64, limit: usize, count: bool) -> String {
        let mut q = self.rest.clone();
        q.push(format!("limit={limit}"));
        q.push(format!("offset={offset}"));
        if count {
            q.push("count=true".to_owned());
        }
        format!("/ngsi-ld/v1/entities?{}", q.join("&"))
    }
}

/// Whether the caller takes plain JSON (`Some(true)`), JSON-LD
/// (`Some(false)`) or neither.
fn wants_json(headers: &HeaderMap) -> Option<bool> {
    let Some(accept) = headers.get(header::ACCEPT) else {
        return Some(true);
    };
    let accept = accept.to_str().ok()?;
    if accept.contains(LD) {
        Some(false)
    } else if ["application/json", "*/*", "application/*"]
        .iter()
        .any(|m| accept.contains(m))
    {
        Some(true)
    } else {
        None
    }
}

fn not_acceptable() -> Response {
    (
        StatusCode::NOT_ACCEPTABLE,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"availableRepresentations": [LD]}).to_string(),
    )
        .into_response()
}

/// One view's part of the union.
struct Part {
    view: Option<ViewOps>,
    /// How far the union has read this view.
    read: u64,
    buf: VecDeque<Value>,
    done: bool,
}

/// One page of the union: each Entity with the index of its view.
struct Page {
    docs: Vec<(Value, usize)>,
    ids: Vec<String>,
    count: Option<u64>,
    next: Option<String>,
}

/// The answer that ends a union early.
type Stop = Box<Response>;
/// One view's page: its Entities and count, `None` when it gives nothing.
type Fetched = Result<Option<(Vec<Value>, u64)>, Stop>;

async fn run(ops: &Ops, caller: &Caller, headers: &HeaderMap, ask: &Ask) -> Result<Page, Stop> {
    let mut parts: Vec<Part> = ask
        .views
        .iter()
        .zip(&ask.offsets)
        .map(|(id, read)| {
            let view = ops
                .mapped_view(id)
                .filter(|v| v.allows(OperationId::QueryEntities));
            Part {
                done: view.is_none(),
                view,
                read: *read,
                buf: VecDeque::new(),
            }
        })
        .collect();
    let live = parts.iter().filter(|p| !p.done).count().max(1);
    let chunk = ROWS_HELD
        .checked_div(live)
        .unwrap_or(1)
        .clamp(1, ask.limit.max(1));
    let first = if ask.limit == 0 { 0 } else { chunk };
    let mut total = 0u64;
    if ask.limit > 0 || ask.count {
        refill(
            &mut parts, caller, headers, ask, first, ask.count, &mut total,
        )
        .await?;
    }
    let mut docs = Vec::new();
    while docs.len() < ask.limit {
        refill(&mut parts, caller, headers, ask, chunk, false, &mut total).await?;
        let head = parts
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.buf.front().map(|d| (id_of(d), i)))
            .min()
            .map(|(_, i)| i);
        let Some((i, part)) = head.and_then(|i| parts.get_mut(i).map(|p| (i, p))) else {
            break;
        };
        if let Some(doc) = part.buf.pop_front() {
            part.read = part.read.saturating_add(1);
            docs.push((doc, i));
        }
    }
    let more = ask.limit > 0 && parts.iter().any(|p| !p.buf.is_empty() || !p.done);
    let next = more.then(|| {
        parts
            .iter()
            .map(|p| p.read.to_string())
            .collect::<Vec<_>>()
            .join(".")
    });
    Ok(Page {
        ids: ask.views.clone(),
        docs,
        count: ask.count.then_some(total),
        next,
    })
}

fn id_of(doc: &Value) -> &str {
    doc.get("id").and_then(Value::as_str).unwrap_or_default()
}

/// Read the next `limit` Entities of every part whose buffer ran out, at
/// most [`WIDTH`] views at a time. A limit of 0 reads counts alone.
async fn refill(
    parts: &mut [Part],
    caller: &Caller,
    headers: &HeaderMap,
    ask: &Ask,
    limit: usize,
    count: bool,
    total: &mut u64,
) -> Result<(), Stop> {
    let jobs: Vec<(usize, ViewOps, u64)> = parts
        .iter()
        .enumerate()
        .filter(|(_, p)| p.buf.is_empty() && !p.done)
        .filter_map(|(i, p)| p.view.clone().map(|v| (i, v, p.read)))
        .collect();
    let answers: Vec<(usize, Fetched)> = stream::iter(jobs)
        .map(|(i, view, offset)| async move {
            let target = ask.target(offset, limit, count);
            (i, read(&view, caller, headers, target).await)
        })
        .buffer_unordered(WIDTH)
        .collect()
        .await;
    for (i, answer) in answers {
        let Some(part) = parts.get_mut(i) else {
            continue;
        };
        match answer? {
            Some((docs, n)) => {
                *total = total.saturating_add(n);
                part.done = limit == 0 || docs.len() < limit;
                part.buf.extend(docs);
            }
            None => part.done = true,
        }
    }
    Ok(())
}

/// One view's page as a direct read through it answers: its Entities and
/// count, `None` when the view gives the caller nothing (unknown, refused,
/// out of reach: no sign which), or the answer that ends the union (a
/// request every view refuses alike, a failure inside the broker).
async fn read(view: &ViewOps, caller: &Caller, headers: &HeaderMap, target: String) -> Fetched {
    let mut req = OpsRequest::new(Method::GET, target);
    for (name, value) in headers {
        if name != header::ACCEPT {
            req = req.header(name.clone(), value.clone());
        }
    }
    let req = req.header(header::ACCEPT, HeaderValue::from_static(LD));
    let answer = match view.run(OperationId::QueryEntities, caller, req).await {
        Ok(answer) => answer,
        Err(
            Refusal::NotFound
            | Refusal::NotAllowed { .. }
            | Refusal::NotSupported
            | Refusal::Denied(_)
            | Refusal::Closed,
        ) => return Ok(None),
        Err(other) => return Err(Box::new(other.ngsi_ld())),
    };
    let status = answer.status();
    let count = answer
        .headers()
        .get("ngsild-results-count")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let (head, body) = answer.into_parts();
    let bytes = to_bytes(body, PAGE_BYTES).await.map_err(|_| {
        Box::new(
            Refusal::Limit("a view's page is over the 32 MiB a read across views takes").ngsi_ld(),
        )
    })?;
    if status == StatusCode::OK {
        let docs = serde_json::from_slice(&bytes)
            .map_err(|_| Box::new(Refusal::Limit("a view answered no Entity list").ngsi_ld()))?;
        return Ok(Some((docs, count)));
    }
    // 403 is also TooManyResults (6.3.2), which every view would answer alike
    let too_many = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|p| p.get("type").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|t| t.ends_with("/TooManyResults"));
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND if !too_many => {
            Ok(None)
        }
        _ => Err(Box::new(Response::from_parts(
            head,
            axum::body::Body::from(bytes),
        ))),
    }
}

impl Page {
    fn answer(self, ask: &Ask, json: bool) -> Response {
        let tags: Vec<&str> = self
            .docs
            .iter()
            .filter_map(|(_, i)| self.ids.get(*i).map(String::as_str))
            .collect();
        let mut docs: Vec<Value> = self.docs.into_iter().map(|(d, _)| d).collect();
        let mut response_headers = HeaderMap::new();
        let content = if json {
            match one_context(&docs) {
                Ok(context) => {
                    for d in &mut docs {
                        if let Some(o) = d.as_object_mut() {
                            o.remove("@context");
                        }
                    }
                    if let Some(url) = context {
                        let link = format!(
                            "<{url}>; rel=\"http://www.w3.org/ns/json-ld#context\"; type=\"{LD}\""
                        );
                        response_headers
                            .extend(HeaderValue::from_str(&link).ok().map(|v| (header::LINK, v)));
                    }
                    "application/json"
                }
                Err(()) => return not_acceptable(),
            }
        } else {
            LD
        };
        response_headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content));
        // view ids are `[a-z0-9-]`, so the list is a header value
        response_headers.extend(
            HeaderValue::from_str(&tags.join(","))
                .ok()
                .map(|v| (axum::http::HeaderName::from_static(VIEWS_HEADER), v)),
        );
        if let Some(n) = self.count {
            response_headers.insert("ngsild-results-count", HeaderValue::from(n));
        }
        if let Some(cursor) = self.next {
            let mut q: Vec<String> = ask.rest.clone();
            q.push(format!("views={}", ask.views.join(",")));
            q.push(format!("limit={}", ask.limit));
            if ask.count {
                q.push("count=true".to_owned());
            }
            q.push(format!("cursor={cursor}"));
            let link = format!(
                "<{}{ROUTE}?{}>; rel=\"next\"; type=\"{LD}\"",
                crate::PREFIX,
                q.join("&")
            );
            response_headers.extend(HeaderValue::from_str(&link).ok().map(|v| (header::LINK, v)));
        }
        let mut response = (StatusCode::OK, Value::Array(docs).to_string()).into_response();
        response.headers_mut().extend(response_headers);
        response
    }
}

/// The one context URL every Entity of `docs` names, `None` for no Entity;
/// `Err` when they name different ones or more than a URL.
fn one_context(docs: &[Value]) -> Result<Option<String>, ()> {
    let mut seen: Option<&str> = None;
    for d in docs {
        let url = match d.get("@context") {
            Some(Value::String(s)) => s.as_str(),
            Some(Value::Array(a)) if a.len() == 1 => a.first().and_then(Value::as_str).ok_or(())?,
            _ => return Err(()),
        };
        match seen {
            Some(s) if s != url => return Err(()),
            _ => seen = Some(url),
        }
    }
    Ok(seen.map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(query: &str) -> Result<Ask, Refusal> {
        let req = axum::http::Request::builder()
            .uri(format!("/x/views{ROUTE}?{query}"))
            .body(())
            .map_err(|_| Refusal::Malformed("test uri"))?;
        Ask::of(&req.into_parts().0)
    }

    fn refused(query: &str) -> bool {
        ask(query).is_err()
    }

    #[test]
    fn the_union_reads_its_own_parameters_and_passes_the_rest() -> Result<(), String> {
        let a = ask("type=Car&views=b,a,b,%20&q=speed%3E1&count=true&limit=5")
            .map_err(|r| format!("{r:?}"))?;
        assert_eq!(a.views, ["b", "a"]);
        assert_eq!(a.offsets, [0, 0]);
        assert_eq!((a.limit, a.count), (5, true));
        assert_eq!(a.rest, ["type=Car", "q=speed%3E1"]);
        assert_eq!(
            a.target(7, 3, false),
            "/ngsi-ld/v1/entities?type=Car&q=speed%3E1&limit=3&offset=7"
        );
        let c = ask("views=a,b&cursor=4.0").map_err(|r| format!("{r:?}"))?;
        assert_eq!((c.offsets, c.limit), (vec![4, 0], DEFAULT_LIMIT));
        Ok(())
    }

    #[test]
    fn what_a_union_cannot_honour_is_refused() {
        for query in [
            "type=Car",
            "views=",
            "views=a&orderBy=id",
            "views=a&collation=de",
            "views=a&offset=2",
            "views=a&limit=1001",
            "views=a&limit=x",
            "views=a,b&cursor=1",
            "views=a&cursor=x",
            "views=a&views=b",
            "vi%65ws=a",
            "views=a&entityMap=true",
        ] {
            assert!(refused(query), "{query}");
        }
        let many: Vec<String> = (0..=200).map(|i| format!("v{i}")).collect();
        assert!(refused(&format!("views={}", many.join(","))));
        assert!(!refused("views=a&offset=0&limit=0"));
    }

    #[test]
    fn plain_json_keeps_one_context_or_none() {
        let core = serde_json::json!({"id": "a", "@context": "c"});
        let arr = serde_json::json!({"id": "b", "@context": ["c"]});
        let other = serde_json::json!({"id": "c", "@context": "d"});
        assert_eq!(one_context(&[]), Ok(None));
        assert_eq!(one_context(&[core.clone(), arr]), Ok(Some("c".to_owned())));
        assert_eq!(one_context(&[core, other]), Err(()));
        assert_eq!(wants_json(&HeaderMap::new()), Some(true));
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/ld+json"),
        );
        assert_eq!(wants_json(&h), Some(false));
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/geo+json"),
        );
        assert_eq!(wants_json(&h), None);
    }
}
