// SPDX-License-Identifier: EUPL-1.2
//! Entity type summary profiling (T-4680): pure functions with no I/O
//! to compute per-attribute statistics, rank attributes, extract sample rows,
//! and format a compact textual and JSON representation within a 6,000-character cap.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Reason why row loading stopped for this profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BasisReason {
    /// All available rows were loaded.
    All,
    /// Hit the configured row limit.
    RowLimit,
    /// Hit the memory limit.
    MemoryLimit,
    /// Server is busy.
    Busy,
    /// Server is in tight resource state.
    Tight,
}

impl BasisReason {
    /// String identifier for structured representation.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BasisReason::All => "All",
            BasisReason::RowLimit => "RowLimit",
            BasisReason::MemoryLimit => "MemoryLimit",
            BasisReason::Busy => "Busy",
            BasisReason::Tight => "Tight",
        }
    }
}

/// The basis of the summary profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Basis {
    /// Count of loaded rows.
    pub loaded: usize,
    /// Total row count if reported by broker.
    pub total: Option<u64>,
    /// The reason why loading finished.
    pub reason: BasisReason,
    /// Rows skipped due to size limits.
    pub skipped: usize,
}

/// Formatted sample of an entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SampleRow {
    /// The Entity `id`.
    pub id: String,
    /// Optional Entity name.
    pub name: Option<String>,
    /// Formatted attribute name-value pairs.
    pub values: Vec<(String, String)>,
}

/// Summary statistics of an individual attribute.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AttrStat {
    /// Numeric attribute statistics.
    Number {
        /// Attribute name.
        name: String,
        /// Number of non-null observations.
        n: usize,
        /// Count of null observations.
        nulls: usize,
        /// Minimum value.
        min: f64,
        /// Maximum value.
        max: f64,
        /// Arithmetic mean.
        mean: f64,
        /// Median value.
        median: f64,
        /// Standard deviation.
        stddev: f64,
        /// 10th percentile.
        p10: f64,
        /// 90th percentile.
        p90: f64,
        /// Histogram buckets: `(start, end, count)`.
        hist: Vec<(f64, f64, usize)>,
        /// Count of outliers.
        outliers: usize,
        /// Optional unit code.
        unit_code: Option<String>,
        /// Top 3 integer values if all values are integers.
        top3: Option<Vec<(i64, usize)>>,
    },
    /// Boolean attribute statistics.
    Boolean {
        /// Attribute name.
        name: String,
        /// Number of observations.
        n: usize,
        /// Share of `true` values.
        true_share: f64,
    },
    /// Date or time attribute statistics.
    DateTime {
        /// Attribute name.
        name: String,
        /// Number of observations.
        n: usize,
        /// Minimum date string.
        min: String,
        /// Maximum date string.
        max: String,
        /// Median date string.
        median: String,
    },
    /// Categorical attribute statistics.
    Category {
        /// Attribute name.
        name: String,
        /// Number of observations.
        n: usize,
        /// Number of distinct categories.
        distinct: usize,
        /// Top 5 categories with count and percentage.
        top5: Vec<(String, usize, f64)>,
        /// Count of observations in categories outside top 5.
        other: usize,
    },
    /// Free text attribute statistics.
    Text {
        /// Attribute name.
        name: String,
        /// Number of observations.
        n: usize,
        /// Share of entities where this attribute is present.
        share_present: f64,
    },
    /// Geographic attribute statistics.
    Geo {
        /// Attribute name.
        name: String,
        /// Number of observations.
        n: usize,
        /// Bounding box: `[min_lon, min_lat, max_lon, max_lat]`.
        bbox: [f64; 4],
        /// Centroid: `[lon, lat]`.
        centroid: [f64; 2],
    },
    /// Relationship attribute statistics.
    Relationship {
        /// Attribute name.
        name: String,
        /// Number of observations.
        n: usize,
        /// Number of distinct target entities.
        distinct: usize,
        /// Top 3 target entities with occurrence counts.
        top3: Vec<(String, usize)>,
    },
    /// Structured attribute statistics.
    Structured {
        /// Attribute name.
        name: String,
        /// Number of observations.
        n: usize,
        /// Share of entities where this attribute is present.
        share_present: f64,
    },
}

impl AttrStat {
    /// Return the attribute name.
    pub(crate) fn name(&self) -> &str {
        match self {
            AttrStat::Number { name, .. }
            | AttrStat::Boolean { name, .. }
            | AttrStat::DateTime { name, .. }
            | AttrStat::Category { name, .. }
            | AttrStat::Text { name, .. }
            | AttrStat::Geo { name, .. }
            | AttrStat::Relationship { name, .. }
            | AttrStat::Structured { name, .. } => name,
        }
    }
}

/// The complete profile of an entity type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Profile {
    /// Entity type name.
    pub entity_type: String,
    /// Total count of entities if known.
    pub count: Option<u64>,
    /// The profile basis.
    pub basis: Basis,
    /// When the profile was computed.
    pub computed: chrono::DateTime<chrono::Utc>,
    /// Range of `observedAt` timestamps if present: `(min, max)`.
    pub observed_at: Option<(String, String)>,
    /// Top profiled attributes.
    pub attributes: Vec<AttrStat>,
    /// Names of attributes beyond the top ones.
    pub other_attributes: Vec<String>,
    /// Sample entities.
    pub samples: Vec<SampleRow>,
}

