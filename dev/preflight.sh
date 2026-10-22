#!/usr/bin/env bash
# SPDX-License-Identifier: EUPL-1.2
# Every CI-only check of the crates a branch touches, in one local run before
# the push (T-3763): a train then needs one CI cycle, not one per red check.
# Each step prints `PASS|FAIL <step> <seconds>s`; a failed step prints the
# tail of its output and the run goes on. A tool that is missing is FAIL with
# the reason, never a PASS. Exit 1 when a step failed.
#
#   dev/preflight.sh [--base REF] [--no-coverage] [--log FILE]
#   dev/preflight.sh --self-test       one deliberate break per step class
#
# The touched crates are those of `git diff --name-only $(merge-base REF HEAD)`
# plus untracked files (default REF origin/dev); antares-api brings its
# reverse dependent joinedcontext-policy (train 4). With --log the output also
# goes to FILE, so an agent can start it with run_in_background and read FILE.
#
# Each step names the CI job it mirrors. A new CI-only check lands here in
# the same change (CONTRIBUTING.md, "Before the push").
set -uo pipefail
self=$(cd "$(dirname "$0")" && pwd)/preflight.sh
# the checkout the caller stands in (the self-test runs this file in a scratch worktree)
cd "$(git rev-parse --show-toplevel)" || exit 2
root=$PWD

base=origin/dev cover=1 log="" selftest=0
while (($#)); do
  case $1 in
    --base) base=${2:?--base needs a ref}; shift 2 ;;
    --no-coverage) cover=0; shift ;;
    --log) log=${2:?--log needs a file}; shift 2 ;;
    --self-test) selftest=1; shift ;;
    *) echo "usage: dev/preflight.sh [--base REF] [--no-coverage] [--log FILE] | --self-test" >&2; exit 2 ;;
  esac
done
[[ -n $log ]] && exec > >(tee "$log") 2>&1

# --self-test: a scratch worktree of HEAD, one commit per break class on
# odata-axum (small, role protocol, outside the vanilla tree), and the steps
# each run must turn red, exactly those
self_test() {
  export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target} # the warm build
  local tmp wt head lib=crates/odata-axum/src/lib.rs bad=0
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/preflight-self.XXXXXX")
  wt=$tmp/wt head=$(git rev-parse HEAD)
  git worktree add -q --detach "$wt" "$head" || return 2
  # shellcheck disable=SC2064 # the paths are fixed now
  trap "git -C '$wt' reset -q --hard; git -C '$root' worktree remove '$wt'; rm -rf '${tmp:?}'" EXIT
  probe() { # <case> <expected red steps, | separated> <preflight flags> <break script>
    git -C "$wt" reset -q --hard "$head"
    if [[ -n $4 ]]; then
      (cd "$wt" && bash -c "$4" && git add -A && git -c user.name=preflight -c user.email=preflight@localhost commit -qm "probe $1") || {
        echo "self-test $1: the break did not apply"; bad=1; return; }
    fi
    local run got
    # shellcheck disable=SC2086 # $3 is a flag list
    # a hang is a red run, never a self-test that waits forever
    run=$(cd "$wt" && timeout 3600 "$self" --base HEAD~"$([[ -n $4 ]] && echo 1 || echo 0)" $3 2>&1)
    got=$(sed -n 's/^FAIL \(.*\) [0-9]*s$/\1/p' <<< "$run" | paste -sd'|')
    if ! grep -q '^preflight: [0-9]* steps' <<< "$run"; then
      echo "FAIL $1: the run did not finish"; sed 's/^/    /' <<< "$run" | tail -n 20; bad=1
    elif [[ $got == "$2" ]]; then echo "ok   $1: red ${2:-nothing}"; else echo "FAIL $1: red '$got', expected '${2}'"; bad=1; fi
  }
  # with --log as the train runs it: the final wait must not wait for its tee
  probe clean "" "--log $tmp/clean.log" ""
  grep -q '^preflight: [0-9]* steps' "$tmp/clean.log" || { echo "FAIL clean: --log holds no verdict"; bad=1; }
  # a lane killed mid-step: that step and the lane read red, the run still ends with a verdict
  PREFLIGHT_DIE_AT=vanilla-symbols probe dead-lane "vanilla-symbols|lane debug_lane" --no-coverage ""
  probe fmt fmt --no-coverage "sed -i '0,/^use /s//use  /' $lib"
  probe unused-dependency machete --no-coverage \
    "sed -i '/^axum = /a uuid = { version = \"1\", default-features = false }' crates/odata-axum/Cargo.toml && cargo metadata --format-version 1 > /dev/null"
  # a dependency fuzz/Cargo.lock does not hold, on a crate a fuzz target uses
  probe fuzz-lock "machete|fuzz-compile" --no-coverage \
    "sed -i '/^\\[dependencies\\]/a hex = { version = \"0.4\", default-features = false }' crates/odata-url/Cargo.toml && cargo metadata --format-version 1 > /dev/null"
  probe architecture-row spec --no-coverage "mkdir -p crates/zz-probe/src && echo '//! probe' > crates/zz-probe/src/lib.rs"
  probe red-alone "alone odata-axum::probe_b_needs_the_other" --no-coverage "cat >> $lib <<'RS'

