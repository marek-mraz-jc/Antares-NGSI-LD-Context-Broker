// SPDX-License-Identifier: EUPL-1.2
//! NGSI-LD Entities as the resources of OGC SensorThings API v1.1, Sensing profile, written
//! against the ISO 19156:2023 (OMS) concepts each set stands for, so a SensorThings 2.0
//! encoding can render the same mapping (`docs/analysis/sta-mapping.md`).
//!
//! Every function reads one Entity the view answered, under the core @context, and builds the
//! resources it holds; nothing is invented for what the Entity does not carry (EP-13).

use serde_json::{json, Map, Value};

/// The UN/CEFACT unit code list a `unitCode` is read from.
pub const UNCEFACT: &str = "https://vocabulary.uncefact.org/UnitMeasureCode#";

/// The `observationType` of a numeric measurement.
const MEASUREMENT: &str = "http://www.opengis.net/def/observationType/OGC-OM/2.0/OM_Measurement";

/// One resource's self link, its id as an OData key literal (a quote written twice).
#[must_use]
pub fn self_link(base: &str, set: &str, id: &str) -> String {
    format!("{base}/{set}('{}')", id.replace('\'', "''"))
}

/// The last segment of a name an answer spells as an IRI, the name itself otherwise: the term a
/// Smart Data Model and the gateway call the Attribute by.
#[must_use]
pub fn local(name: &str) -> &str {
    name.rsplit(['/', '#']).next().unwrap_or(name)
}

/// One text member of an Entity, a Property's value or the plain string; empty is no text.
fn text<'a>(entity: &'a Value, name: &str) -> Option<&'a str> {
    let member = entity.get(name)?;
    member
        .get("value")
        .unwrap_or(member)
        .as_str()
        .filter(|t| !t.is_empty())
}

/// The Entity's id.
fn id(entity: &Value) -> Option<&str> {
    entity.get("id").and_then(Value::as_str)
}

/// The instances of one Attribute: the array of a multi-instance one, else the one.
fn instances(member: &Value) -> &[Value] {
    match member {
        Value::Array(items) => items,
        one => std::slice::from_ref(one),
    }
}

/// The measurements of an Entity (ISO 19156 Observation results): every instance of a Property
/// whose value is a number, with its Attribute's name. A Relationship, a text, a geometry and
/// the Entity's own members carry no numeric `value` and are none.
#[must_use]
pub fn measurements(entity: &Value) -> Vec<(&str, &Value)> {
    entity
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(name, member)| instances(member).iter().map(move |i| (name.as_str(), i)))
        .filter(|(_, i)| i.get("value").is_some_and(Value::is_number))
        .collect()
}

/// A Datastream's id (rule 2): `{urn}/{attribute}`, `#{datasetId}` after it for an instance
/// that names one.
#[must_use]
pub fn stream_id(urn: &str, name: &str, instance: &Value) -> String {
    match dataset(instance) {
        Some(ds) => format!("{urn}/{}#{ds}", local(name)),
        None => format!("{urn}/{}", local(name)),
    }
}

/// An instance's `datasetId`, when it names one.
#[must_use]
pub fn dataset(instance: &Value) -> Option<&str> {
    instance
        .get("datasetId")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
}

/// The Entity URN in front of any id this surface makes; a URN holds no slash.
#[must_use]
pub fn urn_of(id: &str) -> Option<&str> {
    let urn = id.split_once('/').map_or(id, |(urn, _)| urn);
    urn.starts_with("urn:").then_some(urn)
}

/// The Datastream id in front of an Observation id (`{urn}/{attribute}/{time}`): a URN and an
/// Attribute's local name hold no slash.
#[must_use]
pub fn stream_of(observation: &str) -> Option<&str> {
    let (urn, rest) = observation.split_once('/')?;
    let name = rest.split_once('/').map_or(rest, |(name, _)| name);
    observation.get(..urn.len().checked_add(1)?.checked_add(name.len())?)
}

