// SPDX-License-Identifier: EUPL-1.2
//! The temporal seam's producer side (ADR-0013): the write path pushes
//! `TemporalEvent`s into a per-request buffer; the buffer is drained ONCE
//! per request — after the handler, before the response leaves — so the
//! driver sees the whole request in one `event_list` call and a client
//! reading its own history right after the write always finds it.
//!
//! Outside a request (background jobs, tests without the router) there is
//! no buffer, and a push drains immediately: the seam degrades to today's
//! per-change recording rather than losing events. A driver error in the
//! drain is logged and counted (`/q/health` temporalDrainErrors) — it never
//! changes the response of a write that already committed.

use crate::state::AppState;
use antares_model::TenantId;
use antares_store::{TemporalDriverExt as _, TemporalEvent};
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static DRAIN_ERRORS: AtomicU64 = AtomicU64::new(0);

/// Drains that failed in the driver (the events of that request are lost;
/// the write itself stood).
pub fn drain_errors() -> u64 {
    DRAIN_ERRORS.load(Ordering::Relaxed)
}

// The buffer rides the request's task; tokio task-locals need the `rt`
// feature the single-threaded wasm build does not carry, so wasm records
// immediately.
#[cfg(not(target_arch = "wasm32"))]
tokio::task_local! {
    static BUFFER: std::cell::RefCell<Vec<TemporalEvent>>;
    static CHANGES: std::cell::RefCell<Vec<crate::mirror::Change>>;
    static PROVENANCE: Value;
    static TENANT: Option<TenantId>;
}

/// `ANTARES_HISTORY_BACKFILL=on` (T-4665, ADR-N-054 2.1): a Core API write of
/// an instance older than the one the Entity holds goes to history only.
/// Off (the default) keeps NGSI-LD's replace, which the ETSI suites assume.
fn backfill_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("ANTARES_HISTORY_BACKFILL").as_deref() == Ok("on"))
}

/// Whether `incoming`, about to replace `current` of `attr` on the Entity
/// `doc` (expanded, as stored), is a backfill: both observed and `incoming`
/// the older. A backfill is recorded into the request's history here, and
/// the caller keeps `current`. Outside a request, or with backfill off,
/// nothing is one.
pub(crate) fn backfilled(doc: &Value, attr: &str, current: &Value, incoming: &Value) -> bool {
    if !backfill_on() {
        return false;
    }
    let at = |i: &Value| {
        i.get("observedAt").and_then(Value::as_str).and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(&antares_store::filter::canonical_datetime(s)).ok()
        })
    };
    let (Some(was), Some(now)) = (at(current), at(incoming)) else {
        return false;
    };
    let Some(id) = doc.get("id").and_then(Value::as_str) else {
        return false;
    };
    if now >= was {
        return false;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Ok(Some(tenant)) = TENANT.try_with(Clone::clone) else {
            return false;
        };
        let mut instance = incoming.clone();
        if let Some(o) = instance.as_object_mut() {
            let iid = crate::notify::instance_id(id, attr, o);
            o.entry("instanceId".to_owned())
                .or_insert(Value::String(iid));
            o.insert(
                antares_store::PROVENANCE_KEY.to_owned(),
                provenance("backfill"),
            );
        }
        let mut shell = Map::new();
        for k in ["id", "type", "createdAt", "modifiedAt", "scope"] {
            if let Some(v) = doc.get(k) {
                shell.insert(k.to_owned(), v.clone());
            }
        }
        let event = TemporalEvent {
            op: antares_store::TemporalOp::AttrModified,
            tenant,
            entity_id: id.to_owned(),
            shell: Value::Object(shell),
            attr: attr.to_owned(),
            instance,
        };
        BUFFER.try_with(|b| b.borrow_mut().push(event)).is_ok()
    }
    #[cfg(target_arch = "wasm32")]
    false
}

/// The provenance of the request in flight with its `operation`
/// (T-4665); outside a request, a write of the broker's own: `api`.
pub(crate) fn provenance(operation: &str) -> Value {
    #[cfg(not(target_arch = "wasm32"))]
    if let Ok(p) = PROVENANCE.try_with(|p| crate::provenance::with_operation(p, operation)) {
        return p;
    }
    crate::provenance::with_operation(&serde_json::json!({"channel": "api"}), operation)
}

/// Buffer one entity change for the request in flight so the matcher
/// receives the whole request at once. Handed back when no request is in
/// flight — the caller then gives it to the matcher on the spot.
pub(crate) fn buffer_change(change: crate::mirror::Change) -> Option<crate::mirror::Change> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut slot = Some(change);
        let _ = CHANGES.try_with(|b| {
            if let Some(c) = slot.take() {
                b.borrow_mut().push(c);
            }
        });
        slot
    }
    #[cfg(target_arch = "wasm32")]
    Some(change)
}

