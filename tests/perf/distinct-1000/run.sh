#!/usr/bin/env bash
# The distinct-1000 bench (T-5062, docs/src/performance.md "1 000 distinct queries a second"):
# the platform broker binary under its policy engine, connected as a role without BYPASSRLS
# (ANTARES_REQUIRE_RLS=1), on PostgreSQL in a container capped at CPUS_PG cores and PG_MEM,
# the broker pinned to CPUS_BROKER cores; seeded with dev/perf/k6-surfaces.js's data (ENTITIES over
# ten types by COPY from seed.py, a 30-day pm25 history, the view `load` and its LinkML schema); then distinct.js at
# RATE for DURATION from LOCAL_IPS source addresses, the broker's and PostgreSQL's CPU and RSS
# sampled beside it, and pg_stat_statements' top statements written after.
#
#   BIN=$CARGO_TARGET_DIR/release/antares tests/perf/distinct-1000/run.sh
#
# Env: BIN (a build with full,jc-policy,tabular), OUT (results/distinct-1000), PORT (9471),
#      ENTITIES (581000), RATE (1000), DURATION (10m), CPUS_BROKER (4), CPUS_PG (2), PG_MEM (2g),
#      PG_PORT (55433), PG_IMAGE (the PostGIS image of compose-files), LOCAL_IPS
#      (127.0.0.10-127.0.0.73), KEEP=1 keeps the seeded database for another run (SEEDED=1 reuses it).
# Throwaway credentials only: the container lives for the run and binds to loopback.
set -euo pipefail
cd "$(dirname "$0")/../../.."

: "${BIN:?set BIN to a broker built with --features full,jc-policy,tabular}"
OUT="${OUT:-results/distinct-1000}"
PORT="${PORT:-9471}"
ENTITIES="${ENTITIES:-581000}"
RATE="${RATE:-1000}"
DURATION="${DURATION:-10m}"
CPUS_BROKER="${CPUS_BROKER:-4}"
PG_PORT="${PG_PORT:-55433}"
PG=antares-distinct-pg
LOCAL_IPS="${LOCAL_IPS:-127.0.0.10-127.0.0.73}"
for t in k6 psql docker curl taskset; do command -v "$t" >/dev/null || { echo "$t missing" >&2; exit 1; }; done
mkdir -p "$OUT"
ADMIN="postgresql://antares:bench@127.0.0.1:$PG_PORT/antares"
APP="postgresql://antares_app:bench-app@127.0.0.1:$PG_PORT/antares"
PID=""
SAMPLER=""
cleanup() {
  [ -n "$SAMPLER" ] && kill "$SAMPLER" 2>/dev/null
  [ -n "$PID" ] && kill "$PID" 2>/dev/null
  wait 2>/dev/null || true
  [ "${KEEP:-}" = 1 ] || docker rm -f "$PG" >/dev/null 2>&1 || true
}
trap cleanup EXIT

broker() {  # broker <log> <env…>: start the pinned broker, wait for /q/health
  local log=$1; shift
  env ANTARES_STORE=postgres ANTARES_HTTP_PORT="$PORT" ANTARES_ALLOW_SHARED_LOCAL=1 \
    ANTARES_API_SURFACES=admin,views,odata,ogc,mcp ANTARES_SCHEMA_MAX_BYTES=65536 "$@" \
    taskset -c "0-$((CPUS_BROKER - 1))" "$BIN" > "$OUT/$log" 2>&1 & PID=$!
  for _ in $(seq 600); do curl -sf -o /dev/null "http://127.0.0.1:$PORT/q/health" && return; sleep 0.1; done
  echo "broker did not start" >&2; tail "$OUT/$log" >&2; exit 1
}
stop() { kill "$PID"; wait "$PID" 2>/dev/null || true; PID=; }
put() {  # put <path> <json>: fails the run on a refusal
  curl -sf -o /dev/null -X PUT -H 'Content-Type: application/json' --data "$2" "http://127.0.0.1:$PORT$1" \
    || { echo "PUT $1 refused" >&2; exit 1; }
}

