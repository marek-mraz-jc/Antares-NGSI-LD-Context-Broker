// SPDX-License-Identifier: EUPL-1.2
//! One Entity as one RFC 5545 `VEVENT` through the view's field mapping
//! (`docs/analysis/ical-mapping.md`, platform API/02 §8b, T-4901).
//!
//! A mapping names, for each calendar field, the NGSI-LD attribute it comes from and at most the
//! remaps of a fixed allow-list (`firstOf`, `join`, `constant`, `language`, `date`, `timezone`,
//! `endInclusive`, `map`, `truncate`): data, never an expression. `UID` is the entity id and
//! `DTSTAMP` its `modifiedAt`, fixed. No mapping is the `sdm-event` preset: `startDate`,
//! `endDate` (the last day, so a date `DTEND` is the day after it), `name`, `description`,
//! `address`, `location`, `url` and `organizer` (`{name, url}` as `ORGANIZER;CN=`). An Entity
//! with no value for `DTSTART` or `SUMMARY`, a date it cannot read, a `DTEND` not after
//! `DTSTART`, a `GEO` off the globe or a `URL` that is not
//! http(s) is no `VEVENT` at all, and its reason is counted (EP-126).

use chrono::{DateTime, Days, NaiveDate, NaiveDateTime, NaiveTime, TimeZone as _, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The last segment of an expanded IRI, or the term itself.
#[must_use]
pub fn local(name: &str) -> &str {
    name.rsplit(['/', '#']).next().unwrap_or(name)
}

/// A point or a day of a calendar: a date, a UTC instant or a time without zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// A whole day (`2026-10-20`): `VALUE=DATE`.
    Date(NaiveDate),
    /// An instant with an offset, kept in UTC (`2026-10-20T18:00:00+02:00`).
    At(DateTime<Utc>),
    /// A wall-clock time without an offset (`2026-10-20T18:00:00`): a floating time.
    Floating(NaiveDateTime),
}

impl When {
    /// An RFC 3339 date-time, a date-time without offset or a date; anything else is `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if let Ok(t) = DateTime::parse_from_rfc3339(text) {
            return Some(Self::At(t.with_timezone(&Utc)));
        }
        if let Ok(t) = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f") {
            return Some(Self::Floating(t));
        }
        NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .ok()
            .map(Self::Date)
    }

    /// Where it starts on one time line: a date at its midnight, a floating time as if in UTC.
    #[must_use]
    pub fn key(self) -> NaiveDateTime {
        match self {
            Self::Date(d) => d.and_time(NaiveTime::MIN),
            Self::At(t) => t.naive_utc(),
            Self::Floating(t) => t,
        }
    }

    /// Just past what it covers: a date's next midnight, an instant itself.
    #[must_use]
    pub fn past(self) -> NaiveDateTime {
        match self {
            Self::Date(d) => next_day(d).and_time(NaiveTime::MIN),
            other => other.key(),
        }
    }

    /// `end` written in the value type of `self`, as RFC 5545 3.8.2.2 requires of `DTEND`: a
    /// date end is exclusive, so an inclusive last day becomes the day after it.
    fn end_like(self, end: Self, inclusive: bool) -> Self {
        match (self, end) {
            (Self::Date(_), Self::Date(e)) if inclusive => Self::Date(next_day(e)),
            (Self::Date(_), Self::Date(e)) => Self::Date(e),
            (Self::Date(_), other) => Self::Date(next_day(other.key().date())),
            (Self::At(_), Self::At(t)) => Self::At(t),
            (Self::At(_), other) => Self::At(other.past().and_utc()),
            (Self::Floating(_), Self::At(t)) => Self::Floating(t.naive_utc()),
            (Self::Floating(_), other) => Self::Floating(other.past()),
        }
    }

    /// As `mode` asks, a floating time read in `zone` when one is named; `None` for a wall
    /// time the zone skips.
    fn read(self, mode: DateMode, zone: Option<Tz>) -> Option<Self> {
        let wall = match (mode, self) {
            (DateMode::Datetime, Self::Date(d)) => Self::Floating(d.and_time(NaiveTime::MIN)),
            (_, other) => other,
        };
        let zoned = match (wall, zone) {
            (Self::Floating(t), Some(z)) => {
                Self::At(z.from_local_datetime(&t).earliest()?.to_utc())
            }
            (other, _) => other,
        };
        Some(match (mode, zoned) {
            (DateMode::Date, Self::At(t)) => {
                Self::Date(zone.map_or(t.date_naive(), |z| t.with_timezone(&z).date_naive()))
            }
            (DateMode::Date, Self::Floating(t)) => Self::Date(t.date()),
            (_, other) => other,
        })
    }

    /// The property value, with `;VALUE=DATE` for a date: `DTSTART;VALUE=DATE:20261020`.
    fn property(self, name: &str) -> String {
        match self {
            Self::Date(d) => format!("{name};VALUE=DATE:{}", d.format("%Y%m%d")),
            Self::At(t) => format!("{name}:{}", t.format("%Y%m%dT%H%M%SZ")),
            Self::Floating(t) => format!("{name}:{}", t.format("%Y%m%dT%H%M%S")),
        }
    }
}

