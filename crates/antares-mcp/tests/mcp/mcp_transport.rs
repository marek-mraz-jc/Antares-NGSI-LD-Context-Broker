// SPDX-License-Identifier: EUPL-1.2
//! The transport (MCP 6.3; MC6, MC8, MC13, MC14).

use crate::common::{endpoint, fixture, post_raw, rpc, rpc_with, tool_names, MODERN};
use axum::http::StatusCode;
use serde_json::{json, Value};

/// MC6: a JSON-RPC batch array is refused whole.
#[tokio::test(flavor = "multi_thread")]
async fn a_json_rpc_batch_is_refused() -> Result<(), String> {
    let f = fixture().await?;
    let one = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}});
    let body = json!([one, one]).to_string();
    let a = post_raw(
        &f.app,
        &endpoint("cars"),
        &[("MCP-Protocol-Version", MODERN)],
        &body,
    )
    .await;
    assert_ne!(a.status, StatusCode::OK, "{}", a.text);
    assert_eq!(
        a.error().get("code"),
        Some(&json!(-32600)),
        "{} {}",
        a.status,
        a.text
    );
    assert!(!a.text.contains("query_entities"), "{}", a.text);
    Ok(())
}

/// A body past 1 MiB is refused before it is parsed (MCP 6.3).
#[tokio::test(flavor = "multi_thread")]
async fn a_body_past_the_bound_is_413() -> Result<(), String> {
    let f = fixture().await?;
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"pad":"{}"}}}}"#,
        "x".repeat(1024 * 1024)
    );
    let a = post_raw(
        &f.app,
        &endpoint("cars"),
        &[("MCP-Protocol-Version", MODERN)],
        &body,
    )
    .await;
    assert_eq!(a.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", a.text);
    assert_eq!(a.error().get("code"), Some(&json!(-32600)), "{}", a.text);
    Ok(())
}

/// MC8: a browser Origin the deployment did not list is 403.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_origin_is_403() -> Result<(), String> {
    let f = fixture().await?;
    let a = rpc_with(
        &f.app,
        "cars",
        "tools/list",
        json!({}),
        &[("Origin", "https://evil.example")],
    )
    .await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{}", a.text);
    let b = post_raw(&f.app, &endpoint("cars"), &[("Host", "evil.example")], "{}").await;
    assert_ne!(b.status, StatusCode::OK, "{}", b.text);
    Ok(())
}

/// MC14: a header that disagrees with the body is refused before dispatch.
#[tokio::test(flavor = "multi_thread")]
async fn a_header_that_disagrees_with_the_body_is_400_header_mismatch() -> Result<(), String> {
    let f = fixture().await?;
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
        "name": "get_entity", "arguments": {"id": "urn:ngsi-ld:Car:1"},
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": MODERN,
            "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {}
        }
    }})
    .to_string();
    for (method, name) in [("tools/call", "list_types"), ("tools/list", "get_entity")] {
        let a = post_raw(
            &f.app,
            &endpoint("cars"),
            &[
                ("MCP-Protocol-Version", MODERN),
                ("Mcp-Method", method),
                ("Mcp-Name", name),
            ],
            &body,
        )
        .await;
        assert_eq!(
            a.status,
            StatusCode::BAD_REQUEST,
            "{method} {name}: {}",
            a.text
        );
        assert!(!a.text.contains("speed"), "{}", a.text);
    }
    Ok(())
}

/// MC13: the legacy `initialize` era lists the same read-only tools as the
/// modern one and keeps no session.
#[tokio::test(flavor = "multi_thread")]
async fn the_legacy_handshake_offers_no_write_tool() -> Result<(), String> {
    let f = fixture().await?;
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "old", "version": "1"}
    }})
    .to_string();
    let a = post_raw(&f.app, &endpoint("cars"), &[], &init).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.text);
    assert_eq!(
        a.result().get("protocolVersion"),
        Some(&json!("2025-06-18")),
        "{}",
        a.text
    );
    assert!(a.headers.get("mcp-session-id").is_none());
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}).to_string();
    let legacy = post_raw(
        &f.app,
        &endpoint("cars"),
        &[("MCP-Protocol-Version", "2025-06-18")],
        &list,
    )
    .await;
    let tools: Vec<&Value> = legacy
        .result()
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let mut names: Vec<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    names.sort();
    assert_eq!(names, tool_names(&f.app, "cars").await, "{}", legacy.text);
    assert!(tools
        .iter()
        .all(|t| t.pointer("/annotations/readOnlyHint") == Some(&json!(true))));
    Ok(())
}

/// A view that does not exist is 404 like any unknown path; the modern
/// era reports its versions in `server/discover`.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_view_is_404_and_discover_names_the_versions() -> Result<(), String> {
    let f = fixture().await?;
    let a = rpc(&f.app, "nosuch", "tools/list", json!({})).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "{}", a.text);
    let d = rpc(&f.app, "cars", "server/discover", json!({})).await;
    assert_eq!(
        d.result().get("supportedVersions"),
        Some(&json!([MODERN, "2025-11-25", "2025-06-18"])),
        "{}",
        d.text
    );
    Ok(())
}
