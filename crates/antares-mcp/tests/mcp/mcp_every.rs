// SPDX-License-Identifier: EUPL-1.2
//! A view of every type and every Attribute (ADR-0023 rule 12, T-3666):
//! its tools take any well-formed name, by pattern, and answer what the
//! view root answers; a name the view does not know is no refusal.

use crate::common::{call, fixture, put_view, rpc, READS};
use serde_json::{json, Value};

#[tokio::test(flavor = "multi_thread")]
async fn every_name_is_offered_by_its_pattern() -> Result<(), String> {
    let f = fixture().await?;
    put_view(
        &f.app,
        &json!({"id": "all", "tenant": "city", "selector": ["*"],
                "rules": [{"types": ["*"], "attributes": ["*"]}], "operations": READS}),
    )
    .await?;
    let tools = rpc(&f.app, "all", "tools/list", json!({})).await;
    let query = tools
        .result()
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|t| t.get("name") == Some(&json!("query_entities")))
        .ok_or("query_entities is offered")?;
    let ty = query
        .pointer("/inputSchema/properties/type/items")
        .ok_or("a type argument")?;
    assert!(ty.get("enum").is_none(), "{ty}");
    assert!(ty.get("pattern").is_some(), "{ty}");

    let a = call(
        &f.app,
        "all",
        "query_entities",
        json!({"type": ["Car", "Bike"], "attrs": ["secret"]}),
    )
    .await;
    assert!(!a.is_error(), "{}", a.text);
    let mut ids: Vec<&str> = a
        .structured()
        .get("entities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.get("id").and_then(Value::as_str))
        .collect();
    ids.sort_unstable();
    // `attrs` selects the Entities that hold one of them (4.17): the Bike
    // has no `secret`; Car:3, out of view `cars`, is in.
    assert_eq!(
        ids,
        [
            "urn:ngsi-ld:Car:1",
            "urn:ngsi-ld:Car:2",
            "urn:ngsi-ld:Car:3"
        ]
    );
    assert!(a.text.contains("s3cr3t"), "every Attribute: {}", a.text);
    assert!(!a.text.contains("Car:9"), "the Tenant stays pinned");

    let bike = rpc(
        &f.app,
        "all",
        "resources/read",
        json!({"uri": "ngsi-ld://all/types/Bike"}),
    )
    .await;
    assert!(
        bike.result().pointer("/contents/0/text").is_some(),
        "{}",
        bike.text
    );
    Ok(())
}