#[cfg(test)]
mod preflight_probe {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    static SEEN: AtomicBool = AtomicBool::new(false);

    #[test]
    fn probe_a_sets() {
        SEEN.store(true, Ordering::SeqCst);
    }

    #[test]
    fn probe_b_needs_the_other() {
        let start = Instant::now();
        while !SEEN.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(5) {
            std::thread::yield_now();
        }
        assert!(SEEN.load(Ordering::SeqCst));
    }
}
RS"
  # a test only a build without a feature holds runs alone there, not "no test named"
  probe feature-off-alone "" --no-coverage "sed -i '/^\\[dependencies\\]/i [features]\\nprobe = []\\n' crates/odata-axum/Cargo.toml && cat >> $lib <<'RS'

#[cfg(all(test, not(feature = \"probe\")))]
mod preflight_probe_off {
    #[test]
    fn probe_without_the_feature() {
        assert_eq!(1 + 1, 2);
    }
}
RS"
  # before the test module: clippy refuses items after it
  probe uncovered-line "coverage odata-axum" "" \
    "perl -0pi -e 's/^#\\[cfg\\(test\\)\\]/\\/\\/\\/ A function no test calls.\\n#[must_use]\\npub fn preflight_probe(n: u8) -> u8 {\\n    n.wrapping_add(1)\\n}\\n\\n#[cfg(test)]/m' $lib"
  return "$bad"
}
if ((selftest)); then
  self_test
  exit
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/preflight.XXXXXX")
mkdir -p "$work/res" "$work/done"
start=$SECONDS

# step <key> <name> <command...>: run, time, write one line to res/<key>; on
# FAIL the tail too. The steps run side by side (T-4032), and the keys sort in
# the serial order, so the report reads as it always did, each tail under its
# own FAIL line. The FAIL goes in first and the verdict overwrites it, so a
# step whose lane is killed under it reads red, never as a missing line.
step() {
  local key=$1 name=$2 at=$SECONDS out
  shift 2
  out="$work/$key-${name//[^A-Za-z0-9_.-]/_}.txt"
  { echo "FAIL $name 0s"; echo "    did not finish: its lane ended before the verdict"; } > "$work/res/$key"
  # the self-test's lane that dies mid-step
  [[ ${PREFLIGHT_DIE_AT:-} == "$name" ]] && kill -KILL "$BASHPID"
  if "$@" > "$out" 2>&1 < /dev/null; then
    echo "PASS $name $((SECONDS - at))s" > "$work/res/$key"
  else
    { echo "FAIL $name $((SECONDS - at))s"; tail -n 25 "$out" | sed 's/^/    /'; } > "$work/res/$key"
  fi
}

