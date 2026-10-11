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
    run=$(cd "$wt" && "$self" --base HEAD~"$([[ -n $4 ]] && echo 1 || echo 0)" $3 2>&1)
    got=$(sed -n 's/^FAIL \(.*\) [0-9]*s$/\1/p' <<< "$run" | paste -sd'|')
    if ! grep -q '^preflight: [0-9]* steps' <<< "$run"; then
      echo "FAIL $1: the run did not finish"; sed 's/^/    /' <<< "$run" | tail -n 20; bad=1
    elif [[ $got == "$2" ]]; then echo "ok   $1: red ${2:-nothing}"; else echo "FAIL $1: red '$got', expected '${2}'"; bad=1; fi
  }
  probe clean "" "" ""
  probe fmt fmt --no-coverage "sed -i '0,/^use /s//use  /' $lib"
  probe unused-dependency machete --no-coverage \
    "sed -i '/^axum = /a uuid = { version = \"1\", default-features = false }' crates/odata-axum/Cargo.toml && cargo metadata --format-version 1 > /dev/null"
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
steps=0 red=0 start=$SECONDS

# step <name> <command...>: run, time, print one line; on FAIL the tail
step() {
  local name=$1 at=$SECONDS out
  shift
  out="$work/$((steps + 1))-${name//[^A-Za-z0-9_.-]/_}.txt"
  if "$@" > "$out" 2>&1 < /dev/null; then
    echo "PASS $name $((SECONDS - at))s"
  else
    echo "FAIL $name $((SECONDS - at))s"
    tail -n 25 "$out" | sed 's/^/    /'
    red=$((red + 1))
  fi
  steps=$((steps + 1))
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

alone() { # <crate> <fn>: the test by its full path, in a process of its own
  local list=$work/list-$1.txt full
  [[ -s $list ]] || cargo test -q --locked -p "$1" --all-features --tests -- --list > "$list" 2> /dev/null
  full=$(sed -n "s/^\(\(.*::\)\{0,1\}$2\): test$/\1/p" "$list")
  [[ -n $full ]] || { echo "$1: no test named $2 in --list"; return 1; }
  local f
  for f in $full; do
    echo "== $f"
    cargo test -q --locked -p "$1" --all-features --tests -- --exact "$f" || return 1
  done
}

# workspace.yml "cargo fmt --all --check"
step fmt cargo fmt --all --check
if ((${#crates[@]})); then
  # workspace.yml "cargo clippy" (the touched crates of its --workspace wall)
  step clippy cargo clippy --locked "${pflags[@]}" --all-targets --all-features -- -D warnings
  # workspace.yml "dev/unit-junit.sh all-features"
  step test cargo test --locked "${pflags[@]}" --all-features
  # ci 38133016763: a test red only when it runs alone (env and order)
  while read -r path name; do
    c=$(crate_of "$path")
    [[ -n $c ]] && step "alone $c::$name" alone "$c" "$name"
  done < <(added_tests)
else
  echo "PASS clippy 0s (no crate touched)"
  echo "PASS test 0s (no crate touched)"
  steps=$((steps + 2))
fi
# workspace.yml "cargo-machete"
step machete bash -c 'need() { "$@" >/dev/null 2>&1 || { echo "missing tool: $*"; exit 1; }; }; need cargo machete --version && cargo machete'
# workspace.yml "conformance ledger check"
step spec bash -c 'python3 dev/spec.py check && python3 dev/spec_test.py'
# workspace.yml "Module rules"
step module-rules python3 dev/check-module-rules.py
# workspace.yml "Vanilla feature tree is committed"
step vanilla-tree bash -c 'set -o pipefail; dev/vanilla-tree.sh | diff -u ci/vanilla-tree.txt -'
# workspace.yml "Vanilla binary links no surface"
step vanilla-symbols dev/check-vanilla-symbols.sh
# workspace.yml "SBOM (vanilla and full)"
step sbom bash -c 'cargo cyclonedx --version >/dev/null 2>&1 || { echo "missing tool: cargo cyclonedx"; exit 1; }; dev/sbom.sh "$1"' _ "$work/sbom"
# config.rs KNOWN_KEYS (memory: antares-fuzz-outside-workspace) when an
# ANTARES_ key or a manifest changed
if grep -q 'Cargo.toml$' <<< "$changed" || git diff "$mb" -- '*.rs' | grep -q '^+.*ANTARES_'; then
  step config-keys cargo test --locked -p antares-broker --bins config_key_tests
fi
# workspace.yml "Route docs check" and "Env-var docs check"
step route-docs dev/check-route-docs.sh
step env-docs dev/check-env-docs.sh
# strict.yml "coverage": dev/coverage-gate.py on each touched crate with a role
if ((cover)); then
  roles=$(python3 - "${crates[@]}" <<'PY'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("gate", "dev/coverage-gate.py")
g = importlib.util.module_from_spec(spec); spec.loader.exec_module(g)
print(" ".join(c["name"] for c in g.new_crates() if c["name"] in sys.argv[1:]))
PY
  )
  for c in $roles; do
    step "coverage $c" bash -c 'cargo +nightly llvm-cov --version >/dev/null 2>&1 || { echo "missing tool: cargo +nightly llvm-cov"; exit 1; }
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
fi

echo "preflight: $steps steps, $red red, $((SECONDS - start)) s (step output in $work)"
((red == 0))