/// Whether an id is an absolute URI (RFC 3986 section 3.1: a scheme, then `:`), as every
/// NGSI-LD Entity id is.
#[must_use]
pub fn is_uri(id: &str) -> bool {
    id.split_once(':').is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// A Datastream id as its Entity URN, its Attribute's local name and its `datasetId`.
#[must_use]
pub fn split_stream(id: &str) -> Option<(&str, &str, Option<&str>)> {
    let (urn, rest) = id.split_once('/')?;
    let (name, ds) = match rest.split_once('#') {
        Some((name, ds)) => (name, Some(ds)),
        None => (rest, None),
    };
    (urn.starts_with("urn:") && !name.is_empty() && !name.contains('/')).then_some((urn, name, ds))
}

/// The Entity as a Thing (ISO 19156 Host): `name` and `description` from the members a Smart
/// Data Model keeps them in, the URN for a missing name; every other non-measurement member in
/// `properties`. `expand` inlines `Locations` and `Datastreams`.
#[must_use]
pub fn thing(base: &str, entity: &Value, expand: &[&str]) -> Option<Value> {
    let urn = id(entity)?;
    let link = self_link(base, "Things", urn);
    let mut out = Map::new();
    out.insert("@iot.id".into(), json!(urn));
    out.insert("@iot.selfLink".into(), json!(link));
    out.insert("name".into(), json!(text(entity, "name").unwrap_or(urn)));
    out.insert(
        "description".into(),
        json!(text(entity, "description").unwrap_or_default()),
    );
    out.insert("properties".into(), properties(entity));
    for set in ["Locations", "Datastreams", "HistoricalLocations"] {
        let items = match set {
            "Locations" if expand.contains(&set) => location(base, entity).into_iter().collect(),
            "Datastreams" if expand.contains(&set) => datastreams(base, entity),
            _ => {
                out.insert(
                    format!("{set}@iot.navigationLink"),
                    json!(format!("{link}/{set}")),
                );
                continue;
            }
        };
        out.insert(format!("{set}@iot.count"), json!(items.len()));
        out.insert(set.into(), Value::Array(items));
    }
    Some(Value::Object(out))
}

/// The Entity's type and its members that are no measurement and no `location`, each flattened
/// to its value or object.
fn properties(entity: &Value) -> Value {
    let mut out = Map::new();
    for (name, member) in entity.as_object().into_iter().flatten() {
        if matches!(name.as_str(), "id" | "location" | "@context") {
            continue;
        }
        if instances(member)
            .iter()
            .any(|i| i.get("value").is_some_and(Value::is_number))
        {
            continue;
        }
        let flat = member
            .get("value")
            .or_else(|| member.get("object"))
            .unwrap_or(member);
        out.insert(local(name).to_owned(), flat.clone());
    }
    Value::Object(out)
}

/// The geometry of a GeoProperty instance, when it is one.
fn geometry(member: &Value) -> Option<&Value> {
    let geometry = member.get("value").unwrap_or(member);
    geometry
        .get("coordinates")
        .or_else(|| geometry.get("geometries"))
        .map(|_| geometry)
}

/// The Entity's `location` as a Location (ISO 19156 Host's spatial position), id
/// `{urn}/location` as the gateway made it.
#[must_use]
pub fn location(base: &str, entity: &Value) -> Option<Value> {
    let urn = id(entity)?;
    let geometry = geometry(entity.get("location")?)?;
    let lid = format!("{urn}/location");
    let link = self_link(base, "Locations", &lid);
    Some(json!({
        "@iot.id": lid,
        "@iot.selfLink": link,
        "name": "location",
        "description": "The entity's primary GeoProperty",
        "encodingType": "application/geo+json",
        "location": geometry,
        "Things@iot.navigationLink": format!("{link}/Things"),
        "HistoricalLocations@iot.navigationLink": format!("{link}/HistoricalLocations"),
    }))
}

/// A Datastream's `unitOfMeasurement` from the instance's `unitCode` (rule 6).
fn unit(instance: &Value) -> Option<Value> {
    let code = instance.get("unitCode").and_then(Value::as_str)?;
    Some(json!({ "name": code, "symbol": code, "definition": format!("{UNCEFACT}{code}") }))
}

/// One Datastream (ISO 19156 ObservationGroup) per measured instance (rules 2 and 6).
#[must_use]
pub fn datastreams(base: &str, entity: &Value) -> Vec<Value> {
    let Some(urn) = id(entity) else {
        return Vec::new();
    };
    measurements(entity)
        .into_iter()
        .map(|(name, instance)| {
            let sid = stream_id(urn, name, instance);
            let link = self_link(base, "Datastreams", &sid);
            let mut out = Map::new();
            out.insert("@iot.id".into(), json!(sid));
            out.insert("@iot.selfLink".into(), json!(link));
            out.insert("name".into(), json!(local(name)));
            out.insert(
                "description".into(),
                json!(format!("{} of {urn}", local(name))),
            );
            out.insert("observationType".into(), json!(MEASUREMENT));
            if let Some(unit) = unit(instance) {
                out.insert("unitOfMeasurement".into(), unit);
            }
            out.insert(
                "Observations@iot.navigationLink".into(),
                json!(format!("{link}/Observations")),
            );
            out.insert(
                "Thing@iot.navigationLink".into(),
                json!(self_link(base, "Things", urn)),
            );
            out.insert(
                "ObservedProperty@iot.navigationLink".into(),
                json!(format!("{link}/ObservedProperty")),
            );
            // A Sensor only for an Entity that names its Device (rule 3).
            if object(entity, "refDevice").is_some() {
                out.insert(
                    "Sensor@iot.navigationLink".into(),
                    json!(format!("{link}/Sensor")),
                );
            }
            Value::Object(out)
        })
        .collect()
}

/// One Observation per measured instance: the current one each holds.
#[must_use]
pub fn observations(base: &str, entity: &Value) -> Vec<Value> {
    let Some(urn) = id(entity) else {
        return Vec::new();
    };
    let fallback = entity.get("observedAt").and_then(Value::as_str);
    measurements(entity)
        .into_iter()
        .map(|(name, instance)| {
            observation(base, &stream_id(urn, name, instance), instance, fallback)
        })
        .collect()
}

/// The Observations of one Datastream from the temporal answer of its Entity: the instances of
/// `attribute` with the Datastream's `datasetId` (none for the default one).
#[must_use]
pub fn series(base: &str, entity: &Value, attribute: &str, stream: &str) -> Vec<Value> {
    let ds = split_stream(stream).and_then(|(_, _, ds)| ds);
    let fallback = entity.get("observedAt").and_then(Value::as_str);
    entity
        .get(attribute)
        .map(instances)
        .unwrap_or_default()
        .iter()
        .filter(|i| i.get("value").is_some() && dataset(i) == ds)
        .map(|i| observation(base, stream, i, fallback))
        .collect()
}

/// One instance as an Observation (ISO 19156 Observation): id
/// `{datastream id}/{phenomenonTime}`, the time the instance's `observedAt` or the Entity's.
fn observation(base: &str, stream: &str, instance: &Value, fallback: Option<&str>) -> Value {
    let at = instance
        .get("observedAt")
        .and_then(Value::as_str)
        .or(fallback);
    let oid = at.map_or_else(|| stream.to_owned(), |at| format!("{stream}/{at}"));
    let link = self_link(base, "Observations", &oid);
    let mut out = Map::new();
    out.insert("@iot.id".into(), json!(oid));
    out.insert("@iot.selfLink".into(), json!(link));
    out.insert(
        "result".into(),
        instance.get("value").cloned().unwrap_or_default(),
    );
    if let Some(at) = at {
        out.insert("phenomenonTime".into(), json!(at));
    }
    // Mandatory, null allowed (SensorThings 1.1 table 11): the instance's `modifiedAt` if known.
    let modified = instance.get("modifiedAt").and_then(Value::as_str);
    out.insert("resultTime".into(), json!(modified));
    out.insert(
        "Datastream@iot.navigationLink".into(),
        json!(self_link(base, "Datastreams", stream)),
    );
    out.insert(
        "FeatureOfInterest@iot.navigationLink".into(),
        json!(format!("{link}/FeatureOfInterest")),
    );
    Value::Object(out)
}

/// One ObservedProperty (ISO 19156 ObservableProperty) per measured Attribute (rule 1): id and
/// `definition` its expanded IRI, so one Attribute measured anywhere is one ObservedProperty.
#[must_use]
pub fn observed_properties(base: &str, entity: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for (name, _) in measurements(entity) {
        let iri = antares_ops::core_iri(name);
        if out.iter().any(|p| p.get("@iot.id") == Some(&json!(iri))) {
            continue;
        }
        let link = self_link(base, "ObservedProperties", &iri);
        out.push(json!({
            "@iot.id": iri,
            "@iot.selfLink": link,
            "Datastreams@iot.navigationLink": format!("{link}/Datastreams"),
            "name": local(name),
            "definition": iri,
            "description": format!("Observed property {}", local(name)),
        }));
    }
    out
}

/// The object of one Relationship of the Entity.
#[must_use]
pub fn object<'a>(entity: &'a Value, name: &str) -> Option<&'a str> {
    entity.get(name)?.get("object")?.as_str()
}

