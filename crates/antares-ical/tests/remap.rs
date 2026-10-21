// SPDX-License-Identifier: EUPL-1.2
//! `antares_ical::mapping::Plan`: a view's field mapping, its presets and its remaps
//! (platform API/02 §8b, T-4901), over the shared cases of `fixtures/cases.json`.

#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use antares_ical::mapping::{reason, IcalMapping, Outcome, Plan, Window};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-18T00:00:00Z")
        .expect("time")
        .with_timezone(&Utc)
}

fn plan(ical: &Value) -> Result<Plan, String> {
    serde_json::from_value::<IcalMapping>(ical.clone())
        .map_err(|e| e.to_string())
        .and_then(|m| Plan::compile(&m))
}

/// The unfolded content lines between `BEGIN:VEVENT` and `END:VEVENT`.
fn lines(outcome: &Outcome) -> Vec<String> {
    match outcome {
        Outcome::Event(text) => {
            let all: Vec<String> = text
                .replace("\r\n ", "")
                .split("\r\n")
                .map(str::to_owned)
                .collect();
            assert_eq!(all.first().map(String::as_str), Some("BEGIN:VEVENT"));
            assert_eq!(all.last().map(String::as_str), Some(""), "ends in CRLF");
            all.get(1..all.len().saturating_sub(2))
                .unwrap_or_default()
                .to_vec()
        }
        other => vec![format!("{other:?}")],
    }
}

fn render(ical: &Value, entity: &Value) -> Vec<String> {
    let plan = plan(ical).expect("a mapping that holds");
    lines(&plan.vevent(entity, &Window::default(), now()))
}

#[test]
fn every_shared_case_renders_skips_or_is_refused_as_written() {
    let cases: Value =
        serde_json::from_str(include_str!("fixtures/cases.json")).expect("cases.json");
    let entity = &cases["entity"];
    let all = cases["cases"].as_object().expect("cases");
    assert!(all.len() >= 18, "{}", all.len());
    for (name, case) in all {
        let ical = &case["ical"];
        if let Some(refused) = case["refused"].as_str() {
            let err = plan(ical)
                .err()
                .unwrap_or_else(|| panic!("{name}: accepted"));
            assert!(err.contains(refused), "{name}: {err}");
        } else if let Some(skip) = case["skip"].as_str() {
            assert_eq!(
                render(ical, entity),
                [format!("Skipped({skip:?})")],
                "{name}"
            );
        } else {
            let want: Vec<String> = serde_json::from_value(case["lines"].clone()).expect("lines");
            assert_eq!(render(ical, entity), want, "{name}");
        }
    }
}

fn entity(members: &Value) -> Value {
    let mut e = json!({"id": "urn:ngsi-ld:Event:2", "type": "Event",
                       "modifiedAt": "2026-10-01T08:00:00Z",
                       "startDate": {"type": "Property", "value": "2026-10-20"},
                       "name": {"type": "Property", "value": "Trh"}});
    for (k, v) in members.as_object().expect("members") {
        e[k] = v.clone();
    }
    e
}

fn has(lines: &[String], line: &str) -> bool {
    lines.iter().any(|l| l == line)
}

#[test]
fn a_language_a_sub_attribute_and_a_constant_category_are_read() {
    let ical = json!({"preset": "sdm-event", "mapping": {
        "SUMMARY": {"attribute": "title", "language": "en"},
        "DESCRIPTION": "name.note",
        "CATEGORIES": {"constant": "Kultúra"}}});
    let e = entity(&json!({
        "title": {"type": "LanguageProperty", "languageMap": {"sk": "Trh", "en": "Market"}},
        "name": {"type": "Property", "value": "Trh", "note": {"type": "Property", "value": "pod holým nebom"}}}));
    let l = render(&ical, &e);
    assert!(has(&l, "SUMMARY:Market"), "{l:?}");
    assert!(has(&l, "DESCRIPTION:pod holým nebom"), "{l:?}");
    assert!(has(&l, "CATEGORIES:Kultúra"), "{l:?}");
    // a language the entity lacks is no value
    let e = entity(&json!({"title": {"type": "LanguageProperty", "languageMap": {"sk": "Trh"}}}));
    assert_eq!(
        render(&ical, &e),
        [format!("Skipped({:?})", reason::MISSING_SUMMARY)]
    );
}