impl Profile {
    /// Convert the `Profile` to a JSON structure for tool `structuredContent`.
    pub(crate) fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("type".into(), json!(self.entity_type));
        if let Some(c) = self.count {
            map.insert("count".into(), json!(c));
        }
        map.insert(
            "basis".into(),
            json!({
                "loaded": self.basis.loaded,
                "total": self.basis.total,
                "reason": self.basis.reason.as_str(),
                "skipped": self.basis.skipped,
            }),
        );
        map.insert("computed".into(), json!(self.computed.to_rfc3339()));

        if self.basis.reason == BasisReason::Tight {
            map.insert(
                "unavailable".into(),
                json!("summary unavailable now, server busy"),
            );
            return Value::Object(map);
        }

        if let Some((min, max)) = &self.observed_at {
            map.insert(
                "observedAt".into(),
                json!({
                    "min": min,
                    "max": max,
                }),
            );
        }

        let mut attrs_json = serde_json::Map::new();
        for attr in &self.attributes {
            attrs_json.insert(attr.name().to_string(), attr_to_json(attr));
        }
        map.insert("attributes".into(), Value::Object(attrs_json));

        if !self.other_attributes.is_empty() {
            map.insert("otherAttributes".into(), json!(self.other_attributes));
        }

        let samples_json: Vec<Value> = self
            .samples
            .iter()
            .map(|s| {
                let mut row = serde_json::Map::new();
                row.insert("id".into(), json!(s.id));
                if let Some(name) = &s.name {
                    row.insert("name".into(), json!(name));
                }
                for (k, v) in &s.values {
                    row.insert(k.clone(), json!(v));
                }
                Value::Object(row)
            })
            .collect();
        map.insert("samples".into(), Value::Array(samples_json));

        Value::Object(map)
    }
}

/// A number attribute's statistics as JSON; any other kind is `null`.
fn number_to_json(attr: &AttrStat) -> Value {
    let AttrStat::Number {
        n,
        nulls,
        min,
        max,
        mean,
        median,
        stddev,
        p10,
        p90,
        hist,
        outliers,
        unit_code,
        top3,
        ..
    } = attr
    else {
        return Value::Null;
    };
    let (n, nulls, min, max, mean, median, stddev, p10, p90, outliers) = (
        *n, *nulls, *min, *max, *mean, *median, *stddev, *p10, *p90, *outliers,
    );
    let unit_code = unit_code.as_deref();
    let top3 = top3.as_deref();
    let hist_json: Vec<Value> = hist
        .iter()
        .map(|(s, e, c)| json!({"start": s, "end": e, "count": c}))
        .collect();
    let mut map = serde_json::Map::new();
    map.insert("kind".into(), json!("number"));
    map.insert("n".into(), json!(n));
    map.insert("nulls".into(), json!(nulls));
    map.insert("min".into(), json!(min));
    map.insert("max".into(), json!(max));
    map.insert("mean".into(), json!(mean));
    map.insert("median".into(), json!(median));
    map.insert("stddev".into(), json!(stddev));
    map.insert("p10".into(), json!(p10));
    map.insert("p90".into(), json!(p90));
    map.insert("hist".into(), Value::Array(hist_json));
    map.insert("outliers".into(), json!(outliers));
    if let Some(u) = unit_code {
        map.insert("unitCode".into(), json!(u));
    }
    if let Some(top) = top3 {
        let top_json: Vec<Value> = top
            .iter()
            .map(|(v, c)| json!({"value": v, "count": c}))
            .collect();
        map.insert("top3".into(), Value::Array(top_json));
    }
    Value::Object(map)
}

fn category_to_json(
    n: usize,
    distinct: usize,
    top5: &[(String, usize, f64)],
    other: usize,
) -> Value {
    let top_json: Vec<Value> = top5
        .iter()
        .map(|(v, c, pct)| json!({"value": v, "count": c, "pct": pct}))
        .collect();
    json!({
        "kind": "category",
        "n": n,
        "distinct": distinct,
        "top5": top_json,
        "other": other,
    })
}

fn relationship_to_json(n: usize, distinct: usize, top3: &[(String, usize)]) -> Value {
    let top_json: Vec<Value> = top3
        .iter()
        .map(|(v, c)| json!({"target": v, "count": c}))
        .collect();
    json!({
        "kind": "relationship",
        "n": n,
        "distinct": distinct,
        "top3": top_json,
    })
}

fn attr_to_json(attr: &AttrStat) -> Value {
    match attr {
        AttrStat::Number { .. } => number_to_json(attr),
        AttrStat::Boolean { n, true_share, .. } => json!({
            "kind": "boolean",
            "n": n,
            "trueShare": true_share,
        }),
        AttrStat::DateTime {
            n,
            min,
            max,
            median,
            ..
        } => json!({
            "kind": "dateTime",
            "n": n,
            "min": min,
            "max": max,
            "median": median,
        }),
        AttrStat::Category {
            n,
            distinct,
            top5,
            other,
            ..
        } => category_to_json(*n, *distinct, top5, *other),
        AttrStat::Text {
            n, share_present, ..
        } => json!({
            "kind": "text",
            "n": n,
            "sharePresent": share_present,
        }),
        AttrStat::Geo {
            n, bbox, centroid, ..
        } => json!({
            "kind": "geo",
            "n": n,
            "bbox": bbox,
            "centroid": centroid,
        }),
        AttrStat::Relationship {
            n, distinct, top3, ..
        } => relationship_to_json(*n, *distinct, top3),
        AttrStat::Structured {
            n, share_present, ..
        } => json!({
            "kind": "structured",
            "n": n,
            "sharePresent": share_present,
        }),
    }
}

