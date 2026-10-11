#!/usr/bin/env bash
# SPDX-License-Identifier: EUPL-1.2
# CycloneDX SBOMs of the broker (surfaces-threat-model 11.1, ADR-0027
# section 7): `antares-vanilla.cdx.json` from the default features, and
# `antares-full.cdx.json` from `--features full` once antares-broker has that
# feature. The vanilla one must name no surface, ops or protocol crate
# (rule M3, `dev/check-vanilla-symbols.sh --sbom`); the check first proves it
# finds one in an SBOM of a build with `views`.
#
#   dev/sbom.sh <outdir>
set -euo pipefail
out=$(mkdir -p "${1:?usage: dev/sbom.sh <outdir>}" && cd "$1" && pwd)
cd "$(dirname "$0")/.."

sbom() { # <name> [cargo-cyclonedx feature flags]
  local name=$1
  shift
  cargo cyclonedx --manifest-path crates/antares-broker/Cargo.toml -f json \
    --spec-version 1.5 --override-filename "antares-$name.cdx" -q "$@"
  mv "crates/antares-broker/antares-$name.cdx.json" "$out/"
  # cargo-cyclonedx writes one file per workspace member; the broker's is
  # the shipped binary's, the others go.
  find crates examples -name "antares-$name.cdx.json" -type f -delete
  jq -e '.components | length > 0' "$out/antares-$name.cdx.json" >/dev/null || {
    echo "antares-$name.cdx.json lists no component"
    exit 1
  }
}

sbom selftest --features views
if ./dev/check-vanilla-symbols.sh --sbom "$out/antares-selftest.cdx.json" >/dev/null; then
  echo "self-test failed: the SBOM check finds no surface in a build with views"
  exit 1
fi
rm -f "${out:?}/antares-selftest.cdx.json"

sbom vanilla
./dev/check-vanilla-symbols.sh --sbom "$out/antares-vanilla.cdx.json"
if grep -q '^full = ' crates/antares-broker/Cargo.toml; then
  sbom full --features full
else
  echo "antares-broker has no full feature yet: the vanilla SBOM is the only one"
fi
