#!/usr/bin/env bash
# The views the surface conformance jobs read through (surfaces-conformance.md
# section 1), loaded into a running `views` broker at $1: every
# ci/views/<id>.json through PUT /q/views/<id>, then each Tenant seeded from
# ci/views/seed/<tenant>.json (Batch Create). Fails on the first refusal.
set -euo pipefail
base=${1:?usage: dev/views-rig.sh <broker base URL>}
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
for f in "$root"/ci/views/*.json; do
  id=$(basename "$f" .json)
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X PUT -H 'Content-Type: application/json' \
    --data-binary @"$f" "$base/q/views/$id")
  [[ $code == 201 || $code == 204 ]] || { echo "view $id: HTTP $code" >&2; exit 1; }
done
for f in "$root"/ci/views/seed/*.json; do
  tenant=$(basename "$f" .json)
  code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    -H "NGSILD-Tenant: $tenant" --data-binary @"$f" "$base/ngsi-ld/v1/entityOperations/create")
  [[ $code == 201 ]] || { echo "seed $tenant: HTTP $code" >&2; exit 1; }
done
echo "views and seed loaded into $base"
