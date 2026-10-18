// SPDX-License-Identifier: EUPL-1.2
//! The query guide of a view's types (T-4486): for each type the view names, which attribute is
//! its time axis and whether it is an instant or a range, where its place is, the few attributes
//! worth filtering on, the usual sort, and two example calls. It rides on the read tools'
//! descriptions, so an agent opening the view sees how to ask before its first call.
//!
//! The roles come from the type's Smart Data Model (the same names the platform's SDM table
//! classifies by, T-4498), never typed per view; a role names only an attribute the view shows,
//! so the guide never names one it hides. A type the table does not know gets no guide line.

use crate::schema::Names;

/// A time axis: one instant attribute, or a range of a start and an end.
#[derive(Clone, Copy)]
enum Time {
    Instant(&'static str),
    Range(&'static str, &'static str),
}

struct Guide {
    entity_type: &'static str,
    /// Candidates in order; the first the view shows is the axis.
    time: &'static [Time],
    place: &'static [&'static str],
    filters: &'static [&'static str],
}

/// The SDM types the platform serves (ponytail: grown when a type without a guide is filed).
const GUIDES: &[Guide] = &[
    Guide {
        entity_type: "Event",
        time: &[
            Time::Range("startDate", "endDate"),
            Time::Instant("startDate"),
        ],
        place: &["location", "address"],
        filters: &["category", "eventType", "name", "organizer"],
    },
    Guide {
        entity_type: "AirQualityObserved",
        time: &[Time::Instant("dateObserved")],
        place: &["location", "address"],
        filters: &["pm10", "pm25", "no2", "o3", "airQualityIndex"],
    },
    Guide {
        entity_type: "WeatherObserved",
        time: &[Time::Instant("dateObserved")],
        place: &["location", "address"],
        filters: &[
            "temperature",
            "precipitation",
            "windSpeed",
            "relativeHumidity",
        ],
    },
    Guide {
        entity_type: "WeatherForecast",
        time: &[
            Time::Range("validFrom", "validTo"),
            Time::Instant("dateIssued"),
        ],
        place: &["location", "address"],
        filters: &["temperature", "precipitationProbability", "windSpeed"],
    },
    Guide {
        entity_type: "Alert",
        time: &[
            Time::Range("validFrom", "validTo"),
            Time::Instant("dateIssued"),
        ],
        place: &["location", "address"],
        filters: &["category", "subCategory", "severity"],
    },
    Guide {
        entity_type: "KeyPerformanceIndicator",
        time: &[
            Time::Instant("refPeriod"),
            Time::Instant("dateObserved"),
            Time::Range("calculationPeriod[from]", "calculationPeriod[to]"),
        ],
        place: &["refArea", "area", "location", "address"],
        filters: &[
            "indicator",
            "refArea",
            "dimensionKey",
            "kpiValue",
            "category",
        ],
    },
    Guide {
        entity_type: "Vehicle",
        time: &[Time::Instant("location.observedAt")],
        place: &["location"],
        filters: &["vehicleType", "speed", "serviceStatus"],
    },
    Guide {
        entity_type: "ParkingSpot",
        time: &[Time::Instant("status.observedAt")],
        place: &["location", "address"],
        filters: &["status", "category"],
    },
    Guide {
        entity_type: "OffStreetParking",
        time: &[Time::Instant("availableSpotNumber.observedAt")],
        place: &["location", "address"],
        filters: &["availableSpotNumber", "totalSpotNumber", "category"],
    },
    Guide {
        entity_type: "BikeHireDockingStation",
        time: &[Time::Instant("availableBikeNumber.observedAt")],
        place: &["location", "address"],
        filters: &["availableBikeNumber", "freeSlotNumber", "status"],
    },
    Guide {
        entity_type: "TrafficFlowObserved",
        time: &[
            Time::Range("dateObservedFrom", "dateObservedTo"),
            Time::Instant("dateObserved"),
        ],
        place: &["location", "address"],
        filters: &["intensity", "averageVehicleSpeed", "congested"],
    },
    Guide {
        entity_type: "PointOfInterest",
        time: &[],
        place: &["location", "address"],
        filters: &["category", "name"],
    },
];

/// Whether the view shows `attribute`: its leading name, before a `.` or `[`.
fn shows(names: &Names, attribute: &str) -> bool {
    let head = attribute.split(['.', '[']).next().unwrap_or(attribute);
    names.every || names.attributes.iter().any(|a| a == head)
}

/// Monday 00:00 UTC of the week holding `now`, and the Monday after, as `YYYY-MM-DDT00:00:00Z`.
fn this_week(now: chrono::DateTime<chrono::Utc>) -> (String, String) {
    use chrono::Datelike as _;
    let today = now.date_naive();
    let monday = today
        .checked_sub_days(chrono::Days::new(u64::from(
            today.weekday().num_days_from_monday(),
        )))
        .unwrap_or(today);
    let next = monday
        .checked_add_days(chrono::Days::new(7))
        .unwrap_or(monday);
    let stamp = |day: chrono::NaiveDate| format!("{}T00:00:00Z", day.format("%Y-%m-%d"));
    (stamp(monday), stamp(next))
}