fn next_day(d: NaiveDate) -> NaiveDate {
    d.checked_add_days(Days::new(1)).unwrap_or(d)
}

/// The `from`/`to` of a feed: an Event is in it when it starts before `to` and has not ended
/// before `from`; a bound left out does not narrow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Window {
    /// The first instant served: a date's midnight.
    pub from: Option<NaiveDateTime>,
    /// Just past the last instant served: a date's next midnight, so `to` is inclusive.
    pub to: Option<NaiveDateTime>,
}

impl Window {
    fn holds(&self, start: When, end: Option<When>) -> bool {
        let begins = start.key();
        let ends = end.map_or_else(|| start.past(), When::past).max(begins);
        self.to.is_none_or(|to| begins < to)
            && self.from.is_none_or(|from| ends > from || begins >= from)
    }
}

/// The `VEVENT` fields a mapping names, in the order a `VEVENT` writes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "SCREAMING-KEBAB-CASE")]
pub enum Field {
    /// §3.8.4.7, fixed: the entity id.
    Uid,
    /// §3.8.7.2, fixed: the entity's `modifiedAt`.
    Dtstamp,
    /// §3.8.2.4, required.
    Dtstart,
    /// §3.8.2.2.
    Dtend,
    /// §3.8.2.5.
    Duration,
    /// §3.8.1.12, required by the platform.
    Summary,
    /// §3.8.1.5.
    Description,
    /// §3.8.1.7.
    Location,
    /// §3.8.1.6.
    Geo,
    /// §3.8.4.6.
    Url,
    /// §3.8.1.2.
    Categories,
    /// §3.8.1.11.
    Status,
    /// §3.8.4.3.
    Organizer,
    /// §3.8.1.3, `PUBLIC` only.
    Class,
    /// §3.8.7.3; unmapped, the entity's `modifiedAt`.
    LastModified,
}

impl Field {
    /// Whether the field takes the remap `key`: the allow-list of API/02 §8b.
    fn allows(self, key: &str) -> bool {
        use Field as F;
        let text = matches!(
            self,
            F::Summary | F::Description | F::Location | F::Categories | F::Status
        );
        match key {
            "firstOf" => !matches!(self, F::Class),
            "join" => text && self != F::Status,
            "constant" => matches!(self, F::Class | F::Categories),
            "language" => text,
            "date" => matches!(self, F::Dtstart | F::Dtend | F::LastModified),
            "timezone" => matches!(self, F::Dtstart | F::Dtend),
            "endInclusive" => self == F::Dtend,
            "map" => matches!(self, F::Status | F::Categories),
            // `truncate`, the last key `compile_field` asks about
            _ => matches!(self, F::Summary | F::Description),
        }
    }
}

/// A named starting mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Smart Data Models `Event`, the default.
    SdmEvent,
    /// schema.org `Event`: `sdm-event` and `STATUS` from `eventStatus`.
    SchemaOrg,
}

/// How a date field reads its value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DateMode {
    /// As the value is.
    #[default]
    Auto,
    /// A whole day.
    Date,
    /// A date-time.
    Datetime,
}

/// The version of the `representations.ical` contract this broker reads (T-5207,
/// mapping-contract rule 1): a member added or given a new meaning takes the next one.
pub const MAPPING_VERSION: u64 = 1;

/// The view's `representations.ical` member, as the Portal writes it from the Endpoint's
/// `spec.representations.ical`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IcalMapping {
    /// The contract version the Portal wrote; `1` when absent.
    #[serde(default = "first_version")]
    pub version: u64,
    /// The NGSI-LD type the feed maps, by its local name; default `Event`.
    #[serde(rename = "type", default = "event")]
    pub entity_type: String,
    /// The preset the mapping starts from; with no `mapping` either, `sdm-event`.
    #[serde(default)]
    pub preset: Option<Preset>,
    /// The fields that replace the preset's, or the whole mapping without a preset.
    #[serde(default)]
    pub mapping: BTreeMap<Field, Source>,
}