# Cargo lanes in the background, at most `slots` at a time: one below 6 jobs,
# so a small worker builds one thing at once as before, else one per 3 jobs, at
# most 3. Each lane builds in a profile or target directory of its own (debug,
# release, llvm-cov, fuzz), so no lane waits on another's lock. The steps
# without a build run beside them unbounded. Each cargo lane gets its share of
# the jobs, never all of them: on a shared host the limit is memory during
# linking, and three lanes of full CARGO_BUILD_JOBS link three times at once.
total=${CARGO_BUILD_JOBS:-$(nproc)}
slots=$((total < 6 ? 1 : total / 3 < 3 ? total / 3 : 3))
share=$((total / slots > 1 ? total / slots : 1))
lanes=() lane_of=()
lane() { # <function> [args...]
  local p live
  while :; do
    live=()
    for p in "${lanes[@]}"; do kill -0 "$p" 2> /dev/null && live+=("$p"); done
    ((${#live[@]} < slots)) && break
    wait -n "${live[@]}"
  done
  bg -j "$share" "$@"
}
# bg [-j JOBS] <function> [args...]: in the background; a lane that ends
# before its last command (killed, out of memory, a shell error) leaves no
# done marker and reads red below
bg() {
  if [[ $1 == -j ]]; then
    { export CARGO_BUILD_JOBS=$2; shift 2; "$@"; touch "$work/done/$BASHPID"; } &
  else
    { "$@"; touch "$work/done/$BASHPID"; } &
  fi
  lanes+=($!)
  lane_of[$!]=$([[ $1 == -j ]] && shift 2; [[ $1 == step ]] && echo "$3" || echo "$1")
}

mb=$(git merge-base "$base" HEAD) || { echo "preflight: no merge base with $base" >&2; exit 2; }
changed=$({ git diff --name-only "$mb"; git ls-files --others --exclude-standard; } | sort -u)

# touched workspace crates, longest manifest dir first; antares-api adds its
# reverse dependent addon
meta=$(cargo metadata --locked --no-deps --format-version 1) || { echo "preflight: cargo metadata failed" >&2; exit 2; }
mapfile -t crates < <(python3 -c '
import json, os, sys
root, changed = sys.argv[1], sys.argv[2].split()
pkgs = sorted(((os.path.relpath(os.path.dirname(p["manifest_path"]), root), p["name"])
               for p in json.load(sys.stdin)["packages"]), key=lambda d: -len(d[0]))
hit = {next((n for d, n in pkgs if d != "." and f.startswith(d + "/")), None) for f in changed} - {None}
if "antares-api" in hit:
    hit.add("joinedcontext-policy")
print("\n".join(sorted(hit)))
' "$root" "$changed" <<< "$meta" | grep .)
pflags=()
for c in "${crates[@]}"; do pflags+=(-p "$c"); done
echo "preflight: base $base (${mb:0:7}), crates: ${crates[*]:-none}"

# tests the diff adds: `crate name` per #[test] / #[tokio::test] fn on a + line
added_tests() {
  { git diff -U0 "$mb" -- '*.rs'
    git ls-files --others --exclude-standard -- '*.rs' | while read -r f; do
      echo "+++ b/$f"; sed 's/^/+/' "$f"; done
  } | python3 -c '
import re, sys
attr, fn = re.compile(r"^\+\s*#\[(tokio::)?test\b"), re.compile(r"^\+\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)")
path, pending = None, False
for line in sys.stdin:
    if line.startswith("+++ "):
        path, pending = line[6:].strip(), False
    elif attr.match(line):
        pending = True
    elif pending and (m := fn.match(line)):
        print(path, m.group(1))
        pending = False
'
}

crate_of() { # <path>: the package whose manifest dir holds it
  local d
  d=$(dirname "$1")
  while [[ $d != . && ! -f $d/Cargo.toml ]]; do d=$(dirname "$d"); done
  [[ -f $d/Cargo.toml && $d != . ]] && sed -n 's/^name = "\(.*\)"/\1/p' "$d/Cargo.toml" | head -1
}

# A test behind cfg(not(feature = ...)) is compiled out of --all-features
# (T-4854, trains 0421 and 0538): the default and the no-default build are
# asked next, and the test runs in the first build that holds it.
alone() { # <crate> <fn>: the test by its full path, in a process of its own
  local feat list full="" i=0
  for feat in --all-features "" --no-default-features; do
    i=$((i + 1)) list=$work/list-$1-$i.txt
    # shellcheck disable=SC2086 # $feat is one flag or none
    [[ -s $list ]] || cargo test -q --locked -p "$1" $feat --tests -- --list > "$list" 2> /dev/null
    full=$(sed -n "s/^\(\(.*::\)\{0,1\}$2\): test$/\1/p" "$list")
    [[ -n $full ]] && break
  done
  [[ -n $full ]] || { echo "$1: no test named $2 in --list (all, default or no default features)"; return 1; }
  local f
  for f in $full; do
    echo "== $f ${feat:-(default features)}"
    # shellcheck disable=SC2086
    cargo test -q --locked -p "$1" $feat --tests -- --exact "$f" || return 1
  done
}

target=$(python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])' <<< "$meta")

# the debug lane: clippy, the tests, the added tests alone, the debug builds
debug_lane() {
  if ((${#crates[@]})); then
    # workspace.yml "cargo clippy" (the touched crates of its --workspace wall)
    step 02 clippy cargo clippy --locked "${pflags[@]}" --all-targets --all-features -- -D warnings
    # workspace.yml "dev/unit-junit.sh all-features"
    step 03 test cargo test --locked "${pflags[@]}" --all-features
    # ci 38133016763: a test red only when it runs alone (env and order)
    local k=0 path name c
    while read -r path name; do
      c=$(crate_of "$path")
      [[ -n $c ]] && k=$((k + 1)) && step "04-$(printf %04d "$k")" "alone $c::$name" alone "$c" "$name"
    done < <(added_tests)
  else
    echo "PASS clippy 0s (no crate touched)" > "$work/res/02"
    echo "PASS test 0s (no crate touched)" > "$work/res/03"
  fi
  # workspace.yml "Vanilla binary links no surface"
  step 10 vanilla-symbols dev/check-vanilla-symbols.sh
  # config.rs KNOWN_KEYS (memory: antares-fuzz-outside-workspace) when an
  # ANTARES_ key or a manifest changed
  if grep -q 'Cargo.toml$' <<< "$changed" || git diff "$mb" -- '*.rs' | grep -q '^+.*ANTARES_'; then
    step 12 config-keys cargo test --locked -p antares-broker --bins config_key_tests
  fi
}

# strict.yml "coverage": dev/coverage-gate.py on each touched crate with a
# role, one after the other in llvm-cov's own target directory
coverage_lane() {
  local c k=0
  for c in "$@"; do
    k=$((k + 1))
    step "16-$(printf %02d "$k")" "coverage $c" bash -c 'cargo +nightly llvm-cov --version >/dev/null 2>&1 || { echo "missing tool: cargo +nightly llvm-cov"; exit 1; }
      python3 - "$1" <<'"'PY'"'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("gate", "dev/coverage-gate.py")
g = importlib.util.module_from_spec(spec); spec.loader.exec_module(g)
c = next(c for c in g.new_crates() if c["name"] == sys.argv[1])
errors = g.check(c["name"], c["dir"], g.measure(c), [e for e in g.exclusions() if e["crate"] == c["name"]])
print("\n".join(errors) or "OK")
sys.exit(1 if errors else 0)
PY' _ "$c"
  done
}

# The binary size per surface feature (ADR-0027 section 6) is fast's own job (workspace.yml
# "Binary size per surface feature"); preflight ran the same release builds again and took ~16
# of its 18 minutes on a train (T-4134), so it is left to fast.
lane debug_lane
if ((cover)); then
  roles=$(python3 - "${crates[@]}" <<'PY'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("gate", "dev/coverage-gate.py")
g = importlib.util.module_from_spec(spec); spec.loader.exec_module(g)
print(" ".join(c["name"] for c in g.new_crates() if c["name"] in sys.argv[1:]))
PY
  )
  # shellcheck disable=SC2086 # one crate name per word
  [[ -n $roles ]] && lane coverage_lane $roles
fi
# workspace.yml "The fuzz targets still compile": fuzz/ has a lock of its
# own, so a new dependency of a crate a target uses is red there only (train
# 15, T-3821); a target directory of its own beside the debug lane's
if grep -qE '(^|/)Cargo\.(toml|lock)$|^(crates|fuzz)/' <<< "$changed"; then
  lane step 06 fuzz-compile bash -c 'CARGO_TARGET_DIR="$1/preflight-fuzz" cargo check --locked --manifest-path fuzz/Cargo.toml --bins || {
    echo "fix: cargo update --manifest-path fuzz/Cargo.toml -p <the new crate>, and commit fuzz/Cargo.lock (never drop --locked)"; exit 1; }' _ "$target"
fi

# workspace.yml "New-surface template smoke" (T-5063): the template, or the seam its
# generated crate calls, changed; it builds in the warm target, after cargo's lock
if grep -qE '^dev/new-surface|^crates/antares-ops/' <<< "$changed"; then
  lane step 17 new-surface env CARGO_TARGET_DIR="$target" dev/new-surface.test.sh
fi
# workspace.yml "cargo fmt --all --check"
bg step 01 fmt cargo fmt --all --check
# workspace.yml "cargo-machete"
bg step 05 machete bash -c 'need() { "$@" >/dev/null 2>&1 || { echo "missing tool: $*"; exit 1; }; }; need cargo machete --version && cargo machete'
# workspace.yml "conformance ledger check"
bg step 07 spec bash -c 'python3 dev/spec.py check && python3 dev/spec_test.py'
# workspace.yml "Module rules"
bg step 08 module-rules python3 dev/check-module-rules.py
# workspace.yml "Vanilla feature tree is committed"
bg step 09 vanilla-tree bash -c 'set -o pipefail; dev/vanilla-tree.sh | diff -u ci/vanilla-tree.txt -'
# workspace.yml "SBOM (vanilla and full)"
bg step 11 sbom bash -c 'cargo cyclonedx --version >/dev/null 2>&1 || { echo "missing tool: cargo cyclonedx"; exit 1; }; dev/sbom.sh "$1"' _ "$work/sbom"
# workspace.yml "Route docs check" and "Env-var docs check"
bg step 14 route-docs dev/check-route-docs.sh
bg step 15 env-docs dev/check-env-docs.sh
# workspace.yml "Migration rollback manifest"
bg step 16 migrations bash -c 'python3 dev/check-migrations.py --self-test && python3 dev/check-migrations.py'
# Our own pids only: a bare `wait` also waits for the `tee` behind --log
# (bash 5.1+), which never exits while this shell holds its stdout.
wait "${lanes[@]}"
k=0
for p in "${lanes[@]}"; do
  [[ -e $work/done/$p ]] && continue
  k=$((k + 1))
  { echo "FAIL lane ${lane_of[$p]} 0s"; echo "    the lane ended before its last step: a step it never reached has no line"; } > "$work/res/99-$(printf %02d "$k")"
done

cat "$work"/res/*
steps=$(cat "$work"/res/* | grep -c '^\(PASS\|FAIL\) ')
red=$(cat "$work"/res/* | grep -c '^FAIL ')
echo "preflight: $steps steps, $red red, $((SECONDS - start)) s (step output in $work)"
((red == 0))