/// Hand one event to the seam: buffered when a request is in flight,
/// drained on the spot otherwise.
pub(crate) async fn push(st: &AppState, ev: TemporalEvent) {
    #[cfg(not(target_arch = "wasm32"))]
    let ev = {
        let mut slot = Some(ev);
        let buffered = BUFFER
            .try_with(|b| {
                if let Some(ev) = slot.take() {
                    b.borrow_mut().push(ev);
                }
            })
            .is_ok();
        if buffered {
            return;
        }
        match slot {
            Some(ev) => ev,
            None => return,
        }
    };
    drain(st, vec![ev]).await;
}

/// The gate chain: an event enters history only if every gate admits it.
/// Gate 1 (value-change) runs in the producer — an unchanged instance never
/// becomes an event (`changed_instances`) — and so does gate 3, retention
/// ([`records_history`]), which needs the whole Entity. Adding a gate = one more entry
/// here; producers and drivers stay untouched.
const GATES: &[fn(&AppState, &TemporalEvent) -> bool] = &[observed_gate];

/// Gate 2: ANTARES_TEMPORAL_RECORD. `all` admits everything; `observed`
/// keeps only instances that carry `observedAt` — the spec's own
/// measurement axis (4.5.7: observedAt is the default timeproperty), so
/// metadata-shaped writes leave no history; `none` admits nothing.
fn observed_gate(st: &AppState, ev: &TemporalEvent) -> bool {
    use crate::state::TemporalRecord::*;
    match st.temporal_record {
        All => true,
        Observed => ev.instance.get("observedAt").is_some(),
        None => false,
    }
}

/// An Entity's own history retention (DM-97), read from its
/// [`antares_store::HISTORY_RETENTION`] Property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnRetention {
    /// `off`: the Entity keeps its current state only.
    Off,
    /// A duration: its history is kept that long, whatever its type says.
    Keep(Duration),
}

/// What the Entity document `doc` (expanded, as stored) says of its own
/// history: `None` when it carries no `historyRetention`, `Some(Err)` with
/// the reason when the value is neither a duration nor `off` (a typo,
/// `forever`), which the rules of its type then override.
pub fn own_retention(doc: &Value) -> Option<Result<OwnRetention, String>> {
    doc.get(antares_store::HISTORY_RETENTION).map(retention_of)
}

/// What one stored `historyRetention` Property (one instance, or an array
/// whose first instance counts) says.
///
/// # Errors
/// Why it is neither a duration nor `off`.
pub fn retention_of(own: &Value) -> Result<OwnRetention, String> {
    let first = match own {
        Value::Array(all) => all.first(),
        one => Some(one),
    };
    let Some(raw) = first.and_then(|p| p.get("value")).and_then(Value::as_str) else {
        return Err("historyRetention is not a Property with a string value".into());
    };
    if raw == "off" {
        Ok(OwnRetention::Off)
    } else {
        parse_duration(raw).map(OwnRetention::Keep)
    }
}

/// Gate 3 (T-4275, DM-97), run in the producer where the whole Entity is at
/// hand: whether a write of `doc` in `tenant` records history. The most
/// specific level wins: the Entity's own `historyRetention`, then the rule
/// of one of its types (any `off` among them records nothing), then the
/// Tenant's `*` rule.
pub(crate) fn records_history(st: &AppState, tenant: &TenantId, doc: &Value) -> bool {
    match own_retention(doc) {
        Some(Ok(OwnRetention::Off)) => return false,
        Some(Ok(OwnRetention::Keep(_))) => return true,
        Some(Err(_)) | None => {}
    }
    let rules = st
        .history_off
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if rules.is_empty() {
        return true;
    }
    let tenant = tenant.as_str();
    let types: Vec<&str> = match doc.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let has = |set: &std::collections::HashSet<(String, String)>| {
        types
            .iter()
            .any(|t| set.contains(&(tenant.to_owned(), (*t).to_owned())))
    };
    if has(&rules.off) {
        return false;
    }
    has(&rules.ruled) || !rules.space_off.contains(tenant)
}

/// The last row each attribute recorded `onChange` wrote, by (Tenant,
/// Entity, attribute, datasetId): its payload and its time (T-4665).
type LastRows = std::collections::HashMap<
    (String, String, String, String),
    (Value, chrono::DateTime<chrono::Utc>),
