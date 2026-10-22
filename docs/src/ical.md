# iCalendar feed of a view

An `ical` build (`cargo build -p antares-broker --features ical`) serves the
Events of every loaded [view](views.md) as one iCalendar (RFC 5545) feed a
calendar application subscribes to:

```text
GET /x/views/{view}/ical/v1/events.ics
GET /x/views/{view}/ical/v1/events.ics?from=2026-10-01&to=2026-10-31
GET /x/views/{view}/ical/v1/events.ics?q=address[addressLocality]=="Zvolen"
```

Mount it with `ANTARES_API_SURFACES=admin,views,ical`. The feed is one NGSI-LD
Query Entities through the view: `q`, `scopeQ`, `georel`, `geometry`,
`coordinates`, `geoproperty`, `id`, `idPattern`, `lang` and `attrs` mean what
they mean there, and `type` may name the mapped type alone. `from` and `to`
take a date or an RFC 3339 date-time; a date `to` is inclusive. Any other
parameter is `400`. The answer is `text/calendar; charset=utf-8`.

Which attribute becomes which calendar field is the view's
`representations.ical` member (`version`, `type`, `preset`, `mapping`, as the platform's
Endpoint writes it); a view without one serves the SDM `Event` table. `version` is the
contract version, `1` when absent (T-5207): a mapping of version 1 is read strictly, and
one of a newer version (a Portal released before the broker) is served as its `type`
over its `preset` with a log warning, never as a `500` for the whole feed. A view
that shows no Entity of the mapped type, or does not allow `query_entities`,
answers `404`. An Entity the mapping cannot render (no start or title, an
unreadable date, an end before its start, a bad URL or point) is left out,
never served invalid, and counted in
`antares_surface_records_skipped_total{surface="ical",reason}` and the
`antares-skipped-records` header. `HEAD` answers like `GET` without the body;
every other method but `OPTIONS` is `405` with `Allow`.

`POST /x/views/{view}/ical/v1/preview` renders a candidate mapping over the
first 50 Entities for the Portal's Endpoint form; it answers the broker's
admin bearer token alone.

The field mapping, the remaps and their reasons are
`docs/analysis/ical-mapping.md` (T-4487, T-4901).