fn event() -> String {
    "Event".to_owned()
}

fn first_version() -> u64 {
    1
}

/// The plan of a written `representations.ical` member, and whether it is degraded: a
/// mapping of a newer contract version than [`MAPPING_VERSION`] (the Portal released before
/// the broker) is served as its `type` over its `preset` (else `sdm-event`), the members this
/// broker reads alone, instead of failing the whole feed (T-5207, mapping-contract rule 1).
/// A mapping of this version is read strictly: the Portal checked it when it was saved.
///
/// # Errors
/// Why a mapping of this version does not compile.
///
/// ```
/// let newer = serde_json::json!({"version": 9, "type": "Concert", "rrule": "eventSchedule"});
/// let (plan, degraded) = antares_ical::mapping::plan_of(&newer).unwrap();
/// assert!(degraded);
/// assert_eq!(plan.entity_type(), "Concert");
/// ```
pub fn plan_of(written: &serde_json::Value) -> Result<(Plan, bool), String> {
    let version = written.get("version").and_then(serde_json::Value::as_u64);
    if version.is_some_and(|v| v > MAPPING_VERSION) {
        let entity_type = written
            .get("type")
            .and_then(serde_json::Value::as_str)
            .filter(|t| is_name(t))
            .map_or_else(event, str::to_owned);
        let preset = written
            .get("preset")
            .and_then(|p| serde_json::from_value::<Preset>(p.clone()).ok());
        let mapping = IcalMapping {
            version: MAPPING_VERSION,
            entity_type,
            preset,
            mapping: BTreeMap::new(),
        };
        tracing::warn!(version, "newer calendar mapping: preset served");
        return Plan::compile(&mapping).map(|p| (p, true));
    }
    let mapping: IcalMapping =
        serde_json::from_value(written.clone()).map_err(|e| e.to_string())?;
    Plan::compile(&mapping).map(|p| (p, false))
}

/// A field's source: an attribute path (`startDate`) or the object form.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Source {
    /// `DTSTART: startDate`.
    Attribute(String),
    /// `SUMMARY: {attribute: name, language: sk}`.
    Remap(Box<Remap>),
}

/// The object form of a [`Source`]: exactly one of `attribute`, `firstOf`, `join` and
/// `constant`, and the remaps its field allows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Remap {
    /// An attribute, or a dotted path into its value (`address.addressLocality`).
    #[serde(default)]
    pub attribute: Option<String>,
    /// The first of these attributes that has a value.
    #[serde(default)]
    pub first_of: Vec<String>,
    /// The values of these attributes that have one, joined by `separator`.
    #[serde(default)]
    pub join: Vec<String>,
    /// The `join` separator; default `", "`.
    #[serde(default)]
    pub separator: Option<String>,
    /// A fixed value.
    #[serde(default)]
    pub constant: Option<String>,
    /// Which text of a `LanguageProperty`: a two-letter code or `first` (the default).
    #[serde(default)]
    pub language: Option<String>,
    /// How a date field reads its value.
    #[serde(default)]
    pub date: Option<DateMode>,
    /// The IANA zone a floating time is read in; the instant is written in UTC.
    #[serde(default)]
    pub timezone: Option<String>,
    /// The source names the last day, so a date `DTEND` is the day after.
    #[serde(default)]
    pub end_inclusive: Option<bool>,
    /// Source value to calendar value.
    #[serde(default)]
    pub map: BTreeMap<String, String>,
    /// The calendar value of a source value `map` does not name.
    #[serde(default)]
    pub default: Option<String>,
    /// At most this many characters.
    #[serde(default)]
    pub truncate: Option<u32>,
}

impl Source {
    fn remap(&self) -> Remap {
        match self {
            Self::Attribute(name) => Remap {
                attribute: Some(name.clone()),
                ..Remap::default()
            },
            Self::Remap(remap) => (**remap).clone(),
        }
    }
}

/// The `STATUS` values RFC 5545 §3.8.1.11 gives a `VEVENT`.
const STATUS_VALUES: [&str; 3] = ["TENTATIVE", "CONFIRMED", "CANCELLED"];