if [ "${SEEDED:-}" != 1 ]; then
  docker rm -f "$PG" >/dev/null 2>&1 || true
  # the PostgreSQL of the node's share: 2 cores and 2 GiB, as dev runs it
  docker run -d --name "$PG" --cpus "${CPUS_PG:-2}" --memory "${PG_MEM:-2g}" -p "127.0.0.1:$PG_PORT:5432" \
    -e POSTGRES_USER=antares -e POSTGRES_PASSWORD=bench -e POSTGRES_DB=antares \
    "${PG_IMAGE:-ghcr.io/baosystems/postgis:17-3.5}" \
    -c shared_buffers=512MB -c max_connections=200 -c shared_preload_libraries=pg_stat_statements >/dev/null
  for _ in $(seq 120); do psql "$ADMIN" -qXc 'SELECT 1' >/dev/null 2>&1 && break; sleep 1; done
  # the migrations as the owner, then the entities by COPY (seed.py | dev/bulk-load.sh, no broker
  # serving), then schema, view and history through the API, no policy (the seed writes as nobody)
  broker migrate-broker.log ANTARES_DATABASE_URL="$ADMIN"
  stop
  echo "seeding $ENTITIES entities"
  fifo="$OUT/entities.fifo"; rm -f "$fifo"; mkfifo "$fifo"
  python3 tests/perf/distinct-1000/seed.py --entities "$ENTITIES" > "$fifo" & gen=$!
  DATABASE_URL="$ADMIN" dev/bulk-load.sh "$fifo" default > "$OUT/seed.log" 2>&1 \
    || { echo "bulk load failed" >&2; tail -20 "$OUT/seed.log" >&2; exit 1; }
  wait "$gen"; rm -f "$fifo"
  broker seed-broker.log ANTARES_DATABASE_URL="$ADMIN"
  k6 run --quiet -e BROKER_URL="http://127.0.0.1:$PORT" tests/perf/distinct-1000/seed-extras.js > "$OUT/seed-extras.log" 2>&1
  ! grep -q 'level=error' "$OUT/seed-extras.log" || { echo "seed failed" >&2; grep -m3 'level=error' "$OUT/seed-extras.log" >&2; exit 1; }
  stop
  psql "$ADMIN" -qX -v ON_ERROR_STOP=1 \
    -c "CREATE ROLE antares_app LOGIN PASSWORD 'bench-app'" \
    -c 'GRANT USAGE ON SCHEMA public TO antares_app' \
    -c 'GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO antares_app' \
    -c 'GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO antares_app' \
    -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' \
    -c 'VACUUM (ANALYZE)' -c 'CHECKPOINT'
fi

# the measured broker: RLS enforced, the policy engine deciding every request by the subject
# headers the gateway sends, a policy of 20 permissions (one grants the bench role the ten types)
broker broker.log ANTARES_DATABASE_URL="$APP" ANTARES_MIGRATE=0 ANTARES_REQUIRE_RLS=1 \
  ANTARES_POLICY=joinedcontext ANTARES_POLICY_SUBJECT_HEADERS=x-jc-subject,x-jc-roles
d=https://uri.etsi.org/ngsi-ld/default-context/
types=$(python3 -c "import json; print(json.dumps([{'type': '$d' + t} for t in json.load(open('/dev/stdin'))]))" <<< \
  '["AirQualityObserved","Vehicle","Building","ParkingSpot","Streetlight","WeatherObserved","WaterQualityObserved","TrafficFlowObserved","Device","Event"]')
others=$(for i in $(seq 1 19); do
  printf ', {"id": "bench/other-%s", "assignee": {"kind": "role", "id": "endpoint:bench/e%s"},
    "operations": ["5.7.1", "5.7.2"], "information": [{"entities": [{"type": "%sType%s"}]}], "q": "level>%s"}' "$i" "$i" "$d" "$i" "$i"
done)
put /q/policy/default "{\"policies\": [{\"id\": \"bench/read\", \"assignee\": {\"kind\": \"role\", \"id\": \"endpoint:bench/read\"},
  \"operations\": [\"5.7.1\", \"5.7.2\", \"5.7.3\", \"5.7.4\"], \"information\": [{\"entities\": $types}]}$others]}"
psql "$ADMIN" -qXc 'SELECT pg_stat_statements_reset()' >/dev/null

python3 dev/perf/surfaces_gate.py sample --pid "$PID" --container "$PG" --out "$OUT/resources.json" & SAMPLER=$!
status=0
k6 run --local-ips="$LOCAL_IPS" -e BASE_URL="http://127.0.0.1:$PORT" -e RATE="$RATE" -e DURATION="$DURATION" \
  -e ENTITIES="$ENTITIES" -e SUBJECT=bench -e ROLES=endpoint:bench/read \
  --summary-export "$OUT/summary.json" tests/perf/distinct-1000/distinct.js > "$OUT/k6.log" 2>&1 || status=$?
kill "$SAMPLER"; wait "$SAMPLER" 2>/dev/null || true; SAMPLER=
psql "$ADMIN" -qX -P pager=off -c "SELECT calls, round(total_exec_time)::bigint AS total_ms,
    round(mean_exec_time::numeric, 2) AS mean_ms, rows, left(regexp_replace(query, '\s+', ' ', 'g'), 160) AS query
  FROM pg_stat_statements ORDER BY total_exec_time DESC LIMIT 15" > "$OUT/pg-top.txt"
sed -n '/THRESHOLDS/,$p' "$OUT/k6.log" | tee "$OUT/result.txt"
cat "$OUT/resources.json"; echo
exit "$status"
