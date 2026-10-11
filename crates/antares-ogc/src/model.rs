// SPDX-License-Identifier: EUPL-1.2
//! The collections of a view (mapping rows M1 to M7): each class of the Tenant's `LinkML`
//! schema whose type the view shows, with the slots it shows of it.

use std::collections::{BTreeMap, BTreeSet};

/// The NGSI-LD default `GeoProperty` (4.5.16.1), the primary geometry when no slot is
/// annotated `ogc_role: primary-geometry`.
pub(crate) const LOCATION: &str = "https://uri.etsi.org/ngsi-ld/location";

/// The annotation that gives a slot its OGC API - Features Part 5 role.
const ROLE: &str = "ogc_role";

/// One collection: an Entity type the view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Collection {
    /// The `LinkML` class name, the `collectionId`.
    pub id: String,
    /// The class description, if the schema gives one.
    pub description: Option<String>,
    /// The Entity type's expanded IRI.
    pub type_iri: String,
    /// The primary geometry's Attribute IRI, if the view shows it (M4).
    pub geometry: Option<String>,
    /// The temporal geometry, if the schema declares one the view shows (M6).
    pub time: Option<Time>,
    /// The other Attributes the view shows: `(slot name, IRI)` (M5).
    pub properties: Vec<(String, String)>,
}

/// A temporal geometry (Part 5 Req 10 to 12): Attribute IRIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Time {
    /// `primary-instant`
    Instant(String),
    /// `primary-interval-start` / `primary-interval-end`, either may be undeclared.
    Interval(Option<String>, Option<String>),
}

/// A `collectionId` is a path segment as written: letters, digits, `_`, `-`, `.`.
fn path_safe(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}

