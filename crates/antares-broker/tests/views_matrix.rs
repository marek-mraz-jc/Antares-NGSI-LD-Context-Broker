// SPDX-License-Identifier: EUPL-1.2
//! The view and policy matrix (T-3777): every scenario of
//! `tests/views-matrix/` (a view, the policies of the joinedcontext engine,
//! a caller, the Entities and Attributes NGSI-LD admits, written by hand
//! from the seed) is read through every surface of a `full,jc-policy`
//! build, and each must answer exactly the expected Entities with exactly
//! the expected Attribute names: no more, no fewer.
//!
//! Each scenario runs in a Tenant of its own over the same seed
//! (`_seed.json`) and LinkML schema (`_schema.json`). A surface is read
//! page by page, so a next link or cursor that leaked or lost an Entity is
//! red, and its count (`NGSILD-Results-Count`, `@odata.count`, MCP
//! `total`, OGC `numberMatched`) must be the expected size. A retrieve of a
//! seed Entity the scenario does not admit must answer the bytes an absent
//! id gets. A denied caller gets a refusal naming no seed Entity.
//!
//! A scenario may carry `steps`: a move of an Entity (written on the
//! Tenant root, outside the engine) or a write by the caller through the
//! view, each with the expected answer after it.
#![cfg(all(feature = "full", feature = "jc-policy"))]

use antares_api::AppState;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tower::Service;

#[path = "common/ws_client.rs"]
mod ws_client;
use ws_client::Socket;

const SEED: &str = include_str!("views-matrix/_seed.json");
const SCHEMA: &str = include_str!("views-matrix/_schema.json");
/// The types of the seed, in the order a `*` view is queried by.
const TYPES: [&str; 3] = ["Vehicle", "Parking", "Sensor"];
/// The page size every surface is read with: small, so every answer pages.
const PAGE: usize = 3;
/// The read and subscription clauses a scenario without policies grants
/// the public role whole, so the view alone decides.
const READS: [&str; 15] = [
    "5.7.1", "5.7.2", "5.7.3", "5.7.4", "5.7.5", "5.7.6", "5.7.7", "5.7.8", "5.7.9", "5.7.10",
    "5.8.1", "5.8.2", "5.8.3", "5.8.4", "5.8.5",
];
/// The service account the harness writes the seed and the moves as: a
/// permission of its own in every document, never the scenario's caller.
const SEEDER: &str = "matrix-seed";

/// The members of an Entity that are not Attributes.
const FRAME: [&str; 7] = [
    "id",
    "type",
    "scope",
    "@context",
    "createdAt",
    "modifiedAt",
    "deletedAt",
];

/// Entity id → its Attribute names.
type Answer = BTreeMap<String, BTreeSet<String>>;

/// What a scenario expects: the Entities and their Attributes, or a refusal.
#[derive(Debug, Clone, PartialEq)]
enum Expect {
    Admitted(Answer),
    Denied,
}

fn expect(v: &Value) -> Expect {
    if v == "denied" {
        return Expect::Denied;
    }
    Expect::Admitted(
        v.as_object()
            .into_iter()
            .flatten()
            .map(|(id, names)| {
                let names = names
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                (id.clone(), names)
            })
            .collect(),
    )
}

struct Scenario {
    name: String,
    tenant: String,
    doc: Value,
}

impl Scenario {
    fn view(&self) -> &str {
        &self.tenant
    }