/// The skip reasons, as `antares_surface_records_skipped_total{reason}` names them.
pub mod reason {
    /// No value for `DTSTART`.
    pub const MISSING_DTSTART: &str = "missing-dtstart";
    /// No value for `SUMMARY`.
    pub const MISSING_SUMMARY: &str = "missing-summary";
    /// A date field holds a value that is no date.
    pub const UNPARSABLE_DATE: &str = "unparsable-date";
    /// `DURATION` holds a value that is no RFC 5545 duration.
    pub const UNPARSABLE_DURATION: &str = "unparsable-duration";
    /// `DTEND` is not after `DTSTART`.
    pub const END_NOT_AFTER_START: &str = "dtend-not-after-dtstart";
    /// A `GEO` point outside -90..90 / -180..180.
    pub const GEO_OUT_OF_RANGE: &str = "geo-out-of-range";
    /// A `URL` that is not http(s) or holds white space.
    pub const URL_NOT_HTTP: &str = "url-not-http";
}

fn preset(preset: Preset) -> BTreeMap<Field, Remap> {
    let attribute = |name: &str| Remap {
        attribute: Some(name.to_owned()),
        ..Remap::default()
    };
    let mut fields: BTreeMap<_, _> = [
        (Field::Dtstart, "startDate"),
        (Field::Summary, "name"),
        (Field::Description, "description"),
        (Field::Location, "address"),
        (Field::Geo, "location"),
        (Field::Url, "url"),
        (Field::Organizer, "organizer"),
    ]
    .into_iter()
    .map(|(field, name)| (field, attribute(name)))
    .collect();
    // SDM and schema.org name the last day of a date range
    fields.insert(
        Field::Dtend,
        Remap {
            end_inclusive: Some(true),
            ..attribute("endDate")
        },
    );
    if preset == Preset::SchemaOrg {
        let map = [
            ("EventScheduled", "CONFIRMED"),
            ("EventRescheduled", "CONFIRMED"),
            ("EventMovedOnline", "CONFIRMED"),
            ("EventPostponed", "TENTATIVE"),
            ("EventCancelled", "CANCELLED"),
        ]
        .into_iter()
        .flat_map(|(from, to)| {
            [
                (from.to_owned(), to.to_owned()),
                (format!("https://schema.org/{from}"), to.to_owned()),
            ]
        })
        .collect();
        fields.insert(
            Field::Status,
            Remap {
                map,
                default: Some("CONFIRMED".to_owned()),
                ..attribute("eventStatus")
            },
        );
    }
    fields
}

/// An NGSI-LD name: a letter or `_`, then letters, digits and `_`, at most 128.
fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 128
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_path(path: &str) -> bool {
    path.split('.').count() <= 3 && path.split('.').all(is_name)
}

fn is_plain(value: &str, max: usize) -> bool {
    !value.is_empty() && value.chars().count() <= max && !value.chars().any(char::is_control)
}

/// One field compiled: its remap and its zone.
#[derive(Debug, Clone)]
struct Compiled {
    remap: Remap,
    zone: Option<Tz>,
}

/// A mapping checked and resolved over its preset: what [`Plan::vevent`] renders.
#[derive(Debug, Clone)]
pub struct Plan {
    entity_type: String,
    fields: BTreeMap<Field, Compiled>,
}

impl Default for Plan {
    /// The `sdm-event` preset over `Event`: the feed of a view that writes no mapping.
    fn default() -> Self {
        Self {
            entity_type: event(),
            fields: preset(Preset::SdmEvent)
                .into_iter()
                .map(|(field, remap)| (field, Compiled { remap, zone: None }))
                .collect(),
        }
    }
}

impl Plan {
    /// The mapping in force (the preset's fields with the written ones over them), checked as
    /// API/02 §8b shapes it; the model check is the platform's, at propose.
    ///
    /// # Errors
    /// The first reason the mapping cannot be rendered.
    pub fn compile(mapping: &IcalMapping) -> Result<Self, String> {
        if mapping.version != MAPPING_VERSION {
            return Err(format!(
                "version {} is not one this broker reads ({MAPPING_VERSION})",
                mapping.version
            ));
        }
        if !is_name(&mapping.entity_type) {
            return Err(format!(
                "type {:?} is not an NGSI-LD type name",
                mapping.entity_type
            ));
        }
        let base = match (mapping.preset, mapping.mapping.is_empty()) {
            (Some(p), _) => preset(p),
            (None, true) => preset(Preset::SdmEvent),
            (None, false) => BTreeMap::new(),
        };
        let mut fields = BTreeMap::new();
        for (field, remap) in base {
            fields.insert(field, Compiled { remap, zone: None });
        }
        for (field, source) in &mapping.mapping {
            fields.insert(*field, compile_field(*field, source.remap())?);
        }
        for (field, name) in [(Field::Dtstart, "DTSTART"), (Field::Summary, "SUMMARY")] {
            if !fields.contains_key(&field) {
                return Err(format!("the mapping maps no {name}"));
            }
        }
        if fields.contains_key(&Field::Dtend) && fields.contains_key(&Field::Duration) {
            return Err("the mapping maps both DTEND and DURATION".to_owned());
        }
        Ok(Self {
            entity_type: mapping.entity_type.clone(),
            fields,
        })
    }

