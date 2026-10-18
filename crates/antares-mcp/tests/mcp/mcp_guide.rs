// SPDX-License-Identifier: EUPL-1.2
//! The query guide on the read tools (T-4486): a view of Events says on `query_entities` that the
//! time axis is the `startDate..endDate` range and how to ask for this week, and names no
//! attribute the view hides; `describe_type` answers one type's guide as text, then JSON (T-4537).

use crate::common::{call, fixture, put_view, rpc, READS};
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

#[tokio::test(flavor = "multi_thread")]
async fn describe_type_answers_the_guide_text_first_and_nothing_for_a_hidden_type(
) -> Result<(), String> {
    let f = fixture().await?;
    put_view(
        &f.app,
        &json!({"id": "events", "tenant": "city", "selector": ["Event"],
                "rules": [{"types": ["Event"], "attributes": ["startDate", "endDate", "location", "category"]}],
                "operations": READS}),
    )
    .await?;
    let tools = rpc(&f.app, "events", "tools/list", json!({})).await;
    assert!(description(tools.result(), "describe_type").contains("time axis"));
    let a = call(&f.app, "events", "describe_type", json!({"type": "Event"})).await;
    assert!(!a.is_error(), "{}", a.text);
    let first = a
        .result()
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        first.starts_with("Event: time startDate..endDate (a range)"),
        "{}",
        a.text
    );
    assert_eq!(
        a.structured().pointer("/guide/time"),
        Some(&json!({"start": "startDate", "end": "endDate"}))
    );
    assert!(
        !a.text.contains("organizer"),
        "a hidden attribute: {}",
        a.text
    );
    let hidden = call(
        &f.app,
        "events",
        "describe_type",
        json!({"type": "Vehicle"}),
    )
    .await;
    assert!(hidden.is_error(), "{}", hidden.text);
    assert!(
        hidden.structured().get("guide").is_none(),
        "{}",
        hidden.text
    );
    assert!(!hidden.text.contains("location"), "{}", hidden.text);
    Ok(())
}
