// SPDX-License-Identifier: EUPL-1.2
//! The operation matrix against the surfaces a `views` build composes
//! (surfaces-test-plan section 2, T-3500).
//!
//! The matrix rows are [`OperationId`]: `antares-ops/tests/operation_ids.rs`
//! proves its 69 ids, bindings and gate clauses are the rows of
//! `docs/analysis/operation-matrix.yaml`, so this file reads the yaml only
//! for what `OperationId` does not carry: the `non_operation_routes` and
//! each row's `write` flag.
//!
//! - Every route the NGSI-LD router serves is a matrix cell or a listed
//!   non-operation route, and every row the broker answers is reached by a
//!   route: a route added without a row is red.
//! - Every cell answers as stated, on the root and through the view root
//!   (ADR-0023) under view `all` (every operation) and view `restricted`
//!   (one type, one hidden Attribute, read rows only): a served cell asks
//!   the policy engine exactly once, with the row's gate clause; any other
//!   is refused (404, 405 or 422, ADR-0023 rule 4) before the engine is
//!   asked; and no answer through `restricted` carries the hidden
//!   Attribute.
//! - In an `odata` build, the OData column (T-3508): every row marked
//!   `odata: supported` is sent in its OData form through both views; a
//!   read asks the engine once with its clause, the service document and
//!   `$metadata` ask nothing, a write asks once under `all` and is 405
//!   before the engine under `restricted`, the forms of a later round
//!   are refused before the engine, and a supported row this column does
//!   not send is red.
//!
//! The MCP column is T-3656's: the tools name rows the matrix's proposal
//! names do not match yet.
#![cfg(feature = "views")]

use antares_api::policy::{
    Decision, DecisionFuture, NotifyDecision, Operation, PolicyEngine, Subject,
};
use antares_api::AppState;
use antares_ops::OperationId;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tower::Service;

const API: &str = "/ngsi-ld/v1";
const MATRIX: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/analysis/operation-matrix.yaml"
);
/// The Attribute view `restricted` hides.
const HIDDEN: &str = "secret";
/// The Tenant both views serve.
const TENANT: &str = "city";

/// Records the clause of every decision it is asked for, and allows.
#[derive(Clone, Default)]
struct Clauses(Arc<Mutex<Vec<&'static str>>>);

impl Clauses {
    fn take(&self) -> Vec<&'static str> {
        self.0
            .lock()
            .map(|mut seen| std::mem::take(&mut *seen))
            .unwrap_or_default()
    }
}

impl PolicyEngine for Clauses {
    fn name(&self) -> &str {
        "clauses"
    }

    fn decide<'a>(&'a self, _s: &'a Subject, op: &'a Operation<'a>) -> DecisionFuture<'a> {
        if let Ok(mut seen) = self.0.lock() {
            seen.push(op.clause);
        }
        Box::pin(std::future::ready(Decision::Allow))
    }

    fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
        NotifyDecision::Deliver
    }
}

/// The yaml's lines under the top-level key `section`.
fn section<'a>(text: &'a str, section: &str) -> impl Iterator<Item = &'a str> {
    let header = format!("{section}:");
    text.lines()
        .skip_while(move |l| *l != header)
        .skip(1)
        .take_while(|l| l.starts_with(' ') || l.starts_with('-'))
}

/// `non_operation_routes` that name methods and an NGSI-LD path: `(methods,
/// path)`, e.g. `(["PATCH", "PUT"], "/ngsi-ld/v1/entities")`.
fn non_operation_routes(text: &str) -> Vec<(Vec<String>, String)> {
    section(text, "non_operation_routes")
        .filter_map(|l| l.strip_prefix("- route: "))
        .filter_map(|r| r.split_once(' '))
        .filter(|(_, path)| path.starts_with(API))
        .map(|(methods, path)| {
            (
                methods.split('|').map(str::to_owned).collect(),
                path.trim().to_owned(),
            )
        })
        .collect()
}

/// The ids of the rows whose `write:` is true.
fn writes(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut row = None;
    for l in section(text, "operations") {
        if let Some(id) = l.strip_prefix("- id: ") {
            row = Some(id.trim().to_owned());
        } else if l == "  write: true" {
            out.extend(row.take());
        }
    }
    out
}

