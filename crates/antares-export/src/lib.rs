//! File encodings of an answer read page by page (T-4465, EP-119). Each encoding sits behind
//! its own feature and none depends on a surface: a surface hands over `GeoJSON` Features one
//! page at a time with the columns its schema gives, the encoder writes as it goes, and it
//! refuses to grow past the byte ceiling it was given, so a file is whole or not written.

use serde_json::Value;

#[cfg(feature = "parquet")]
pub mod parquet;
#[cfg(feature = "parquet")]
mod wkb;

/// What one column holds, read from the JSON Schema of its property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A string.
    Text,
    /// A whole number.
    Integer,
    /// Any other number.
    Double,
    /// `true` or `false`.
    Boolean,
    /// Anything structured: an object or an array, written as JSON text.
    Json,
}

impl Kind {
    /// The kind of a property whose value `schema` describes; a nullable type (`["number",
    /// "null"]`) is its other member, and anything that is no scalar is [`Kind::Json`].
    #[must_use]
    pub fn of_schema(schema: &Value) -> Self {
        let named = match &schema["type"] {
            Value::String(name) => Some(name.as_str()),
            Value::Array(names) => names
                .iter()
                .filter_map(Value::as_str)
                .find(|n| *n != "null"),
            _ => None,
        };
        match named {
            Some("string") => Self::Text,
            Some("integer") => Self::Integer,
            Some("number") => Self::Double,
            Some("boolean") => Self::Boolean,
            _ => Self::Json,
        }
    }
}

/// One column of the file: a Feature property's name and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// The property's name in the Features' `properties`.
    pub name: String,
    /// What it holds.
    pub kind: Kind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_column_is_typed_from_its_schema() {
        for (schema, kind) in [
            (json!({"type": "string"}), Kind::Text),
            (json!({"type": "string", "format": "date-time"}), Kind::Text),
            (json!({"type": "integer"}), Kind::Integer),
            (json!({"type": "number"}), Kind::Double),
            (json!({"type": ["null", "number"]}), Kind::Double),
            (json!({"type": "boolean"}), Kind::Boolean),
            (
                json!({"type": "array", "items": {"type": "number"}}),
                Kind::Json,
            ),
            (json!({"type": "object"}), Kind::Json),
            (json!({}), Kind::Json),
        ] {
            assert_eq!(Kind::of_schema(&schema), kind, "{schema}");
        }
    }
}
