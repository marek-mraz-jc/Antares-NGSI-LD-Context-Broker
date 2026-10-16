// SPDX-License-Identifier: EUPL-1.2
//! One parse of a stored schema per revision and bounds, shared by every
//! surface of the pod (T-4354): the `OData` and `LinkML` surfaces asking for the
//! same revision read the same [`Model`].
//!
//! The cache is bounded by the bytes of the schemas it holds, as the JSON-LD
//! loader's is (`antares-jsonld` `loader.rs`), not by a count: 20 schemas of
//! 1 MiB weigh what 20 MiB do. A refused schema is kept with its error, so a
//! hostile one is not read again on every request.
//!
//! ```
//! use linkml_model::cache;
//!
//! let json = br#"{"id": "https://example.org/s", "name": "s", "classes": {"Room": {}}}"#;
//! let (first, fresh) = cache::read(7, json, &linkml_model::SURFACE_LIMITS);
//! let (again, again_fresh) = cache::read(7, json, &linkml_model::SURFACE_LIMITS);
//! assert!(fresh && !again_fresh);
//! assert!(std::sync::Arc::ptr_eq(&first.unwrap(), &again.unwrap()));
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use moka::sync::Cache;

use crate::{Error, Limits, Model};

/// The bytes of schema source the cache holds at most.
pub const MAX_BYTES: u64 = 32 << 20;

/// What one entry weighs at least, so tiny schemas cannot crowd the cache
/// past any sensible count.
const FLOOR: u32 = 4096;

/// A read: the model, or why the schema was refused.
pub type Read = Result<Arc<Model>, Arc<Error>>;

static CACHE: LazyLock<Cache<(u64, Limits), (Read, u32)>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(MAX_BYTES)
        .weigher(|_key: &(u64, Limits), value: &(Read, u32)| value.1)
        .build()
});

static PARSES: AtomicU64 = AtomicU64::new(0);

/// The schema `bytes` of `revision` read under `limits`, parsed once per
/// pair on this pod; the flag is `true` for the call that parsed it, so a
/// caller logs a refused schema once. `revision` names `bytes`: the caller
/// passes the digest its store gave them.
pub fn read(revision: u64, bytes: &[u8], limits: &Limits) -> (Read, bool) {
    let entry = CACHE.entry((revision, *limits)).or_insert_with(|| {
        PARSES.fetch_add(1, Ordering::Relaxed);
        let weight = u32::try_from(bytes.len()).unwrap_or(u32::MAX).max(FLOOR);
        (
            Model::read(bytes, limits).map(Arc::new).map_err(Arc::new),
            weight,
        )
    });
    let fresh = entry.is_fresh();
    (entry.into_value().0, fresh)
}

/// How many schemas this pod has parsed through [`read`] since it started.
#[must_use]
pub fn parses() -> u64 {
    PARSES.load(Ordering::Relaxed)
}

/// The bytes of schema source the cache holds now (after pending evictions).
#[must_use]
pub fn held_bytes() -> u64 {
    CACHE.run_pending_tasks();
    CACHE.weighted_size()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(name: &str, pad: usize) -> Vec<u8> {
        let description = "x".repeat(pad);
        format!(r#"{{"id": "https://e.org/{name}", "name": "{name}", "classes": {{"Room": {{"description": "{description}"}}}}}}"#)
            .into_bytes()
    }

    const BIG: Limits = Limits {
        max_bytes: 2 << 20,
        max_depth: 32,
        max_elements: 20_000,
    };

    #[test]
    fn a_revision_is_parsed_once_per_bounds() {
        let bytes = schema("once", 10);
        let before = parses();
        let (a, fresh) = read(1_001, &bytes, &BIG);
        let (b, again) = read(1_001, &bytes, &BIG);
        assert!(fresh && !again);
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
        // Other bounds are another read: a stricter caller is never answered
        // from a looser one's parse.
        let tight = Limits {
            max_bytes: 10,
            ..BIG
        };
        let (refused, fresh) = read(1_001, &bytes, &tight);
        assert!(fresh && refused.is_err());
        assert!(parses() >= before + 2);
    }

    #[test]
    fn a_refused_schema_is_kept_with_its_error() {
        let (first, fresh) = read(1_002, b"{not json", &BIG);
        let (second, again) = read(1_002, b"{not json", &BIG);
        assert!(fresh && !again);
        assert!(first.is_err() && second.is_err());
    }

    #[test]
    fn forty_schemas_of_a_mebibyte_stay_under_the_byte_cap() {
        // 40 MiB asked for, 32 MiB held at most.
        for n in 0..40u64 {
            let bytes = schema(&format!("big{n}"), 1 << 20);
            let (read, _) = read(2_000 + n, &bytes, &BIG);
            assert!(read.is_ok());
        }
        assert!(held_bytes() <= MAX_BYTES, "{} > {MAX_BYTES}", held_bytes());
    }
}
