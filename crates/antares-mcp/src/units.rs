// SPDX-License-Identifier: EUPL-1.2
//! A unit a person reads beside each `unitCode` of a tool result.
//!
//! NGSI-LD writes a quantity's unit as a UN/CEFACT Recommendation 20
//! common code (`GQ`), which a model reading the answer takes for "no
//! unit". A tool result gains `"unit": "µg/m³"` beside every code the
//! table knows; the stored entity and the NGSI-LD answers are unchanged.

use serde_json::Value;

/// Rec 20 common code to the symbol, or the name of a unit with none.
static LABELS: &[(&str, &str)] = include!("rec20_labels.rs");

/// The label of `code`, when Rec 20 has the code.
fn label(code: &str) -> Option<&'static str> {
    let at = LABELS.partition_point(|(c, _)| *c < code);
    LABELS
        .get(at)
        .filter(|(c, _)| *c == code)
        .map(|(_, label)| *label)
}

/// Adds `unit` beside every known `unitCode` below `value` that has no
/// `unit` of its own.
pub(crate) fn name(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let unit = map
                .get("unitCode")
                .and_then(Value::as_str)
                .and_then(label)
                .filter(|_| !map.contains_key("unit"));
            if let Some(unit) = unit {
                map.insert("unit".to_owned(), Value::String(unit.to_owned()));
            }
            map.values_mut().for_each(name);
        }
        Value::Array(items) => items.iter_mut().for_each(name),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_table_is_sorted_and_each_code_once() {
        assert!(LABELS.is_sorted_by(|a, b| a.0 < b.0));
        assert_eq!(label("GQ"), Some("µg/m³"));
        assert_eq!(label("H87"), Some("piece"));
        // past the last code, and between two codes
        assert_eq!(label("ZZZ9"), None);
        assert_eq!(label("GQA"), None);
    }

    #[test]
    fn a_known_code_gains_its_label_and_nothing_else_changes() {
        let mut got = json!({ "entities": [{
            "pm10": { "type": "Property", "value": 8.93, "unitCode": "GQ" },
            "co": { "type": "Property", "value": 0.3, "unitCode": "GP" },
            "odd": { "type": "Property", "value": 1, "unitCode": "ZZZ9" },
            "own": { "unitCode": "GQ", "unit": "ug/m3" },
            "notText": { "unitCode": 7 },
            "name": { "type": "Property", "value": "unitCode" },
        }] });
        name(&mut got);
        let at = |path: &str| got.pointer(&format!("/entities/0/{path}")).cloned();
        assert_eq!(at("pm10/unit"), Some(json!("µg/m³")));
        assert_eq!(at("pm10/unitCode"), Some(json!("GQ")));
        assert_eq!(at("co/unit"), Some(json!("mg/m³")));
        assert_eq!(at("own/unit"), Some(json!("ug/m3")));
        for plain in ["odd", "notText", "name"] {
            assert_eq!(at(&format!("{plain}/unit")), None, "{plain}");
        }
    }
}