    /// The types a read asks for: the view's selector, or every seed type
    /// for a `*` view.
    fn types(&self) -> Vec<String> {
        let selector: Vec<String> = self.doc["view"]["selector"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        if selector.iter().any(|t| t == "*") {
            TYPES.map(str::to_owned).to_vec()
        } else {
            selector
        }
    }

    fn caller(&self) -> Vec<(String, String)> {
        self.doc["caller"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
            .collect()
    }

    /// The rule document: the scenario's policies (the public role reading
    /// everything when it has none) and its view binding.
    fn policy(&self) -> Value {
        let policies = match self.doc.get("policies") {
            Some(p) => p.clone(),
            None => json!([{"id": "open", "assignee": {"kind": "role", "id": "public"},
                            "operations": READS}]),
        };
        let mut doc = json!({ "policies": policies });
        if let Some(all) = doc["policies"].as_array_mut() {
            all.push(
                json!({"id": "matrix-seed", "assignee": {"kind": "serviceAccount", "id": SEEDER},
                            "operations": ["5.6.1", "5.6.2", "5.6.8"],
                            "information": [{"entities": TYPES.map(|t| json!({"type": t}))}]}),
            );
        }
        if let Some(bound) = self.doc.get("bind") {
            doc["views"] = json!({ self.view(): bound });
        }
        doc
    }
}

fn corpus() -> Vec<Scenario> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/views-matrix");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("the corpus")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().is_some_and(|x| x == "json")
                && !p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('_'))
        })
        .collect();
    files.sort();
    files
        .iter()
        .enumerate()
        .map(|(n, path)| {
            let text = std::fs::read_to_string(path).expect("a scenario");
            Scenario {
                name: path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_owned(),
                tenant: format!("m{n:02}"),
                doc: serde_json::from_str(&text)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display())),
            }
        })
        .collect()
}

/// The broker of a `full` build with the joinedcontext engine set before
/// it is wired, as `main` sets it, so notifications are decided by it too.
async fn broker() -> (Router, antares_ops::views::Reload) {
    std::env::set_var(antares_ops::views::SCHEMA_MAX_ENV, "65536");
    std::env::set_var(
        "ANTARES_POLICY_SUBJECT_HEADERS",
        joinedcontext_policy::subject::HEADERS.join(","),
    );
    let mut state = AppState::new("views-matrix".into())
        .with_sink(Box::new(antares_ws::Sink::new(antares_ws::Hub::process())))
        .with_policy(std::sync::Arc::new(
            joinedcontext_policy::JoinedcontextPolicy::new(),
        ));
    antares_api::wire(&mut state).await;
    let surfaces: Vec<Box<dyn antares_api::ApiSurface>> = vec![
        Box::new(antares_api::Admin),
        antares_ops::mount(antares_views::Views),
        antares_ops::mount(antares_odata::OData),
        antares_ops::mount(antares_ogc::Ogc),
        antares_ops::mount(antares_mcp::Mcp),
        antares_ops::mount(antares_ws::ViewWs::process()),
    ];
    let state = state.with_surfaces(surfaces).expect("the surfaces mount");
    let reload = antares_ops::views::install(&state)
        .await
        .expect("the view table");
    (antares_api::router(state), reload)
}

/// One request; status, headers and the JSON body (the text as a string
/// when it is not JSON).
async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(String, String)],
    body: Option<&Value>,
) -> (StatusCode, HeaderMap, Value) {
    let text = body.map(Value::to_string).unwrap_or_default();
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("Host", "localhost");
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if body.is_some() {
        let kind = if method == "PATCH" && uri.contains("/ogc/") {
            "application/merge-patch+json"
        } else {
            "application/json"
        };
        req = req
            .header("Content-Type", kind)
            .header("Content-Length", text.len());
    }
    let Ok(res) = app
        .clone()
        .call(req.body(Body::from(text)).expect("a request"))
        .await;
    let (status, headers) = (res.status(), res.headers().clone());
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, headers, body)
}

fn tenant_header(s: &Scenario) -> (String, String) {
    ("NGSILD-Tenant".to_owned(), s.tenant.clone())
}

fn seeder(s: &Scenario) -> Vec<(String, String)> {
    vec![
        ("x-jc-client".to_owned(), SEEDER.to_owned()),
        tenant_header(s),
    ]
}

