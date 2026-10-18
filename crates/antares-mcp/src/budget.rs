// SPDX-License-Identifier: EUPL-1.2
//! Adaptive summary budget (T-4680): monitors system resources (cgroup v2 memory,
//! CPU pressure, `/proc/meminfo`) to categorize system state into Normal, Busy, or
//! Tight, enforcing row and byte limits as well as concurrency bounds.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// The adaptive operating state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Normal,
    Busy,
    Tight,
}

impl State {
    /// String representation for metrics and logging.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            State::Normal => "normal",
            State::Busy => "busy",
            State::Tight => "tight",
        }
    }
}

/// The resource budget allocated for a summary execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Budget {
    pub max_rows: usize,
    pub max_bytes: usize,
}

fn parse_env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn parse_env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(default, |n: usize| n.clamp(min, max))
}

pub(crate) static BUSY_MEMORY_PCT: LazyLock<f64> =
    LazyLock::new(|| parse_env_f64(crate::SUMMARY_BUSY_MEMORY_PCT_ENV, 70.0));
pub(crate) static TIGHT_MEMORY_PCT: LazyLock<f64> =
    LazyLock::new(|| parse_env_f64(crate::SUMMARY_TIGHT_MEMORY_PCT_ENV, 85.0));
pub(crate) static BUSY_CPU_AVG10: LazyLock<f64> =
    LazyLock::new(|| parse_env_f64(crate::SUMMARY_BUSY_CPU_AVG10_ENV, 30.0));
pub(crate) static TIGHT_CPU_AVG10: LazyLock<f64> =
    LazyLock::new(|| parse_env_f64(crate::SUMMARY_TIGHT_CPU_AVG10_ENV, 60.0));
pub(crate) static BUSY_NODE_MIB: LazyLock<u64> = LazyLock::new(|| {
    u64::try_from(parse_env_usize(
        crate::SUMMARY_BUSY_NODE_MIB_ENV,
        1536,
        1,
        usize::MAX,
    ))
    .unwrap_or(u64::MAX)
});
pub(crate) static TIGHT_NODE_MIB: LazyLock<u64> = LazyLock::new(|| {
    u64::try_from(parse_env_usize(
        crate::SUMMARY_TIGHT_NODE_MIB_ENV,
        819,
        1,
        usize::MAX,
    ))
    .unwrap_or(u64::MAX)
});

pub(crate) static SUMMARY_ROWS: LazyLock<usize> =
    LazyLock::new(|| parse_env_usize(crate::SUMMARY_ROWS_ENV, 1000, 1, 5000));
pub(crate) static SUMMARY_MAX_BYTES: LazyLock<usize> = LazyLock::new(|| {
    parse_env_usize(
        crate::SUMMARY_MAX_BYTES_ENV,
        32 * 1024 * 1024,
        1024,
        1024 * 1024 * 1024,
    )
});
pub(crate) static SUMMARY_CONCURRENCY: LazyLock<usize> =
    LazyLock::new(|| parse_env_usize(crate::SUMMARY_CONCURRENCY_ENV, 4, 1, 256));

pub(crate) static CONCURRENCY: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(*SUMMARY_CONCURRENCY));

/// Compute budget limits for a given state.
pub(crate) fn budget_for(state: State) -> Budget {
    match state {
        State::Normal => Budget {
            max_rows: *SUMMARY_ROWS,
            max_bytes: *SUMMARY_MAX_BYTES,
        },
        State::Busy => Budget {
            max_rows: 250,
            max_bytes: 8 * 1024 * 1024,
        },
        State::Tight => Budget {
            max_rows: 0,
            max_bytes: 0,
        },
    }
}

fn ratio_pct(current: u64, max: u64) -> f64 {
    let mut c = current;
    let mut m = max;
    while m > u64::from(u32::MAX) || c > u64::from(u32::MAX) {
        c = c.checked_div(2).unwrap_or(0);
        m = m.checked_div(2).unwrap_or(1);
    }
    let cur_u32 = u32::try_from(c).unwrap_or(u32::MAX);
    let max_u32 = u32::try_from(m).unwrap_or(1).max(1);
    (f64::from(cur_u32) / f64::from(max_u32)) * 100.0
}