>;
static LAST_ROWS: std::sync::LazyLock<std::sync::Mutex<LastRows>> =
    std::sync::LazyLock::new(Default::default);
/// The most attributes [`LAST_ROWS`] remembers.
// ponytail: one map per process, cleared when full or restarted; the next
// write of each attribute then records a row, which costs a row, never one
// lost. A shared store if replicas ever record far more than one row per
// heartbeat between them.
const LAST_ROWS_MAX: usize = 500_000;

/// The Attributes of the temporal `doc` its type records `onChange`
/// (ADR-N-054 2.5): a query carries their level into a window and weighs
/// their values by the time each held.
pub(crate) fn held_attrs(
    st: &AppState,
    tenant: &TenantId,
    doc: &Value,
) -> std::collections::HashSet<String> {
    let rules = st
        .history_off
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (false, Some(obj)) = (rules.recording.is_empty(), doc.as_object()) else {
        return Default::default();
    };
    let types: Vec<&str> = match obj.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let recordings: Vec<_> = types
        .iter()
        .filter_map(|t| {
            rules
                .recording
                .get(&(tenant.as_str().to_owned(), (*t).to_owned()))
        })
        .collect();
    obj.keys()
        .filter(|k| recordings.iter().any(|r| r.of(k).is_some()))
        .cloned()
        .collect()
}

/// Does some type of `tenant` record an Attribute `onChange`? Its
/// aggregations are then computed here, never pushed into the store.
pub(crate) fn records_on_change(st: &AppState, tenant: &TenantId) -> bool {
    st.history_off
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .recording
        .keys()
        .any(|(t, _)| t == tenant.as_str())
}

/// Gate 4 (T-4665, ADR-N-054 2.2): whether an instance of `attr`, changed
/// on the Entity `entity` with `types` in `tenant`, writes a row. `every`
/// (no rule) always does; `onChange` when its value moved past the
/// deadband or minimum distance since the attribute's last row, when it
/// differs at all from a row of the same `observedAt` (a correction), when
/// it is older than that row (a backfill), or once the heartbeat passed.
pub(crate) fn admits_change(
    st: &AppState,
    tenant: &TenantId,
    types: &[&str],
    entity: &str,
    attr: &str,
    instance: &Value,
) -> bool {
    let mode = {
        let rules = st
            .history_off
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if rules.recording.is_empty() {
            return true;
        }
        types.iter().find_map(|t| {
            rules
                .recording
                .get(&(tenant.as_str().to_owned(), (*t).to_owned()))
                .and_then(|r| r.of(attr))
        })
    };
    let Some(mode) = mode else { return true };
    let at = instance
        .get("observedAt")
        .and_then(Value::as_str)
        .and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(&antares_store::filter::canonical_datetime(s)).ok()
        })
        .map_or_else(chrono::Utc::now, |t| t.with_timezone(&chrono::Utc));
    let payload = payload_of(instance);
    let key = (
        tenant.as_str().to_owned(),
        entity.to_owned(),
        attr.to_owned(),
        instance
            .get("datasetId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    );
    let mut last = LAST_ROWS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let writes = match last.get(&key) {
        None => true,
        Some((was, when)) if at == *when => payload != *was,
        Some((_, when)) if at < *when => return true,
        Some((was, when)) => {
            at.signed_duration_since(*when)
                .to_std()
                .is_ok_and(|d| d >= mode.heartbeat)
                || moved(&mode, was, &payload)
        }
    };
    if writes {
        if last.len() >= LAST_ROWS_MAX {
            last.clear();
        }
        last.insert(key, (payload, at));
    }
    writes
}

/// An instance without its stamps and provenance: what a row records.
fn payload_of(instance: &Value) -> Value {
    let mut p = instance.clone();
    if let Some(o) = p.as_object_mut() {
        for k in [
            "instanceId",
            "observedAt",
            "createdAt",
            "modifiedAt",
            antares_store::PROVENANCE_KEY,
        ] {
            o.remove(k);
        }
    }
    p
}

/// Whether `now` moved from `was` enough to be a row under `mode`.
fn moved(mode: &crate::state::OnChange, was: &Value, now: &Value) -> bool {
    let number = |v: &Value| v.get("value").and_then(Value::as_f64);
    if let (Some(band), Some(a), Some(b)) = (mode.deadband, number(was), number(now)) {
        return (a - b).abs() > band;
    }
    let point = |v: &Value| {
        let g = v.get("value")?;
        let c = g.get("coordinates")?.as_array()?;
        (g.get("type")?.as_str()? == "Point").then_some((c.first()?.as_f64()?, c.get(1)?.as_f64()?))
    };
    if let (Some(metres), Some(a), Some(b)) = (mode.min_distance, point(was), point(now)) {
        return haversine(a, b) > metres;
    }
    was != now
}

/// Metres between two (longitude, latitude) points on a sphere of the
/// Earth's mean radius: within a fraction of a percent at a city's scale.
fn haversine((lon1, lat1): (f64, f64), (lon2, lat2): (f64, f64)) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * a.sqrt().asin()
}