/// The view, schema and rule document of `s`, in its Tenant.
async fn install(app: &Router, s: &Scenario) {
    let ops: Vec<&str> = antares_ops::OperationId::ALL
        .iter()
        .map(|op| op.id())
        .collect();
    let mut view = s.doc["view"].clone();
    view["id"] = json!(s.view());
    view["tenant"] = json!(s.tenant);
    view["operations"] = json!(ops);
    let schema: Value = serde_json::from_str(SCHEMA).expect("the schema");
    for (uri, doc) in [
        (format!("/q/views/{}", s.view()), view),
        (format!("/q/schemas/{}", s.tenant), schema),
        (format!("/q/policy/{}", s.tenant), s.policy()),
    ] {
        let (status, _, body) = send(app, "PUT", &uri, &[], Some(&doc)).await;
        assert!(status.is_success(), "{}: {uri}: {status} {body}", s.name);
    }
}

/// The seed, written in the Tenant of `s` as the seeder.
async fn seed(app: &Router, s: &Scenario) {
    let seed: Value = serde_json::from_str(SEED).expect("the seed");
    let uri = "/ngsi-ld/v1/entityOperations/upsert";
    let (status, _, body) = send(app, "POST", uri, &seeder(s), Some(&seed)).await;
    assert!(status.is_success(), "{}: seed: {status} {body}", s.name);
}

/// The Attribute names of an NGSI-LD Entity.
fn ngsi_names(e: &Value) -> BTreeSet<String> {
    e.as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k.clone())
        .filter(|k| !FRAME.contains(&k.as_str()))
        .collect()
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A next link as a path below the view root.
fn local(href: &str) -> String {
    href.find("/x/views/")
        .map_or(href, |i| &href[i..])
        .to_owned()
}

/// What one surface answered: the Entities read, the count it reported, or
/// a refusal with its status and body.
#[derive(Debug)]
enum Read {
    Entities(Answer, Option<u64>),
    Refused(StatusCode, Value),
}

/// Insert `id` with `names`, or say it came twice.
fn add(out: &mut Answer, id: String, names: BTreeSet<String>, twice: &mut Vec<String>) {
    if out.insert(id.clone(), names).is_some() {
        twice.push(id);
    }
}

/// NGSI-LD Query Entities through the view, following `Link: rel="next"`.
/// With `temporal`, Query Temporal Evolution of Entities (11.3.3) over
/// every instance since 2000 instead.
async fn ngsi_ld(app: &Router, s: &Scenario, temporal: bool) -> Read {
    let who = with_tenant(s);
    let (path, window) = if temporal {
        (
            "temporal/entities",
            "&timerel=after&timeAt=2000-01-01T00:00:00Z&timeproperty=modifiedAt",
        )
    } else {
        ("entities", "")
    };
    let mut uri = format!(
        "/x/views/{}/ngsi-ld/v1/{path}?type={}&limit={PAGE}&count=true{window}",
        s.view(),
        s.types().join(",")
    );
    let (mut out, mut twice, mut count) = (Answer::new(), Vec::new(), None);
    loop {
        let (status, headers, body) = send(app, "GET", &uri, &who, None).await;
        if status != StatusCode::OK {
            return Read::Refused(status, body);
        }
        if count.is_none() {
            count = headers
                .get("NGSILD-Results-Count")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.parse().ok());
        }
        for e in body.as_array().into_iter().flatten() {
            add(&mut out, text(e, "id"), ngsi_names(e), &mut twice);
        }
        let next = headers
            .get_all("Link")
            .iter()
            .filter_map(|h| h.to_str().ok())
            .find(|l| l.contains("rel=\"next\""))
            .and_then(|l| Some(l[l.find('<')? + 1..l.find('>')?].to_owned()));
        match next {
            Some(n) => uri = local(&n),
            None => break,
        }
    }
    if !twice.is_empty() {
        out.insert(format!("twice: {twice:?}"), BTreeSet::new());
    }
    Read::Entities(out, count)
}

