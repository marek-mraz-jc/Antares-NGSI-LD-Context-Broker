// SPDX-License-Identifier: EUPL-1.2
//! `OperationId` is the matrix: the same row ids in the same order, each with
//! the binding the row writes (ADR-0027 section 1, T-3495).

use antares_ops::OperationId;

/// `(id, binding)` of every row under `operations:` in the matrix, read
/// line by line: a row starts at `- id:` and its first `binding:` is the
/// NGSI-LD one, the first surface every row lists.
fn matrix_rows(text: &str) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut in_operations = false;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.starts_with('-') {
            in_operations = line == "operations:";
            continue;
        }
        if !in_operations {
            continue;
        }
        if let Some(id) = line.strip_prefix("- id: ") {
            rows.push((id.trim().to_owned(), String::new()));
        } else if let Some(binding) = line.trim_start().strip_prefix("binding: ") {
            if let Some(last) = rows.last_mut().filter(|r| r.1.is_empty()) {
                binding.trim().trim_matches('\'').clone_into(&mut last.1);
            }
        }
    }
    rows
}

#[test]
fn operation_ids_match_the_matrix() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/analysis/operation-matrix.yaml"
    );
    let rows = matrix_rows(&std::fs::read_to_string(path).expect("the operation matrix"));
    assert_eq!(
        rows.len(),
        69,
        "the matrix has {} operation rows",
        rows.len()
    );
    let ours: Vec<(String, String)> = OperationId::ALL
        .iter()
        .map(|op| (op.id().to_owned(), op.binding().to_owned()))
        .collect();
    assert_eq!(ours, rows);
}

#[test]
fn every_binding_resolves_to_its_own_operation() {
    let sample = |segment: &str| {
        if segment.starts_with('{') {
            "urn:ngsi-ld:Thing:1".to_owned()
        } else {
            segment.to_owned()
        }
    };
    for op in OperationId::ALL {
        let binding = op.binding();
        if binding.contains("broker-initiated") {
            assert_eq!(OperationId::of("POST", "/ngsi-ld/v1/notify", None), None);
            continue;
        }
        let (methods, rest) = binding.split_once(' ').expect("method and path");
        let path = rest.split(" and ").next().expect("a path");
        let (path, query) = match path.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (path, None),
        };
        let concrete: Vec<String> = path.split('/').map(sample).collect();
        for method in methods.split('|') {
            assert_eq!(
                OperationId::of(method, &concrete.join("/"), query),
                Some(op),
                "{method} {binding}"
            );
        }
    }
}

#[test]
fn a_path_off_the_matrix_is_no_operation() {
    for (method, path) in [
        ("GET", "/q/health"),
        ("GET", "/x/views"),
        ("GET", "/ngsi-ld/v1/entities/../../q/health"),
        ("GET", "/ngsi-ld/v1/entities/."),
        ("GET", "/ngsi-ld/v1/entities//attrs"),
        ("PUT", "/ngsi-ld/v1/entities"),
        ("GET", "/ngsi-ld/v1/entities/a/b/c"),
        ("GET", "/ngsi-ld/v1"),
    ] {
        assert_eq!(OperationId::of(method, path, None), None, "{method} {path}");
    }
    assert_eq!(
        OperationId::of("GET", "/ngsi-ld/v1/entities/urn:a/attrs/speed/value", None),
        Some(OperationId::RetrieveAttributeValue),
        "the second path of a two-path binding"
    );
}
