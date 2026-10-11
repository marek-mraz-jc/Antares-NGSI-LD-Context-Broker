// SPDX-License-Identifier: EUPL-1.2
//! The writers against golden files, and names that try to break out of
//! the format they are printed in (threat LK3).

use linkml_model::{Limits, Model};

const LIMITS: Limits = Limits {
    max_bytes: 1 << 20,
    max_depth: 32,
    max_elements: 10_000,
};

type Writer = fn(&Model) -> String;

const WRITERS: [(&str, Writer); 7] = [
    ("linkml.yaml", linkml_model::yaml),
    ("schema.json", linkml_model::json_schema),
    ("context.jsonld", linkml_model::context),
    ("shacl.ttl", linkml_model::shacl),
    ("owl.ttl", linkml_model::owl),
    ("rdf.ttl", linkml_model::rdfs),
    ("md", linkml_model::markdown),
];

fn golden_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// `LINKML_GOLDEN_UPDATE=1 cargo test -p linkml-model --test emit` rewrites
/// the files; the diff is then reviewed like code.
#[test]
fn every_writer_matches_its_golden_file() {
    let dir = golden_dir();
    let model = Model::read(&std::fs::read(dir.join("building.json")).unwrap(), &LIMITS).unwrap();
    let update = std::env::var_os("LINKML_GOLDEN_UPDATE").is_some();
    for (ext, write) in WRITERS {
        let path = dir.join(format!("building.{ext}"));
        let got = write(&model);
        assert_eq!(got, write(&model), "{ext}: same model, same bytes");
        if update {
            std::fs::write(&path, &got).unwrap();
        }
        let want = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, want, "{ext} differs from {}", path.display());
    }
}

#[test]
fn the_json_writers_emit_json() {
    let model = Model::read(
        &std::fs::read(golden_dir().join("building.json")).unwrap(),
        &LIMITS,
    )
    .unwrap();
    let schema: serde_json::Value =
        serde_json::from_str(&linkml_model::json_schema(&model)).unwrap();
    let room = &schema["definitions"]["Room"];
    // inherited from Asset, refined by Room's slot_usage
    assert_eq!(
        room["properties"]["name"]["description"],
        "The room's \"label\"."
    );
    assert_eq!(room["required"], serde_json::json!(["name", "floor"]));
    assert_eq!(room["properties"]["sensors"]["type"], "array");
    assert_eq!(
        room["properties"]["sensors"]["x-ngsi-ld-kind"],
        "Relationship"
    );
    assert_eq!(
        room["properties"]["status"]["enum"],
        serde_json::json!(["active", "retired"])
    );
    let ctx: serde_json::Value = serde_json::from_str(&linkml_model::context(&model)).unwrap();
    assert_eq!(ctx["@context"]["Room"], "https://schema.org/Room");
    assert_eq!(
        ctx["@context"]["floor"],
        serde_json::json!({"@id": "https://example.org/building#floor",
                           "@type": "http://www.w3.org/2001/XMLSchema#integer"})
    );
    assert_eq!(
        ctx["@context"]["name"], "https://schema.org/name",
        "a string: no coercion"
    );
    assert!(
        ctx["@context"].get("schema").is_none(),
        "no prefix terms (LK6)"
    );
}

/// T-3780: an Entity in keyValues, read with L4, is what L3 and L5 describe:
/// a Relationship holds the target's `@id`, a float its datatype, and a
/// `GeoProperty` a `GeoJSON` geometry, an object and never a string.
#[test]
fn a_view_s_data_reads_as_its_documents_describe_it() {
    let doc = serde_json::json!({
        "id": "https://e.org/s", "name": "s",
        "classes": {"Station": {"slots": ["temperature", "location", "nearest"]}},
        "slots": {
            "temperature": {"range": "float"},
            "location": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "GeoProperty"}}},
            "nearest": {"range": "Station"}
        }
    });
    let model = Model::read(doc.to_string().as_bytes(), &LIMITS).unwrap();
    let ctx: serde_json::Value = serde_json::from_str(&linkml_model::context(&model)).unwrap();
    let terms = &ctx["@context"];
    assert_eq!(
        terms["nearest"],
        serde_json::json!({"@id": "https://e.org/s/nearest", "@type": "@id"})
    );
    assert_eq!(
        terms["temperature"]["@type"],
        "http://www.w3.org/2001/XMLSchema#float"
    );
    assert_eq!(terms["location"], "https://e.org/s/location", "no coercion");
    let schema: serde_json::Value =
        serde_json::from_str(&linkml_model::json_schema(&model)).unwrap();
    let location = &schema["definitions"]["Station"]["properties"]["location"];
    assert_eq!(location["type"], "object", "{location}");
    let shapes = linkml_model::shacl(&model);
    let geo = shapes
        .split("sh:property [")
        .find(|p| p.contains("sh:name \"location\""))
        .unwrap();
    assert!(!geo.contains("sh:datatype"), "{geo}");
    assert!(shapes.contains("sh:datatype xsd:float"), "{shapes}");
}

