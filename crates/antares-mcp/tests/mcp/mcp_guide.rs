// SPDX-License-Identifier: EUPL-1.2
//! The query guide on the read tools (T-4486): a view of Events says on `query_entities` that the
//! time axis is the `startDate..endDate` range and how to ask for this week, and names no
//! attribute the view hides.

use crate::common::{fixture, put_view, rpc, READS};
use serde_json::{json, Value};

fn description<'a>(tools: &'a Value, name: &str) -> &'a str {
    tools
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|t| t.get("name") == Some(&json!(name)))
        .and_then(|t| t.get("description").and_then(Value::as_str))
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_read_tools_say_how_to_ask_each_type() -> Result<(), String> {
    let f = fixture().await?;
    put_view(
        &f.app,
        &json!({"id": "events", "tenant": "city", "selector": ["Event"],
                "rules": [{"types": ["Event"], "attributes": ["startDate", "endDate", "location", "category"]}],
                "operations": READS}),
    )
    .await?;
    let tools = rpc(&f.app, "events", "tools/list", json!({})).await;
    let query = description(tools.result(), "query_entities");
    assert!(
        query.contains("Event: time startDate..endDate (a range)"),
        "{query}"
    );
    assert!(query.contains("q=startDate<"), "{query}");
    assert!(query.contains("Filter on category."), "{query}");
    assert!(!query.contains("organizer"), "a hidden attribute: {query}");
    assert!(!description(tools.result(), "get_entity").contains("Query guide"));
    Ok(())
}
