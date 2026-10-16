#!/usr/bin/env bash
# The views the surface conformance jobs read through (surfaces-conformance.md
# section 1), loaded into a running `views` broker at $1: every
# ci/views/<id>.json through PUT /q/views/<id>, each Tenant's LinkML schema
# ci/views/schemas/<tenant>.json through PUT /q/schemas/<tenant> (the OData
# model; the broker needs ANTARES_SCHEMA_MAX_BYTES), then each Tenant seeded
# from ci/views/seed/<tenant>.json (Batch Create). Fails on the first refusal.
# A second argument names another directory of the same layout (ci/views/ogc,
# the OGC matrix's own Tenant, T-3831).
set -euo pipefail
base=${1:?usage: dev/views-rig.sh <broker base URL> [views directory]}
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
views=${2:-$root/ci/views}
for f in "$views"/*.json; do
  id=$(basename "$f" .json)
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X PUT -H 'Content-Type: application/json' \
    --data-binary @"$f" "$base/q/views/$id")
  [[ $code == 201 || $code == 204 ]] || { echo "view $id: HTTP $code" >&2; exit 1; }
done
for f in "$views"/schemas/*.json; do
  tenant=$(basename "$f" .json)
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X PUT -H 'Content-Type: application/json' \
    --data-binary @"$f" "$base/q/schemas/$tenant")
  [[ $code == 201 || $code == 204 ]] || { echo "schema $tenant: HTTP $code" >&2; exit 1; }
done
for f in "$views"/seed/*.json; do
  tenant=$(basename "$f" .json)
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    -H "NGSILD-Tenant: $tenant" --data-binary @"$f" "$base/ngsi-ld/v1/entityOperations/create")
  [[ $code == 201 ]] || { echo "seed $tenant: HTTP $code" >&2; exit 1; }
done
echo "views and seed loaded into $base"
