# iCalendar feed of a view

An `ical` build (`cargo build -p antares-broker --features ical`) serves the
SDM `Event` entities of every loaded [view](views.md) as one iCalendar
(RFC 5545) feed a calendar application subscribes to:

```text
GET /x/views/{view}/ical/v1/events.ics
GET /x/views/{view}/ical/v1/events.ics?from=2026-10-01&to=2026-10-31
```

Mount it with `ANTARES_API_SURFACES=admin,views,ical`. `from` and `to` take a
date or an RFC 3339 date-time; a date `to` is inclusive. The answer is
`text/calendar; charset=utf-8`. A view that shows no `Event` type answers
`404`. An Event without `startDate` or `name` is left out, never served
invalid, and counted in `antares_surface_records_skipped_total` and the
`antares-skipped-records` header. `HEAD` answers like `GET` without the body;
every other method but `OPTIONS` is `405` with `Allow`.

The field mapping and its reasons are `docs/analysis/ical-mapping.md`
(T-4487).
