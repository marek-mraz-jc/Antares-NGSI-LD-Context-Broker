//! Apache Parquet, and GeoParquet 1.1 when the Features carry a geometry (T-4465, EP-119).
//!
//! The file is written while the answer is paged: each page becomes one row group, zstd
//! compressed, so the writer holds one page of rows and the bytes written so far. Those bytes
//! never grow past the ceiling the writer was given: past it the file stops and the caller
//! answers an error, never a truncated file. The footer is written last, with the GeoParquet
//! `geo` metadata (the geometry types and the bounding box of every row) when there is a
//! geometry column.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parquet::basic::{Compression, LogicalType, Repetition, Type as Physical, ZstdLevel};
use parquet::data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int64Type};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::Type;
use serde_json::{json, Value};

use crate::{wkb, Column, Kind};

/// The media type of a Parquet file.
pub const MEDIA_TYPE: &str = "application/vnd.apache.parquet";

/// The column holding each Feature's id.
pub const ID: &str = "id";
/// The column holding each Feature's geometry as WKB.
pub const GEOMETRY: &str = "geometry";

/// Why no file was written.
#[derive(Debug)]
pub enum Error {
    /// The file grew past the ceiling it was given.
    TooLarge,
    /// The writer refused the schema or a value.
    Write(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => f.write_str("the file is larger than the answer may be"),
            Self::Write(why) => write!(f, "the file does not serialize: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// A Parquet file being written one page at a time.
pub struct Writer {
    file: SerializedFileWriter<Bounded>,
    columns: Vec<Column>,
    /// Whether the file has a geometry column, and what it has seen in it.
    geometry: Option<Seen>,
    over: Arc<AtomicBool>,
}

/// The geometry types and the bounding box of the rows written so far.
#[derive(Default)]
struct Seen {
    types: BTreeSet<String>,
    bbox: Option<[f64; 4]>,
}

impl Writer {
    /// A file of an `id` column, a `geometry` column when `geometry`, and `columns`, refusing to
    /// grow past `max_bytes`. A column named like the id or the geometry, or like an earlier
    /// column, is left out: the Feature's own member holds that name.
    ///
    /// # Errors
    /// [`Error::Write`] when the schema does not build.
    pub fn new(columns: Vec<Column>, geometry: bool, max_bytes: usize) -> Result<Self, Error> {
        let mut named = BTreeSet::from([ID.to_owned(), GEOMETRY.to_owned()]);
        let columns: Vec<Column> = columns
            .into_iter()
            .filter(|c| named.insert(c.name.clone()))
            .collect();
        let mut fields = vec![field(
            ID,
            Physical::BYTE_ARRAY,
            Some(LogicalType::String),
            false,
        )?];
        if geometry {
            fields.push(field(GEOMETRY, Physical::BYTE_ARRAY, None, true)?);
        }
        for c in &columns {
            let (physical, logical) = match c.kind {
                Kind::Integer => (Physical::INT64, None),
                Kind::Double => (Physical::DOUBLE, None),
                Kind::Boolean => (Physical::BOOLEAN, None),
                Kind::Text => (Physical::BYTE_ARRAY, Some(LogicalType::String)),
                Kind::Json => (Physical::BYTE_ARRAY, Some(LogicalType::Json)),
            };
            fields.push(field(&c.name, physical, logical, true)?);
        }
        let schema = Type::group_type_builder("schema")
            .with_fields(fields)
            .build()
            .map_err(|e| written(&e))?;
        let properties = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::default()))
            .set_max_row_group_row_count(None)
            .build();
        let over = Arc::new(AtomicBool::new(false));
        let sink = Bounded {
            bytes: Vec::new(),
            max: max_bytes,
            over: Arc::clone(&over),
        };
        let file = SerializedFileWriter::new(sink, Arc::new(schema), Arc::new(properties))
            .map_err(|e| failed(&over, &e))?;
        Ok(Self {
            file,
            columns,
            geometry: geometry.then(Seen::default),
            over,
        })
    }

    /// One page of `GeoJSON` Features as one row group; an empty page writes nothing.
    ///
    /// # Errors
    /// [`Error::TooLarge`] past the ceiling, [`Error::Write`] when the writer refuses a value.
    pub fn page(&mut self, features: &[Value]) -> Result<(), Error> {
        if features.is_empty() {
            return Ok(());
        }
        let over = Arc::clone(&self.over);
        let fail = |e: parquet::errors::ParquetError| failed(&over, &e);
        let mut group = self.file.next_row_group().map_err(fail)?;
        let mut index = 0usize;
        while let Some(mut column) = group.next_column().map_err(fail)? {
            match (index, &mut self.geometry) {
                (0, _) => {
                    let ids: Vec<ByteArray> = features
                        .iter()
                        .map(|f| text(&f["id"]).into_bytes().into())
                        .collect();
                    column
                        .typed::<ByteArrayType>()
                        .write_batch(&ids, None, None)
                        .map_err(fail)?;
                }
                (1, Some(seen)) => {
                    let encoded: Vec<Option<Vec<u8>>> = features
                        .iter()
                        .map(|f| {
                            let (bytes, name, bbox) = wkb::encode(&f["geometry"])?;
                            seen.types.insert(name);
                            seen.bbox = merge(seen.bbox, bbox);
                            Some(bytes)
                        })
                        .collect();
                    let levels: Vec<i16> = encoded.iter().map(|g| i16::from(g.is_some())).collect();
                    let values: Vec<ByteArray> =
                        encoded.into_iter().flatten().map(ByteArray::from).collect();
                    column
                        .typed::<ByteArrayType>()
                        .write_batch(&values, Some(&levels), None)
                        .map_err(fail)?;
                }
                _ => {
                    let offset = if self.geometry.is_some() { 2 } else { 1 };
                    let c = index
                        .checked_sub(offset)
                        .and_then(|i| self.columns.get(i))
                        .ok_or_else(|| Error::Write("a column the schema does not hold".into()))?;
                    let cells: Vec<&Value> = features
                        .iter()
                        .map(|f| {
                            f.get("properties")
                                .and_then(|p| p.get(c.name.as_str()))
                                .unwrap_or(&Value::Null)
                        })
                        .collect();
                    write_cells(&mut column, c.kind, &cells).map_err(fail)?;
                }
            }
            column.close().map_err(fail)?;
            index = index.saturating_add(1);
        }
        group.close().map_err(fail)?;
        Ok(())
    }

    /// The whole file, its footer and the `geo` metadata written.
    ///
    /// # Errors
    /// [`Error::TooLarge`] when the footer does not fit under the ceiling.
    pub fn finish(mut self) -> Result<Vec<u8>, Error> {
        if let Some(seen) = &self.geometry {
            self.file
                .append_key_value_metadata(KeyValue::new("geo".to_owned(), geo(seen).to_string()));
        }
        let over = Arc::clone(&self.over);
        self.file
            .into_inner()
            .map(|sink| sink.bytes)
            .map_err(|e| failed(&over, &e))
    }
}

/// The GeoParquet 1.1 file metadata of the geometry column: WKB in OGC:CRS84 (the default
/// `crs`, so it is not written), its geometry types and the box of every row.
fn geo(seen: &Seen) -> Value {
    let mut column = json!({
        "encoding": "WKB",
        "geometry_types": seen.types.iter().collect::<Vec<_>>(),
    });
    if let (Some(bbox), Some(members)) = (seen.bbox, column.as_object_mut()) {
        members.insert("bbox".into(), json!(bbox));
    }
    json!({"version": "1.1.0", "primary_column": GEOMETRY, "columns": {GEOMETRY: column}})
}

fn merge(a: Option<[f64; 4]>, b: Option<[f64; 4]>) -> Option<[f64; 4]> {
    match (a, b) {
        (Some([a0, a1, a2, a3]), Some([b0, b1, b2, b3])) => {
            Some([a0.min(b0), a1.min(b1), a2.max(b2), a3.max(b3)])
        }
        (a, b) => a.or(b),
    }
}

fn field(
    name: &str,
    physical: Physical,
    logical: Option<LogicalType>,
    optional: bool,
) -> Result<Arc<Type>, Error> {
    Type::primitive_type_builder(name, physical)
        .with_repetition(if optional {
            Repetition::OPTIONAL
        } else {
            Repetition::REQUIRED
        })
        .with_logical_type(logical)
        .build()
        .map(Arc::new)
        .map_err(|e| written(&e))
}

/// One column of a page: definition level 1 a value, 0 a null. A value of another type than
/// the column's (a text in a number column) is a null, as an absent one is.
fn write_cells(
    column: &mut parquet::file::writer::SerializedColumnWriter<'_>,
    kind: Kind,
    cells: &[&Value],
) -> Result<(), parquet::errors::ParquetError> {
    match kind {
        Kind::Integer => {
            let values: Vec<Option<i64>> = cells.iter().map(|v| v.as_i64()).collect();
            let (levels, values) = split(values);
            column
                .typed::<Int64Type>()
                .write_batch(&values, Some(&levels), None)?;
        }
        Kind::Double => {
            let values: Vec<Option<f64>> = cells.iter().map(|v| v.as_f64()).collect();
            let (levels, values) = split(values);
            column
                .typed::<DoubleType>()
                .write_batch(&values, Some(&levels), None)?;
        }
        Kind::Boolean => {
            let values: Vec<Option<bool>> = cells.iter().map(|v| v.as_bool()).collect();
            let (levels, values) = split(values);
            column
                .typed::<BoolType>()
                .write_batch(&values, Some(&levels), None)?;
        }
        Kind::Text | Kind::Json => {
            let values: Vec<Option<ByteArray>> = cells
                .iter()
                .map(|v| (!v.is_null()).then(|| text(v).into_bytes().into()))
                .collect();
            let (levels, values) = split(values);
            column
                .typed::<ByteArrayType>()
                .write_batch(&values, Some(&levels), None)?;
        }
    }
    Ok(())
}

fn split<T>(values: Vec<Option<T>>) -> (Vec<i16>, Vec<T>) {
    let levels = values.iter().map(|v| i16::from(v.is_some())).collect();
    (levels, values.into_iter().flatten().collect())
}

/// A string as itself, anything else as its JSON text.
fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn written(e: &parquet::errors::ParquetError) -> Error {
    Error::Write(e.to_string())
}

/// The ceiling's refusal reaches the caller as the writer's I/O error; the flag tells it apart.
fn failed(over: &AtomicBool, e: &parquet::errors::ParquetError) -> Error {
    if over.load(Ordering::Relaxed) {
        Error::TooLarge
    } else {
        Error::Write(e.to_string())
    }
}

/// The file's bytes, refusing to grow past `max`.
struct Bounded {
    bytes: Vec<u8>,
    max: usize,
    over: Arc<AtomicBool>,
}

impl Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(buf.len()) > self.max {
            self.over.store(true, Ordering::Relaxed);
            return Err(io::Error::other(
                "the file is larger than the answer may be",
            ));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::Field;