fn parse_rfc3339_or_date(s: &str) -> Option<String> {
    if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
        if let Some(date_part) = s.get(..10) {
            return Some(date_part.to_owned());
        }
        return Some(s.to_owned());
    }
    if chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() {
        return Some(s.to_owned());
    }
    None
}

fn extract_coords(val: &Value, out: &mut Vec<(f64, f64)>) {
    if let Value::Array(arr) = val {
        if arr.len() >= 2 && arr.first().is_some_and(Value::is_number) {
            let lon = arr.first().and_then(Value::as_f64);
            let lat = arr.get(1).and_then(Value::as_f64);
            if let (Some(x), Some(y)) = (lon, lat) {
                out.push((x, y));
                return;
            }
        }
        for item in arr {
            extract_coords(item, out);
        }
    }
}

fn to_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// Each attribute's values across the rows, by name.
type Presences<'a> = BTreeMap<String, Vec<&'a Value>>;

fn collect_presences_and_dates(rows: &[Value]) -> (Presences<'_>, Option<(String, String)>) {
    let mut attr_presences: Presences<'_> = BTreeMap::new();
    let mut observed_ats: Vec<String> = Vec::new();

    for row in rows {
        let Some(map) = row.as_object() else { continue };
        for (k, v) in map {
            if matches!(k.as_str(), "id" | "type" | "@context" | "scope") {
                continue;
            }
            attr_presences.entry(k.clone()).or_default().push(v);
            if let Some(obs) = v
                .as_object()
                .and_then(|o| o.get("observedAt"))
                .and_then(Value::as_str)
            {
                if let Some(d) = parse_rfc3339_or_date(obs) {
                    observed_ats.push(d);
                }
            }
        }
    }

    let observed_at = if observed_ats.is_empty() {
        None
    } else {
        observed_ats.sort();
        let min = observed_ats.first().cloned().unwrap_or_default();
        let max = observed_ats.last().cloned().unwrap_or_default();
        Some((min, max))
    };

    (attr_presences, observed_at)
}

fn rank_attributes(presences: &BTreeMap<String, Vec<&Value>>) -> (Vec<String>, Vec<String>) {
    let mut attr_names: Vec<(String, usize)> = presences
        .iter()
        .map(|(k, v)| (k.clone(), v.len()))
        .collect();
    attr_names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let top12: Vec<String> = attr_names.iter().take(12).map(|(k, _)| k.clone()).collect();
    let other: Vec<String> = attr_names.iter().skip(12).map(|(k, _)| k.clone()).collect();
    (top12, other)
}

fn build_sample_rows(
    rows: &[Value],
    sample_attrs: &[String],
    lang: Option<&str>,
) -> Vec<SampleRow> {
    let mut samples = Vec::new();
    for row in rows.iter().take(5) {
        let Some(map) = row.as_object() else { continue };
        let id = map
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let name = map.get("name").and_then(|v| resolve_str(v, lang)).map(|s| {
            if s.len() > 80 {
                let trimmed: String = s.chars().take(80).collect();
                trimmed
            } else {
                s
            }
        });

        let mut values = Vec::new();
        for key in sample_attrs {
            if let Some(val_node) = map.get(key) {
                let formatted = format_sample_value(val_node, lang);
                values.push((key.clone(), formatted));
            }
        }
        samples.push(SampleRow { id, name, values });
    }
    samples
}

