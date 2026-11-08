// SPDX-License-Identifier: EUPL-1.2
//! The narrowed handles of the views a surface served (T-5896). A handle
//! carries the view's state and, after its first in-process call, that
//! state's router: about 250 KiB each, measured over 690 views. A handle no
//! request used for [`VIEW_IDLE_ENV`] (default one hour) is dropped, and the
//! next request builds it again from the table as it is then (~2 ms).
//!
//! The handle is a cache of the table, never its source: the narrowing,
//! operations and policy live in the view table and the engine, which
//! eviction never touches. A build runs under its view's own build lock,
//! outside the cache's lock (T-6611, R3-003): concurrent first requests for
//! a view share one build and none answers before it is complete, while
//! requests through every other view go on; a failed build caches nothing.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// How long a view's handle is kept after the last request through it,
/// as `antares_api::history::parse_duration` reads it (`1h`, `PT30M`).
/// Default one hour.
pub const VIEW_IDLE_ENV: &str = "ANTARES_VIEW_IDLE";

/// [`VIEW_IDLE_ENV`] unset.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(3600);

/// The longest a cache goes between sweeps while every request hits.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

static IDLE: OnceLock<Duration> = OnceLock::new();

/// The idle bound `raw` names, [`DEFAULT_IDLE`] when unset.
///
/// # Errors
/// A value that is not a duration of one minute to ten years.
pub(crate) fn parse_idle(raw: Option<&str>) -> Result<Duration, String> {
    raw.map_or(Ok(DEFAULT_IDLE), |raw| {
        antares_api::history::parse_duration(raw.trim())
            .map_err(|e| format!("{VIEW_IDLE_ENV}: {e}"))
    })
}

/// Read [`VIEW_IDLE_ENV`] once, at startup: a bad value stops the broker.
///
/// # Errors
/// [`parse_idle`]'s.
pub(crate) fn configure() -> Result<(), String> {
    let idle = parse_idle(std::env::var(VIEW_IDLE_ENV).ok().as_deref())?;
    let _ = IDLE.set(idle);
    Ok(())
}

/// The idle bound in force.
pub(crate) fn idle() -> Duration {
    IDLE.get().copied().unwrap_or(DEFAULT_IDLE)
}

/// Built handles by view id, each with when a request last used it.
pub(crate) struct ViewCache<T> {
    held: BTreeMap<String, (T, Instant)>,
    swept: Option<Instant>,
    /// The build lock of each view a request is building now.
    building: BTreeMap<String, Arc<Mutex<()>>>,
}