/// The bounds a duration has to sit in: one minute to ten years.
const SHORTEST: Duration = Duration::from_secs(60);
const LONGEST: Duration = Duration::from_hours(87_600);

/// A duration as ISO 8601 (`PT5M`, `PT8H`, `P30D`, `P1DT12H`) or in short
/// form (`90s`, `5m`, `8h`, `14d`), between one minute and ten years.
///
/// # Errors
/// Why `raw` is not one.
pub fn parse_duration(raw: &str) -> Result<Duration, String> {
    parse_duration_within(raw, SHORTEST, LONGEST)
}

/// A duration as [`parse_duration`] reads it, between `shortest` and
/// `longest` (the quality sweep's `maxAge` starts at one second, T-4018).
///
/// # Errors
/// Why `raw` is not one.
pub fn parse_duration_within(
    raw: &str,
    shortest: Duration,
    longest: Duration,
) -> Result<Duration, String> {
    let bad = || format!("{raw:?} is not a duration such as PT5M, PT8H, P30D, 5m, 8h or 14d");
    let secs = if let Some(iso) = raw.strip_prefix('P') {
        let (days, time) = match iso.split_once('T') {
            Some((d, t)) if !t.is_empty() => (d, Some(t)),
            Some(_) => return Err(bad()),
            None => (iso, None),
        };
        let mut total = 0u64;
        if !days.is_empty() {
            total = days
                .strip_suffix('D')
                .and_then(|n| n.parse::<u64>().ok())
                .ok_or_else(bad)?
                .checked_mul(86_400)
                .ok_or_else(bad)?;
        } else if time.is_none() {
            return Err(bad());
        }
        let mut rest = time.unwrap_or_default();
        for (unit, scale) in [('H', 3600u64), ('M', 60), ('S', 1)] {
            if let Some((n, tail)) = rest.split_once(unit) {
                let n = n.parse::<u64>().map_err(|_| bad())?;
                total = n
                    .checked_mul(scale)
                    .and_then(|n| total.checked_add(n))
                    .ok_or_else(bad)?;
                rest = tail;
            }
        }
        if !rest.is_empty() {
            return Err(bad());
        }
        total
    } else {
        let split = raw.len().checked_sub(1).ok_or_else(bad)?;
        let (n, unit) = (
            raw.get(..split).ok_or_else(bad)?,
            raw.get(split..).ok_or_else(bad)?,
        );
        let scale = match unit {
            "s" => 1u64,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            _ => return Err(bad()),
        };
        n.parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
            .ok_or_else(bad)?
    };
    let d = Duration::from_secs(secs);
    if d < shortest || d > longest {
        return Err(format!(
            "{raw:?} is outside {}s to {}s",
            shortest.as_secs(),
            longest.as_secs()
        ));
    }
    Ok(d)
}

/// The consumer side: one `event_list` call per drained batch.
pub(crate) async fn drain(st: &AppState, evs: Vec<TemporalEvent>) {
    let evs: Vec<TemporalEvent> = evs
        .into_iter()
        .filter(|ev| GATES.iter().all(|gate| gate(st, ev)))
        .collect();
    if evs.is_empty() {
        return;
    }
    if let Err(e) = st.temporal.event_list(&evs).await {
        DRAIN_ERRORS.fetch_add(1, Ordering::Relaxed);
        metrics::counter!("antares_temporal_drain_errors_total").increment(1);
        tracing::warn!(events = evs.len(), "temporal drain failed: {e}");
    }
}

/// Router layer: scopes the buffer over the handler and drains it once the
/// response is built. Drained BEFORE the response is returned so
/// read-your-writes holds at any store latency (the ETSI temporal suites
/// read history straight after the write).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn layer(
    axum::extract::State(st): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let provenance = crate::provenance::of_request(req.headers(), st.provenance_from.as_deref());
    let tenant = crate::negotiate::tenant_from(req.headers()).ok();
    let (resp, evs, changes) = BUFFER
        .scope(std::cell::RefCell::new(Vec::new()), async {
            CHANGES
                .scope(std::cell::RefCell::new(Vec::new()), async {
                    PROVENANCE
                        .scope(
                            provenance,
                            TENANT.scope(tenant, async {
                                let resp = next.run(req).await;
                                (resp, BUFFER.with(|b| b.take()), CHANGES.with(|c| c.take()))
                            }),
                        )
                        .await
                })
                .await
        })
        .await;
    drain(&st, evs).await;
    if !changes.is_empty() {
        if let Some(flush) = &st.change_flush {
            flush(changes);
        }
    }
    resp
}