/// Compute `Profile` over normalized NGSI-LD entities.
pub(crate) fn profile(
    rows: &[Value],
    total: Option<u64>,
    basis: Basis,
    lang: Option<&str>,
) -> Profile {
    let entity_type = rows
        .first()
        .and_then(|r| r.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();

    let computed = chrono::Utc::now();
    let total_loaded = rows.len();

    if basis.reason == BasisReason::Tight || total_loaded == 0 {
        return Profile {
            entity_type,
            count: total,
            basis,
            computed,
            observed_at: None,
            attributes: Vec::new(),
            other_attributes: Vec::new(),
            samples: Vec::new(),
        };
    }

    let (attr_presences, observed_at) = collect_presences_and_dates(rows);
    let (top12, other_attributes) = rank_attributes(&attr_presences);

    let mut attributes = Vec::new();
    for attr_name in &top12 {
        if let Some(values) = attr_presences.get(attr_name) {
            if let Some(stat) = analyze_attribute(attr_name, values, total_loaded, lang) {
                attributes.push(stat);
            }
        }
    }

    let sample_attrs: Vec<String> = attributes
        .iter()
        .filter(|a| {
            !matches!(
                a,
                AttrStat::Geo { .. } | AttrStat::Text { .. } | AttrStat::Structured { .. }
            )
        })
        .take(5)
        .map(|a| a.name().to_string())
        .collect();

    let samples = build_sample_rows(rows, &sample_attrs, lang);

    Profile {
        entity_type,
        count: total,
        basis,
        computed,
        observed_at,
        attributes,
        other_attributes,
        samples,
    }
}

fn resolve_str(node: &Value, lang: Option<&str>) -> Option<String> {
    let obj = node.as_object()?;
    if obj.get("type").and_then(Value::as_str) == Some("LanguageProperty") {
        if let Some(lang_map) = obj.get("languageMap").and_then(Value::as_object) {
            if let Some(target) = lang {
                if let Some(s) = lang_map.get(target).and_then(Value::as_str) {
                    return Some(s.to_string());
                }
            }
            if let Some((_, v)) = lang_map.iter().next() {
                if let Some(s) = v.as_str() {
                    return Some(s.to_string());
                }
            }
        }
    }
    if let Some(s) = obj.get("value").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    None
}

fn format_sample_value(node: &Value, lang: Option<&str>) -> String {
    if let Some(obj) = node.as_object() {
        if obj.get("type").and_then(Value::as_str) == Some("Relationship") {
            if let Some(obj_uri) = obj.get("object").and_then(Value::as_str) {
                return obj_uri.to_string();
            }
        }
        if let Some(s) = resolve_str(node, lang) {
            if s.len() > 80 {
                let trimmed: String = s.chars().take(80).collect();
                return trimmed;
            }
            return s;
        }
        if let Some(num) = obj.get("value").and_then(Value::as_f64) {
            return fmt_num(num);
        }
        if let Some(b) = obj.get("value").and_then(Value::as_bool) {
            return b.to_string();
        }
    }
    let text = node.to_string();
    if text.len() > 80 {
        let trimmed: String = text.chars().take(80).collect();
        trimmed
    } else {
        text
    }
}

fn analyze_relationship(name: &str, values: &[&Value]) -> Option<AttrStat> {
    let n = values.len();
    let rel_count = values
        .iter()
        .filter(|v| {
            v.as_object()
                .and_then(|o| o.get("type"))
                .and_then(Value::as_str)
                == Some("Relationship")
        })
        .count();
    if rel_count <= n.checked_div(2).unwrap_or(0) {
        return None;
    }

    let mut target_counts: BTreeMap<String, usize> = BTreeMap::new();
    for v in values {
        if let Some(obj) = v
            .as_object()
            .and_then(|o| o.get("object"))
            .and_then(Value::as_str)
        {
            let count = target_counts.entry(obj.to_string()).or_default();
            *count = count.saturating_add(1);
        }
    }
    let distinct = target_counts.len();
    let mut sorted: Vec<(String, usize)> = target_counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let top3 = sorted.into_iter().take(3).collect();
    Some(AttrStat::Relationship {
        name: name.to_string(),
        n,
        distinct,
        top3,
    })
}

fn analyze_geo(name: &str, values: &[&Value]) -> Option<AttrStat> {
    let n = values.len();
    let geo_count = values
        .iter()
        .filter(|v| {
            v.as_object().is_some_and(|o| {
                o.get("type").and_then(Value::as_str) == Some("GeoProperty")
                    || o.get("value").is_some_and(|val| {
                        val.as_object()
                            .is_some_and(|vo| vo.contains_key("coordinates"))
                    })
            })
        })
        .count();
    if geo_count <= n.checked_div(2).unwrap_or(0) {
        return None;
    }

    let mut all_coords = Vec::new();
    let mut entity_centres = Vec::new();
    for v in values {
        let mut coords = Vec::new();
        if let Some(geom) = v.as_object().and_then(|o| o.get("value")) {
            extract_coords(geom, &mut coords);
        }
        if !coords.is_empty() {
            let mut min_x = f64::INFINITY;
            let mut max_x = f64::NEG_INFINITY;
            let mut min_y = f64::INFINITY;
            let mut max_y = f64::NEG_INFINITY;
            for (x, y) in &coords {
                min_x = min_x.min(*x);
                max_x = max_x.max(*x);
                min_y = min_y.min(*y);
                max_y = max_y.max(*y);
                all_coords.push((*x, *y));
            }
            entity_centres.push((f64::midpoint(min_x, max_x), f64::midpoint(min_y, max_y)));
        }
    }
    let (bbox, centroid) = if all_coords.is_empty() {
        ([0.0, 0.0, 0.0, 0.0], [0.0, 0.0])
    } else {
        let mut min_x = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for (x, y) in &all_coords {
            min_x = min_x.min(*x);
            max_x = max_x.max(*x);
            min_y = min_y.min(*y);
            max_y = max_y.max(*y);
        }
        let count = to_f64(entity_centres.len());
        let c_x = entity_centres.iter().map(|(x, _)| *x).sum::<f64>() / count;
        let c_y = entity_centres.iter().map(|(_, y)| *y).sum::<f64>() / count;
        ([min_x, min_y, max_x, max_y], [c_x, c_y])
    };
    Some(AttrStat::Geo {
        name: name.to_string(),
        n,
        bbox,
        centroid,
    })
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return *sorted.first().unwrap_or(&0.0);
    }
    let n_sub_1 = sorted.len().saturating_sub(1);
    let idx = p * to_f64(n_sub_1);
    let k = (0..=n_sub_1)
        .rev()
        .find(|&i| to_f64(i) <= idx)
        .unwrap_or(0)
        .min(n_sub_1);
    let f = idx - to_f64(k);
    let current = *sorted.get(k).unwrap_or(&0.0);
    if let Some(&next) = sorted.get(k.saturating_add(1)) {
        current + f * (next - current)
    } else {
        current
    }
}

fn compute_histogram(
    numbers: &[f64],
    min: f64,
    max: f64,
    num_count: usize,
) -> Vec<(f64, f64, usize)> {
    let mut hist = Vec::new();
    if max > min {
        let width = (max - min) / 5.0;
        let mut counts = [0usize; 5];
        for &x in numbers {
            let b = (0..5)
                .rev()
                .find(|&i| x >= min + to_f64(i) * width)
                .unwrap_or(0)
                .min(4);
            if let Some(c) = counts.get_mut(b) {
                *c = c.saturating_add(1);
            }
        }
        for i in 0..5 {
            let start = min + to_f64(i) * width;
            let end = if i == 4 {
                max
            } else {
                min + to_f64(i.saturating_add(1)) * width
            };
            let cnt = *counts.get(i).unwrap_or(&0);
            hist.push((start, end, cnt));
        }
    } else {
        hist.push((min, max, num_count));
    }
    hist
}

