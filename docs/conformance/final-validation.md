---
title: Final validation against Coraine's functional suite and the ETSI suite
task: T-3549
---

# Final validation

Antares, the vanilla binary and the all-features binary from one dev commit, against two suites it did not write, on each store it ships (memory, file, postgres, timescale). The job is `.github/workflows/external-suites.yml`; the harness and how it maps each suite onto Antares is `tests/external/README.md`. The full run happens once every other T-3446 dependency is finished, and again after any later round.

## 1. What runs

| suite | source | cells |
|---|---|---|
| Coraine functional | SEAMWARE/coraine `test/funcTests/cases` at `d313d87fabd74f67e0d4d1f222ccf0c45239a0a9` (831 cases), corTest from SEAMWARE/corLibs | 2 binaries × 4 stores |
| ETSI (a) | `ngsi-ld-test-suite/` as this repository carries it at the commit under test | 2 binaries × 4 stores |
| ETSI (b) | forge.etsi.org `cim/ngsi-ld-test-suite`, default branch on the run day | 2 binaries × 4 stores |
| cross-surface oracle | `crates/antares-broker/tests/surfaces_equivalence.rs` (T-3522) | all-features binary |

## 2. Data sources

Every number in section 3 comes from the `final-validation` artifact of one external-suites run, named by its run id:

- Coraine counts: `coraine-<binary>-<store>/coraine-report.json`, field `counts` (compare.py); a failure's first difference is its `reason`.
- ETSI counts per suite commit: `tp-status.csv` of `etsi-<pinned|latest>-<binary>-<store>`, column `status`, folded by `tests/external/etsi/tp-diff.py`; the suite commit is the cell's `suite.txt`.
- Oracle: `oracle/oracle.log`.

## 3. Results

No full run yet: the T-3446 dependencies are open. A smoke run (vanilla, file store, the first 60 Coraine cases, the Provision suites) proves the harness on every push that changes it.

## 4. Triage

Each failure goes into exactly one bucket, with the clause (CIM 009 V1.9.1, TS 104 175/176/243/179) quoted:

- Antares bug: a task filed with `tasks/newid`, red-first test, in T-3446 `depends-on`.
- Coraine-specific behaviour the spec leaves open: listed here, no change.
- Test bug: listed here, and reported upstream when the suite is public.

| suite | case or TP | bucket | clause | task or upstream report |
|---|---|---|---|---|

## Related

- `docs/analysis/coraine-comparison.md` (T-3548)
- `docs/upstream/etsi-raises.md`
- `docs/analysis/surfaces-test-plan.md`, section 3 (the oracle)