/// One type's guide line, or `None` for a type the table does not know or the view hides.
fn line(guide: &Guide, names: &Names, week: &(String, String)) -> Option<String> {
    if !names.has_type(guide.entity_type) {
        return None;
    }
    let t = guide.entity_type;
    let (from, to) = week;
    let time = guide.time.iter().copied().find(|time| match time {
        Time::Instant(a) => shows(names, a),
        Time::Range(a, b) => shows(names, a) && shows(names, b),
    });
    let place: Vec<&str> = guide
        .place
        .iter()
        .copied()
        .filter(|a| shows(names, a))
        .collect();
    let filters: Vec<&str> = guide
        .filters
        .iter()
        .copied()
        .filter(|a| shows(names, a))
        .take(5)
        .collect();
    let mut parts = vec![match time {
        Some(Time::Range(a, b)) => {
            format!("{t}: time {a}..{b} (a range); this week: type={t}, q={a}<{to};{b}>={from}.")
        }
        Some(Time::Instant(a)) => format!(
            "{t}: time {a} (an instant), sort orderBy={a};desc; this week: type={t}, q={a}>={from};{a}<{to}."
        ),
        None => format!("{t}: no time axis."),
    }];
    match place.first() {
        Some(&"location") => parts.push(format!(
            "Place {}; near a point: type={t}, georel=near;maxDistance==2000, geometry=Point, coordinates=[longitude,latitude].",
            place.join(", ")
        )),
        Some(first) => parts.push(format!(
            "Place {}; by name: type={t}, q={first}~=\"(?i)bystric\".",
            place.join(", ")
        )),
        None => {}
    }
    if !filters.is_empty() {
        parts.push(format!("Filter on {}.", filters.join(", ")));
    }
    Some(parts.join(" "))
}

/// The guide of every type the view names, one line each, or `None` when it names none the
/// table knows; the example weeks are the one holding `now`.
pub(crate) fn text(names: &Names, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    if names.every {
        // ponytail: a `*` view names no type; its schema-less guide is the primer's q grammar
        return None;
    }
    let week = this_week(now);
    let lines: Vec<String> = GUIDES
        .iter()
        .filter_map(|g| line(g, names, &week))
        .collect();
    (!lines.is_empty()).then(|| format!("Query guide, per type:\n{}", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    /// Saturday 2026-10-17 12:00 UTC.
    fn saturday() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(2026, 10, 17, 12, 0, 0)
            .single()
            .unwrap_or_default()
    }

    fn names(types: &[&str], attributes: &[&str]) -> Names {
        Names::new(
            types.iter().map(|t| (*t).to_owned()).collect(),
            attributes.iter().map(|a| (*a).to_owned()).collect(),
        )
    }

    #[test]
    fn the_week_is_monday_to_monday() {
        assert_eq!(
            this_week(saturday()),
            (
                "2026-10-12T00:00:00Z".to_owned(),
                "2026-10-19T00:00:00Z".to_owned()
            )
        );
    }

    #[test]
    fn an_event_is_a_range_filtered_by_the_week() -> Result<(), String> {
        let guide = text(
            &names(
                &["Event"],
                &["startDate", "endDate", "location", "category", "name"],
            ),
            saturday(),
        )
        .ok_or("a guide")?;
        assert!(
            guide.contains("Event: time startDate..endDate (a range)"),
            "{guide}"
        );
        assert!(
            guide.contains("q=startDate<2026-10-19T00:00:00Z;endDate>=2026-10-12T00:00:00Z"),
            "{guide}"
        );
        assert!(guide.contains("georel=near"), "{guide}");
        assert!(guide.contains("Filter on category, name."), "{guide}");
        Ok(())
    }

    #[test]
    fn a_vehicle_is_located_and_observed() -> Result<(), String> {
        let guide =
            text(&names(&["Vehicle"], &["location", "speed"]), saturday()).ok_or("a guide")?;
        assert!(
            guide.contains("time location.observedAt (an instant)"),
            "{guide}"
        );
        assert!(guide.contains("Place location"), "{guide}");
        Ok(())
    }

    #[test]
    fn a_kpi_reads_its_period_and_region() -> Result<(), String> {
        let guide = text(
            &names(
                &["KeyPerformanceIndicator"],
                &["refPeriod", "refArea", "indicator", "kpiValue"],
            ),
            saturday(),
        )
        .ok_or("a guide")?;
        assert!(guide.contains("time refPeriod (an instant)"), "{guide}");
        assert!(guide.contains("Place refArea; by name"), "{guide}");
        assert!(
            guide.contains("Filter on indicator, refArea, kpiValue."),
            "{guide}"
        );
        Ok(())
    }

    #[test]
    fn half_a_range_is_an_instant_and_a_type_without_filters_says_none() -> Result<(), String> {
        let event =
            text(&names(&["Event"], &["startDate", "location"]), saturday()).ok_or("a guide")?;
        assert!(event.contains("time startDate (an instant)"), "{event}");
        let poi = text(&names(&["PointOfInterest"], &["location"]), saturday()).ok_or("a guide")?;
        assert!(poi.contains("PointOfInterest: no time axis."), "{poi}");
        assert!(!poi.contains("Filter on"), "{poi}");
        assert!(shows(&Names::every(), "anything"));
        Ok(())
    }

    #[test]
    fn a_hidden_attribute_is_never_named_and_an_unknown_type_has_no_line() -> Result<(), String> {
        let guide = text(&names(&["Event", "Thing"], &["name"]), saturday()).ok_or("a guide")?;
        assert!(!guide.contains("startDate"), "{guide}");
        assert!(!guide.contains("location"), "{guide}");
        assert!(guide.contains("Event: no time axis."), "{guide}");
        assert!(!guide.contains("Thing"), "{guide}");
        assert_eq!(text(&names(&["Thing"], &["name"]), saturday()), None);
        assert_eq!(text(&Names::every(), saturday()), None);
        Ok(())
    }
}