/// One request the walk sends: a route of the NGSI-LD router with the
/// query and body that carry it to its gate.
struct Cell {
    method: String,
    /// The route's template, below the API root.
    route: String,
    query: &'static str,
    body: Value,
}

impl Cell {
    fn path(&self) -> String {
        format!("{API}{}", concrete(&self.route))
    }

    fn op(&self) -> Option<OperationId> {
        let query = Some(self.query).filter(|q| !q.is_empty());
        OperationId::of(&self.method, &self.path(), query)
    }

    fn request(&self, root: &str) -> Request<Body> {
        let sep = if self.query.is_empty() { "" } else { "?" };
        let uri = format!("{root}{}{sep}{}", self.path(), self.query);
        let b = Request::builder().method(self.method.as_str()).uri(uri);
        if self.body.is_null() {
            return b.body(Body::empty()).expect("request");
        }
        let payload = self.body.to_string();
        b.header("Content-Type", "application/json")
            .header("Content-Length", payload.len())
            .body(Body::from(payload))
            .expect("request")
    }
}

/// A route's parameters filled in with values the handlers accept.
fn concrete(route: &str) -> String {
    route
        .replace("{id}", "urn:ngsi-ld:Vehicle:matrix:1")
        .replace("{attr}", "speed")
        .replace("{type}", "Vehicle")
        .replace("{instance}", "urn:ngsi-ld:Instance:matrix:1")
}

/// What a route needs to reach its gate (`policy_gate_seam.rs`'s
/// `request_for`): the queries need a filter, the writes a body their
/// parser accepts.
fn carried(method: &str, route: &str) -> (&'static str, Value) {
    let entity = json!({"id": "urn:ngsi-ld:Vehicle:matrix:1", "type": "Vehicle",
                        "speed": {"type": "Property", "value": 1}});
    let fragment = json!({"speed": {"type": "Property", "value": 2}});
    let reads_as = if method == "HEAD" { "GET" } else { method };
    match (reads_as, route) {
        ("GET" | "DELETE", "/entities") | ("GET", "/entityMaps") => ("type=Vehicle", Value::Null),
        ("GET", "/temporal/entities" | "/temporal/entityMaps") => (
            "type=Vehicle&timerel=after&timeAt=1970-01-01T00:00:00Z",
            Value::Null,
        ),
        ("DELETE", "/snapshots") => ("q=status==%22completed%22", Value::Null),
        ("POST", "/entityMaps" | "/temporal/entityMaps" | "/entityOperations/query") => (
            "",
            json!({"type": "Query", "entities": [{"type": "Vehicle"}]}),
        ),
        ("POST", "/temporal/entityOperations/query") => (
            "",
            json!({"type": "Query", "entities": [{"type": "Vehicle"}],
                   "temporalQ": {"timerel": "after", "timeAt": "1970-01-01T00:00:00Z"}}),
        ),
        ("POST", p) if p.starts_with("/entityOperations") => ("", json!([entity])),
        ("POST", "/entities" | "/temporal/entities") => ("", entity),
        ("POST", "/subscriptions" | "/csourceSubscriptions") => (
            "",
            json!({"type": "Subscription", "entities": [{"type": "Vehicle"}],
                   "notification": {"endpoint": {"uri": "http://sink.invalid/n"}}}),
        ),
        ("POST", "/csourceRegistrations") => (
            "",
            json!({"type": "ContextSourceRegistration",
                   "information": [{"entities": [{"type": "Vehicle"}]}],
                   "endpoint": "http://csr.invalid/"}),
        ),
        ("POST", "/jsonldContexts") => (
            "",
            json!(["https://uri.etsi.org/ngsi-ld/v1/ngsi-ld-core-context-v1.9.jsonld"]),
        ),
        ("POST", "/snapshots") => ("", json!({"type": "Snapshot"})),
        ("POST", p) if p.ends_with("/clone") => ("", json!({})),
        ("POST" | "PATCH" | "PUT", _) => ("", fragment),
        _ => ("", Value::Null),
    }
}