/// The OData EntitySet of each type, following `@odata.nextLink`; a set
/// the view does not serve (404) holds nothing.
async fn odata(app: &Router, s: &Scenario) -> Read {
    let who = with_tenant(s);
    let (mut out, mut twice, mut total) = (Answer::new(), Vec::new(), 0u64);
    for t in s.types() {
        let mut uri = format!("/x/views/{}/odata/v4/{t}?$count=true", s.view());
        let mut first = true;
        let mut paged = who.clone();
        paged.push(("Prefer".to_owned(), format!("odata.maxpagesize={PAGE}")));
        loop {
            let (status, _, body) = send(app, "GET", &uri, &paged, None).await;
            if status == StatusCode::NOT_FOUND && first {
                break;
            }
            if status != StatusCode::OK {
                return Read::Refused(status, body);
            }
            if first {
                total += body
                    .get("@odata.count")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::MAX / 4);
                first = false;
            }
            for row in body
                .get("value")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let names = row
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(k, v)| {
                        !k.contains('@')
                            && !matches!(k.as_str(), "id" | "_types" | "scope")
                            && !v.is_null()
                    })
                    .map(|(k, _)| k.clone())
                    .collect();
                add(&mut out, text(row, "id"), names, &mut twice);
            }
            match body.get("@odata.nextLink").and_then(Value::as_str) {
                Some(next) => uri = local(next),
                None => break,
            }
        }
    }
    if !twice.is_empty() {
        out.insert(format!("twice: {twice:?}"), BTreeSet::new());
    }
    Read::Entities(out, Some(total))
}

/// OGC API Features items of each collection, following `rel=next`; a
/// collection the view does not serve (404) holds nothing. The geometry is
/// `location`.
async fn ogc(app: &Router, s: &Scenario) -> Read {
    let who = with_tenant(s);
    let (mut out, mut twice, mut total) = (Answer::new(), Vec::new(), 0u64);
    for t in s.types() {
        let mut uri = format!(
            "/x/views/{}/ogc/features/collections/{t}/items?limit={PAGE}",
            s.view()
        );
        let mut first = true;
        loop {
            let (status, _, body) = send(app, "GET", &uri, &who, None).await;
            if status == StatusCode::NOT_FOUND && first {
                break;
            }
            if status != StatusCode::OK {
                return Read::Refused(status, body);
            }
            if first {
                total += body
                    .get("numberMatched")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::MAX / 4);
                first = false;
            }
            for f in body
                .get("features")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let mut names: BTreeSet<String> = f
                    .get("properties")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, _)| k.clone())
                    .collect();
                if f.get("geometry").is_some_and(|g| !g.is_null()) {
                    names.insert("location".to_owned());
                }
                add(&mut out, text(f, "id"), names, &mut twice);
            }
            let next = body
                .get("links")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|l| l["rel"] == "next")
                .and_then(|l| l["href"].as_str())
                .map(local);
            match next {
                Some(n) => uri = n,
                None => break,
            }
        }
    }
    if !twice.is_empty() {
        out.insert(format!("twice: {twice:?}"), BTreeSet::new());
    }
    Read::Entities(out, Some(total))
}

/// One MCP `tools/call` through the view; the JSON-RPC answer.
async fn mcp_call(app: &Router, s: &Scenario, tool: &str, args: Value) -> Value {
    const MODERN: &str = "2026-07-28";
    let params = json!({"name": tool, "arguments": args, "_meta": {
        "io.modelcontextprotocol/protocolVersion": MODERN,
        "io.modelcontextprotocol/clientInfo": {"name": "views-matrix", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {}
    }});
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params});
    let mut who = with_tenant(s);
    for (k, v) in [
        ("Accept", "application/json, text/event-stream"),
        ("MCP-Protocol-Version", MODERN),
        ("Mcp-Method", "tools/call"),
        ("Mcp-Name", tool),
    ] {
        who.push((k.to_owned(), v.to_owned()));
    }
    let uri = format!("/x/views/{}/mcp", s.view());
    send(app, "POST", &uri, &who, Some(&body)).await.2
}

