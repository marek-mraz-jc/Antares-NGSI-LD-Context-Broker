// SPDX-License-Identifier: EUPL-1.2
//! The view's schema as one caller may see it (odata-linkml-mapping.md
//! section 8 row L2, threats LK1 and LK6): the classes whose type the
//! caller may see, each with the slots its type may show, `is_a` kept only
//! between visible classes, a range to a hidden class turned into
//! `uriorcurie`, `inverse` only when both ends are visible, and only the
//! enums and prefixes a kept element uses. Every document is rendered from
//! this narrowed model, never from the source (LK1).

use antares_ops::Visible;
use linkml_model::{
    Annotation, Builtin, Class, Error, Limits, Model, Range, Schema, Slot, KIND_TAG,
};
use std::collections::{BTreeMap, BTreeSet};

/// The bounds the Tenant's schema is read under: 1 MiB whatever
/// `ANTARES_SCHEMA_MAX_BYTES` lets the schema table store (a larger schema
/// closes the surface, LK7), a nesting deep enough for LinkML's JSON form,
/// and as many elements as a large model holds.
pub(crate) const LIMITS: Limits = Limits {
    max_bytes: 1024 * 1024,
    max_depth: 32,
    max_elements: 20_000,
};

/// A class's slots a caller may see, each as the class refines it.
type Shown = Vec<(String, Slot)>;

/// The name `visible` knows class `class` by, `None` when it shows it under neither: the
/// class's own IRI, or its name read under the core @context, which a view's rules, the engine
/// and the stored Entities use (ADR-0020). A schema whose `class_uri` is another vocabulary's
/// (`sdm:`) still shows the classes of a view written with short names (T-3645), as the OGC and
/// `OData` surfaces read it (T-3752). A slot is matched the same way, under the type found here.
fn as_viewed(source: &Model, visible: &Visible, class: &str) -> Option<String> {
    let own = source.class_iri(class);
    if visible.shows_type(&own) {
        Some(own)
    } else {
        visible.shows_type(class).then(|| class.to_owned())
    }
}