/// One answer of `app`: the status and the body as text.
async fn call(app: &Router, req: Request<Body>) -> (StatusCode, String) {
    let Ok(res) = app.clone().call(req).await;
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Every route of the NGSI-LD router, each method the router's own OPTIONS
/// answer names, plus the `?details=true` variants of the two routes whose
/// operation a query parameter decides.
async fn cells() -> Vec<Cell> {
    let app = antares_api::router(AppState::new("matrix".into()));
    let mut out = Vec::new();
    for (route, _) in antares_api::api_routes() {
        let options = Request::builder()
            .method("OPTIONS")
            .uri(format!("{API}{}", concrete(route)))
            .body(Body::empty())
            .expect("request");
        let Ok(res) = app.clone().call(options).await;
        let allow = res
            .headers()
            .get("allow")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(!allow.is_empty(), "OPTIONS {route} named no method");
        for method in allow.split(',').map(str::trim).filter(|m| *m != "OPTIONS") {
            let (query, body) = carried(method, route);
            out.push(Cell {
                method: method.to_owned(),
                route: route.to_owned(),
                query,
                body,
            });
        }
    }
    for route in ["/types", "/attributes"] {
        out.push(Cell {
            method: "GET".into(),
            route: route.into(),
            query: "details=true",
            body: Value::Null,
        });
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_route_of_every_surface_is_a_matrix_cell() {
    let text = std::fs::read_to_string(MATRIX).expect("the operation matrix");
    let listed = non_operation_routes(&text);
    assert!(!listed.is_empty(), "no non_operation_routes read");
    let cells = cells().await;
    // 69 is what the policy seam's walk finds, HEAD not counted
    assert!(cells.len() >= 69, "the route walk came out short");
    let mut reached = HashSet::new();
    let mut unknown = Vec::new();
    for c in &cells {
        match c.op() {
            Some(op) => {
                reached.insert(op.id());
            }
            None if listed
                .iter()
                .any(|(ms, p)| ms.contains(&c.method) && *p == c.path()) => {}
            None => unknown.push(format!("{} {}", c.method, c.route)),
        }
    }
    assert!(
        unknown.is_empty(),
        "routes that are no matrix cell: {unknown:?}"
    );
    // the broker-initiated rows (no gate clause) have no route
    let unreached: Vec<&str> = OperationId::ALL
        .iter()
        .filter(|op| op.gate_clause().is_some() && !reached.contains(op.id()))
        .map(|op| op.id())
        .collect();
    assert!(unreached.is_empty(), "rows no route serves: {unreached:?}");
}

/// Where a pass sends its requests.
#[derive(Clone, Copy, Debug)]
enum Pass {
    Root,
    View(&'static str),
}

/// The Tenant's `LinkML` schema: Vehicle with both Attributes, under the
/// core context's default vocabulary the views' names expand to.
const SCHEMA: &str = r#"{
  "id": "https://example.org/matrix", "name": "matrix",
  "prefixes": {"d": {"prefix_prefix": "d", "prefix_reference": "https://uri.etsi.org/ngsi-ld/default-context/"}},
  "default_prefix": "d",
  "classes": {"Vehicle": {"slots": ["speed", "secret", "owner"]}},
  "slots": {"speed": {"range": "integer"}, "secret": {}, "owner": {"range": "Vehicle"}}
}"#;

/// The OData surface, in a build that has it.
#[cfg(feature = "odata")]
fn odata() -> Option<Box<dyn antares_api::ApiSurface>> {
    Some(antares_ops::mount(antares_odata::OData))
}

#[cfg(not(feature = "odata"))]
fn odata() -> Option<Box<dyn antares_api::ApiSurface>> {
    None
}

/// A `views` build's surfaces over the built-in store, asking `engine`,
/// with views `all` and `restricted` loaded, and Vehicle `matrix:1`
/// (speed and the hidden Attribute) in the views' Tenant.
async fn broker(engine: Clauses, writes: &HashSet<String>) -> (Router, antares_ops::views::Reload) {
    if cfg!(feature = "odata") {
        // The OData model is the Tenant's schema; a broker takes none
        // without a size limit.
        std::env::set_var(antares_ops::views::SCHEMA_MAX_ENV, "65536");
    }
    let mut state = AppState::new("matrix".into());
    antares_api::wire(&mut state).await;
    let mut surfaces: Vec<Box<dyn antares_api::ApiSurface>> = vec![
        Box::new(antares_api::Admin),
        antares_ops::mount(antares_views::Views),
    ];
    surfaces.extend(odata());
    let state = state
        .with_policy(Arc::new(engine))
        .with_surfaces(surfaces)
        .expect("the surfaces mount");
    let reload = antares_ops::views::install(&state)
        .await
        .expect("the view table");
    let app = antares_api::router(state);
    let all: Vec<&str> = OperationId::ALL.iter().map(|op| op.id()).collect();
    let reads: Vec<&str> = all
        .iter()
        .copied()
        .filter(|id| !writes.contains(*id))
        .collect();
    for (id, attributes, operations) in [
        ("all", vec!["speed", HIDDEN, "owner"], all),
        ("restricted", vec!["speed"], reads),
    ] {
        let doc = json!({"id": id, "tenant": TENANT, "selector": ["Vehicle"],
                         "rules": [{"types": ["Vehicle"], "attributes": attributes}],
                         "operations": operations});
        let put = Cell {
            method: "PUT".into(),
            route: String::new(),
            query: "",
            body: doc,
        };
        let mut req = put.request("");
        *req.uri_mut() = format!("/q/views/{id}").parse().expect("uri");
        let (status, body) = call(&app, req).await;
        assert!(status.is_success(), "view {id}: {status} {body}");
    }
    if cfg!(feature = "odata") {
        let schema = Cell {
            method: "PUT".into(),
            route: String::new(),
            query: "",
            body: serde_json::from_str(SCHEMA).expect("the schema"),
        };
        let mut req = schema.request("");
        *req.uri_mut() = format!("/q/schemas/{TENANT}").parse().expect("uri");
        let (status, body) = call(&app, req).await;
        assert!(status.is_success(), "schema: {status} {body}");
    }
    let vehicle = json!({"id": "urn:ngsi-ld:Vehicle:matrix:1", "type": "Vehicle",
                         "speed": {"type": "Property", "value": 1},
                         HIDDEN: {"type": "Property", "value": "s3"}});
    let seed = Cell {
        method: "POST".into(),
        route: "/entities".into(),
        query: "",
        body: vehicle,
    };
    let mut req = seed.request("");
    req.headers_mut()
        .insert("NGSILD-Tenant", TENANT.parse().expect("header"));
    let (status, body) = call(&app, req).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    (app, reload)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_cell_is_reachable_or_rejected_as_stated() {
    let text = std::fs::read_to_string(MATRIX).expect("the operation matrix");
    let writes = writes(&text);
    assert!(writes.contains("create_entity") && !writes.contains("retrieve_entity"));
    let cells = cells().await;
    let mut wrong = Vec::new();
    for pass in [Pass::Root, Pass::View("all"), Pass::View("restricted")] {
        let engine = Clauses::default();
        let (app, _reload) = broker(engine.clone(), &writes).await;
        engine.take();
        let root = match pass {
            Pass::Root => String::new(),
            Pass::View(v) => format!("{}/{v}", antares_views::PREFIX),
        };
        for c in &cells {
            let Some(op) = c.op() else {
                continue;
            };
            let (status, body) = call(&app, c.request(&root)).await;
            let seen = engine.take();
            let served = match pass {
                Pass::Root => true,
                Pass::View(v) => {
                    antares_views::SERVED.contains(&op) && (v == "all" || !writes.contains(op.id()))
                }
            };
            let cell = format!("{pass:?} {} {}?{}", c.method, c.route, c.query);
            if served && seen != op.gate_clause().into_iter().collect::<Vec<_>>() {
                wrong.push(format!(
                    "{cell}: asked {seen:?}, the row's clause is {:?} ({status})",
                    op.gate_clause()
                ));
            }
            if !served && (!seen.is_empty() || ![404, 405, 422].contains(&status.as_u16())) {
                wrong.push(format!(
                    "{cell}: not served, answered {status} after {seen:?}"
                ));
            }
            if matches!(pass, Pass::View("restricted")) && body.contains(HIDDEN) {
                wrong.push(format!("{cell}: the hidden Attribute is in {body}"));
            }
        }
        // through a view, a path that is no cell is no route
        if let Pass::View(v) = pass {
            let nowhere = Request::get(format!("{root}{API}/nowhere"))
                .body(Body::empty())
                .expect("request");
            let (status, _) = call(&app, nowhere).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
            assert!(
                engine.take().is_empty(),
                "{v}: a non-route asked the engine"
            );
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The `odata:` status of every row of the operation matrix.
#[cfg(feature = "odata")]
fn odata_status(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut row = None;
    let mut inside = false;
    for l in section(text, "operations") {
        if let Some(id) = l.strip_prefix("- id: ") {
            row = Some(id.trim().to_owned());
            inside = false;
        } else if l == "    odata:" {
            inside = true;
        } else if let Some(status) = l.strip_prefix("      status: ").filter(|_| inside) {
            out.extend(row.take().map(|id| (id, status.trim().to_owned())));
            inside = false;
        }
    }
    out
}

/// The OData column (T-3508): every row the matrix marks `odata:
/// supported` is sent in its OData form through views `all` and
/// `restricted`. A read this round serves asks the engine once, with the
/// row's clause; a document the model answers asks nothing; a write is
/// T-3509's and is refused before the engine. A row marked supported that
/// this column does not send is red.
#[cfg(feature = "odata")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_odata_column_is_served_as_the_matrix_states() {
    const KEY: &str = "Vehicle('urn:ngsi-ld:Vehicle:matrix:1')";
    /// How a row answers.
    enum Want {
        /// 200 after asking the engine once, with the row's clause.
        Gated,
        /// 200 from the model, asking nothing.
        Model,
        /// A write with this body: under view `all` a 2xx after asking
        /// the engine once, with the row's clause; under `restricted`,
        /// which offers no write, 405 before the engine.
        Write(&'static str),
        /// A `$ref` write with this body: under `all` the target read
        /// (5.7.1) and then the row's clause; under `restricted`, whose
        /// model has no navigation, 404 before the engine.
        Linked(&'static str),
    }
    let sent: &[(&str, &str, String, Want)] = &[
        (
            "query_entities",
            "GET",
            "Vehicle?$top=1".into(),
            Want::Gated,
        ),
        ("retrieve_entity", "GET", KEY.into(), Want::Gated),
        (
            "retrieve_attribute_value",
            "GET",
            format!("{KEY}/speed"),
            Want::Gated,
        ),
        ("retrieve_entity_types", "GET", String::new(), Want::Model),
        (
            "retrieve_entity_type_details",
            "GET",
            "$metadata".into(),
            Want::Model,
        ),
        // paging is by offset: no EntityMap is made (the row's binding says "may")
        (
            "create_entity_map",
            "GET",
            "Vehicle?$top=1&$skip=1".into(),
            Want::Gated,
        ),
        (
            "create_entity",
            "POST",
            "Vehicle".into(),
            Want::Write(r#"{"id": "urn:ngsi-ld:Vehicle:matrix:2", "speed": 2}"#),
        ),
        (
            "merge_entity",
            "PATCH",
            KEY.into(),
            Want::Write(r#"{"speed": 3}"#),
        ),
        (
            "replace_entity",
            "PUT",
            KEY.into(),
            Want::Write(r#"{"speed": 4}"#),
        ),
        (
            "delete_entity",
            "DELETE",
            "Vehicle('urn:ngsi-ld:Vehicle:matrix:2')".into(),
            Want::Write(""),
        ),
        (
            "append_attributes",
            "PUT",
            format!("{KEY}/owner/$ref"),
            Want::Linked(r#"{"@odata.id": "Vehicle('urn:ngsi-ld:Vehicle:matrix:1')"}"#),
        ),
        (
            "partial_attribute_update",
            "PATCH",
            format!("{KEY}/speed"),
            Want::Write(r#"{"value": 5}"#),
        ),
        (
            "replace_attribute",
            "PUT",
            format!("{KEY}/speed"),
            Want::Write(r#"{"value": 6}"#),
        ),
        (
            "delete_attribute",
            "DELETE",
            format!("{KEY}/speed"),
            Want::Write(""),
        ),
    ];
    let text = std::fs::read_to_string(MATRIX).expect("the operation matrix");
    let supported: Vec<String> = odata_status(&text)
        .into_iter()
        .filter(|(_, status)| status == "supported")
        .map(|(id, _)| id)
        .collect();
    assert!(supported.len() >= 10, "the column is read: {supported:?}");
    let mut wrong: Vec<String> = supported
        .iter()
        .filter(|id| !sent.iter().any(|(row, ..)| row == id))
        .map(|id| format!("{id}: marked supported, not sent"))
        .collect();
    let writes = writes(&text);
    for view in ["all", "restricted"] {
        let engine = Clauses::default();
        let (app, _reload) = broker(engine.clone(), &writes).await;
        engine.take();
        for (row, method, path, want) in sent {
            if !supported.iter().any(|id| id == row) {
                wrong.push(format!(
                    "{row}: sent, but the matrix does not mark it supported"
                ));
                continue;
            }
            let body = match want {
                Want::Write(body) | Want::Linked(body) => body,
                _ if *method == "GET" || *method == "DELETE" => "",
                _ => "{}",
            };
            // A create answers without reading the Entity back: one ask.
            let mut req = Request::builder()
                .method(*method)
                .uri(format!("/x/views/{view}/odata/v4/{path}"))
                .header("Prefer", "return=minimal");
            if !body.is_empty() {
                req = req
                    .header("Content-Type", "application/json")
                    .header("Content-Length", body.len());
            }
            let req = req.body(Body::from(body)).expect("request");
            let (status, answer) = call(&app, req).await;
            let seen = engine.take();
            let cell = format!("{view} {row}: {method} {path}");
            let op = OperationId::from_id(row);
            let clause: Vec<&str> = op.and_then(OperationId::gate_clause).into_iter().collect();
            let read = !writes.contains(*row);
            match want {
                Want::Gated if status != StatusCode::OK => {
                    wrong.push(format!("{cell}: {status} {answer}"));
                }
                Want::Gated if *row == "create_entity_map" => {
                    if seen != ["5.7.2"] {
                        wrong.push(format!(
                            "{cell}: asked {seen:?}, paging is one Query Entities"
                        ));
                    }
                }
                Want::Gated if seen != clause => {
                    wrong.push(format!(
                        "{cell}: asked {seen:?}, the row's clause is {clause:?}"
                    ));
                }
                Want::Model if status != StatusCode::OK || !seen.is_empty() => {
                    wrong.push(format!("{cell}: {status} after {seen:?}"));
                }
                Want::Write(_) if view == "all" && (!status.is_success() || seen != clause) => {
                    wrong.push(format!(
                        "{cell}: {status} {answer} after {seen:?}, the row's clause is {clause:?}"
                    ));
                }
                Want::Linked(_)
                    if view == "all"
                        && (!status.is_success()
                            || seen.first() != Some(&"5.7.1")
                            || seen.get(1..) != Some(&clause[..])) =>
                {
                    wrong.push(format!(
                        "{cell}: {status} {answer} after {seen:?}, the target read and then {clause:?}"
                    ));
                }
                Want::Linked(_)
                    if view == "restricted"
                        && (status != StatusCode::NOT_FOUND || !seen.is_empty()) =>
                {
                    wrong.push(format!("{cell}: answered {status} after {seen:?}"));
                }
                Want::Write(_)
                    if view == "restricted"
                        && (status != StatusCode::METHOD_NOT_ALLOWED || !seen.is_empty()) =>
                {
                    wrong.push(format!(
                        "{cell}: a write the view does not offer answered {status} after {seen:?}"
                    ));
                }
                _ => {}
            }
            if view == "restricted" && answer.contains(HIDDEN) {
                wrong.push(format!("{cell}: the hidden Attribute is in {answer}"));
            }
            assert!(
                read != matches!(want, Want::Write(_) | Want::Linked(_)),
                "{cell}: a read sent as a write or a write as a read"
            );
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