/// MCP `query_entities`, cursor by cursor; an `isError` result is a refusal.
async fn mcp(app: &Router, s: &Scenario) -> Read {
    let (mut out, mut twice, mut total) = (Answer::new(), Vec::new(), None);
    let mut cursor: Option<String> = None;
    loop {
        let mut args = json!({"type": s.types(), "limit": PAGE, "count": true});
        if let Some(c) = &cursor {
            args["cursor"] = json!(c);
        }
        let answer = mcp_call(app, s, "query_entities", args).await;
        let Some(page) = answer
            .pointer("/result/structuredContent")
            .filter(|_| answer.pointer("/result/isError") != Some(&json!(true)))
        else {
            return Read::Refused(StatusCode::FORBIDDEN, answer);
        };
        if total.is_none() {
            total = page.get("total").and_then(Value::as_u64);
        }
        for e in page
            .get("entities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            add(&mut out, text(e, "id"), ngsi_names(e), &mut twice);
        }
        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    if !twice.is_empty() {
        out.insert(format!("twice: {twice:?}"), BTreeSet::new());
    }
    Read::Entities(out, total)
}

fn with_tenant(s: &Scenario) -> Vec<(String, String)> {
    let mut who = s.caller();
    who.push(tenant_header(s));
    who
}

/// Every seed id, retrieved one by one through the NGSI-LD view and MCP
/// `get_entity`: an admitted one with its Attributes, any other with the
/// answer an absent id gets (its own id masked).
async fn by_id(app: &Router, s: &Scenario, want: &Answer, failures: &mut Vec<String>) {
    let seed: Value = serde_json::from_str(SEED).expect("the seed");
    let who = with_tenant(s);
    let absent = "urn:ngsi-ld:Vehicle:absent";
    let masked = |body: &Value, id: &str| body.to_string().replace(id, "ID");
    let root = format!("/x/views/{}/ngsi-ld/v1/entities", s.view());
    let (_, _, nothing) = send(app, "GET", &format!("{root}/{absent}"), &who, None).await;
    let mcp_nothing = mcp_call(app, s, "get_entity", json!({"id": absent})).await;
    let refused = s.doc["byId"] == "denied";
    for e in seed.as_array().into_iter().flatten() {
        let id = text(e, "id");
        let (status, _, body) = send(app, "GET", &format!("{root}/{id}"), &who, None).await;
        let answer = mcp_call(app, s, "get_entity", json!({"id": id})).await;
        if refused {
            if status != StatusCode::FORBIDDEN || masked(&body, &id) != masked(&nothing, absent) {
                failures.push(format!(
                    "NGSI-LD retrieve {id} refused: {status} {body}, an absent id gets {nothing}"
                ));
            }
            if masked(&answer, &id) != masked(&mcp_nothing, absent) {
                failures.push(format!(
                    "MCP get_entity {id} refused: {answer}, an absent id gets {mcp_nothing}"
                ));
            }
            continue;
        }
        let mcp_entity = answer
            .pointer("/result/structuredContent/entity")
            .filter(|_| answer.pointer("/result/isError") != Some(&json!(true)));
        match want.get(&id) {
            Some(names) => {
                if status != StatusCode::OK || &ngsi_names(&body) != names {
                    failures.push(format!("NGSI-LD retrieve {id}: {status} {body}"));
                }
                if mcp_entity.map(ngsi_names).as_ref() != Some(names) {
                    failures.push(format!("MCP get_entity {id}: {answer}"));
                }
            }
            None => {
                if status != StatusCode::NOT_FOUND || masked(&body, &id) != masked(&nothing, absent)
                {
                    failures.push(format!(
                        "NGSI-LD retrieve {id} not admitted: {status} {body}, an absent id gets {nothing}"
                    ));
                }
                if mcp_entity.is_some() || masked(&answer, &id) != masked(&mcp_nothing, absent) {
                    failures.push(format!(
                        "MCP get_entity {id} not admitted: {answer}, an absent id gets {mcp_nothing}"
                    ));
                }
            }
        }
    }
}

/// `read` against `want` on the surface `surface`.
fn judge(surface: &str, read: Read, want: &Expect, failures: &mut Vec<String>) {
    let seed_ids: Vec<String> = serde_json::from_str::<Value>(SEED)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|e| text(e, "id"))
        .collect();
    match (read, want) {
        (Read::Entities(got, count), Expect::Admitted(want)) => {
            if &got != want {
                failures.push(format!("{surface}: got {got:?}, want {want:?}"));
            }
            if count != u64::try_from(want.len()).ok() {
                failures.push(format!("{surface}: count {count:?}, want {}", want.len()));
            }
        }
        (Read::Refused(status, body), Expect::Admitted(_)) => {
            failures.push(format!("{surface}: refused {status} {body}"));
        }
        (Read::Entities(got, count), Expect::Denied) => {
            failures.push(format!(
                "{surface}: denied caller read {got:?} (count {count:?})"
            ));
        }
        (Read::Refused(status, body), Expect::Denied) => {
            let text = body.to_string();
            if status.is_success() || seed_ids.iter().any(|id| text.contains(id.as_str())) {
                failures.push(format!("{surface}: refusal {status} {text}"));
            }
        }
    }
}