fn analyze_number(name: &str, values: &[&Value], total_loaded: usize) -> Option<AttrStat> {
    let n = values.len();
    let mut numbers = Vec::new();
    let mut integer_only = true;
    let mut unit_codes: Vec<String> = Vec::new();

    for v in values {
        if let Some(obj) = v.as_object() {
            if let Some(u) = obj.get("unitCode").and_then(Value::as_str) {
                if !u.is_empty() {
                    unit_codes.push(u.to_string());
                }
            }
            if let Some(num) = obj.get("value").and_then(Value::as_f64) {
                if num.fract() != 0.0 {
                    integer_only = false;
                }
                numbers.push(num);
            }
        }
    }

    if numbers.is_empty() || numbers.len() < n.checked_div(2).unwrap_or(0) {
        return None;
    }

    numbers.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let num_count = numbers.len();
    let nulls = total_loaded.saturating_sub(num_count);
    let min = *numbers.first().unwrap_or(&0.0);
    let max = *numbers.last().unwrap_or(&0.0);
    let mean = numbers.iter().sum::<f64>() / to_f64(num_count);
    let median = if num_count % 2 == 1 {
        *numbers
            .get(num_count.checked_div(2).unwrap_or(0))
            .unwrap_or(&0.0)
    } else {
        let mid = num_count.checked_div(2).unwrap_or(0);
        let left = *numbers.get(mid.saturating_sub(1)).unwrap_or(&0.0);
        let right = *numbers.get(mid).unwrap_or(&0.0);
        f64::midpoint(left, right)
    };

    let stddev = if num_count <= 1 {
        0.0
    } else {
        let var = numbers.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
            / to_f64(num_count.saturating_sub(1));
        var.sqrt()
    };

    let p10 = percentile(&numbers, 0.10);
    let p90 = percentile(&numbers, 0.90);
    let q1 = percentile(&numbers, 0.25);
    let q3 = percentile(&numbers, 0.75);
    let iqr = q3 - q1;
    let lower = q1 - 1.5 * iqr;
    let upper = q3 + 1.5 * iqr;
    let outliers = numbers.iter().filter(|&&x| x < lower || x > upper).count();

    let hist = compute_histogram(&numbers, min, max, num_count);
    let unit_code = unit_codes
        .first()
        .filter(|first| unit_codes.iter().all(|u| u == *first))
        .cloned();

    let top3 = if integer_only {
        let mut freq: BTreeMap<i64, usize> = BTreeMap::new();
        for &num in &numbers {
            if let Ok(k) = format!("{num:.0}").parse::<i64>() {
                let c = freq.entry(k).or_default();
                *c = c.saturating_add(1);
            }
        }
        let mut sorted: Vec<(i64, usize)> = freq.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Some(sorted.into_iter().take(3).collect())
    } else {
        None
    };

    Some(AttrStat::Number {
        name: name.to_string(),
        n: num_count,
        nulls,
        min,
        max,
        mean,
        median,
        stddev,
        p10,
        p90,
        hist,
        outliers,
        unit_code,
        top3,
    })
}

fn analyze_boolean(name: &str, values: &[&Value]) -> Option<AttrStat> {
    let n = values.len();
    let mut bools = Vec::new();
    for v in values {
        if let Some(b) = v
            .as_object()
            .and_then(|o| o.get("value"))
            .and_then(Value::as_bool)
        {
            bools.push(b);
        }
    }
    if bools.is_empty() || bools.len() < n.checked_div(2).unwrap_or(0) {
        return None;
    }
    let true_count = bools.iter().filter(|&&b| b).count();
    let true_share = to_f64(true_count) / to_f64(bools.len());
    Some(AttrStat::Boolean {
        name: name.to_string(),
        n: bools.len(),
        true_share,
    })
}

fn analyze_datetime(name: &str, values: &[&Value]) -> Option<AttrStat> {
    let n = values.len();
    let mut dates = Vec::new();
    for v in values {
        let Some(obj) = v.as_object() else { continue };
        if let Some(s) = obj.get("value").and_then(Value::as_str) {
            if let Some(d) = parse_rfc3339_or_date(s) {
                dates.push(d);
                continue;
            }
        }
        if let Some(sub) = obj.get("value").and_then(Value::as_object) {
            if sub.get("@type").and_then(Value::as_str) == Some("DateTime") {
                if let Some(s) = sub.get("@value").and_then(Value::as_str) {
                    if let Some(d) = parse_rfc3339_or_date(s) {
                        dates.push(d);
                    }
                }
            }
        }
    }
    if dates.is_empty() || dates.len() < n.checked_div(2).unwrap_or(0) {
        return None;
    }
    dates.sort();
    let min = dates.first().cloned().unwrap_or_default();
    let max = dates.last().cloned().unwrap_or_default();
    let median = dates
        .get(dates.len().checked_div(2).unwrap_or(0))
        .cloned()
        .unwrap_or_default();
    Some(AttrStat::DateTime {
        name: name.to_string(),
        n: dates.len(),
        min,
        max,
        median,
    })
}