/// No buffer on wasm (pushes drain on the spot): the layer is a pass-through
/// so the router composes identically on both targets.
#[cfg(target_arch = "wasm32")]
pub(crate) async fn layer(
    axum::extract::State(_st): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    next.run(req).await
}

/// delete_temporal_on_core_delete: entity deletion removes its temporal
/// representation too (suite configuration parity). Skipped on bus=nats
/// api pods — the recorder applies the entityDeleted fence instead.
pub(crate) async fn mirror_delete_entity(st: &AppState, tenant: &TenantId, id: &str) {
    if !st.record_locally() {
        return;
    }
    if let Err(e) = st.temporal.delete(tenant, id).await {
        tracing::warn!("temporal mirror delete failed: {e}");
    }
}

/// 4.5.7/4.5.8: "In case the Property is deleted, an instance of the
/// Property is recorded with its value set to the URI "urn:ngsi-ld:null"
/// and the deletedAt Temporal Property set" (object for a Relationship;
/// typed null shapes for the LanguageProperty/JsonProperty/Vocab/List
/// subtypes). Each recorded instance carries an instanceId — the clause
/// SHOULD that makes 5.6.14/5.6.15 selective modification possible.
pub(crate) async fn mirror_delete_attr(
    st: &AppState,
    tenant: &TenantId,
    id: &str,
    attr_iri: &str,
    dataset_id: Option<&str>,
    ts: &str,
) {
    let r = st
        .temporal
        .mutate(tenant, id, |doc| {
            // The mirror writes nothing into a document the temporal driver
            // handed back in a shape the contract forbids.
            let Some(target) = doc.as_object_mut() else {
                return Ok::<(), std::convert::Infallible>(());
            };
            if attr_iri == "scope" {
                // scope deletion: a deleted scope is the NGSI-LD Null (4.18:
                // urn:ngsi-ld:null appears only for deleted scopes; 4.5.0)
                let inst = serde_json::json!({
                    "type": "Property",
                    "value": "urn:ngsi-ld:null",
                    "instanceId": format!("urn:ngsi-ld:Instance:{}", uuid::Uuid::new_v4()),
                    "deletedAt": ts,
                });
                match target.get_mut("scope").and_then(Value::as_array_mut) {
                    Some(arr) if arr.first().is_some_and(|i| i.is_object()) => arr.push(inst),
                    _ => {
                        target.insert("scope".into(), Value::Array(vec![inst]));
                    }
                }
                return Ok::<(), std::convert::Infallible>(());
            }
            if let Some(arr) = target.get_mut(attr_iri).and_then(Value::as_array_mut) {
                if arr.is_empty() {
                    return Ok(());
                }
                let atype = arr
                    .first()
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("Property")
                    .to_owned();
                let mut inst = Map::new();
                inst.insert("type".into(), Value::String(atype.clone()));
                let null = Value::String("urn:ngsi-ld:null".into());
                match atype.as_str() {
                    "Relationship" => {
                        inst.insert("object".into(), null);
                    }
                    "LanguageProperty" => {
                        inst.insert(
                            "languageMap".into(),
                            serde_json::json!({"@none": "urn:ngsi-ld:null"}),
                        );
                    }
                    "JsonProperty" => {
                        inst.insert("json".into(), null);
                    }
                    "VocabProperty" => {
                        inst.insert("vocab".into(), null);
                    }
                    "ListProperty" => {
                        inst.insert("valueList".into(), null);
                    }
                    "ListRelationship" => {
                        inst.insert("objectList".into(), null);
                    }
                    _ => {
                        inst.insert("value".into(), null);
                    }
                }
                if let Some(ds) = dataset_id {
                    inst.insert("datasetId".into(), Value::String(ds.to_owned()));
                }
                inst.insert(
                    "instanceId".into(),
                    Value::String(format!("urn:ngsi-ld:Instance:{}", uuid::Uuid::new_v4())),
                );
                inst.insert("deletedAt".into(), Value::String(ts.to_owned()));
                inst.insert(antares_store::PROVENANCE_KEY.into(), provenance("delete"));
                arr.push(Value::Object(inst));
            }
            Ok(())
        })
        .await;
    if let Err(e) = r {
        tracing::warn!("temporal attr mirror failed: {e}");
    }
}