/// An SDM Device as a Sensor (ISO 19156 Observer, rule 3): `metadata` its `controlledProperty`.
/// An Entity of another type is no Sensor.
#[must_use]
pub fn sensor(base: &str, device: &Value) -> Option<Value> {
    let did = id(device)?;
    let kind = device.get("type").and_then(Value::as_str)?;
    if local(kind) != "Device" {
        return None;
    }
    let metadata = device
        .get("controlledProperty")
        .map(|m| m.get("value").unwrap_or(m));
    let link = self_link(base, "Sensors", did);
    Some(json!({
        "@iot.id": did,
        "@iot.selfLink": link,
        "Datastreams@iot.navigationLink": format!("{link}/Datastreams"),
        "name": text(device, "name").unwrap_or(did),
        "description": text(device, "description").unwrap_or_default(),
        "encodingType": "application/json",
        "metadata": metadata.cloned().unwrap_or_default(),
    }))
}

/// A FeatureOfInterest (rule 4): the target of `refPointOfInterest` with its own URN and
/// location, or for an Entity without one its own `location`, id `{urn}/location`. Without a
/// geometry there is none.
#[must_use]
pub fn feature(base: &str, entity: &Value, fid: &str) -> Option<Value> {
    let geometry = geometry(entity.get("location")?)?;
    let link = self_link(base, "FeaturesOfInterest", fid);
    Some(json!({
        "@iot.id": fid,
        "@iot.selfLink": link,
        "Observations@iot.navigationLink": format!("{link}/Observations"),
        "name": text(entity, "name").unwrap_or(fid),
        "description": text(entity, "description").unwrap_or_default(),
        "encodingType": "application/geo+json",
        "feature": geometry,
    }))
}