/// The source schema narrowed to `visible`, under the id
/// `<source id>/views/<view>`, so a narrowed document never claims to be the
/// full model (section 8 decision). The second value is whether anything of
/// the source was left out.
pub(crate) fn narrow(
    source: &Model,
    visible: &Visible,
    view: &str,
) -> Result<(Model, bool), Error> {
    let s = source.schema();
    let kept: BTreeSet<&str> = s
        .classes
        .keys()
        .filter(|c| as_viewed(source, visible, c).is_some())
        .map(String::as_str)
        .collect();
    let shown = |class: &str| -> Shown {
        let Some(ty) = as_viewed(source, visible, class) else {
            return Vec::new();
        };
        source
            .class_slots(class)
            .into_iter()
            .filter(|(name, _)| {
                visible.shows_attribute(&ty, &source.slot_iri(name))
                    || visible.shows_attribute(&ty, name)
            })
            .map(|(name, slot)| (name.to_owned(), slot))
            .collect()
    };
    let per_class: BTreeMap<&str, (&Class, Shown)> = s
        .classes
        .iter()
        .filter(|(c, _)| kept.contains(c.as_str()))
        .map(|(c, class)| (c.as_str(), (class, shown(c))))
        .collect();
    let slot_names: BTreeSet<&str> = per_class
        .values()
        .flat_map(|(_, slots)| slots)
        .map(|(n, _)| n.as_str())
        .collect();

    let rewrite = |slot: &Slot| rewrite(source, &kept, &slot_names, slot);

    let mut classes = BTreeMap::new();
    for (name, (class, slots)) in &per_class {
        // `is_a` holds only when the parent is visible and shows nothing the
        // child does not: LinkML would otherwise hand the child the
        // parent's slots back.
        let own: BTreeSet<&str> = slots.iter().map(|(n, _)| n.as_str()).collect();
        let is_a = class.is_a.clone().filter(|p| {
            per_class
                .get(p.as_str())
                .is_some_and(|(_, ps)| ps.iter().all(|(n, _)| own.contains(n.as_str())))
        });
        let slot_usage = slots
            .iter()
            .filter_map(|(n, refined)| {
                let base = s.slots.get(n)?;
                let refined = rewrite(refined);
                (refined != rewrite(base)).then(|| (n.clone(), refined))
            })
            .collect();
        classes.insert(
            (*name).to_owned(),
            Class {
                name: class.name.clone(),
                class_uri: class.class_uri.clone(),
                is_a,
                slots: slots.iter().map(|(n, _)| n.clone()).collect(),
                slot_usage,
                description: class.description.clone(),
                annotations: class.annotations.clone(),
            },
        );
    }
    let slots: BTreeMap<String, Slot> = slot_names
        .iter()
        .filter_map(|n| Some(((*n).to_owned(), rewrite(s.slots.get(*n)?))))
        .collect();

    let enums = used_enums(source, &slots, &classes);

    // Left out: a hidden class, or a slot a kept class does not show. A slot
    // no class uses goes too; it names nothing a caller could see.
    let left_out = classes.len() != s.classes.len()
        || per_class
            .iter()
            .any(|(c, (_, slots))| slots.len() != source.class_slots(c).len());
    let mut narrowed = Schema {
        id: format!("{}/views/{view}", s.id.trim_end_matches(['/', '#'])),
        name: s.name.clone(),
        version: s.version.clone(),
        prefixes: BTreeMap::new(),
        default_prefix: s.default_prefix.clone(),
        classes,
        slots,
        enums,
    };
    let used = used_prefixes(&narrowed);
    narrowed.prefixes = s
        .prefixes
        .iter()
        .filter(|(k, _)| used.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Ok((Model::new(narrowed, &LIMITS)?, left_out))
}

/// `slot` as the narrowed schema states it: a range to a hidden class is
/// `uriorcurie` with the kind it implied stated (LK1), an `inverse` to a
/// hidden slot is dropped.
fn rewrite(
    source: &Model,
    kept: &BTreeSet<&str>,
    slot_names: &BTreeSet<&str>,
    slot: &Slot,
) -> Slot {
    let mut out = slot.clone();
    if let Range::Class(target) = source.range(slot) {
        if !kept.contains(target) {
            // Still a Relationship (DM-69, an external reference): the
            // kind the class range implied is stated, the class is not.
            let kind = source.kind(slot).to_owned();
            out.annotations
                .entry(KIND_TAG.to_owned())
                .or_insert(Annotation {
                    tag: KIND_TAG.to_owned(),
                    value: kind,
                });
            out.range = Some(Builtin::Uriorcurie.name().to_owned());
            out.inverse = None;
        }
    }
    if out
        .inverse
        .as_deref()
        .is_some_and(|inv| !slot_names.contains(inv))
    {
        out.inverse = None;
    }
    out
}

/// The enums a kept slot or slot usage ranges over.
fn used_enums(
    source: &Model,
    slots: &BTreeMap<String, Slot>,
    classes: &BTreeMap<String, Class>,
) -> BTreeMap<String, linkml_model::Enum> {
    let used: BTreeSet<&str> = slots
        .values()
        .chain(classes.values().flat_map(|c| c.slot_usage.values()))
        .filter_map(|slot| match source.range(slot) {
            Range::Enum(e) => Some(e),
            _ => None,
        })
        .collect();
    source
        .schema()
        .enums
        .iter()
        .filter(|(k, _)| used.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// The prefixes a kept element names: the default prefix and the prefix of
/// every CURIE in a URI, a meaning or a unit mapping (LK6: a prefix no kept
/// element uses may name a hidden namespace).
fn used_prefixes(s: &Schema) -> BTreeSet<String> {
    let mut curies: Vec<&str> = s.default_prefix.iter().map(String::as_str).collect();
    let slots = s
        .slots
        .values()
        .chain(s.classes.values().flat_map(|c| c.slot_usage.values()));
    for slot in slots {
        curies.extend(slot.slot_uri.as_deref());
        if let Some(unit) = &slot.unit {
            curies.extend(unit.exact_mappings.iter().map(String::as_str));
        }
    }
    curies.extend(s.classes.values().filter_map(|c| c.class_uri.as_deref()));
    curies.extend(
        s.enums
            .values()
            .flat_map(|e| e.permissible_values.values())
            .filter_map(|v| v.meaning.as_deref()),
    );
    curies
        .into_iter()
        .map(|c| c.split_once(':').map_or(c, |(p, _)| p).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use antares_ops::policy::{Filter, Rule};

    fn model(json: &str) -> Model {
        Model::read(json.as_bytes(), &LIMITS).expect("a model")
    }

    const SOURCE: &str = r#"{
      "id": "https://e.org/city", "name": "city", "version": "2.1.0",
      "prefixes": {
        "city": {"prefix_prefix": "city", "prefix_reference": "https://e.org/city/"},
        "secret": {"prefix_prefix": "secret", "prefix_reference": "https://hidden.e.org/"},
        "unece": {"prefix_prefix": "unece", "prefix_reference": "http://unece.org/rec20/"}
      },
      "default_prefix": "city",
      "classes": {
        "Thing": {"slots": ["name"]},
        "Car": {"is_a": "Thing", "slots": ["speed", "owner", "fuel", "vin"],
                "slot_usage": {"speed": {"required": true}}},
        "Person": {"class_uri": "secret:Person", "slots": ["name", "car"]}
      },
      "slots": {
        "name": {},
        "speed": {"range": "float", "unit": {"ucum_code": "km/h", "exact_mappings": ["unece:KMH"]}},
        "owner": {"range": "Person", "inverse": "car"},
        "car": {"range": "Car", "inverse": "owner"},
        "fuel": {"range": "Fuel"}, "vin": {}
      },
      "enums": {"Fuel": {"permissible_values": {"diesel": {}}},
                "Secret": {"permissible_values": {"x": {}}}}
    }"#;

    fn visible(types: &[&str], attrs: &[&str]) -> Visible {
        let iri = |n: &&str| format!("https://e.org/city/{n}");
        antares_ops::Visible::from_filter(&Filter {
            rules: vec![Rule {
                types: types.iter().map(iri).collect(),
                attributes: attrs.iter().map(iri).collect(),
                ..Rule::default()
            }],
            ..Filter::default()
        })
    }

    #[test]
    fn a_hidden_class_goes_with_its_prefix_ranges_and_inverses() {
        let source = model(SOURCE);
        let (m, left_out) = narrow(
            &source,
            &visible(&["Car", "Thing"], &["name", "speed", "owner", "fuel"]),
            "cars",
        )
        .expect("narrowed");
        let s = m.schema();
        assert!(left_out);
        assert_eq!(s.id, "https://e.org/city/views/cars");
        assert_eq!(s.version.as_deref(), Some("2.1.0"));
        assert_eq!(s.classes.keys().collect::<Vec<_>>(), ["Car", "Thing"]);
        assert_eq!(
            s.prefixes.keys().collect::<Vec<_>>(),
            ["city", "unece"],
            "LK6: the unit's prefix stays, the hidden class's goes"
        );
        let owner = &s.slots["owner"];
        assert_eq!(owner.range.as_deref(), Some("uriorcurie"), "LK1");
        assert_eq!(owner.inverse, None);
        assert_eq!(m.kind(owner), "Relationship", "the kind survives the range");
        assert!(!s.slots.contains_key("vin"));
        assert!(!s.slots.contains_key("car"));
        assert_eq!(s.enums.keys().collect::<Vec<_>>(), ["Fuel"]);
        let car = &s.classes["Car"];
        assert_eq!(car.is_a.as_deref(), Some("Thing"));
        assert_eq!(car.slot_usage["speed"].required, Some(true));
        assert!(!linkml_model::yaml(&m).contains("Person"));
    }

    #[test]
    fn is_a_is_cut_when_the_parent_shows_what_the_child_hides() {
        let source = model(SOURCE);
        let iri = |n: &str| format!("https://e.org/city/{n}");
        let v = antares_ops::Visible::from_filter(&Filter {
            rules: vec![
                Rule {
                    types: vec![iri("Thing")],
                    attributes: vec![iri("name")],
                    ..Rule::default()
                },
                Rule {
                    types: vec![iri("Car")],
                    attributes: vec![iri("speed")],
                    ..Rule::default()
                },
            ],
            ..Filter::default()
        });
        let (m, _) = narrow(&source, &v, "cars").expect("narrowed");
        let car = &m.schema().classes["Car"];
        assert_eq!(car.is_a, None);
        assert_eq!(car.slots, ["speed"]);
    }

    #[test]
    fn both_ends_visible_keep_the_relationship_whole() {
        let source = model(SOURCE);
        let v = antares_ops::Visible::from_filter(&Filter {
            rules: vec![Rule {
                types: vec![
                    "https://e.org/city/Car".into(),
                    "https://hidden.e.org/Person".into(),
                ],
                attributes: ["owner", "car", "name"]
                    .iter()
                    .map(|n| format!("https://e.org/city/{n}"))
                    .collect(),
                ..Rule::default()
            }],
            ..Filter::default()
        });
        let (m, _) = narrow(&source, &v, "all").expect("narrowed");
        let s = m.schema();
        assert_eq!(s.slots["owner"].range.as_deref(), Some("Person"));
        assert_eq!(s.slots["owner"].inverse.as_deref(), Some("car"));
        assert!(s.prefixes.contains_key("secret"), "Person's own prefix");
    }

    #[test]
    fn nothing_visible_is_an_empty_schema_not_an_error() {
        let (m, left_out) = narrow(&model(SOURCE), &Visible::default(), "none").expect("ok");
        assert!(left_out);
        assert!(m.schema().classes.is_empty());
        assert!(m.schema().slots.is_empty());
    }

    /// Every class and every slot, Person's under its own namespace.
    fn everything(attrs: &[&str]) -> Visible {
        antares_ops::Visible::from_filter(&Filter {
            rules: vec![Rule {
                types: [
                    "https://e.org/city/Thing",
                    "https://e.org/city/Car",
                    "https://hidden.e.org/Person",
                ]
                .map(str::to_owned)
                .to_vec(),
                attributes: attrs
                    .iter()
                    .map(|a| format!("https://e.org/city/{a}"))
                    .collect(),
                ..Rule::default()
            }],
            ..Filter::default()
        })
    }

    #[test]
    fn everything_visible_leaves_nothing_out() {
        let all = ["name", "speed", "owner", "car", "fuel", "vin"];
        let (m, left_out) = narrow(&model(SOURCE), &everything(&all), "all").expect("narrowed");
        assert!(!left_out);
        assert_eq!(m.schema().classes.len(), 3);
        let (_, left_out) =
            narrow(&model(SOURCE), &everything(&all[..5]), "all").expect("narrowed");
        assert!(left_out, "every class kept, one slot hidden");
    }

    #[test]
    fn an_inverse_to_a_hidden_slot_goes_and_the_range_stays() {
        let (m, _) =
            narrow(&model(SOURCE), &everything(&["owner", "name"]), "v").expect("narrowed");
        let owner = &m.schema().slots["owner"];
        assert_eq!(owner.range.as_deref(), Some("Person"));
        assert_eq!(owner.inverse, None);
    }

    /// T-3645: a schema in another vocabulary (`sdm:`) and a view written with short names, as
    /// the Portal writes them: the classes and slots the view names are shown, read under the
    /// core @context, the rest stays hidden, and the document keeps the model's own IRIs.
    #[test]
    fn a_schema_of_another_vocabulary_shows_what_the_view_names() {
        let source = model(
            r#"{"id": "https://e.org/m", "name": "m",
              "prefixes": {"sdm": {"prefix_prefix": "sdm",
                                   "prefix_reference": "https://smartdatamodels.org/"}},
              "default_prefix": "sdm",
              "classes": {"BikeHireDockingStation": {"slots": ["status", "secretNote"]},
                          "Depot": {"slots": ["status"]}},
              "slots": {"status": {}, "secretNote": {}}}"#,
        );
        let core = |n: &str| format!("https://uri.etsi.org/ngsi-ld/default-context/{n}");
        let v = antares_ops::Visible::from_filter(&Filter {
            rules: vec![Rule {
                types: vec![core("BikeHireDockingStation")],
                attributes: vec![core("status")],
                ..Rule::default()
            }],
            ..Filter::default()
        });
        assert!(!v.shows_type(&source.class_iri("BikeHireDockingStation")));
        let (m, left_out) = narrow(&source, &v, "bikes").expect("narrowed");
        assert!(left_out);
        let s = m.schema();
        assert_eq!(
            s.classes.keys().collect::<Vec<_>>(),
            ["BikeHireDockingStation"]
        );
        assert_eq!(s.slots.keys().collect::<Vec<_>>(), ["status"]);
        assert_eq!(
            m.class_iri("BikeHireDockingStation"),
            "https://smartdatamodels.org/BikeHireDockingStation"
        );
    }
}