fn analyze_category_or_text(
    name: &str,
    values: &[&Value],
    total_loaded: usize,
    lang: Option<&str>,
) -> Option<AttrStat> {
    let n = values.len();
    let mut strings = Vec::new();
    for v in values {
        if let Some(s) = resolve_str(v, lang) {
            strings.push(s);
        }
    }
    if strings.is_empty() || strings.len() < n.checked_div(2).unwrap_or(0) {
        return None;
    }

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for s in &strings {
        let c = counts.entry(s.clone()).or_default();
        *c = c.saturating_add(1);
    }
    let distinct = counts.len();
    if distinct <= 20 && distinct <= strings.len().checked_div(2).unwrap_or(0) {
        let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut top5 = Vec::new();
        let mut other = 0usize;
        for (idx, (val, cnt)) in sorted.into_iter().enumerate() {
            if idx < 5 {
                let pct = (to_f64(cnt) / to_f64(strings.len())) * 100.0;
                top5.push((val, cnt, pct));
            } else {
                other = other.saturating_add(cnt);
            }
        }
        return Some(AttrStat::Category {
            name: name.to_string(),
            n: strings.len(),
            distinct,
            top5,
            other,
        });
    }

    Some(AttrStat::Text {
        name: name.to_string(),
        n: strings.len(),
        share_present: to_f64(strings.len()) / to_f64(total_loaded),
    })
}

fn analyze_attribute(
    name: &str,
    values: &[&Value],
    total_loaded: usize,
    lang: Option<&str>,
) -> Option<AttrStat> {
    let n = values.len();
    if n == 0 {
        return None;
    }
    if let Some(rel) = analyze_relationship(name, values) {
        return Some(rel);
    }
    if let Some(geo) = analyze_geo(name, values) {
        return Some(geo);
    }
    if let Some(num) = analyze_number(name, values, total_loaded) {
        return Some(num);
    }
    if let Some(b) = analyze_boolean(name, values) {
        return Some(b);
    }
    if let Some(dt) = analyze_datetime(name, values) {
        return Some(dt);
    }
    if let Some(cat) = analyze_category_or_text(name, values, total_loaded, lang) {
        return Some(cat);
    }
    Some(AttrStat::Structured {
        name: name.to_string(),
        n,
        share_present: to_f64(n) / to_f64(total_loaded),
    })
}

/// Format a float to up to 3 significant figures, or whole for integers.
pub(crate) fn fmt_num(v: f64) -> String {
    if v.is_nan() || v.is_infinite() {
        return "0".into();
    }
    if v.fract() == 0.0 {
        return format!("{v:.0}");
    }
    let abs = v.abs();
    if abs >= 99.95 {
        format!("{v:.0}")
    } else if abs >= 9.995 {
        format!("{v:.1}")
    } else if abs >= 0.9995 {
        format!("{v:.2}")
    } else if abs >= 0.09995 {
        format!("{v:.3}")
    } else {
        format!("{v:.4}")
    }
}

fn render_basis(basis: &Basis) -> String {
    let total_str = basis
        .total
        .map_or_else(|| "unknown".to_string(), |t| t.to_string());
    let skipped_str = if basis.skipped > 0 {
        format!(", {} skipped (>1MiB)", basis.skipped)
    } else {
        String::new()
    };
    match basis.reason {
        BasisReason::All => format!("{} of {total_str} (all){skipped_str}", basis.loaded),
        BasisReason::RowLimit => {
            format!("{} of {total_str} (first by id){skipped_str}", basis.loaded)
        }
        BasisReason::MemoryLimit => format!(
            "{} of {total_str} (stopped by memory limit){skipped_str}",
            basis.loaded
        ),
        BasisReason::Busy => {
            format!("{} of {total_str} (server busy){skipped_str}", basis.loaded)
        }
        BasisReason::Tight => "summary unavailable now, server busy".to_string(),
    }
}

/// A number attribute's line; any other kind renders nothing.
fn render_number_stat(attr: &AttrStat) -> String {
    let AttrStat::Number {
        name,
        n,
        nulls,
        min,
        max,
        mean,
        median,
        stddev,
        p10,
        p90,
        hist,
        outliers,
        unit_code,
        top3,
        ..
    } = attr
    else {
        return String::new();
    };
    let (n, nulls, min, max, mean, median, stddev, p10, p90, outliers) = (
        *n, *nulls, *min, *max, *mean, *median, *stddev, *p10, *p90, *outliers,
    );
    let unit_code = unit_code.as_deref();
    let top3 = top3.as_deref();
    let mut parts = vec![format!("n={n}")];
    if nulls > 0 {
        parts.push(format!("nulls={nulls}"));
    }
    parts.push(format!("min={}", fmt_num(min)));
    parts.push(format!("max={}", fmt_num(max)));
    parts.push(format!("mean={}", fmt_num(mean)));
    parts.push(format!("median={}", fmt_num(median)));
    parts.push(format!("sd={}", fmt_num(stddev)));
    parts.push(format!("p10={}", fmt_num(p10)));
    parts.push(format!("p90={}", fmt_num(p90)));

    let hist_parts: Vec<String> = hist
        .iter()
        .map(|(s, e, c)| format!("{}-{}:{c}", fmt_num(*s), fmt_num(*e)))
        .collect();
    let hist_joined = hist_parts.join(",");
    parts.push(format!("hist=[{hist_joined}]"));

    if outliers > 0 {
        parts.push(format!("outliers={outliers}"));
    }
    if let Some(u) = unit_code {
        parts.push(format!("unitCode={u}"));
    }
    if let Some(top) = top3 {
        let top_parts: Vec<String> = top.iter().map(|(v, c)| format!("{v}:{c}")).collect();
        let top_joined = top_parts.join(",");
        parts.push(format!("top=[{top_joined}]"));
    }
    let joined = parts.join(" ");
    format!("{name}: {joined}")
}

