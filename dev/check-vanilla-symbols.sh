#!/usr/bin/env bash
# Rule M3, the linker half (surfaces-threat-model 9.5 items 2 and 3, ADR-0027
# section 3): a vanilla broker links no surface, ops or protocol crate.
#
#   dev/check-vanilla-symbols.sh                      the dev-profile binary
#   dev/check-vanilla-symbols.sh --audit-info <bin>   a shipped (stripped) one
#
# The first mode builds `-p antares-broker` with the default features in the
# dev profile (release is stripped, F6), greps its demangled symbols for the
# crate paths below, and starts it with a surface it was not built with: it
# must refuse naming "unknown surface". Its self-test builds the same binary
# with an addon (`plugin-example`; `full` once it exists) and requires the
# grep to find it, so the gate is not a grep that never matched anything.
#
# The second mode reads the package list `cargo auditable` embeds
# (Dockerfile) with `rust-audit-info`; that checks the image's /antares
# itself, which has no symbols left to read.
#
# The third mode, `--sbom <cdx.json>`, reads the component names of a
# CycloneDX SBOM of the broker (dev/sbom.sh) the same way.
set -euo pipefail
cd "$(dirname "$0")/.."

# Rust paths, as `nm --demangle` prints them.
SYMBOLS='(^|[^A-Za-z0-9_])(antares_(ops|views|odata|mcp|linkml|ws|icsr|plugin_[a-z_]+)|odata_(url|csdl|json|batch|axum)|linkml_model|rmcp|tungstenite)::'
# The same crates as package names, as the auditable list carries them.
PACKAGES='^(antares-(ops|views|odata|mcp|linkml|ws|icsr|plugin-.+)|odata-.+|linkml-model|rmcp|tungstenite)$'

if [ "${1:-}" = --sbom ]; then
  sbom=${2:?usage: --sbom <cdx.json>}
  names=$(jq -r '.components[]?.name' "$sbom")
  [ -n "$names" ] || { echo "no component in $sbom: the check would pass vacuously"; exit 1; }
  if found=$(printf '%s\n' "$names" | grep -E "$PACKAGES"); then
    echo "SURFACE IN THE VANILLA SBOM $sbom:"
    printf '%s\n' "$found"
    exit 1
  fi
  echo "vanilla SBOM: OK ($(printf '%s\n' "$names" | wc -l) components, none a surface)"
  exit 0
fi

if [ "${1:-}" = --audit-info ]; then
  bin=${2:?usage: --audit-info <binary>}
  packages=$(rust-audit-info "$bin" | jq -r '.packages[].name')
  printf '%s\n' "$packages" | grep -qx antares-broker || {
    echo "no antares-broker in the embedded package list of $bin: was it built with cargo auditable?"
    exit 1
  }
  if found=$(printf '%s\n' "$packages" | grep -E "$PACKAGES"); then
    echo "SURFACE IN THE SHIPPED BINARY $bin:"
    printf '%s\n' "$found"
    exit 1
  fi
  echo "vanilla audit-info: OK ($(printf '%s\n' "$packages" | wc -l) packages, none a surface)"
  exit 0
fi

# Wherever cargo puts it (CARGO_TARGET_DIR included).
bin="$(cargo metadata --locked --format-version 1 --no-deps | jq -r .target_directory)/debug/antares"
symbols() { nm --defined-only --demangle "$1" | grep -E "$SYMBOLS" || true; }
scratch=$(mktemp -d)
trap 'rm -rf "${scratch:?}"' EXIT

# Self-test first, so the binary left behind is the vanilla one.
cargo build --locked -p antares-broker --features plugin-example
cp "$bin" "$scratch/addon"
[ -n "$(symbols "$scratch/addon")" ] || {
  echo "self-test failed: the pattern finds no symbol in a build that DOES link plugin-example"
  exit 1
}

cargo build --locked -p antares-broker
total=$(nm --defined-only "$bin" | wc -l)
[ "$total" -gt 10000 ] || { echo "nm read $total symbols: the gate would pass vacuously"; exit 1; }
found=$(symbols "$bin")
if [ -n "$found" ]; then
  echo "SURFACE CODE LINKED INTO THE VANILLA BINARY:"
  printf '%s\n' "$found" | head -40
  exit 1
fi

# The route half at startup: a name off the shelf is fatal, never ignored.
set +e
out=$(ANTARES_API_SURFACES=admin,odata timeout 60 "$bin" 2>&1)
code=$?
set -e
if [ "$code" -eq 0 ] || [ "$code" -eq 124 ] || ! grep -q "unknown surface" <<<"$out"; then
  echo "the vanilla binary did not refuse ANTARES_API_SURFACES=admin,odata (exit $code):"
  printf '%s\n' "$out" | tail -20
  exit 1
fi
echo "vanilla symbols: OK ($total symbols, none a surface; admin,odata refused)"