/// Read pod memory current and max from cgroup root and return percentage if bounded.
fn read_memory_pct(cgroup_root: &Path) -> Option<f64> {
    let current_str = std::fs::read_to_string(cgroup_root.join("memory.current")).ok()?;
    let current = current_str.trim().parse::<u64>().ok()?;

    let max_str = std::fs::read_to_string(cgroup_root.join("memory.max")).ok()?;
    let max_trimmed = max_str.trim();
    if max_trimmed == "max" {
        return None;
    }
    let max = max_trimmed.parse::<u64>().ok()?;
    if max == 0 {
        return None;
    }
    Some(ratio_pct(current, max))
}

/// Read cpu.pressure avg10 value.
fn read_cpu_pressure(cgroup_root: &Path) -> Option<f64> {
    let content = std::fs::read_to_string(cgroup_root.join("cpu.pressure")).ok()?;
    for line in content.lines() {
        if line.starts_with("some ") {
            if let Some(pos) = line.find("avg10=") {
                let rest = line.get(pos.saturating_add(6)..line.len())?;
                let val_str = rest.split_whitespace().next()?;
                return val_str.parse::<f64>().ok();
            }
        }
    }
    None
}

/// Read `MemAvailable` in MiB from `/proc/meminfo`.
fn read_mem_available(proc_root: &Path) -> Option<u64> {
    let content = std::fs::read_to_string(proc_root.join("meminfo")).ok()?;
    for line in content.lines() {
        if line.starts_with("MemAvailable:") {
            let mut parts = line.split_whitespace();
            let _ = parts.next(); // "MemAvailable:"
            let kb_str = parts.next()?;
            let kb = kb_str.parse::<u64>().ok()?;
            return Some(kb.checked_div(1024).unwrap_or(0));
        }
    }
    None
}

static WARNED: AtomicBool = AtomicBool::new(false);

/// Evaluate state against directories.
pub(crate) fn evaluate_state(cgroup_root: &Path, proc_root: &Path) -> State {
    let mem_pct = read_memory_pct(cgroup_root);
    let cpu_avg10 = read_cpu_pressure(cgroup_root);
    let mem_avail_mib = read_mem_available(proc_root);

    if (mem_pct.is_none() || cpu_avg10.is_none() || mem_avail_mib.is_none())
        && !WARNED.swap(true, Ordering::Relaxed)
    {
        tracing::warn!("cgroup v2 or proc files not available or unparsable; unknown signals treated as normal");
    }

    let tight_mem = mem_pct.is_some_and(|p| p > *TIGHT_MEMORY_PCT);
    let tight_cpu = cpu_avg10.is_some_and(|c| c > *TIGHT_CPU_AVG10);
    let tight_node = mem_avail_mib.is_some_and(|m| m < *TIGHT_NODE_MIB);

    if tight_mem || tight_cpu || tight_node {
        return State::Tight;
    }

    let normal_mem = mem_pct.is_none_or(|p| p < *BUSY_MEMORY_PCT);
    let normal_cpu = cpu_avg10.is_none_or(|c| c < *BUSY_CPU_AVG10);
    let normal_node = mem_avail_mib.is_none_or(|m| m > *BUSY_NODE_MIB);

    if normal_mem && normal_cpu && normal_node {
        State::Normal
    } else {
        State::Busy
    }
}

static STATE_CACHE: Mutex<Option<(Instant, State)>> = Mutex::new(None);

/// Return the current system state, cached for 1 second.
pub(crate) fn current_state() -> State {
    let now = Instant::now();
    if let Ok(guard) = STATE_CACHE.lock() {
        if let Some((cached_at, state)) = *guard {
            if now.duration_since(cached_at) < Duration::from_secs(1) {
                return state;
            }
        }
    }

    let state = evaluate_state(Path::new("/sys/fs/cgroup"), Path::new("/proc"));

    if let Ok(mut guard) = STATE_CACHE.lock() {
        *guard = Some((now, state));
    }

    state
}