    /// The type the feed maps, by its local name.
    #[must_use]
    pub fn entity_type(&self) -> &str {
        &self.entity_type
    }
}

fn compile_field(field: Field, remap: Remap) -> Result<Compiled, String> {
    let fail = |why: &str| Err(format!("{field:?}: {why}"));
    if matches!(field, Field::Uid | Field::Dtstamp) {
        return fail("fixed by the broker, never mapped");
    }
    let sources = [
        remap.attribute.is_some(),
        !remap.first_of.is_empty(),
        !remap.join.is_empty(),
        remap.constant.is_some(),
    ];
    if sources.iter().filter(|on| **on).count() != 1 {
        return fail("needs exactly one of attribute, firstOf, join or constant");
    }
    let used = [
        ("firstOf", !remap.first_of.is_empty()),
        ("join", !remap.join.is_empty() || remap.separator.is_some()),
        ("constant", remap.constant.is_some()),
        ("language", remap.language.is_some()),
        ("date", remap.date.is_some()),
        ("timezone", remap.timezone.is_some()),
        ("endInclusive", remap.end_inclusive.is_some()),
        ("map", !remap.map.is_empty() || remap.default.is_some()),
        ("truncate", remap.truncate.is_some()),
    ];
    if let Some((key, _)) = used.iter().find(|(key, on)| *on && !field.allows(key)) {
        return fail(&format!("does not take `{key}`"));
    }
    let paths = remap
        .attribute
        .iter()
        .chain(&remap.first_of)
        .chain(&remap.join);
    if let Some(bad) = paths.clone().find(|p| !is_path(p)) {
        return fail(&format!("{bad:?} is not an attribute path"));
    }
    let plain = remap.separator.iter().all(|s| is_plain(s, 10))
        && remap.constant.iter().all(|c| is_plain(c, 100))
        && remap
            .map
            .iter()
            .all(|(from, to)| is_plain(from, 200) && is_plain(to, 100))
        && remap.default.iter().all(|d| is_plain(d, 100));
    if !plain {
        return fail("a separator, constant or map value is too long or holds control characters");
    }
    if field == Field::Class && remap.constant.as_deref() != Some("PUBLIC") {
        return fail("takes only `constant: PUBLIC`");
    }
    let status = remap.map.values().chain(&remap.default);
    if field == Field::Status && status.clone().any(|v| !STATUS_VALUES.contains(&v.as_str())) {
        return fail("takes TENTATIVE, CONFIRMED or CANCELLED");
    }
    if let Some(language) = &remap.language {
        let code = language.len() == 2 && language.chars().all(|c| c.is_ascii_lowercase());
        if !code && language != "first" {
            return fail("`language` is a two-letter code or `first`");
        }
    }
    if remap.truncate.is_some_and(|n| n == 0 || n > 10_000) {
        return fail("`truncate` is 1 to 10000");
    }
    let zone = match &remap.timezone {
        None => None,
        Some(name) => Some(
            name.parse::<Tz>()
                .map_err(|_| format!("{field:?}: {name:?} is not an IANA zone"))?,
        ),
    };
    Ok(Compiled { remap, zone })
}

/// An attribute's instance: the default instance of a multi-instance one, else the first.
fn instance<'a>(entity: &'a Value, name: &str) -> Option<&'a Value> {
    let (_, member) = entity
        .as_object()?
        .iter()
        .find(|(key, _)| local(key) == name)?;
    match member {
        Value::Array(all) => all
            .iter()
            .find(|i| i.get("datasetId").is_none())
            .or_else(|| all.first()),
        one => Some(one),
    }
}

