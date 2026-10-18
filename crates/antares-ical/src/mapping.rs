// SPDX-License-Identifier: EUPL-1.2
//! An SDM `Event` as one RFC 5545 `VEVENT` (`docs/analysis/ical-mapping.md`): `id` the `UID`,
//! `modifiedAt` the `DTSTAMP` and `LAST-MODIFIED`, `startDate` and `endDate` the `DTSTART` and
//! `DTEND`, `name` the `SUMMARY`, `description`, `address`, `location` and `url` the
//! `DESCRIPTION`, `LOCATION`, `GEO` and `URL`. What the Entity lacks stays absent; an Entity
//! without a `startDate` or a `name` is no `VEVENT` at all (EP-126).

use chrono::{DateTime, Days, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde_json::Value;

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
    /// date end is exclusive, so the SDM's inclusive last day becomes the day after it.
    fn end_like(self, end: Self) -> Self {
        match (self, end) {
            (Self::Date(_), Self::Date(e)) => Self::Date(next_day(e)),
            (Self::Date(_), other) => Self::Date(next_day(other.key().date())),
            (Self::At(_), Self::At(t)) => Self::At(t),
            (Self::At(_), other) => Self::At(other.past().and_utc()),
            (Self::Floating(_), Self::At(t)) => Self::Floating(t.naive_utc()),
            (Self::Floating(_), other) => Self::Floating(other.past()),
        }
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

/// What one Entity is in a feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Its `VEVENT`, folded, each line ending in CRLF.
    Event(String),
    /// A valid Event outside the window.
    Outside,
    /// No valid `VEVENT`: the mandatory Attribute it lacks or holds unreadable.
    Skipped(&'static str),
}

/// One member's value by its local name: a Property's `value` (a typed literal's `@value`), a
/// `LanguageProperty`'s first language, a `GeoProperty`'s geometry; the default instance of a
/// multi-instance Attribute, else the first.
fn member<'a>(entity: &'a Value, name: &str) -> Option<&'a Value> {
    let (_, member) = entity
        .as_object()?
        .iter()
        .find(|(key, _)| local(key) == name)?;
    let instance = match member {
        Value::Array(all) => all
            .iter()
            .find(|i| i.get("datasetId").is_none())
            .or_else(|| all.first())?,
        one => one,
    };
    if let Some(map) = instance.get("languageMap").and_then(Value::as_object) {
        return map.values().next();
    }
    let value = instance.get("value").unwrap_or(instance);
    Some(value.get("@value").unwrap_or(value))
}

/// A member as non-empty text.
fn text<'a>(entity: &'a Value, name: &str) -> Option<&'a str> {
    member(entity, name)?
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
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