/// Acquire semaphore permits based on state. Normal takes 1, Busy/Tight takes all.
pub(crate) async fn acquire_permit(
    semaphore: &tokio::sync::Semaphore,
    total_permits: usize,
    state: State,
    wait: Duration,
) -> Option<tokio::sync::SemaphorePermit<'_>> {
    let permits = match state {
        State::Normal => 1u32,
        State::Busy | State::Tight => u32::try_from(total_permits).unwrap_or(u32::MAX),
    };
    tokio::time::timeout(wait, semaphore.acquire_many(permits))
        .await
        .ok()
        .and_then(Result::ok)
}

/// How long a summary waits for its permits before it is answered "busy".
pub(crate) const PERMIT_WAIT: Duration = Duration::from_secs(2);

/// A single row larger than this is skipped and counted, never loaded.
pub(crate) const MAX_ROW_BYTES: usize = 1024 * 1024;

/// What the loader does with the next row under the byte budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admit {
    /// Keep it; it costs this many bytes.
    Keep(usize),
    /// Over [`MAX_ROW_BYTES`] alone: skip and count it.
    Skip,
    /// It would pass the budget: stop loading.
    Full,
}

/// The byte budget: `used` bytes are kept, `max` may be.
pub(crate) fn admit(row: &serde_json::Value, used: usize, max: usize) -> Admit {
    let size = serde_json::to_vec(row).map_or(usize::MAX, |b| b.len());
    if size > MAX_ROW_BYTES {
        Admit::Skip
    } else if used.saturating_add(size) > max {
        Admit::Full
    } else {
        Admit::Keep(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_dir(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let base =
            std::env::temp_dir().join(format!("antares_budget_test_{name}_{}", std::process::id()));
        let cgroup = base.join("cgroup");
        let proc = base.join("proc");
        let _ = std::fs::remove_dir_all(&base);
        assert!(std::fs::create_dir_all(&cgroup).is_ok());
        assert!(std::fs::create_dir_all(&proc).is_ok());
        (base, cgroup, proc)
    }

    #[test]
    fn missing_or_unparsable_files_default_to_normal() {
        let (base, cgroup, proc) = temp_test_dir("missing");
        let state = evaluate_state(&cgroup, &proc);
        assert_eq!(state, State::Normal);

        assert!(std::fs::write(cgroup.join("memory.current"), "bad").is_ok());
        assert!(std::fs::write(cgroup.join("memory.max"), "bad").is_ok());
        assert!(std::fs::write(cgroup.join("cpu.pressure"), "some bad").is_ok());
        assert!(std::fs::write(proc.join("meminfo"), "MemAvailable: bad kB").is_ok());

        let state2 = evaluate_state(&cgroup, &proc);
        assert_eq!(state2, State::Normal);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn threshold_edges_evaluate_accurately() {
        let (base, cgroup, proc) = temp_test_dir("edges");

        // Normal: memory < 70%, cpu < 30, mem_avail > 1536 MiB (1536 * 1024 kB)
        assert!(std::fs::write(cgroup.join("memory.current"), "6000").is_ok());
        assert!(std::fs::write(cgroup.join("memory.max"), "10000").is_ok()); // 60%
        assert!(std::fs::write(
            cgroup.join("cpu.pressure"),
            "some avg10=20.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .is_ok());
        assert!(std::fs::write(proc.join("meminfo"), "MemAvailable:    2097152 kB\n").is_ok()); // 2048 MiB
        assert_eq!(evaluate_state(&cgroup, &proc), State::Normal);

        // Busy via memory (75%)
        assert!(std::fs::write(cgroup.join("memory.current"), "7500").is_ok());
        assert_eq!(evaluate_state(&cgroup, &proc), State::Busy);

        // Tight via memory (90%)
        assert!(std::fs::write(cgroup.join("memory.current"), "9000").is_ok());
        assert_eq!(evaluate_state(&cgroup, &proc), State::Tight);

        // Reset memory to 60%, test CPU Busy (35)
        assert!(std::fs::write(cgroup.join("memory.current"), "6000").is_ok());
        assert!(std::fs::write(
            cgroup.join("cpu.pressure"),
            "some avg10=35.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .is_ok());
        assert_eq!(evaluate_state(&cgroup, &proc), State::Busy);

        // Tight via CPU (65)
        assert!(std::fs::write(
            cgroup.join("cpu.pressure"),
            "some avg10=65.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .is_ok());
        assert_eq!(evaluate_state(&cgroup, &proc), State::Tight);

        // Reset CPU to 10, test MemAvailable Busy (1000 MiB: 1024000 kB)
        assert!(std::fs::write(
            cgroup.join("cpu.pressure"),
            "some avg10=10.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .is_ok());
        assert!(std::fs::write(proc.join("meminfo"), "MemAvailable:    1024000 kB\n").is_ok());
        assert_eq!(evaluate_state(&cgroup, &proc), State::Busy);

        // Tight via MemAvailable (700 MiB: 716800 kB < 819 MiB)
        assert!(std::fs::write(proc.join("meminfo"), "MemAvailable:     716800 kB\n").is_ok());
        assert_eq!(evaluate_state(&cgroup, &proc), State::Tight);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn budget_for_each_state_is_correct() {
        let normal = budget_for(State::Normal);
        assert_eq!(normal.max_rows, *SUMMARY_ROWS);
        assert_eq!(normal.max_bytes, *SUMMARY_MAX_BYTES);

        let busy = budget_for(State::Busy);
        assert_eq!(busy.max_rows, 250);
        assert_eq!(busy.max_bytes, 8 * 1024 * 1024);

        let tight = budget_for(State::Tight);
        assert_eq!(tight.max_rows, 0);
        assert_eq!(tight.max_bytes, 0);
    }

    #[tokio::test]
    async fn concurrency_semaphore_busy_times_out_when_permits_held() {
        let sem = tokio::sync::Semaphore::new(4);
        let wait = Duration::from_millis(50);

        // Normal acquires 1 permit
        let p1 = acquire_permit(&sem, 4, State::Normal, wait).await;
        assert!(p1.is_some());

        // Busy wants 4 permits, only 3 available -> times out
        let busy_fail = acquire_permit(&sem, 4, State::Busy, wait).await;
        assert!(busy_fail.is_none());

        drop(p1);
        // All 4 permits free -> Busy succeeds
        let busy_ok = acquire_permit(&sem, 4, State::Busy, wait).await;
        assert!(busy_ok.is_some());
    }

    #[test]
    fn the_byte_budget_stops_at_its_limit_and_skips_a_huge_row() {
        let row = serde_json::json!({"id": "urn:ngsi-ld:Bridge:1", "type": "Bridge"});
        let size = serde_json::to_vec(&row).map_or(0, |b| b.len());
        assert_eq!(admit(&row, 0, size), Admit::Keep(size));
        assert_eq!(admit(&row, 1, size), Admit::Full);
        assert_eq!(admit(&row, 0, 0), Admit::Full);
        let huge = serde_json::json!({"text": "x".repeat(2 * 1024 * 1024)});
        assert_eq!(admit(&huge, 0, usize::MAX), Admit::Skip);
        let (mut used, mut kept, mut skipped) = (0usize, 0usize, 0usize);
        let twice_size = size.saturating_mul(2);
        for r in [row.clone(), huge, row.clone(), row] {
            match admit(&r, used, twice_size) {
                Admit::Keep(n) => {
                    used = used.saturating_add(n);
                    kept = kept.saturating_add(1);
                }
                Admit::Skip => skipped = skipped.saturating_add(1),
                Admit::Full => break,
            }
        }
        assert_eq!((kept, skipped, used), (2, 1, twice_size));
    }
}