/// The collections of `schema` the view shows (`shown`: type IRI to Attribute IRIs), by id.
pub(crate) fn build(
    schema: &linkml_model::Model,
    shown: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<String, Collection> {
    let mut out = BTreeMap::new();
    for (class_name, class) in &schema.schema().classes {
        let type_iri = schema.class_iri(class_name);
        let (Some(attrs), true) = (shown.get(&type_iri), path_safe(class_name)) else {
            continue;
        };
        let slots: Vec<(String, String, Option<String>)> = schema
            .class_slots(class_name)
            .into_iter()
            .map(|(name, slot)| {
                let role = slot.annotations.get(ROLE).map(|a| a.value.clone());
                (name.to_owned(), schema.slot_iri(name), role)
            })
            .filter(|(_, iri, _)| attrs.contains(iri))
            .collect();
        let role = |wanted: &str| {
            slots
                .iter()
                .find(|(_, _, role)| role.as_deref() == Some(wanted))
                .map(|(_, iri, _)| iri.clone())
        };
        let geometry = role("primary-geometry")
            .or_else(|| attrs.contains(LOCATION).then(|| LOCATION.to_owned()));
        let time = match (
            role("primary-instant"),
            role("primary-interval-start"),
            role("primary-interval-end"),
        ) {
            (Some(at), _, _) => Some(Time::Instant(at)),
            (None, None, None) => None,
            (None, start, end) => Some(Time::Interval(start, end)),
        };
        let properties = slots
            .into_iter()
            .filter(|(_, iri, _)| Some(iri) != geometry.as_ref())
            .map(|(name, iri, _)| (name, iri))
            .collect();
        out.insert(
            class_name.clone(),
            Collection {
                id: class_name.clone(),
                description: class.description.clone(),
                type_iri,
                geometry,
                time,
                properties,
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use linkml_model::{Limits, Model};

    const LIMITS: Limits = Limits {
        max_bytes: 1 << 16,
        max_depth: 16,
        max_elements: 1024,
    };

    fn schema(json: &str) -> Model {
        Model::read(json.as_bytes(), &LIMITS).unwrap_or_else(|e| unreachable!("{e:?}"))
    }

    const CITY: &str = r#"{
      "id": "https://example.org/city", "name": "city",
      "prefixes": {"city": {"prefix_prefix": "city", "prefix_reference": "https://example.org/city/"},
                   "ngsi": {"prefix_prefix": "ngsi", "prefix_reference": "https://uri.etsi.org/ngsi-ld/"}},
      "default_prefix": "city",
      "classes": {
        "Car": {"slots": ["speed", "where", "seen"], "description": "A car"},
        "Stop": {"slots": ["name", "location", "from", "until"]},
        "Hidden": {"slots": ["name"]}
      },
      "slots": {
        "speed": {"range": "float"},
        "where": {"range": "string", "annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "GeoProperty"}, "ogc_role": {"tag": "ogc_role", "value": "primary-geometry"}}},
        "seen": {"range": "datetime", "annotations": {"ogc_role": {"tag": "ogc_role", "value": "primary-instant"}}},
        "name": {"range": "string"},
        "location": {"slot_uri": "ngsi:location", "range": "string"},
        "from": {"range": "datetime", "annotations": {"ogc_role": {"tag": "ogc_role", "value": "primary-interval-start"}}},
        "until": {"range": "datetime"}
      }
    }"#;

    fn shown(rows: &[(&str, &[&str])]) -> BTreeMap<String, BTreeSet<String>> {
        rows.iter()
            .map(|(t, attrs)| {
                (
                    (*t).to_owned(),
                    attrs.iter().map(|a| (*a).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn collections_are_the_shown_classes_with_their_roles() {
        let m = schema(CITY);
        let c = build(
            &m,
            &shown(&[
                (
                    "https://example.org/city/Car",
                    &[
                        "https://example.org/city/speed",
                        "https://example.org/city/where",
                        "https://example.org/city/seen",
                    ],
                ),
                (
                    "https://example.org/city/Stop",
                    &[
                        "https://example.org/city/name",
                        LOCATION,
                        "https://example.org/city/from",
                    ],
                ),
            ]),
        );
        assert_eq!(
            c.keys().collect::<Vec<_>>(),
            ["Car", "Stop"],
            "Hidden is not shown"
        );
        let car = c.get("Car").unwrap_or_else(|| unreachable!());
        assert_eq!(car.description.as_deref(), Some("A car"));
        assert_eq!(
            car.geometry.as_deref(),
            Some("https://example.org/city/where")
        );
        assert_eq!(
            car.time,
            Some(Time::Instant("https://example.org/city/seen".into()))
        );
        assert_eq!(
            car.properties,
            [
                (
                    "speed".to_owned(),
                    "https://example.org/city/speed".to_owned()
                ),
                ("seen".into(), "https://example.org/city/seen".into())
            ]
        );
        let stop = c.get("Stop").unwrap_or_else(|| unreachable!());
        assert_eq!(
            stop.geometry.as_deref(),
            Some(LOCATION),
            "location is the default"
        );
        assert_eq!(
            stop.time,
            Some(Time::Interval(
                Some("https://example.org/city/from".into()),
                None
            )),
            "until is hidden"
        );
        assert_eq!(
            stop.properties.len(),
            2,
            "name and from; location is the geometry"
        );
    }

    #[test]
    fn a_hidden_geometry_or_time_is_none() {
        let m = schema(CITY);
        let c = build(
            &m,
            &shown(&[(
                "https://example.org/city/Car",
                &["https://example.org/city/speed"],
            )]),
        );
        let car = c.get("Car").unwrap_or_else(|| unreachable!());
        assert_eq!((car.geometry.as_ref(), car.time.as_ref()), (None, None));
    }

    #[test]
    fn a_class_name_that_is_no_path_segment_is_left_out() {
        assert!(path_safe("Car_2.v-1"));
        assert!(!path_safe("Car 2"));
        assert!(!path_safe(""));
        assert!(!path_safe("Car/2"));
    }
}