/// A Property's `value` (a typed literal's `@value`, a `VocabProperty`'s `vocab`), a
/// `LanguageProperty`'s text in `language` (`first` or none: its first), a `GeoProperty`'s
/// geometry.
fn value_of<'a>(instance: &'a Value, language: Option<&str>) -> Option<&'a Value> {
    if let Some(map) = instance.get("languageMap").and_then(Value::as_object) {
        return match language.filter(|l| *l != "first") {
            Some(code) => map.get(code),
            None => map.values().next(),
        };
    }
    let value = instance
        .get("value")
        .or_else(|| instance.get("vocab"))
        .unwrap_or(instance);
    Some(value.get("@value").unwrap_or(value))
}

/// The value at `path`: an attribute, then members of its value, or its sub-attributes.
fn at<'a>(entity: &'a Value, path: &str, language: Option<&str>) -> Option<&'a Value> {
    let mut names = path.split('.');
    let mut holder = instance(entity, names.next().unwrap_or(path))?;
    let mut value = value_of(holder, language)?;
    for name in names {
        if let Some(inner) = value.get(name) {
            value = inner;
        } else {
            holder = holder.get(name)?;
            value = value_of(holder, language)?;
        }
    }
    Some(value)
}

/// A value as non-empty text: a string trimmed, a number or a boolean.
fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_owned()).filter(|t| !t.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A CAL-ADDRESS: a `mailto:` or http(s) URI as given, an e-mail address as `mailto:`, else none.
fn cal_address(value: String) -> Option<String> {
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    if ["mailto:", "https://", "http://"]
        .iter()
        .any(|s| value.starts_with(s))
    {
        Some(value)
    } else if value.contains('@') && !value.contains(':') {
        Some(format!("mailto:{value}"))
    } else {
        None
    }
}

/// RFC 5545 3.2 param-value, always quoted so `,` `;` `:` stay literal; a `"`, a `^` and a
/// newline are RFC 6868 caret escapes, the other control characters are dropped.
fn param(text: &str) -> String {
    let mut out = String::with_capacity(text.len().saturating_add(2));
    out.push('"');
    for c in text.chars() {
        match c {
            '^' => out.push_str("^^"),
            '"' => out.push_str("^'"),
            '\n' => out.push_str("^n"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A structured postal address as one line: street, postal code, locality and country.
fn address(value: &Value) -> Option<String> {
    let parts: Vec<String> = [
        "streetAddress",
        "postalCode",
        "addressLocality",
        "addressCountry",
    ]
    .iter()
    .filter_map(|k| value.get(k).and_then(text))
    .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// RFC 5545 3.3.11 TEXT: backslash, semicolon, comma and newline escaped; the other control
/// characters, which TEXT excludes, dropped.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// RFC 5545 3.1: a content line of at most 75 octets, continued after CRLF and one space, cut
/// between characters and never inside one.
pub fn fold(line: &str, out: &mut String) {
    let mut width = 0usize;
    for c in line.chars() {
        let n = c.len_utf8();
        if width.saturating_add(n) > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width = width.saturating_add(n);
    }
    out.push_str("\r\n");
}

/// RFC 5545 3.3.6 `dur-value`: `P`, then weeks, or days and a time of hours, minutes and
/// seconds, an optional sign before.
fn is_duration(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    let Some(body) = body.strip_prefix('P') else {
        return false;
    };
    // each unit at most once, in the order `allowed` lists them, after at least one digit
    let units = |part: &str, allowed: &str| -> bool {
        let (mut rest, mut digits, mut any) = (allowed, 0usize, false);
        for c in part.chars() {
            if c.is_ascii_digit() {
                digits = digits.saturating_add(1);
                continue;
            }
            match rest.find(c) {
                Some(i) if digits > 0 => rest = rest.get(i.saturating_add(1)..).unwrap_or_default(),
                _ => return false,
            }
            (digits, any) = (0, true);
        }
        digits == 0 && any
    };
    match body.split_once('T') {
        Some((days, time)) => (days.is_empty() || units(days, "D")) && units(time, "HMS"),
        None => units(body, "W") || units(body, "D"),
    }
}

/// A source value through a field's `map`: its entry, else `default`, else itself when the
/// field maps nothing.
fn mapped(remap: &Remap, value: String) -> Option<String> {
    if remap.map.is_empty() && remap.default.is_none() {
        return Some(value);
    }
    remap.map.get(&value).or(remap.default.as_ref()).cloned()
}

/// What one Entity is in a feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Its `VEVENT`, folded, each line ending in CRLF.
    Event(String),
    /// A valid Event outside the window.
    Outside,
    /// No valid `VEVENT`: the [`reason`].
    Skipped(&'static str),
}

/// A field's value with the reason to skip when the entity holds one that does not read.
type Read<T> = Result<Option<T>, &'static str>;

impl Plan {
    fn field(&self, field: Field) -> Option<&Compiled> {
        self.fields.get(&field)
    }

    /// The field's text: its constant, the first source path holding text, or the joined ones.
    fn text(&self, entity: &Value, field: Field) -> Option<String> {
        let c = self.field(field)?;
        let remap = &c.remap;
        let language = remap.language.as_deref();
        let one = |path: &String| {
            at(entity, path, language).and_then(|v| match field {
                Field::Location if v.is_object() => address(v),
                _ => text(v),
            })
        };
        let value = if let Some(constant) = &remap.constant {
            Some(constant.clone())
        } else if remap.join.is_empty() {
            remap.attribute.iter().chain(&remap.first_of).find_map(one)
        } else {
            let parts: Vec<String> = remap.join.iter().filter_map(one).collect();
            let separator = remap.separator.as_deref().unwrap_or(", ");
            (!parts.is_empty()).then(|| parts.join(separator))
        }?;
        Some(match remap.truncate {
            Some(n) => value
                .chars()
                .take(usize::try_from(n).unwrap_or(usize::MAX))
                .collect(),
            None => value,
        })
    }

    /// A date field read: absent, a date, or unreadable.
    fn when(&self, entity: &Value, field: Field) -> Read<When> {
        let Some(c) = self.field(field) else {
            return Ok(None);
        };
        let Some(raw) = self.text(entity, field) else {
            return Ok(None);
        };
        When::parse(&raw)
            .and_then(|w| w.read(c.remap.date.unwrap_or_default(), c.zone))
            .map(Some)
            .ok_or(reason::UNPARSABLE_DATE)
    }

    fn end(&self, entity: &Value, start: When) -> Read<When> {
        let Some(end) = self.when(entity, Field::Dtend)? else {
            return Ok(None);
        };
        let inclusive = self
            .field(Field::Dtend)
            .and_then(|c| c.remap.end_inclusive)
            .unwrap_or(false);
        let end = start.end_like(end, inclusive);
        if end.key() > start.key() {
            Ok(Some(end))
        } else {
            Err(reason::END_NOT_AFTER_START)
        }
    }

    /// The `GEO` of a Point: latitude first (RFC 5545 3.8.1.6); another geometry has none.
    fn geo(&self, entity: &Value) -> Read<String> {
        let Some(c) = self.field(Field::Geo) else {
            return Ok(None);
        };
        let paths = c.remap.attribute.iter().chain(&c.remap.first_of);
        let Some(geometry) = paths
            .filter_map(|p| at(entity, p, None))
            .find(|g| g.is_object())
        else {
            return Ok(None);
        };
        if geometry.get("type").and_then(Value::as_str) != Some("Point") {
            return Ok(None);
        }
        let coordinates = geometry.get("coordinates").and_then(Value::as_array);
        let number = |i: usize| coordinates.and_then(|c| c.get(i)).and_then(Value::as_f64);
        let (Some(lon), Some(lat)) = (number(0), number(1)) else {
            return Ok(None);
        };
        if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
            Ok(Some(format!("GEO:{lat};{lon}")))
        } else {
            Err(reason::GEO_OUT_OF_RANGE)
        }
    }

    /// An http(s) `URL` without white space or control characters.
    fn url(&self, entity: &Value) -> Read<String> {
        let Some(url) = self.text(entity, Field::Url) else {
            return Ok(None);
        };
        let web = url.starts_with("https://") || url.starts_with("http://");
        if web && !url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            Ok(Some(format!("URL:{url}")))
        } else {
            Err(reason::URL_NOT_HTTP)
        }
    }

    /// `CATEGORIES`: every text of the source (a list or one value), each through `map`.
    fn categories(&self, entity: &Value) -> Option<String> {
        let c = self.field(Field::Categories)?;
        let list = c
            .remap
            .attribute
            .as_ref()
            .and_then(|p| at(entity, p, c.remap.language.as_deref()))
            .and_then(Value::as_array);
        let values: Vec<String> = match list {
            Some(items) => items.iter().filter_map(text).collect(),
            None => self.text(entity, Field::Categories).into_iter().collect(),
        };
        let out: Vec<String> = values
            .into_iter()
            .filter_map(|v| mapped(&c.remap, v))
            .map(|v| escape(&v))
            .collect();
        (!out.is_empty()).then(|| format!("CATEGORIES:{}", out.join(",")))
    }

    fn status(&self, entity: &Value) -> Option<String> {
        let c = self.field(Field::Status)?;
        let value = self.text(entity, Field::Status)?;
        let value = mapped(&c.remap, value)?.to_ascii_uppercase();
        STATUS_VALUES
            .contains(&value.as_str())
            .then(|| format!("STATUS:{value}"))
    }

    /// `ORGANIZER`, a CAL-ADDRESS: the first source holding a `mailto:` or http(s) URI or an
    /// e-mail address, or an organizer object `{name, url}` whose `name` becomes `CN`. An object
    /// without a usable `url` writes nothing: RFC 5545 requires the address, and the event's own
    /// `URL` is no organizer's.
    fn organizer(&self, entity: &Value) -> Option<String> {
        let c = self.field(Field::Organizer)?;
        let language = c.remap.language.as_deref();
        c.remap
            .attribute
            .iter()
            .chain(&c.remap.first_of)
            .filter_map(|path| at(entity, path, language))
            .find_map(|value| match value {
                Value::Object(o) => {
                    let address = o.get("url").and_then(text).and_then(cal_address)?;
                    Some(match o.get("name").and_then(text) {
                        Some(name) => format!("ORGANIZER;CN={}:{address}", param(&name)),
                        None => format!("ORGANIZER:{address}"),
                    })
                }
                _ => text(value)
                    .and_then(cal_address)
                    .map(|a| format!("ORGANIZER:{a}")),
            })
    }

    /// The `VEVENT` of one Entity, or why there is none; `now` stamps an Entity without
    /// `modifiedAt`.
    #[must_use]
    pub fn vevent(&self, entity: &Value, window: &Window, now: DateTime<Utc>) -> Outcome {
        match self.lines(entity, window, now) {
            Ok(Some(lines)) => {
                let mut out = String::new();
                for line in &lines {
                    fold(line, &mut out);
                }
                Outcome::Event(out)
            }
            Ok(None) => Outcome::Outside,
            Err(reason) => Outcome::Skipped(reason),
        }
    }

    fn lines(&self, entity: &Value, window: &Window, now: DateTime<Utc>) -> Read<Vec<String>> {
        // Every Entity a view answers has an id.
        let uid = entity.get("id").and_then(Value::as_str).unwrap_or_default();
        let start = self
            .when(entity, Field::Dtstart)?
            .ok_or(reason::MISSING_DTSTART)?;
        let summary = self
            .text(entity, Field::Summary)
            .ok_or(reason::MISSING_SUMMARY)?;
        let end = self.end(entity, start)?;
        let duration = self.text(entity, Field::Duration);
        if duration.as_deref().is_some_and(|d| !is_duration(d)) {
            return Err(reason::UNPARSABLE_DURATION);
        }
        let (geo, url) = (self.geo(entity)?, self.url(entity)?);
        let modified = match self.when(entity, Field::LastModified)? {
            Some(w) => Some(When::At(w.key().and_utc())),
            None if self.field(Field::LastModified).is_some() => None,
            None => entity
                .get("modifiedAt")
                .and_then(Value::as_str)
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| When::At(t.to_utc())),
        };
        if !window.holds(start, end) {
            return Ok(None);
        }
        let stamp = entity
            .get("modifiedAt")
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map_or(now, |t| t.to_utc());
        let text = |field: Field, name: &str| {
            self.text(entity, field)
                .map(|t| format!("{name}:{}", escape(&t)))
        };
        let mut lines = vec![
            "BEGIN:VEVENT".to_owned(),
            format!("UID:{}", escape(uid)),
            When::At(stamp).property("DTSTAMP"),
            start.property("DTSTART"),
        ];
        lines.extend(end.map(|e| e.property("DTEND")));
        lines.extend(duration.map(|d| format!("DURATION:{d}")));
        lines.push(format!("SUMMARY:{}", escape(&summary)));
        lines.extend(text(Field::Description, "DESCRIPTION"));
        lines.extend(text(Field::Location, "LOCATION"));
        lines.extend(geo);
        lines.extend(url);
        lines.extend(self.categories(entity));
        lines.extend(self.status(entity));
        lines.extend(self.organizer(entity));
        lines.extend(text(Field::Class, "CLASS"));
        lines.extend(modified.map(|m| m.property("LAST-MODIFIED")));
        lines.push("END:VEVENT".to_owned());
        Ok(Some(lines))
    }
}