/// The HistoricalLocations of an Entity from the temporal answer of its `location` (rule 5):
/// one per instance with an `observedAt`, id `{urn}/historicalLocation/{time}`.
#[must_use]
pub fn historical_locations(base: &str, entity: &Value) -> Vec<Value> {
    let Some(urn) = id(entity) else {
        return Vec::new();
    };
    entity
        .get("location")
        .map(instances)
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i.get("observedAt").and_then(Value::as_str))
        .map(|time| {
            let hid = format!("{urn}/historicalLocation/{time}");
            let link = self_link(base, "HistoricalLocations", &hid);
            json!({
                "@iot.id": hid,
                "Locations@iot.navigationLink": format!("{link}/Locations"),
                "@iot.selfLink": link,
                "time": time,
                "Thing@iot.navigationLink": self_link(base, "Things", urn),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(v: &Value, path: &str) -> Value {
        v.pointer(path).cloned().unwrap_or_default()
    }

    const BASE: &str = "http://h/x/views/v/sta/v1.1";

    fn car() -> Value {
        json!({
            "id": "urn:ngsi-ld:Car:1", "type": "Car",
            "name": {"type": "Property", "value": "Žltá"},
            "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [17.1, 48.1]}},
            "speed": {"type": "Property", "value": 42, "unitCode": "KMH", "observedAt": "2026-01-01T00:00:00Z"},
            "https://smartdatamodels.org/dataModel.Environment/NO2": [
                {"type": "Property", "value": 15.0, "datasetId": "urn:ngsi-ld:Dataset:a"},
                {"type": "Property", "value": 16.0}
            ],
            "colour": {"type": "Property", "value": "red"},
            "at": {"type": "Relationship", "object": "urn:ngsi-ld:Stop:1"}
        })
    }

    #[test]
    fn a_thing_carries_its_name_its_properties_and_links_or_inlines_its_sets() {
        let t = thing(BASE, &car(), &[]).unwrap_or_default();
        assert_eq!(at(&t, "/@iot.id"), "urn:ngsi-ld:Car:1");
        assert_eq!(
            at(&t, "/@iot.selfLink"),
            format!("{BASE}/Things('urn:ngsi-ld:Car:1')")
        );
        assert_eq!(at(&t, "/name"), "Žltá");
        assert_eq!(at(&t, "/description"), "");
        assert_eq!(
            at(&t, "/properties"),
            json!({"type": "Car", "name": "Žltá", "colour": "red", "at": "urn:ngsi-ld:Stop:1"})
        );
        assert_eq!(
            at(&t, "/Datastreams@iot.navigationLink"),
            format!("{BASE}/Things('urn:ngsi-ld:Car:1')/Datastreams")
        );
        let t = thing(BASE, &car(), &["Locations", "Datastreams"]).unwrap_or_default();
        assert_eq!(at(&t, "/Locations@iot.count"), 1);
        assert_eq!(at(&t, "/Datastreams@iot.count"), 3);
        assert!(t.get("HistoricalLocations@iot.navigationLink").is_some());
        let bare = thing(BASE, &json!({"id": "urn:a"}), &[]).unwrap_or_default();
        assert_eq!(at(&bare, "/name"), "urn:a");
        assert_eq!(thing(BASE, &json!({}), &[]), None);
    }

    #[test]
    fn each_dataset_of_an_attribute_is_its_own_datastream_named_by_its_local_name() {
        let ds = datastreams(BASE, &car());
        let ids: Vec<&str> = ds
            .iter()
            .filter_map(|d| d.get("@iot.id").and_then(Value::as_str))
            .collect();
        assert_eq!(
            ids,
            [
                "urn:ngsi-ld:Car:1/NO2#urn:ngsi-ld:Dataset:a",
                "urn:ngsi-ld:Car:1/NO2",
                "urn:ngsi-ld:Car:1/speed"
            ]
        );
        let speed = ds
            .iter()
            .find(|d| at(d, "/name") == "speed")
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            at(&speed, "/unitOfMeasurement/definition"),
            format!("{UNCEFACT}KMH")
        );
        assert!(ds
            .iter()
            .find(|d| at(d, "/name") == "NO2")
            .is_some_and(|d| d.get("unitOfMeasurement").is_none()));
        assert_eq!(datastreams(BASE, &json!({"speed": {"value": 1}})).len(), 0);
    }

    #[test]
    fn observations_follow_their_datastream_and_fall_back_to_the_entitys_time() {
        let e = json!({
            "id": "urn:ngsi-ld:Car:1", "observedAt": "2026-02-02T00:00:00Z",
            "speed": {"value": 1, "modifiedAt": "2026-03-03T00:00:00Z"},
            "NO2": {"value": 2, "datasetId": "urn:ngsi-ld:Dataset:a"}
        });
        let obs = observations(BASE, &e);
        let ids: Vec<&str> = obs
            .iter()
            .filter_map(|o| o.get("@iot.id").and_then(Value::as_str))
            .collect();
        assert!(ids.contains(&"urn:ngsi-ld:Car:1/speed/2026-02-02T00:00:00Z"));
        assert!(ids.contains(&"urn:ngsi-ld:Car:1/NO2#urn:ngsi-ld:Dataset:a/2026-02-02T00:00:00Z"));
        let speed = obs
            .iter()
            .find(|o| at(o, "/result") == 1)
            .cloned()
            .unwrap_or_default();
        assert_eq!(at(&speed, "/resultTime"), "2026-03-03T00:00:00Z");
        let timeless = observations(BASE, &json!({"id": "urn:a", "x": {"value": 2}}));
        assert_eq!(
            timeless.first().map(|o| at(o, "/@iot.id")),
            Some(json!("urn:a/x"))
        );
        assert!(timeless
            .first()
            .is_some_and(|o| o.get("phenomenonTime").is_none()));
        assert_eq!(observations(BASE, &json!({"x": {"value": 2}})).len(), 0);
    }

    #[test]
    fn a_series_reads_the_instances_of_its_own_dataset_only() {
        let history = json!({"id": "urn:ngsi-ld:Car:1", "NO2": [
            {"value": 1, "observedAt": "2026-01-01T00:00:00Z"},
            {"value": 2, "observedAt": "2026-01-02T00:00:00Z", "datasetId": "d"},
            {"observedAt": "2026-01-03T00:00:00Z"}
        ]});
        let default = series(BASE, &history, "NO2", "urn:ngsi-ld:Car:1/NO2");
        assert_eq!(default.len(), 1);
        assert_eq!(default.first().map(|o| at(o, "/result")), Some(json!(1)));
        let d = series(BASE, &history, "NO2", "urn:ngsi-ld:Car:1/NO2#d");
        assert_eq!(
            d.first().map(|o| at(o, "/@iot.id")),
            Some(json!("urn:ngsi-ld:Car:1/NO2#d/2026-01-02T00:00:00Z"))
        );
        assert_eq!(
            series(BASE, &history, "speed", "urn:ngsi-ld:Car:1/speed").len(),
            0
        );
        let one = json!({"id": "urn:a", "x": {"value": 3}});
        assert_eq!(series(BASE, &one, "x", "urn:a/x").len(), 1);
    }

    #[test]
    fn one_attribute_is_one_observed_property_by_its_expanded_iri() {
        let ops = observed_properties(BASE, &car());
        let ids: Vec<&str> = ops
            .iter()
            .filter_map(|p| p.get("@iot.id").and_then(Value::as_str))
            .collect();
        assert_eq!(
            ids,
            [
                "https://smartdatamodels.org/dataModel.Environment/NO2",
                antares_ops::core_iri("speed").as_str()
            ]
        );
        assert_eq!(ops.first().map(|p| at(p, "/name")), Some(json!("NO2")));
    }

    #[test]
    fn a_sensor_is_a_device_and_nothing_else() {
        let dev = json!({"id": "urn:ngsi-ld:Device:1", "type": "Device",
            "controlledProperty": {"type": "Property", "value": ["speed"]}, "description": "radar"});
        let s = sensor(BASE, &dev).unwrap_or_default();
        assert_eq!(at(&s, "/metadata"), json!(["speed"]));
        assert_eq!(at(&s, "/name"), "urn:ngsi-ld:Device:1");
        assert_eq!(at(&s, "/description"), "radar");
        let plain = sensor(
            BASE,
            &json!({"id": "urn:d", "type": "https://smartdatamodels.org/dataModel.Device/Device"}),
        );
        assert_eq!(
            plain.map(|s| at(&s, "/metadata").clone()),
            Some(Value::Null)
        );
        assert_eq!(sensor(BASE, &car()), None);
        assert_eq!(sensor(BASE, &json!({"id": "urn:d"})), None);
        assert_eq!(sensor(BASE, &json!({"type": "Device"})), None);
    }

    #[test]
    fn a_location_and_a_feature_need_a_geometry() {
        let l = location(BASE, &car()).unwrap_or_default();
        assert_eq!(at(&l, "/@iot.id"), "urn:ngsi-ld:Car:1/location");
        assert_eq!(
            location(BASE, &json!({"id": "urn:a", "location": {"value": "here"}})),
            None
        );
        assert_eq!(location(BASE, &json!({"id": "urn:a"})), None);
        assert_eq!(
            location(
                BASE,
                &json!({"location": {"value": {"coordinates": [1, 2]}}})
            ),
            None
        );
        let multi =
            json!({"id": "urn:a", "location": {"type": "GeometryCollection", "geometries": []}});
        assert!(location(BASE, &multi).is_some());
        let f = feature(BASE, &car(), "urn:ngsi-ld:Car:1/location").unwrap_or_default();
        assert_eq!(at(&f, "/name"), "Žltá");
        assert_eq!(at(&f, "/feature/type"), "Point");
        assert_eq!(
            feature(BASE, &json!({"id": "urn:a"}), "urn:a/location"),
            None
        );
    }

    #[test]
    fn a_location_history_is_one_historical_location_per_timed_instance() {
        let moving = json!({"id": "urn:ngsi-ld:Vehicle:1", "location": [
            {"value": {"type": "Point", "coordinates": [1, 2]}, "observedAt": "2026-01-01T00:00:00Z"},
            {"value": {"type": "Point", "coordinates": [1, 3]}}
        ]});
        let hl = historical_locations(BASE, &moving);
        assert_eq!(hl.len(), 1);
        assert_eq!(
            hl.first().map(|h| at(h, "/@iot.id")),
            Some(json!(
                "urn:ngsi-ld:Vehicle:1/historicalLocation/2026-01-01T00:00:00Z"
            ))
        );
        assert_eq!(historical_locations(BASE, &json!({"id": "urn:a"})).len(), 0);
        assert_eq!(
            historical_locations(BASE, &json!({"location": []})).len(),
            0
        );
    }

    #[test]
    fn ids_split_into_their_parts() {
        assert_eq!(split_stream("urn:a/speed"), Some(("urn:a", "speed", None)));
        assert_eq!(
            split_stream("urn:a/speed#d"),
            Some(("urn:a", "speed", Some("d")))
        );
        assert_eq!(split_stream("x/speed"), None);
        assert_eq!(split_stream("urn:a/"), None);
        assert_eq!(split_stream("urn:a/b/c"), None);
        assert_eq!(split_stream("urn:a"), None);
        assert_eq!(urn_of("urn:a/speed/t"), Some("urn:a"));
        assert_eq!(urn_of("urn:a"), Some("urn:a"));
        assert_eq!(urn_of("x/y"), None);
        assert_eq!(
            self_link(BASE, "Things", "urn:a'b"),
            format!("{BASE}/Things('urn:a''b')")
        );
        assert_eq!(object(&car(), "at"), Some("urn:ngsi-ld:Stop:1"));
        assert_eq!(object(&car(), "home"), None);
    }

    #[test]
    fn an_observation_id_names_its_datastream_and_an_entity_id_is_a_uri() {
        assert_eq!(
            stream_of("urn:ngsi-ld:Car:1/speed/2026-01-01T00:00:00Z"),
            Some("urn:ngsi-ld:Car:1/speed")
        );
        assert_eq!(
            stream_of("urn:ngsi-ld:Car:1/NO2#urn:ngsi-ld:Dataset:a/2026-01-01T00:00:00Z"),
            Some("urn:ngsi-ld:Car:1/NO2#urn:ngsi-ld:Dataset:a")
        );
        assert_eq!(
            stream_of("urn:ngsi-ld:Car:1/speed"),
            Some("urn:ngsi-ld:Car:1/speed")
        );
        assert_eq!(stream_of("urn:ngsi-ld:Car:1"), None);
        assert!(is_uri("urn:ngsi-ld:Device:1"));
        assert!(is_uri("https://example.org/a"));
        assert!(!is_uri("9223372036854775807"));
        assert!(!is_uri(":x"));
        assert!(!is_uri("1a:x"));
        assert!(!is_uri("a b:x"));
    }
}