    fn features(n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| {
                let x = f64::from(u32::try_from(i).unwrap_or_default());
                json!({
                    "type": "Feature",
                    "id": format!("urn:ngsi-ld:Car:{i}"),
                    "geometry": {"type": "Point", "coordinates": [x, 48.0]},
                    "properties": {"speed": i, "name": format!("car {i}"), "fast": i % 2 == 0,
                                   "tags": ["a", i], "missing": null}
                })
            })
            .collect()
    }

    fn columns() -> Vec<Column> {
        vec![
            Column {
                name: "speed".into(),
                kind: Kind::Integer,
            },
            Column {
                name: "name".into(),
                kind: Kind::Text,
            },
            Column {
                name: "fast".into(),
                kind: Kind::Boolean,
            },
            Column {
                name: "tags".into(),
                kind: Kind::Json,
            },
            Column {
                name: "missing".into(),
                kind: Kind::Double,
            },
            Column {
                name: "id".into(),
                kind: Kind::Text,
            },
        ]
    }

    fn read(bytes: Vec<u8>) -> SerializedFileReader<bytes::Bytes> {
        SerializedFileReader::new(bytes::Bytes::from(bytes))
            .unwrap_or_else(|e| unreachable!("the file reads back: {e}"))
    }

    #[test]
    fn each_page_is_a_row_group_and_every_value_reads_back() {
        let mut writer =
            Writer::new(columns(), true, 1 << 20).unwrap_or_else(|e| unreachable!("{e}"));
        let all = features(5);
        writer
            .page(all.get(..3).unwrap_or_default())
            .unwrap_or_else(|e| unreachable!("{e}"));
        writer.page(&[]).unwrap_or_else(|e| unreachable!("{e}"));
        writer
            .page(all.get(3..).unwrap_or_default())
            .unwrap_or_else(|e| unreachable!("{e}"));
        let file = read(writer.finish().unwrap_or_else(|e| unreachable!("{e}")));
        let meta = file.metadata();
        assert_eq!(meta.num_row_groups(), 2, "an empty page writes no group");
        assert_eq!(meta.file_metadata().num_rows(), 5);
        let names: Vec<&str> = meta
            .file_metadata()
            .schema_descr()
            .columns()
            .iter()
            .map(|c| c.name())
            .collect();
        assert_eq!(
            names,
            ["id", "geometry", "speed", "name", "fast", "tags", "missing"],
            "a property named id is the Feature's id"
        );
        let rows: Vec<_> = file
            .get_row_iter(None)
            .unwrap_or_else(|e| unreachable!("{e}"))
            .flatten()
            .collect();
        let first: Vec<(String, Field)> = rows
            .first()
            .map(|r| {
                r.get_column_iter()
                    .map(|(n, f)| (n.clone(), f.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let get = |name: &str| {
            first
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, f)| f.clone())
        };
        assert_eq!(get("id"), Some(Field::Str("urn:ngsi-ld:Car:0".into())));
        assert_eq!(get("speed"), Some(Field::Long(0)));
        assert_eq!(get("name"), Some(Field::Str("car 0".into())));
        assert_eq!(get("fast"), Some(Field::Bool(true)));
        assert_eq!(get("tags"), Some(Field::Str("[\"a\",0]".into())));
        assert_eq!(get("missing"), Some(Field::Null));
        let Some(Field::Bytes(wkb)) = get("geometry") else {
            unreachable!("a geometry")
        };
        assert_eq!(wkb::decode(wkb.data()), Some(json!([0.0, 48.0])));
        let compressed = meta.row_group(0).column(0).compression();
        assert!(matches!(compressed, Compression::ZSTD(_)), "{compressed:?}");
    }

    #[test]
    fn the_geo_metadata_is_geoparquet_1_1() {
        let mut writer =
            Writer::new(Vec::new(), true, 1 << 20).unwrap_or_else(|e| unreachable!("{e}"));
        let mut page = features(3);
        if let Some(f) = page.get_mut(2) {
            f["geometry"] =
                json!({"type": "LineString", "coordinates": [[-1.0, 47.0], [3.0, 49.5]]});
        }
        if let Some(f) = page.get_mut(1) {
            f["geometry"] = Value::Null;
        }
        writer.page(&page).unwrap_or_else(|e| unreachable!("{e}"));
        let file = read(writer.finish().unwrap_or_else(|e| unreachable!("{e}")));
        let kv = file
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .cloned()
            .unwrap_or_default();
        let geo: Value = kv
            .iter()
            .find(|k| k.key == "geo")
            .and_then(|k| k.value.as_deref())
            .and_then(|v| serde_json::from_str(v).ok())
            .unwrap_or_default();
        // GeoParquet 1.1 schema: version, primary_column and columns are required; a column
        // requires encoding and geometry_types; bbox is four numbers in 2D.
        let at = |path: &str| geo.pointer(path).cloned();
        assert_eq!(at("/version"), Some(json!("1.1.0")));
        assert_eq!(at("/primary_column"), Some(json!("geometry")));
        assert_eq!(at("/columns/geometry/encoding"), Some(json!("WKB")));
        assert_eq!(
            at("/columns/geometry/geometry_types"),
            Some(json!(["LineString", "Point"]))
        );
        assert_eq!(
            at("/columns/geometry/bbox"),
            Some(json!([-1.0, 47.0, 3.0, 49.5]))
        );
        assert_eq!(at("/columns/geometry/crs"), None, "absent crs is OGC:CRS84");
        let rows: Vec<_> = file
            .get_row_iter(None)
            .unwrap_or_else(|e| unreachable!("{e}"))
            .flatten()
            .collect();
        let geometry = |i: usize| {
            rows.get(i).and_then(|r| {
                r.get_column_iter()
                    .find(|(n, _)| *n == "geometry")
                    .map(|(_, f)| f.clone())
            })
        };
        assert_eq!(
            geometry(1),
            Some(Field::Null),
            "a Feature without a geometry is a null"
        );
    }

    #[test]
    fn a_file_without_a_geometry_is_plain_parquet() {
        let mut writer =
            Writer::new(columns(), false, 1 << 20).unwrap_or_else(|e| unreachable!("{e}"));
        writer
            .page(&features(2))
            .unwrap_or_else(|e| unreachable!("{e}"));
        let file = read(writer.finish().unwrap_or_else(|e| unreachable!("{e}")));
        let meta = file.metadata().file_metadata();
        assert!(meta
            .key_value_metadata()
            .is_none_or(|kv| kv.iter().all(|k| k.key != "geo")));
        assert!(meta
            .schema_descr()
            .columns()
            .iter()
            .all(|c| c.name() != "geometry"));
    }

    #[test]
    fn past_the_ceiling_the_file_is_refused_not_cut() {
        let mut writer = Writer::new(columns(), true, 4096).unwrap_or_else(|e| unreachable!("{e}"));
        let mut outcome = Ok(());
        for _ in 0..50 {
            outcome = writer.page(&features(200));
            if outcome.is_err() {
                break;
            }
        }
        let outcome = outcome.and_then(|()| writer.finish().map(|_| ()));
        assert!(matches!(outcome, Err(Error::TooLarge)), "{outcome:?}");
        // The writer buffers its header, so an empty file past the ceiling is refused at the end.
        let tiny = Writer::new(columns(), true, 3).and_then(Writer::finish);
        assert!(
            matches!(tiny, Err(Error::TooLarge)),
            "even an empty file is past three bytes"
        );
    }
}
