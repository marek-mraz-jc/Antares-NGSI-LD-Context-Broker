// SPDX-License-Identifier: EUPL-1.2
//! One entity set, `Things`, served by `table_router` on `127.0.0.1:<port>/odata/`, for the
//! conformance suites (joinedcontext-conformance tests/odata) and the .NET client check:
//!
//! ```sh
//! cargo run -p odata-axum --example tables -- 8090 memory
//! cargo run -p odata-axum --example tables -- 8090 git <repository> <commit> 'things/*.yaml'
//! ```

use std::sync::Arc;

use odata_axum::{metadata_etag, table_router, ODataState};
use odata_csdl::{
    CsdlVersion, EntityContainer, EntitySet, EntityType, Model, Property, Schema, TypeRef,
};
use odata_table::{GitLimits, GitTable, MemoryTable, Scope, Table, TableInfo};
use serde_json::json;

fn model() -> Model {
    let mut thing = EntityType::new("Thing");
    thing.key = vec!["id".into()];
    // a key is never null (CSDL 4.01 8.2)
    let mut id = Property::new("id", TypeRef::single("Edm.String"));
    id.nullable = false;
    thing.properties = vec![
        id,
        Property::new("name", TypeRef::single("Edm.String")),
        Property::new("size", TypeRef::single("Edm.Int64")),
    ];
    let mut schema = Schema::new("Example");
    schema.entity_types.push(thing);
    let mut container = EntityContainer::new("Service");
    container
        .entity_sets
        .push(EntitySet::new("Things", "Example.Thing"));
    schema.entity_container = Some(container);
    Model {
        version: CsdlVersion::V4_01,
        references: vec![],
        schemas: vec![schema],
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: tables <port> memory | tables <port> git <repository> <commit> <pattern>";
    let port: u16 = args.first().and_then(|p| p.parse().ok()).ok_or(usage)?;
    let model = Arc::new(model());
    let info = TableInfo::new(model.clone(), "Things").ok_or("no set Things")?;
    let table: Arc<dyn Table> = match args.get(1..).unwrap_or_default() {
        [source] if source == "memory" => Arc::new(
            MemoryTable::new(
                info,
                vec![
                    json!({"id": "t1", "name": "Alpha", "size": 3}),
                    json!({"id": "t2", "name": "Beta", "size": 1}),
                ],
            )
            .map_err(|e| e.to_string())?,
        ),
        [source, repo, commit, pattern] if source == "git" => Arc::new(
            GitTable::open(info, repo, commit, pattern, GitLimits::strict())
                .map_err(|e| e.to_string())?,
        ),
        _ => return Err(usage.into()),
    };
    let csdl_limits = odata_csdl::Limits::strict();
    let state = ODataState {
        metadata_etag: metadata_etag(&model, &csdl_limits),
        model,
        url_model: None,
        service_root: format!("http://127.0.0.1:{port}/odata/"),
        url_limits: odata_url::Limits::strict(),
        csdl_limits,
        max_header_len: 1024,
        custom_options: vec![],
    };
    let app = axum::Router::new()
        .nest_service("/odata", table_router(Arc::new(state), vec![table], 100))
        // an example with public rows: every request reads every row
        .layer(axum::Extension(Scope::all()));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}