impl<T> Default for ViewCache<T> {
    fn default() -> Self {
        Self {
            held: BTreeMap::new(),
            swept: None,
            building: BTreeMap::new(),
        }
    }
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<T: Clone> ViewCache<T> {
    /// The handle of `id` when one is held and `current` says it was built
    /// from the view the table holds now; else `build`'s, held from `now`
    /// on. `loaded` says which ids the table still holds: every other one,
    /// and every handle idle for `idle`, is dropped on a build and at least
    /// once a minute; the dropped handles are returned, for the caller to
    /// free outside the lock (690 of them take ~140 ms). Builds are counted
    /// on `antares_view_builds_total`. `cache`'s lock is never held over
    /// `build`.
    pub(crate) fn get_or_build(
        cache: &Mutex<Self>,
        id: &str,
        current: impl Fn(&T) -> bool,
        loaded: impl Fn(&str) -> bool,
        now: Instant,
        idle: Duration,
        build: impl FnOnce() -> Option<T>,
    ) -> (Option<T>, Vec<T>) {
        let mut gone = Vec::new();
        let slot = {
            let mut c = locked(cache);
            if c.swept
                .is_none_or(|at| now.saturating_duration_since(at) >= SWEEP_EVERY)
            {
                c.sweep(&loaded, now, idle, &mut gone);
            }
            if let Some(held) = c.hit(id, &current, now) {
                return (Some(held), gone);
            }
            c.building.entry(id.to_owned()).or_default().clone()
        };
        let _building = locked(&slot);
        // a build that ran while this one waited for the slot
        if let Some(held) = locked(cache).hit(id, &current, now) {
            return (Some(held), gone);
        }
        let made = build();
        let mut c = locked(cache);
        if c.building.get(id).is_some_and(|b| Arc::ptr_eq(b, &slot)) {
            c.building.remove(id);
        }
        let Some(made) = made else {
            return (None, gone);
        };
        metrics::counter!("antares_view_builds_total").increment(1);
        c.sweep(&loaded, now, idle, &mut gone);
        c.held.insert(id.to_owned(), (made.clone(), now));
        (Some(made), gone)
    }

    fn hit(&mut self, id: &str, current: &impl Fn(&T) -> bool, now: Instant) -> Option<T> {
        let (held, used) = self.held.get_mut(id).filter(|(h, _)| current(h))?;
        *used = now;
        Some(held.clone())
    }

    fn sweep(
        &mut self,
        loaded: &impl Fn(&str) -> bool,
        now: Instant,
        idle: Duration,
        gone: &mut Vec<T>,
    ) {
        let (keep, drop): (BTreeMap<_, _>, BTreeMap<_, _>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|(id, (_, used))| loaded(id) && now.saturating_duration_since(*used) < idle);
        self.held = keep;
        gone.extend(drop.into_values().map(|(h, _)| h));
        self.swept = Some(now);
    }
}

/// Free `gone` off the request path: on a blocking thread when a tokio
/// runtime runs, else here.
pub(crate) fn release<T: Send + 'static>(gone: Vec<T>) {
    if gone.is_empty() {
        return;
    }
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            rt.spawn_blocking(move || drop(gone));
        }
        Err(_) => drop(gone),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;

    const HOUR: Duration = Duration::from_secs(3600);

    fn get(
        c: &Mutex<ViewCache<u32>>,
        id: &str,
        version: u32,
        now: Instant,
        builds: &AtomicUsize,
    ) -> Option<u32> {
        ViewCache::get_or_build(
            c,
            id,
            |held| *held == version,
            |_| true,
            now,
            HOUR,
            || {
                builds.fetch_add(1, Ordering::SeqCst);
                Some(version)
            },
        )
        .0
    }

    #[test]
    fn concurrent_first_requests_build_once_and_all_get_the_view() {
        let cache = Arc::new(Mutex::new(ViewCache::<u32>::default()));
        let builds = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(16));
        let now = Instant::now();
        let got: Vec<Option<u32>> = (0..16)
            .map(|_| {
                let (cache, builds, start) = (cache.clone(), builds.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    get(&cache, "cars", 7, now, &builds)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap_or(None))
            .collect();
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert_eq!(got, vec![Some(7); 16]);
    }

    /// A view building (cold, ~2 ms in prod) leaves a hit on another view
    /// free to answer (T-6611, R3-003): on dev b683bfc3 the build ran under
    /// the one cache lock every surface request takes.
    #[test]
    fn a_build_does_not_hold_up_a_hit_on_another_view() {
        let cache = Arc::new(Mutex::new(ViewCache::<u32>::default()));
        let builds = AtomicUsize::new(0);
        let now = Instant::now();
        assert_eq!(get(&cache, "cars", 1, now, &builds), Some(1));
        let (building, release) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
        let slow = {
            let (cache, building, release) = (cache.clone(), building.clone(), release.clone());
            std::thread::spawn(move || {
                ViewCache::get_or_build(
                    &cache,
                    "bikes",
                    |_| true,
                    |_| true,
                    now,
                    HOUR,
                    || {
                        building.wait();
                        release.wait();
                        Some(2)
                    },
                )
                .0
            })
        };
        building.wait();
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let cache = cache.clone();
            std::thread::spawn(move || {
                let _ = tx.send(get(&cache, "cars", 1, now, &AtomicUsize::new(0)));
            });
        }
        let hit = rx.recv_timeout(Duration::from_secs(5));
        release.wait();
        assert_eq!(hit, Ok(Some(1)), "the hit waited for another view's build");
        assert_eq!(slow.join().ok().flatten(), Some(2));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_idle_view_is_dropped_and_reloads_the_current_version() {
        let c = Mutex::new(ViewCache::default());
        let builds = AtomicUsize::new(0);
        let t0 = Instant::now();
        assert_eq!(get(&c, "cars", 1, t0, &builds), Some(1));
        assert_eq!(get(&c, "cars", 1, t0 + HOUR / 2, &builds), Some(1));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        // a request on another view an hour after the last use of `cars` sweeps it
        assert_eq!(get(&c, "bikes", 1, t0 + HOUR / 2 + HOUR, &builds), Some(1));
        assert_eq!(locked(&c).held.len(), 1);
        // the view changed while evicted: the reload builds the table's version, not the old one
        assert_eq!(get(&c, "cars", 2, t0 + 2 * HOUR, &builds), Some(2));
        assert_eq!(builds.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_hit_sweeps_idle_views_once_a_minute() {
        let c = Mutex::new(ViewCache::default());
        let builds = AtomicUsize::new(0);
        let t0 = Instant::now();
        get(&c, "cars", 1, t0, &builds);
        get(
            &c,
            "bikes",
            1,
            t0 + HOUR.saturating_sub(SWEEP_EVERY),
            &builds,
        );
        get(&c, "bikes", 1, t0 + HOUR, &builds);
        assert_eq!(locked(&c).held.len(), 1, "cars went on the hit's sweep");
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_changed_view_is_rebuilt_and_a_request_never_gets_the_old_one() {
        let c = Mutex::new(ViewCache::default());
        let builds = AtomicUsize::new(0);
        let t0 = Instant::now();
        assert_eq!(get(&c, "cars", 1, t0, &builds), Some(1));
        assert_eq!(get(&c, "cars", 2, t0, &builds), Some(2));
        assert_eq!(get(&c, "cars", 2, t0, &builds), Some(2));
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_removed_view_goes_on_the_next_build() {
        let c = Mutex::new(ViewCache::default());
        let t0 = Instant::now();
        for id in ["cars", "bikes"] {
            ViewCache::get_or_build(&c, id, |_| true, |_| true, t0, HOUR, || Some(1_u32));
        }
        let (_, gone) = ViewCache::get_or_build(
            &c,
            "buses",
            |_| true,
            |id| id != "cars",
            t0,
            HOUR,
            || Some(2),
        );
        assert_eq!(locked(&c).held.len(), 2);
        assert_eq!(gone, vec![1], "the removed handle is handed back to free");
    }

    #[test]
    fn a_failed_build_is_not_cached_and_is_retried() {
        let c = Mutex::new(ViewCache::<u32>::default());
        let t0 = Instant::now();
        let (none, _) = ViewCache::get_or_build(&c, "cars", |_| true, |_| true, t0, HOUR, || None);
        assert_eq!(none, None);
        assert_eq!(locked(&c).held.len(), 0);
        let (some, _) =
            ViewCache::get_or_build(&c, "cars", |_| true, |_| true, t0, HOUR, || Some(3));
        assert_eq!(some, Some(3));
    }

    #[test]
    fn released_handles_are_freed_with_or_without_a_runtime() {
        let held = Arc::new(());
        release(vec![held.clone()]);
        assert_eq!(
            Arc::strong_count(&held),
            1,
            "freed at once outside a runtime"
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");
        rt.block_on(async {
            release(Vec::<Arc<()>>::new());
            release(vec![held.clone()]);
        });
        drop(rt);
        assert_eq!(Arc::strong_count(&held), 1, "freed on the blocking pool");
    }

    #[test]
    fn the_idle_bound_defaults_to_an_hour_and_refuses_a_bad_value() {
        assert_eq!(parse_idle(None), Ok(DEFAULT_IDLE));
        assert_eq!(parse_idle(Some("30m")), Ok(Duration::from_mins(30)));
        assert_eq!(parse_idle(Some(" PT2H ")), Ok(Duration::from_hours(2)));
        let bad = parse_idle(Some("soon")).unwrap_err();
        assert!(bad.starts_with("ANTARES_VIEW_IDLE: "), "{bad}");
        assert!(parse_idle(Some("10s")).is_err(), "under a minute");
    }
}