/// What a temporal query answers where the snapshot answers `want`: an
/// Entity shows in it only through an instance of a shown Attribute, so one
/// shown with its frame alone is absent (ADR-0023 rule 11, temporal);
/// `temporal` in the scenario or step states it where a move changed the
/// history.
fn temporal(want: &Expect, stated: &Value) -> Expect {
    match (want, stated.is_null()) {
        (_, false) => expect(stated),
        (Expect::Admitted(want), true) => Expect::Admitted(
            want.iter()
                .filter(|(_, names)| !names.is_empty())
                .map(|(id, names)| (id.clone(), names.clone()))
                .collect(),
        ),
        (Expect::Denied, true) => Expect::Denied,
    }
}

/// Every surface against `want`; `stated` is the scenario's or step's
/// `temporal`.
async fn check(
    app: &Router,
    s: &Scenario,
    want: &Expect,
    stated: &Value,
    failures: &mut Vec<String>,
) {
    judge(
        "NGSI-LD query",
        ngsi_ld(app, s, false).await,
        want,
        failures,
    );
    judge(
        "NGSI-LD temporal",
        ngsi_ld(app, s, true).await,
        &temporal(want, stated),
        failures,
    );
    judge("OData", odata(app, s).await, want, failures);
    judge("OGC items", ogc(app, s).await, want, failures);
    judge("MCP query_entities", mcp(app, s).await, want, failures);
    if let Expect::Admitted(want) = want {
        by_id(app, s, want, failures).await;
    }
}

/// A step: `set` writes on the Tenant root as the seeder, `patch` writes as
/// the caller through the view and expects `status`.
async fn step(app: &Router, s: &Scenario, step: &Value, failures: &mut Vec<String>) {
    let attrs = &step["attrs"];
    if let Some(id) = step["set"].as_str() {
        let uri = format!("/ngsi-ld/v1/entities/{id}/attrs");
        let (status, _, body) = send(app, "PATCH", &uri, &seeder(s), Some(attrs)).await;
        assert!(status.is_success(), "{}: set {id}: {status} {body}", s.name);
    }
    if let Some(id) = step["patch"].as_str() {
        let uri = format!("/x/views/{}/ngsi-ld/v1/entities/{id}/attrs", s.view());
        let (status, _, body) = send(app, "PATCH", &uri, &with_tenant(s), Some(attrs)).await;
        if Some(u64::from(status.as_u16())) != step["status"].as_u64() {
            failures.push(format!(
                "write {id}: {status} {body}, want {}",
                step["status"]
            ));
        }
    }
}

/// A WebSocket Subscription through the view, made as the caller before
/// the seed is written: its status, and the socket when it was made.
async fn subscribe(port: u16, s: &Scenario) -> (u64, Option<Socket>) {
    let (path, who) = (format!("/x/views/{}/ws", s.view()), with_tenant(s));
    let types: Vec<Value> = s.types().iter().map(|t| json!({"type": t})).collect();
    let name = s.name.clone();
    tokio::task::spawn_blocking(move || {
        let mut socket = Socket::open(port, &path, &who, Duration::from_millis(800))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let welcome = socket.next().unwrap_or_default();
        assert_eq!(
            welcome["metadata"]["x-op"], "x-welcome",
            "{name}: {welcome}"
        );
        socket
            .send(&json!({"metadata": {"x-op": "x-subscribe", "x-id": "m"},
                          "body": {"type": "Subscription", "entities": types}}))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let made = socket.next().unwrap_or_default();
        let status = made["metadata"]["x-status"].as_u64().unwrap_or_default();
        (status, (status == 201).then_some(socket))
    })
    .await
    .expect("a subscription")
}