fn render_category_stat(
    name: &str,
    distinct: usize,
    top5: &[(String, usize, f64)],
    other: usize,
) -> String {
    let top_parts: Vec<String> = top5
        .iter()
        .map(|(v, c, pct)| format!("{v} {c} {pct:.0}%"))
        .collect();
    let top_joined = top_parts.join(", ");
    let mut s = format!("{name}: distinct={distinct} top=[{top_joined}]");
    if other > 0 {
        let _ = write!(s, " other={other}");
    }
    s
}

fn render_stat(stat: &AttrStat) -> String {
    match stat {
        AttrStat::Number { .. } => render_number_stat(stat),
        AttrStat::Boolean {
            name,
            n,
            true_share,
        } => {
            format!("{name}: n={n} true={:.0}%", true_share * 100.0)
        }
        AttrStat::DateTime {
            name,
            n,
            min,
            max,
            median,
        } => {
            format!("{name}: n={n} min={min} max={max} median={median}")
        }
        AttrStat::Category {
            name,
            distinct,
            top5,
            other,
            ..
        } => render_category_stat(name, *distinct, top5, *other),
        AttrStat::Text {
            name,
            n,
            share_present,
        }
        | AttrStat::Structured {
            name,
            n,
            share_present,
        } => {
            format!("{name}: n={n} present={:.0}%", share_present * 100.0)
        }
        AttrStat::Geo {
            name,
            n,
            bbox: [b0, b1, b2, b3],
            centroid: [c0, c1],
        } => {
            format!(
                "{name}: n={n} bbox=[{}, {}, {}, {}] centroid=[{}, {}]",
                fmt_num(*b0),
                fmt_num(*b1),
                fmt_num(*b2),
                fmt_num(*b3),
                fmt_num(*c0),
                fmt_num(*c1)
            )
        }
        AttrStat::Relationship {
            name,
            n,
            distinct,
            top3,
        } => {
            let top_parts: Vec<String> = top3
                .iter()
                .map(|(target, count)| format!("{target} {count}"))
                .collect();
            let joined = top_parts.join(", ");
            format!("{name}: n={n} distinct={distinct} top=[{joined}]")
        }
    }
}

fn render_internal(profile: &Profile, include_samples: bool, full_attrs_count: usize) -> String {
    let mut lines = Vec::new();

    let count_val = profile
        .count
        .unwrap_or_else(|| u64::try_from(profile.basis.loaded).unwrap_or(u64::MAX));
    let computed_str = profile.computed.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    if profile.basis.reason == BasisReason::Tight {
        lines.push(format!(
            "type={} count={count_val} basis={} computed={computed_str}",
            profile.entity_type,
            render_basis(&profile.basis)
        ));
        return lines.join("\n");
    }

    if profile.basis.loaded == 0 {
        // Rows that exist but did not load (too large, past the budget) are
        // not an empty type (T-4683): the count stays the broker's.
        if count_val == 0 {
            lines.push(format!(
                "type={} count=0 basis=0 of 0 (all) computed={computed_str}",
                profile.entity_type
            ));
            lines.push("no entities found".to_string());
        } else {
            lines.push(format!(
                "type={} count={count_val} basis={} computed={computed_str}",
                profile.entity_type,
                render_basis(&profile.basis)
            ));
            lines.push("no entity could be loaded for statistics now".to_string());
        }
        return lines.join("\n");
    }

    lines.push(format!(
        "type={} count={count_val} basis={} computed={computed_str}",
        profile.entity_type,
        render_basis(&profile.basis)
    ));

    if let Some((min, max)) = &profile.observed_at {
        lines.push(format!("observedAt: min={min} max={max}"));
    }

    let full_stats: Vec<&AttrStat> = profile.attributes.iter().take(full_attrs_count).collect();
    let mut demoted_names: Vec<String> = profile
        .attributes
        .iter()
        .skip(full_attrs_count)
        .map(|a| a.name().to_string())
        .collect();
    demoted_names.extend(profile.other_attributes.clone());

    for stat in full_stats {
        lines.push(render_stat(stat));
    }

    if !demoted_names.is_empty() {
        lines.push(format!("otherAttributes: {}", demoted_names.join(", ")));
    }

    if include_samples && !profile.samples.is_empty() {
        lines.push("sampleEntities:".to_string());
        for sample in &profile.samples {
            let mut parts = Vec::new();
            if let Some(name) = &sample.name {
                parts.push(format!("name=\"{name}\""));
            }
            for (k, v) in &sample.values {
                parts.push(format!("{k}={v}"));
            }
            let id = &sample.id;
            let joined = parts.join(", ");
            lines.push(format!("  {id} ({joined})"));
        }
    }

    lines.join("\n")
}

