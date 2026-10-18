# iCalendar (RFC 5545) feed of a view: SDM `Event` onto `VEVENT` (T-4487)

Sources: RFC 5545 (iCalendar), Smart Data Models `dataModel.Event/Event`, the joinedcontext platform's
`schemas/surface-requirements.md` (the `ical` representation and the model it needs, EP-125, T-4573). The crate is
`crates/antares-ical`; it mounts at `/x/views/{view}/ical/v1/events.ics` (feature and surface name `ical`). The
user page is `docs/src/ical.md`.

## 1. Which Entities

The feed reads Query Entities through the view, naming the view's types whose local name is `Event` (any
@context's IRI ending in `/Event` or `#Event`); a view of every type reads `local=true` and keeps the Entities whose
type's local name is `Event`. A view that shows no such type has no feed: `404`, the answer an unknown view gets.
`Activity` is not mapped. Entities are read under the core @context with `options=sysAttrs`; an Attribute is found
by its local name, so `startDate` and `https://smartdatamodels.org/dataModel.Event/startDate` are the same source.

## 2. Fields

| `VEVENT` property | RFC 5545 | Source | Absent source |
|---|---|---|---|
| `UID` | 3.8.4.7 | the Entity `id` | never absent |
| `DTSTAMP` | 3.8.7.2 | `modifiedAt` | the time of the request |
| `DTSTART` | 3.8.2.4 | `startDate` | no `VEVENT`, counted (section 4) |
| `DTEND` | 3.8.2.2 | `endDate` | no `DTEND` |
| `SUMMARY` | 3.8.1.12 | `name` | no `VEVENT`, counted (section 4) |
| `DESCRIPTION` | 3.8.1.5 | `description` | absent |
| `LOCATION` | 3.8.1.7 | `address`: `streetAddress`, `postalCode`, `addressLocality`, `addressCountry`, comma-joined; or its text | absent |
| `GEO` | 3.8.1.6 | `location`, a `Point`: latitude `;` longitude | absent (another geometry too) |
| `URL` | 3.8.4.6 | `url`, an `http` or `https` URI without white space | absent |
| `LAST-MODIFIED` | 3.8.7.3 | `modifiedAt` | absent |

A value is a Property's `value` (a typed literal's `@value`), a LanguageProperty's first language, the default
instance of a multi-instance Attribute (else its first).

## 3. Dates

- `2026-10-20` is a date: `DTSTART;VALUE=DATE:20261020`. The SDM `endDate` names the last day, RFC 5545 `DTEND`
  the day after it, so a fair of `2026-09-10` to `2026-09-12` ends `DTEND;VALUE=DATE:20260913`.
- An RFC 3339 date-time is written in UTC (`20261020T160000Z`); no `VTIMEZONE` is needed. A date-time without
  offset is a floating time (`20261020T180000`).
- `DTEND` takes the value type of `DTSTART` (3.8.2.2). An `endDate` that is not after the start is dropped.
- `from` and `to` take a date or a date-time. An Event is in the feed when it starts before the end of `to` (a date
  `to` is inclusive) and has not ended before `from`. Bad or reversed bounds answer `400`.

## 4. Records left out

An `Event` without a readable `startDate` or a non-empty `name` is no `VEVENT`: the feed never serves an invalid
record. Each one increments `antares_surface_records_skipped_total{surface="ical",attribute}` (EP-126) and the
response's `antares-skipped-records` header counts them.

## 5. Output

Text is escaped (3.3.11: backslash, semicolon, comma, newline; other control characters dropped) and every content
line is folded at 75 octets between UTF-8 characters (3.1), each ending in CRLF. The calendar carries `VERSION:2.0`,
`PRODID:-//Antares//antares-ical//EN`, `CALSCALE:GREGORIAN` and `X-WR-CALNAME` the view id. A feed reads at most
`RESIDUAL_SCAN` (10 000) Entities and answers `413` past that; the window is applied after the read.

## 6. Security

Read only (`GET`, `HEAD`, `OPTIONS`; any other method `405`). Every read is the view's Query Entities, so a hidden
type or Attribute is in no `VEVENT`; the refusals carry fixed messages that name nothing the view hides.