#[test]
fn a_date_end_is_exclusive_unless_written_inclusive() {
    let e = entity(&json!({"endDate": {"type": "Property", "value": "2026-10-22"}}));
    let exclusive = json!({"preset": "sdm-event", "mapping": {"DTEND": "endDate"}});
    assert!(has(&render(&exclusive, &e), "DTEND;VALUE=DATE:20261022"));
    let inclusive = json!({"preset": "sdm-event", "mapping": {"DTEND": {"attribute": "endDate", "endInclusive": true}}});
    assert!(has(&render(&inclusive, &e), "DTEND;VALUE=DATE:20261023"));
    // an exclusive end on the start day ends nothing
    let same = entity(&json!({"endDate": {"type": "Property", "value": "2026-10-20"}}));
    assert_eq!(
        render(&exclusive, &same),
        [format!("Skipped({:?})", reason::END_NOT_AFTER_START)]
    );
}

#[test]
fn a_duration_must_be_an_rfc_5545_duration() {
    let ical =
        json!({"mapping": {"DTSTART": "startDate", "SUMMARY": "name", "DURATION": "length"}});
    for good in ["P1D", "PT1H30M", "P2W", "-PT15M", "P1DT12H"] {
        let e = entity(&json!({"length": {"type": "Property", "value": good}}));
        assert!(
            has(&render(&ical, &e), &format!("DURATION:{good}")),
            "{good}"
        );
    }
    for bad in ["1 hour", "P", "PT", "P1H", "PT1M1H", "P1W2D", "PDT1H", "P5"] {
        let e = entity(&json!({"length": {"type": "Property", "value": bad}}));
        assert_eq!(
            render(&ical, &e),
            [format!("Skipped({:?})", reason::UNPARSABLE_DURATION)],
            "{bad}"
        );
    }
}

#[test]
fn status_organizer_and_last_modified_follow_their_value_types() {
    let ical = json!({"preset": "sdm-event", "mapping": {
        "STATUS": "state", "ORGANIZER": "contact", "LAST-MODIFIED": "changed"}});
    let e = entity(&json!({
        "state": {"type": "Property", "value": "confirmed"},
        "contact": {"type": "Property", "value": "kultura@zvolen.sk"},
        "changed": {"type": "Property", "value": "2026-09-30T10:00:00+02:00"}}));
    let l = render(&ical, &e);
    assert!(has(&l, "STATUS:CONFIRMED"), "{l:?}");
    assert!(has(&l, "ORGANIZER:mailto:kultura@zvolen.sk"), "{l:?}");
    assert!(has(&l, "LAST-MODIFIED:20260930T080000Z"), "{l:?}");
    // a value that is no calendar status or address is left out; DTSTAMP stays modifiedAt
    let e = entity(&json!({
        "state": {"type": "Property", "value": "postponed"},
        "contact": {"type": "Property", "value": "Mestský úrad"}}));
    let l = render(&ical, &e);
    assert!(
        !l.iter()
            .any(|x| x.starts_with("STATUS") || x.starts_with("ORGANIZER")),
        "{l:?}"
    );
    assert!(!l.iter().any(|x| x.starts_with("LAST-MODIFIED")), "{l:?}");
    assert!(has(&l, "DTSTAMP:20261001T080000Z"), "{l:?}");
}

#[test]
fn a_custom_type_is_the_plan_type_and_a_bad_type_name_is_refused() {
    let p = plan(&json!({"type": "Festival"})).expect("a preset over another type");
    assert_eq!(p.entity_type(), "Festival");
    assert_eq!(Plan::default().entity_type(), "Event");
    let err = plan(&json!({"type": "a/b"})).expect_err("refused");
    assert!(err.contains("not an NGSI-LD type name"), "{err}");
    assert!(plan(&json!({"preset": "outlook"})).is_err());
    assert!(plan(&json!({"extra": 1})).is_err(), "unknown member");
}

fn skipped(why: &str) -> Vec<String> {
    vec![format!("Skipped({why:?})")]
}

