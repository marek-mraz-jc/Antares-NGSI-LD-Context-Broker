// SPDX-License-Identifier: EUPL-1.2
//! The load: unknown input refused, not ignored (threat LK4), limits
//! checked before and after the parse, references resolved.

use linkml_model::{Error, Limits, Model};
use serde_json::{json, Value};

const LIMITS: Limits = Limits {
    max_bytes: 1 << 20,
    max_depth: 32,
    max_elements: 10_000,
};

fn base() -> Value {
    json!({
        "id": "https://example.org/s",
        "name": "s",
        "classes": {"Room": {"slots": ["size"]}},
        "slots": {"size": {"range": "integer"}},
        "enums": {"E": {"permissible_values": {"a": {}}}}
    })
}

fn load(doc: &Value) -> Result<Model, Error> {
    Model::read(doc.to_string().as_bytes(), &LIMITS)
}

fn with(pointer: &str, value: Value) -> Value {
    let mut doc = base();
    if let Some((parent, key)) = pointer.rsplit_once('/') {
        if let Some(o) = doc.pointer_mut(parent).and_then(Value::as_object_mut) {
            o.insert(key.to_owned(), value);
        }
    }
    doc
}

#[test]
fn the_base_document_loads() {
    load(&base()).unwrap();
}

#[test]
fn an_unknown_metaslot_fails_the_load() {
    // A metaslot of LinkML outside the subset, at every level.
    for pointer in [
        "/imports",
        "/classes/Room/mixins",
        "/classes/Room/attributes",
        "/slots/size/pattern",
        "/enums/E/enum_uri",
        "/enums/E/permissible_values/a/is_a",
        "/classes/Room/slot_usage/size",
    ] {
        let doc = if pointer.ends_with("slot_usage/size") {
            with(
                "/classes/Room/slot_usage",
                json!({"size": {"minimum_value": 0}}),
            )
        } else {
            with(pointer, json!("x"))
        };
        match load(&doc) {
            Err(Error::NotImplemented(why)) => {
                assert!(why.contains("unknown field"), "{pointer}: {why}");
            }
            other => panic!("{pointer}: {other:?}"),
        }
    }
}

#[test]
fn a_range_outside_the_subset_is_not_implemented() {
    for range in ["ncname", "MyType", "curie"] {
        let doc = with("/slots/size/range", json!(range));
        assert!(
            matches!(load(&doc), Err(Error::NotImplemented(_))),
            "{range}"
        );
    }
}

#[test]
fn a_class_and_a_slot_with_one_name_is_not_implemented() {
    let doc = with("/slots/Room", json!({}));
    assert!(matches!(load(&doc), Err(Error::NotImplemented(_))));
}