/// Render the `Profile` to a terse textual form clamped to 6,000 characters.
pub(crate) fn render(profile: &Profile) -> String {
    let mut include_samples = true;
    let mut full_attrs_count = profile.attributes.len();

    loop {
        let text = render_internal(profile, include_samples, full_attrs_count);
        if text.len() <= 6000 {
            return text;
        }
        if include_samples {
            include_samples = false;
            continue;
        }
        if full_attrs_count > 0 {
            full_attrs_count = full_attrs_count.saturating_sub(1);
            continue;
        }
        // Last resort: truncate lines cleanly
        let mut truncated = Vec::new();
        let mut cur_len = 0usize;
        for line in text.lines() {
            let next_len = cur_len.saturating_add(line.len()).saturating_add(1);
            if next_len > 6000 {
                break;
            }
            truncated.push(line);
            cur_len = next_len;
        }
        return truncated.join("\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge_fixture() -> Vec<Value> {
        (0u32..10u32)
            .map(|i| {
                let year =
                    1900usize.saturating_add(usize::try_from(i).unwrap_or(0).saturating_mul(10));
                let day = 10u32.saturating_add(i);
                let fi = f64::from(i);
                json!({
                    "id": format!("urn:ngsi-ld:Bridge:{i}"),
                    "type": "Bridge",
                    "yearBuilt": {
                        "type": "Property",
                        "value": year,
                        "unitCode": "ANN"
                    },
                    "isOperational": {
                        "type": "Property",
                        "value": i % 2 == 0
                    },
                    "category": {
                        "type": "Property",
                        "value": if i < 6 { "cestny" } else { "zeleznicny" }
                    },
                    "description": {
                        "type": "Property",
                        "value": format!("Long description of bridge number {i}")
                    },
                    "dateModified": {
                        "type": "Property",
                        "value": format!("2026-10-{day:02}T10:00:00Z")
                    },
                    "location": {
                        "type": "GeoProperty",
                        "value": {
                            "type": "Point",
                            "coordinates": [17.10 + fi * 0.01, 48.14 + fi * 0.01]
                        }
                    },
                    "connectedTo": {
                        "type": "Relationship",
                        "object": if i < 7 { "urn:ngsi-ld:Road:1" } else { "urn:ngsi-ld:Road:2" }
                    }
                })
            })
            .collect()
    }

    #[test]
    fn bridge_statistics_are_exact() {
        let rows = bridge_fixture();
        let basis = Basis {
            loaded: rows.len(),
            total: Some(10),
            reason: BasisReason::All,
            skipped: 0,
        };
        let p = profile(&rows, Some(10), basis, None);
        assert_eq!(p.entity_type, "Bridge");

        let year = p.attributes.iter().find(|a| a.name() == "yearBuilt");
        assert!(matches!(
            year,
            Some(AttrStat::Number { n: 10, min, max, mean, median, unit_code: Some(u), .. })
            if (*min - 1900.0).abs() < f64::EPSILON
                && (*max - 1990.0).abs() < f64::EPSILON
                && (*mean - 1945.0).abs() < f64::EPSILON
                && (*median - 1945.0).abs() < f64::EPSILON
                && u == "ANN"
        ));

        let cat = p.attributes.iter().find(|a| a.name() == "category");
        assert!(matches!(
            cat,
            Some(AttrStat::Category { distinct: 2, top5, .. })
            if top5.first().map(|t| (t.0.as_str(), t.1)) == Some(("cestny", 6))
        ));

        let rel = p.attributes.iter().find(|a| a.name() == "connectedTo");
        assert!(matches!(
            rel,
            Some(AttrStat::Relationship { top3, .. })
            if top3.first().map(|t| (t.0.as_str(), t.1)) == Some(("urn:ngsi-ld:Road:1", 7))
        ));

        let text = render(&p);
        assert!(text.contains("type=Bridge"));
        assert!(text.contains("yearBuilt:"));
        // Text values in description should not appear
        assert!(!text.contains("Long description"));
    }

    #[test]
    fn thirty_attributes_fit_within_budget() {
        let mut entity = json!({
            "id": "urn:ngsi-ld:Test:1",
            "type": "Test"
        });
        if let Some(map) = entity.as_object_mut() {
            for i in 0usize..30usize {
                map.insert(
                    format!("attr_{i:02}"),
                    json!({
                        "type": "Property",
                        "value": i.saturating_mul(10)
                    }),
                );
            }
        }
        let rows = vec![entity; 5];
        let basis = Basis {
            loaded: 5,
            total: Some(5),
            reason: BasisReason::All,
            skipped: 0,
        };
        let p = profile(&rows, Some(5), basis, None);
        assert_eq!(p.attributes.len(), 12);
        assert_eq!(p.other_attributes.len(), 18);

        let rendered = render(&p);
        assert!(rendered.len() <= 6000);
        assert!(rendered.contains("otherAttributes:"));
    }

    #[test]
    fn empty_rows_renders_without_panicking_or_nan() {
        let basis = Basis {
            loaded: 0,
            total: Some(0),
            reason: BasisReason::All,
            skipped: 0,
        };
        let p = profile(&[], Some(0), basis, None);
        let rendered = render(&p);
        assert!(rendered.contains("count=0"));
        assert!(!rendered.contains("NaN"));
        assert!(!rendered.contains("null"));
    }

    #[test]
    fn rows_that_exist_but_did_not_load_are_never_rendered_as_none() {
        let basis = Basis {
            loaded: 0,
            total: Some(40),
            reason: BasisReason::MemoryLimit,
            skipped: 3,
        };
        let mut p = profile(&[], Some(40), basis, None);
        p.entity_type = "Bridge".into();
        let text = render(&p);
        assert!(text.contains("count=40"), "{text}");
        assert!(!text.contains("count=0"), "{text}");
        assert!(!text.contains("no entities found"), "{text}");
    }
}