/// Every Entity the socket was notified until it fell silent, the last
/// notification of each id.
async fn notified(socket: Socket) -> Read {
    tokio::task::spawn_blocking(move || {
        let mut socket = socket;
        let (mut out, mut never) = (Answer::new(), Vec::new());
        while let Some(m) = socket.next() {
            if m["metadata"]["x-op"] == "x-notification" {
                for e in m["body"]["data"].as_array().into_iter().flatten() {
                    out.insert(text(e, "id"), ngsi_names(e));
                }
            } else {
                never.push(m);
            }
        }
        if !never.is_empty() {
            out.insert(format!("other messages: {never:?}"), BTreeSet::new());
        }
        let n = u64::try_from(out.len()).ok();
        Read::Entities(out, n)
    })
    .await
    .expect("the notifications")
}

/// The whole corpus through every surface; every mismatch is listed with
/// its scenario.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_surface_answers_what_ngsi_ld_admits_in_every_scenario() {
    let (app, _reload) = broker().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("an address").port();
    let served = app.clone();
    tokio::spawn(async move { axum::serve(listener, served).await });
    let corpus = corpus();
    assert!(
        corpus.len() >= 40,
        "the corpus has {} scenarios",
        corpus.len()
    );
    let (mut report, mut listed) = (Vec::new(), Vec::new());
    for s in &corpus {
        install(&app, s).await;
        let want = expect(&s.doc["expect"]);
        let mut failures = Vec::new();
        // the Subscription is made before the seed, so the seed notifies it
        let (status, socket) = subscribe(port, s).await;
        seed(&app, s).await;
        match (socket, &want) {
            (Some(socket), _) => {
                // a notification carries what the view shows now; an Entity
                // shown with its frame alone has nothing to notify
                let shown = temporal(&want, &Value::Null);
                judge("WebSocket", notified(socket).await, &shown, &mut failures);
            }
            (None, Expect::Denied) => {}
            (None, Expect::Admitted(_)) => {
                failures.push(format!("WebSocket: the Subscription answered {status}"));
            }
        }
        check(&app, s, &want, &s.doc["temporal"], &mut failures).await;
        for (n, st) in s.doc["steps"].as_array().into_iter().flatten().enumerate() {
            let mut after = Vec::new();
            step(&app, s, st, &mut after).await;
            check(&app, s, &expect(&st["expect"]), &st["temporal"], &mut after).await;
            failures.extend(after.into_iter().map(|f| format!("step {n}: {f}")));
        }
        // A mismatch with a fix task filed is listed in the scenario's
        // `known` by surface; it must still fail, so a fix removes the entry.
        let known: BTreeMap<String, String> = s.doc["known"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
            .collect();
        let of =
            |f: &str, surface: &str| f.starts_with(surface) || f.contains(&format!(": {surface}"));
        for (surface, task) in &known {
            let hits = failures.iter().filter(|f| of(f, surface)).count();
            if hits == 0 {
                report.push(format!(
                    "{}: {surface} matches now; remove its `known` entry ({task})",
                    s.name
                ));
            } else {
                listed.push(format!("{} {surface}: {hits} mismatches, {task}", s.name));
            }
        }
        failures.retain(|f| !known.keys().any(|surface| of(f, surface)));
        report.extend(failures.into_iter().map(|f| format!("{}: {f}", s.name)));
    }
    eprintln!(
        "known mismatches, each with its fix task:\n{}",
        listed.join("\n")
    );
    assert!(
        report.is_empty(),
        "{} mismatches:\n{}",
        report.len(),
        report.join("\n")
    );
}