#[test]
fn malformed_documents_are_malformed() {
    let cases = [
        ("not json", None),
        ("id missing", Some(json!({"name": "s"}))),
        (
            "wrong type",
            Some(with("/slots/size/required", json!("yes"))),
        ),
        ("relative id", Some(with("/id", json!("s")))),
        (
            "dangling is_a",
            Some(with("/classes/Room/is_a", json!("Nope"))),
        ),
        (
            "dangling slot",
            Some(with("/classes/Room/slots", json!(["size", "nope"]))),
        ),
        (
            "dangling inverse",
            Some(with("/slots/size/inverse", json!("nope"))),
        ),
        (
            "usage of a foreign slot",
            Some(with("/slots/other", json!({}))),
        ),
        (
            "name differs from key",
            Some(with("/slots/size/name", json!("other"))),
        ),
        (
            "prefix differs from key",
            Some(with(
                "/prefixes",
                json!({"p": {"prefix_prefix": "q", "prefix_reference": "https://e.org/"}}),
            )),
        ),
        (
            "prefix with a colon",
            Some(with(
                "/prefixes",
                json!({"p:q": {"prefix_prefix": "p:q", "prefix_reference": "https://e.org/"}}),
            )),
        ),
        (
            "prefix not an IRI",
            Some(with(
                "/prefixes",
                json!({"p": {"prefix_prefix": "p", "prefix_reference": "e.org"}}),
            )),
        ),
        (
            "undeclared default_prefix",
            Some(with("/default_prefix", json!("p"))),
        ),
        (
            "class and enum share a name",
            Some(with("/enums/Room", json!({}))),
        ),
        (
            "class named like a type",
            Some(with("/classes/string", json!({}))),
        ),
        (
            "unknown NGSI-LD kind",
            Some(with(
                "/slots/size/annotations",
                json!({"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "Thing"}}),
            )),
        ),
        (
            "annotation tag differs",
            Some(with(
                "/slots/size/annotations",
                json!({"a": {"tag": "b", "value": "c"}}),
            )),
        ),
        (
            "bare name not IRI-safe",
            Some(with("/classes/Two words", json!({}))),
        ),
        (
            "meaning not an IRI",
            Some(with("/enums/E/permissible_values/a/meaning", json!("a b"))),
        ),
    ];
    for (what, doc) in cases {
        let got = match doc {
            None => Model::read(b"{not json", &LIMITS),
            Some(d) if what == "usage of a foreign slot" => {
                let mut d = d;
                d["classes"]["Room"]["slot_usage"] = json!({"other": {}});
                load(&d)
            }
            Some(d) => load(&d),
        };
        assert!(matches!(got, Err(Error::Malformed(_))), "{what}: {got:?}");
    }
}

#[test]
fn an_is_a_cycle_is_malformed_and_does_not_loop() {
    let doc = json!({
        "id": "https://example.org/s", "name": "s",
        "classes": {"A": {"is_a": "B"}, "B": {"is_a": "C"}, "C": {"is_a": "A"}}
    });
    assert!(matches!(load(&doc), Err(Error::Malformed(why)) if why.contains("cycle")));
    let selfish =
        json!({"id": "https://example.org/s", "name": "s", "classes": {"A": {"is_a": "A"}}});
    assert!(matches!(load(&selfish), Err(Error::Malformed(_))));
}

#[test]
fn a_name_with_spaces_loads_when_it_has_its_own_uri() {
    let doc = with(
        "/classes/Two words",
        json!({"class_uri": "https://example.org/TwoWords"}),
    );
    let m = load(&doc).unwrap();
    assert_eq!(m.class_iri("Two words"), "https://example.org/TwoWords");
}

#[test]
fn the_size_bound_is_checked_before_the_parse() {
    let doc = base().to_string();
    let exact = Limits {
        max_bytes: doc.len(),
        ..LIMITS
    };
    Model::read(doc.as_bytes(), &exact).unwrap();
    let under = Limits {
        max_bytes: doc.len() - 1,
        ..LIMITS
    };
    assert_eq!(
        Model::read(doc.as_bytes(), &under),
        Err(Error::LimitExceeded {
            limit: "max_bytes",
            max: doc.len() - 1
        })
    );
    // Not even JSON: the bound answers first.
    let junk = vec![b'x'; 64];
    let tiny = Limits {
        max_bytes: 63,
        ..LIMITS
    };
    assert!(matches!(
        Model::read(&junk, &tiny),
        Err(Error::LimitExceeded {
            limit: "max_bytes",
            ..
        })
    ));
}

#[test]
fn the_depth_bound_is_checked_before_the_parse() {
    // Brackets inside strings do not count; real nesting does.
    let doc = with("/slots/size/description", json!("[[[[[[[[[[[[[[[[[[[[")).to_string();
    // the base document nests five deep: {enums {E {permissible_values {a {
    let depth = Limits {
        max_depth: 5,
        ..LIMITS
    };
    Model::read(doc.as_bytes(), &depth).unwrap();
    let deep = format!("{}{}", "[".repeat(6), "]".repeat(6));
    assert_eq!(
        Model::read(deep.as_bytes(), &depth),
        Err(Error::LimitExceeded {
            limit: "max_depth",
            max: 5
        })
    );
    // A depth serde_json itself would refuse never reaches it.
    let abyss = "[".repeat(100_000);
    assert!(matches!(
        Model::read(abyss.as_bytes(), &LIMITS),
        Err(Error::LimitExceeded {
            limit: "max_depth",
            ..
        })
    ));
}

#[test]
fn the_element_bound_counts_every_kind_of_element() {
    // 1 class + 1 slot + 1 enum + 1 slot reference + 1 permissible value
    let five = Limits {
        max_elements: 5,
        ..LIMITS
    };
    Model::read(base().to_string().as_bytes(), &five).unwrap();
    let four = Limits {
        max_elements: 4,
        ..LIMITS
    };
    assert_eq!(
        Model::read(base().to_string().as_bytes(), &four),
        Err(Error::LimitExceeded {
            limit: "max_elements",
            max: 4
        })
    );
    let annotated = with(
        "/slots/size/annotations",
        json!({"t": {"tag": "t", "value": "v"}}),
    );
    assert!(matches!(
        Model::read(annotated.to_string().as_bytes(), &five),
        Err(Error::LimitExceeded {
            limit: "max_elements",
            ..
        })
    ));
}

#[test]
fn errors_print_their_kind() {
    assert!(Error::Malformed("x".into())
        .to_string()
        .starts_with("malformed"));
    assert!(Error::NotImplemented("x".into())
        .to_string()
        .starts_with("outside"));
    assert_eq!(
        Error::LimitExceeded {
            limit: "max_depth",
            max: 3
        }
        .to_string(),
        "LinkML schema exceeds max_depth (3)"
    );
}

#[test]
fn iris_need_a_scheme_and_something_after_it() {
    for bad in [
        "ht_tp://x",
        "https:",
        "1x:y",
        "-x:y",
        "https://e.org/\u{7}",
        "https://e.org/\u{a0}",
    ] {
        let doc = with("/slots/size/slot_uri", json!(bad));
        assert!(matches!(load(&doc), Err(Error::Malformed(_))), "{bad}");
    }
    for good in ["urn:x:y", "https://e.org/a#b", "tag+x.y-z:1"] {
        load(&with("/slots/size/slot_uri", json!(good))).unwrap();
    }
}

#[test]
fn the_namespace_follows_the_id_or_the_default_prefix() {
    for (id, want) in [
        ("https://e.org/s", "https://e.org/s/size"),
        ("https://e.org/s/", "https://e.org/s/size"),
        ("https://e.org/s#", "https://e.org/s#size"),
    ] {
        assert_eq!(
            load(&with("/id", json!(id))).unwrap().slot_iri("size"),
            want
        );
    }
    let mut doc = with(
        "/prefixes",
        json!({"p": {"prefix_prefix": "p", "prefix_reference": "https://p.org/v#"}}),
    );
    doc["default_prefix"] = json!("p");
    let m = load(&doc).unwrap();
    assert_eq!(m.slot_iri("size"), "https://p.org/v#size");
    assert_eq!(m.expand("p:x"), "https://p.org/v#x");
    assert_eq!(
        m.expand("two words"),
        "two words",
        "no IRI: the text as it is"
    );
}

#[test]
fn class_slots_folds_ancestors_and_usage_and_is_empty_for_an_unknown_class() {
    let doc = json!({
        "id": "https://example.org/s", "name": "s",
        "classes": {
            "Thing": {"slots": ["name"], "slot_usage": {"name": {"annotations": {"a": {"tag": "a", "value": "1"}}}}},
            "Room": {"is_a": "Thing", "slots": ["name", "size"],
                     "slot_usage": {"name": {"slot_uri": "https://e.org/label", "range": "uri", "multivalued": true,
                                             "inlined": false, "inverse": "size", "unit": {"symbol": "m"},
                                             "annotations": {"b": {"tag": "b", "value": "2"}}}}}
        },
        "slots": {"name": {"description": "base"}, "size": {"range": "integer"}}
    });
    let m = load(&doc).unwrap();
    let slots = m.class_slots("Room");
    let names: Vec<&str> = slots.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names,
        ["name", "size"],
        "the ancestor's slot first, no duplicate"
    );
    let name = &slots[0].1;
    assert_eq!(name.description.as_deref(), Some("base"));
    assert_eq!(name.range.as_deref(), Some("uri"));
    assert_eq!(name.inverse.as_deref(), Some("size"));
    assert_eq!(
        name.unit.as_ref().and_then(|u| u.symbol.as_deref()),
        Some("m")
    );
    assert_eq!(
        name.annotations.keys().collect::<Vec<_>>(),
        ["a", "b"],
        "usage annotations accumulate"
    );
    assert_eq!(m.class_slots("Nope"), Vec::new());
}

#[test]
fn every_builtin_type_round_trips_its_name() {
    for name in [
        "string",
        "integer",
        "float",
        "double",
        "decimal",
        "boolean",
        "date",
        "datetime",
        "time",
        "uri",
        "uriorcurie",
    ] {
        let b = linkml_model::Builtin::from_name(name).unwrap();
        assert_eq!(b.name(), name);
        assert!(b.xsd().starts_with("xsd:"));
    }
}
