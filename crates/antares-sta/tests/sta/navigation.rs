// SPDX-License-Identifier: EUPL-1.2
//! SensorThings 1.1 req/resource-path through the `city` view (T-4878): every navigation link
//! a resource carries answers, a member path answers `{name: value}`, and an earlier
//! Observation reads by its own id. The OGC ETS (ets-sta10) checks the links exist; these
//! tests check where they lead.

use crate::common::{at, fixture, ids, send, sta};
use axum::http::StatusCode;
use serde_json::{json, Value};

const BASE: &str = "http://localhost:9090/x/views/city/sta/v1.1";

/// A link of this surface as the path below the view's base, its `#` sent as `%23`.
fn path(link: &Value) -> String {
    let link = link.as_str().unwrap_or_default();
    link.strip_prefix(BASE).unwrap_or(link).replace('#', "%23")
}

#[tokio::test]
async fn every_navigation_link_of_every_set_answers() -> Result<(), String> {
    let f = fixture().await?;
    let mut followed = 0;
    for set in antares_sta::SETS {
        let list = sta(&f.app, &format!("/{set}")).await;
        assert_eq!(list.status, StatusCode::OK, "{set}");
        for item in list
            .body
            .get("value")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let links = item.as_object().into_iter().flatten();
            for (name, link) in links.filter(|(n, _)| n.ends_with("@iot.navigationLink")) {
                let a = sta(&f.app, &path(link)).await;
                // Car:2 names the hidden Radar:1 as its Device: no Sensor the view shows.
                let hidden = link
                    .as_str()
                    .is_some_and(|l| l.contains("Car:2/speed')/Sensor"));
                let want = if hidden {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::OK
                };
                assert_eq!(a.status, want, "{set} {name} {link}: {}", a.body);
                followed += 1;
            }
        }
    }
    assert!(followed > 40, "{followed} links followed");
    Ok(())
}

#[tokio::test]
async fn a_single_navigation_answers_the_entity_and_a_collection_its_page() -> Result<(), String> {
    let f = fixture().await?;
    let stream = "/Datastreams('urn:ngsi-ld:Car:1/speed')";
    let sensor = sta(&f.app, &format!("{stream}/Sensor")).await;
    assert_eq!(at(&sensor.body, "/@iot.id"), "urn:ngsi-ld:Device:1");
    let thing = sta(&f.app, &format!("{stream}/Thing")).await;
    assert_eq!(at(&thing.body, "/@iot.id"), "urn:ngsi-ld:Car:1");
    let property = sta(&f.app, &format!("{stream}/ObservedProperty")).await;
    assert_eq!(at(&property.body, "/name"), "speed");

    let streams = sta(&f.app, "/Sensors('urn:ngsi-ld:Device:1')/Datastreams").await;
    let mut got = ids(&streams.body);
    got.sort();
    assert_eq!(
        got,
        [
            "urn:ngsi-ld:Car:1/NO2",
            "urn:ngsi-ld:Car:1/NO2#urn:ngsi-ld:Dataset:a",
            "urn:ngsi-ld:Car:1/speed"
        ]
    );
    let measuring = sta(
        &f.app,
        &path(&at(&property.body, "/Datastreams@iot.navigationLink")),
    )
    .await;
    let mut got = ids(&measuring.body);
    got.sort();
    assert_eq!(got, ["urn:ngsi-ld:Car:1/speed", "urn:ngsi-ld:Car:2/speed"]);

    let moves = sta(
        &f.app,
        "/Locations('urn:ngsi-ld:Vehicle:1/location')/HistoricalLocations",
    )
    .await;
    assert_eq!(ids(&moves.body).len(), 3);
    let things = sta(
        &f.app,
        "/Locations('urn:ngsi-ld:Vehicle:1/location')/Things",
    )
    .await;
    assert_eq!(ids(&things.body), ["urn:ngsi-ld:Vehicle:1"]);

    // Car:2 points at Stop:1: its Observations are that feature's; Car:1's own location is its.
    let at_stop = sta(
        &f.app,
        "/FeaturesOfInterest('urn:ngsi-ld:Stop:1')/Observations",
    )
    .await;
    // No observedAt: the Observation's time is when the value was set (`modifiedAt`).
    let Ok([oid]) = <[String; 1]>::try_from(ids(&at_stop.body)) else {
        return Err(format!("one Observation at Stop:1: {}", at_stop.body));
    };
    assert!(oid.starts_with("urn:ngsi-ld:Car:2/speed/"), "{oid}");
    let feature = sta(&f.app, &format!("/Observations('{oid}')/FeatureOfInterest")).await;
    assert_eq!(at(&feature.body, "/@iot.id"), "urn:ngsi-ld:Stop:1");
    Ok(())
}

#[tokio::test]
async fn a_member_path_and_an_earlier_observation_read_by_id() -> Result<(), String> {
    let f = fixture().await?;
    let name = sta(&f.app, "/Things('urn:ngsi-ld:Car:1')/name").await;
    assert_eq!(
        (name.status, name.body),
        (StatusCode::OK, json!({"name": "Žltá"}))
    );

    let earlier = "/Observations('urn:ngsi-ld:Car:1/speed/2025-12-30T00:00:00Z')";
    let a = sta(&f.app, earlier).await;
    assert_eq!(
        (a.status, at(&a.body, "/result")),
        (StatusCode::OK, json!(8))
    );
    let stream = sta(&f.app, &format!("{earlier}/Datastream")).await;
    assert_eq!(at(&stream.body, "/@iot.id"), "urn:ngsi-ld:Car:1/speed");
    // Depot:1 measures `capacity` and names no Device: its Datastream has no Sensor.
    let no_device = "/x/views/every/sta/v1.1/Datastreams('urn:ngsi-ld:Depot:1/capacity')";
    assert_eq!(
        send(&f.app, "GET", no_device, "").await.status,
        StatusCode::OK
    );
    let sensor = send(&f.app, "GET", &format!("{no_device}/Sensor"), "").await;
    assert_eq!(sensor.status, StatusCode::NOT_FOUND);
    let unknown = sta(
        &f.app,
        "/Observations('urn:ngsi-ld:Car:1/speed/2001-01-01T00:00:00Z')",
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    Ok(())
}