#[test]
fn a_date_mode_and_a_zone_read_a_value_as_asked_and_a_skipped_wall_time_is_unreadable() {
    let start = |remap: Value, value: &str| {
        let ical = json!({"preset": "sdm-event", "mapping": {"DTSTART": remap}});
        let e = entity(&json!({"startDate": {"type": "Property", "value": value}}));
        render(&ical, &e)
    };
    let zone = "Europe/Bratislava";
    let day = json!({"attribute": "startDate", "date": "date"});
    let day_in_zone = json!({"attribute": "startDate", "date": "date", "timezone": zone});
    let datetime = json!({"attribute": "startDate", "date": "datetime"});
    let datetime_in_zone = json!({"attribute": "startDate", "date": "datetime", "timezone": zone});
    for (remap, value, line) in [
        (&day, "2026-10-20T23:30:00Z", "DTSTART;VALUE=DATE:20261020"),
        (
            &day_in_zone,
            "2026-10-20T23:30:00Z",
            "DTSTART;VALUE=DATE:20261021",
        ),
        (&day, "2026-10-20T18:00:00", "DTSTART;VALUE=DATE:20261020"),
        (&datetime, "2026-10-20", "DTSTART:20261020T000000"),
        (&datetime_in_zone, "2026-10-20", "DTSTART:20261019T220000Z"),
    ] {
        let l = start(remap.clone(), value);
        assert!(has(&l, line), "{remap} {value}: {l:?}");
    }
    // 02:30 on the last Sunday of March does not happen in Bratislava
    let l = start(
        json!({"attribute": "startDate", "timezone": zone}),
        "2026-03-29T02:30:00",
    );
    assert_eq!(l, skipped(reason::UNPARSABLE_DATE));
    let ical = json!({"preset": "sdm-event", "mapping": {"LAST-MODIFIED": "changed"}});
    let e = entity(&json!({"changed": {"type": "Property", "value": "yesterday"}}));
    assert_eq!(render(&ical, &e), skipped(reason::UNPARSABLE_DATE));
}

#[test]
fn values_of_every_kind_read_as_text_or_as_nothing() {
    let ical = json!({"mapping": {"DTSTART": "startDate", "SUMMARY": "count",
        "DESCRIPTION": {"attribute": "title.alt", "language": "en"},
        "LOCATION": "name.missing", "URL": "flag",
        "STATUS": {"attribute": "state", "map": {"done": "CONFIRMED"}},
        "ORGANIZER": "by", "GEO": "location"}});
    let e = entity(&json!({
        "count": {"type": "Property", "value": 42},
        "title": {"type": "Property", "value": "x", "alt": {"type": "LanguageProperty", "languageMap": {"sk": "iba po slovensky"}}},
        "flag": {"type": "Property", "value": [true]},
        "state": {"type": "VocabProperty", "vocab": "open"},
        "by": {"type": "Property", "value": "mailto:kultura@zvolen.sk"},
        "location": {"type": "GeoProperty", "value": {"type": "Point", "coordinates": ["19.1", "48.7"]}}}));
    let l = render(&ical, &e);
    assert_eq!(
        l,
        [
            "UID:urn:ngsi-ld:Event:2",
            "DTSTAMP:20261001T080000Z",
            "DTSTART;VALUE=DATE:20261020",
            "SUMMARY:42",
            "ORGANIZER:mailto:kultura@zvolen.sk",
            "LAST-MODIFIED:20261001T080000Z"
        ]
    );
    let e = entity(&json!({"count": {"type": "Property", "value": false},
        "state": {"type": "VocabProperty", "vocab": "done"}, "by": {"type": "Property", "value": "MsÚ"}}));
    let l = render(&ical, &e);
    assert!(
        has(&l, "SUMMARY:false") && has(&l, "STATUS:CONFIRMED"),
        "{l:?}"
    );
    assert!(!l.iter().any(|x| x.starts_with("ORGANIZER")), "{l:?}");
    let count = json!({"type": "Property", "value": 1});
    let e = entity(&json!({"by": {"type": "Property", "value": "a@b:c"}, "count": count}));
    let l = render(&ical, &e);
    assert!(!l.iter().any(|x| x.starts_with("ORGANIZER")), "{l:?}");
    let off =
        json!({"type": "GeoProperty", "value": {"type": "Point", "coordinates": [190.0, 48.7]}});
    let e = entity(&json!({"location": off, "count": count}));
    assert_eq!(render(&ical, &e), skipped(reason::GEO_OUT_OF_RANGE));
    // a mapped STATUS the entity lacks, and an entity that is no object
    assert!(
        !render(&json!({"preset": "schema-org"}), &entity(&json!({})))
            .iter()
            .any(|x| x.starts_with("STATUS"))
    );
    assert_eq!(
        lines(&Plan::default().vevent(&json!("x"), &Window::default(), now())),
        skipped(reason::MISSING_DTSTART)
    );
}