/// Names and texts that would end a YAML scalar, open a mapping, an anchor,
/// a tag or a comment, a Turtle literal or IRI, or a Markdown link.
const HOSTILE: [&str; 8] = [
    "a: b",
    "x # comment",
    "&anchor *alias",
    "!!python/object",
    "\"quoted\" 'single'",
    "back\\slash",
    "- [list]",
    "{flow: map}",
];

#[test]
fn names_are_quoted_and_cannot_inject_yaml() {
    for (i, name) in HOSTILE.iter().enumerate() {
        let doc = serde_json::json!({
            "id": "https://example.org/s",
            "name": name,
            "version": name,
            "classes": {"C": {"description": name, "slots": [name], "slot_usage": {*name: {"description": name}}}},
            "slots": {*name: {"slot_uri": "https://example.org/p", "description": format!("{name}\nline two\u{2028}")}},
            "enums": {"E": {"permissible_values": {*name: {"description": name}}}}
        });
        let model =
            Model::read(doc.to_string().as_bytes(), &LIMITS).unwrap_or_else(|e| panic!("{i}: {e}"));
        let yaml = linkml_model::yaml(&model);
        // Every line is a quoted key, a quoted list item or a closing marker:
        // nothing the schema supplied stands outside quotes.
        for line in yaml.lines() {
            let t = line.trim_start();
            assert!(
                t.starts_with('"') || t.starts_with("- \""),
                "{i}: unquoted line {line:?}"
            );
        }
        assert_eq!(
            yaml.lines().count(),
            yaml.matches('\n').count(),
            "{i}: no raw line break inside a scalar"
        );
        let ttl = linkml_model::owl(&model);
        assert!(ttl.contains(&format!(
            "rdfs:label \"{}\"",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        )));
        let md = linkml_model::markdown(&model);
        assert!(
            !md.replace("\\[", "").contains('['),
            "{i}: an unescaped [ in {md}"
        );
    }
}

#[test]
fn a_control_character_in_a_name_is_refused_at_load() {
    for bad in ["new\nline", "tab\there", "nul\u{0}", "esc\u{1b}[31m", ""] {
        let doc =
            serde_json::json!({"id": "https://example.org/s", "name": "s", "slots": {bad: {}}});
        let err = Model::read(doc.to_string().as_bytes(), &LIMITS).unwrap_err();
        assert!(
            matches!(err, linkml_model::Error::Malformed(_)),
            "{bad:?}: {err}"
        );
    }
}

#[test]
fn an_iri_that_would_break_turtle_is_refused_at_load() {
    for bad in [
        "https://e.org/a> . <x",
        "https://e.org/a b",
        "1bad:scheme",
        ":empty",
        "https://e.org/\"q\"",
    ] {
        let doc = serde_json::json!({"id": "https://example.org/s", "name": "s", "classes": {"C": {"class_uri": bad}}});
        let err = Model::read(doc.to_string().as_bytes(), &LIMITS).unwrap_err();
        assert!(
            matches!(err, linkml_model::Error::Malformed(_)),
            "{bad:?}: {err}"
        );
    }
}

#[test]
fn every_builtin_type_maps_in_json_schema_and_turtle() {
    let cases = [
        ("string", "string", None, "xsd:string"),
        ("integer", "integer", None, "xsd:integer"),
        ("float", "number", None, "xsd:float"),
        ("double", "number", None, "xsd:double"),
        ("decimal", "number", None, "xsd:decimal"),
        ("boolean", "boolean", None, "xsd:boolean"),
        ("date", "string", Some("date"), "xsd:date"),
        ("datetime", "string", Some("date-time"), "xsd:dateTime"),
        ("time", "string", Some("time"), "xsd:time"),
        ("uri", "string", Some("uri"), "xsd:anyURI"),
        ("uriorcurie", "string", None, "xsd:anyURI"),
    ];
    for (range, kind, format, xsd) in cases {
        let doc = serde_json::json!({
            "id": "https://example.org/s", "name": "s",
            "classes": {"C": {"slots": ["v"]}},
            "slots": {"v": {"range": range, "unit": {"ucum_code": "m"},
                            "annotations": {"note": {"tag": "note", "value": "kept"}}}}
        });
        let model = Model::read(doc.to_string().as_bytes(), &LIMITS).unwrap();
        let schema: serde_json::Value =
            serde_json::from_str(&linkml_model::json_schema(&model)).unwrap();
        let v = &schema["definitions"]["C"]["properties"]["v"];
        assert_eq!(v["type"], kind, "{range}");
        assert_eq!(v["format"].as_str(), format, "{range}");
        assert_eq!(
            v["x-ngsi-ld-kind"], "Property",
            "{range}: a tag other than ngsi_ld_kind is no kind"
        );
        assert!(
            linkml_model::shacl(&model).contains(&format!("sh:datatype {xsd}")),
            "{range}"
        );
        assert!(
            linkml_model::rdfs(&model).contains(&format!("rdfs:range {xsd}")),
            "{range}"
        );
        assert!(
            linkml_model::yaml(&model).contains("\"unit\":\n      \"ucum_code\": \"m\"\n"),
            "{range}"
        );
    }
}