/// The `LOCATION` of an `address`: its street, postal code, locality and country, or its text.
fn address(entity: &Value) -> Option<String> {
    let value = member(entity, "address")?;
    if let Some(t) = value.as_str() {
        return Some(t.trim().to_owned()).filter(|t| !t.is_empty());
    }
    let parts: Vec<&str> = [
        "streetAddress",
        "postalCode",
        "addressLocality",
        "addressCountry",
    ]
    .iter()
    .filter_map(|k| value.get(k).and_then(Value::as_str).map(str::trim))
    .filter(|t| !t.is_empty())
    .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The `GEO` of a Point `location`: latitude first (RFC 5545 3.8.1.6).
fn geo(entity: &Value) -> Option<String> {
    let geometry = member(entity, "location")?;
    if geometry.get("type").and_then(Value::as_str) != Some("Point") {
        return None;
    }
    let coordinates = geometry.get("coordinates")?.as_array()?;
    let lon = coordinates.first()?.as_f64()?;
    let lat = coordinates.get(1)?.as_f64()?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
        .then(|| format!("GEO:{lat};{lon}"))
}

/// An http(s) `url` without white space or control characters, the URI value `URL` takes.
fn url(entity: &Value) -> Option<&str> {
    text(entity, "url").filter(|u| {
        (u.starts_with("https://") || u.starts_with("http://"))
            && !u.chars().any(|c| c.is_whitespace() || c.is_control())
    })
}

/// The `VEVENT` of one Entity, or why there is none; `now` stamps an Entity without
/// `modifiedAt`.
#[must_use]
pub fn vevent(entity: &Value, window: &Window, now: DateTime<Utc>) -> Outcome {
    let Some(uid) = entity.get("id").and_then(Value::as_str) else {
        return Outcome::Skipped("id");
    };
    let Some(start) = text(entity, "startDate").and_then(When::parse) else {
        return Outcome::Skipped("startDate");
    };
    let Some(summary) = text(entity, "name") else {
        return Outcome::Skipped("name");
    };
    let end = text(entity, "endDate")
        .and_then(When::parse)
        .map(|e| start.end_like(e))
        .filter(|e| e.key() > start.key());
    if !window.holds(start, end) {
        return Outcome::Outside;
    }
    let modified = entity
        .get("modifiedAt")
        .and_then(Value::as_str)
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    let stamp = When::At(modified.unwrap_or(now));
    let mut lines = vec![
        "BEGIN:VEVENT".to_owned(),
        format!("UID:{}", escape(uid)),
        stamp.property("DTSTAMP"),
        start.property("DTSTART"),
    ];
    lines.extend(end.map(|e| e.property("DTEND")));
    lines.push(format!("SUMMARY:{}", escape(summary)));
    lines.extend(text(entity, "description").map(|d| format!("DESCRIPTION:{}", escape(d))));
    lines.extend(address(entity).map(|a| format!("LOCATION:{}", escape(&a))));
    lines.extend(geo(entity));
    lines.extend(url(entity).map(|u| format!("URL:{u}")));
    lines.extend(modified.map(|m| When::At(m).property("LAST-MODIFIED")));
    lines.push("END:VEVENT".to_owned());
    let mut out = String::new();
    for line in &lines {
        fold(line, &mut out);
    }
    Outcome::Event(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-18T00:00:00Z")
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_default()
    }

    fn event(extra: &Value) -> Value {
        let mut e = json!({"id": "urn:ngsi-ld:Event:1", "type": "Event",
            "name": {"type": "Property", "value": "Koncert"},
            "startDate": {"type": "Property", "value": "2026-10-20"}});
        if let (Some(e), Some(extra)) = (e.as_object_mut(), extra.as_object()) {
            e.extend(extra.clone());
        }
        e
    }

    fn lines(outcome: &Outcome) -> Vec<String> {
        match outcome {
            Outcome::Event(text) => text
                .replace("\r\n ", "")
                .split("\r\n")
                .map(str::to_owned)
                .collect(),
            other => vec![format!("{other:?}")],
        }
    }

    #[test]
    fn a_line_of_multibyte_letters_folds_at_75_octets_between_characters() {
        let mut out = String::new();
        fold(&format!("SUMMARY:{}", "ž".repeat(60)), &mut out);
        for line in out.trim_end_matches("\r\n").split("\r\n") {
            assert!(line.len() <= 75, "{} octets: {line}", line.len());
        }
        assert_eq!(
            out.replace("\r\n ", ""),
            format!("SUMMARY:{}\r\n", "ž".repeat(60))
        );
    }

    #[test]
    fn text_escapes_what_rfc_5545_reserves_and_drops_control_characters() {
        assert_eq!(escape("a,b;c\\d\ne\u{7}"), "a\\,b\\;c\\\\d\\ne");
    }

    #[test]
    fn a_multi_day_event_ends_the_day_after_its_last_day() {
        let e = event(&json!({"endDate": {"type": "Property", "value": "2026-10-22"}}));
        let l = lines(&vevent(&e, &Window::default(), now()));
        assert!(
            l.contains(&"DTSTART;VALUE=DATE:20261020".to_owned()),
            "{l:?}"
        );
        assert!(l.contains(&"DTEND;VALUE=DATE:20261023".to_owned()), "{l:?}");
    }

    #[test]
    fn an_offset_is_written_in_utc_and_an_end_before_the_start_is_dropped() {
        let e = event(&json!({
            "startDate": {"type": "Property", "value": {"@type": "DateTime", "@value": "2026-10-20T18:00:00+02:00"}},
            "endDate": {"type": "Property", "value": "2026-10-20T17:00:00+02:00"}}));
        let l = lines(&vevent(&e, &Window::default(), now()));
        assert!(l.contains(&"DTSTART:20261020T160000Z".to_owned()), "{l:?}");
        assert!(!l.iter().any(|x| x.starts_with("DTEND")), "{l:?}");
    }

    #[test]
    fn without_start_or_name_there_is_no_event() {
        let mut e = event(&json!({}));
        if let Some(o) = e.as_object_mut() {
            o.remove("name");
        }
        assert_eq!(
            vevent(&e, &Window::default(), now()),
            Outcome::Skipped("name")
        );
        let e = event(&json!({"startDate": {"type": "Property", "value": "soon"}}));
        assert_eq!(
            vevent(&e, &Window::default(), now()),
            Outcome::Skipped("startDate")
        );
    }

    #[test]
    fn the_window_keeps_what_overlaps_it_and_its_last_day_inclusive() {
        let day = |d: &str| When::parse(d).map(When::key);
        let w = Window {
            from: day("2026-10-21"),
            to: When::parse("2026-10-21").map(When::past),
        };
        let multi = event(&json!({"endDate": {"type": "Property", "value": "2026-10-22"}}));
        assert!(matches!(vevent(&multi, &w, now()), Outcome::Event(_)));
        // one day, the 20th: over before the 21st
        assert_eq!(vevent(&event(&json!({})), &w, now()), Outcome::Outside);
        let late =
            event(&json!({"startDate": {"type": "Property", "value": "2026-10-21T23:30:00Z"}}));
        assert!(matches!(vevent(&late, &w, now()), Outcome::Event(_)));
    }

    #[test]
    fn location_geo_and_url_come_from_the_entity_and_a_bad_url_is_left_out() {
        let e = event(&json!({
            "address": {"type": "Property", "value": {"streetAddress": "Námestie SNP 1", "addressLocality": "Banská Bystrica"}},
            "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": [19.146, 48.736]}},
            "url": {"type": "Property", "value": "javascript:alert(1)"}}));
        let l = lines(&vevent(&e, &Window::default(), now()));
        assert!(
            l.contains(&"LOCATION:Námestie SNP 1\\, Banská Bystrica".to_owned()),
            "{l:?}"
        );
        assert!(l.contains(&"GEO:48.736;19.146".to_owned()), "{l:?}");
        assert!(!l.iter().any(|x| x.starts_with("URL")), "{l:?}");
    }
}
