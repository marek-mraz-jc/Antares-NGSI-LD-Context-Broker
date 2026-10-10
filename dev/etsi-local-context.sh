#!/usr/bin/env bash
# The suite's own @context, served from this checkout (T-3550).
#
# The vendored suite names its context by a raw.githubusercontent.com URL
# on `dev`: in variables.py and inside 331 data and expectation files. A
# run on any other commit therefore tested dev's context file, and a GitHub
# outage failed every TP that names it. For the length of a run this
# script rewrites that URL prefix to a static server on CONTEXT_PORT
# (8089; the suite's own 8085-8087 servers start and stop per TP) that
# serves resources/jsonld-contexts/ from the checkout, and puts it back.
#
#   dev/etsi-local-context.sh start    rewrite + serve; prints SUITE_CONTEXT
#   dev/etsi-local-context.sh check    exit 1 unless the rewrite holds and the
#                                      server answered at least one fetch
#   dev/etsi-local-context.sh stop     stop serving, restore exactly the
#                                      files `start` changed
#
# variables.py is left to the callers' sed lines (SUITE_CONTEXT), because
# etsi-run.sh restores that file on exit.
set -euo pipefail
cd "$(dirname "$0")/.."

SUITE=ngsi-ld-test-suite
RAW=https://raw.githubusercontent.com/joinedcontext/Antares-NGSI-LD-Context-Broker/dev/ngsi-ld-test-suite/resources/jsonld-contexts/
PORT="${CONTEXT_PORT:-8089}"
LOCAL="http://localhost:$PORT/"
STATE="${CONTEXT_STATE:-$PWD/${RESULTS:-results}/local-context}"

# every file that names the raw prefix, variables.py excepted
named() { grep -rlF --exclude=variables.py "$1" "$SUITE/resources" "$SUITE/TP" "$SUITE/data" 2>/dev/null || true; }

case "${1:-}" in
  start)
    mkdir -p "$STATE"
    [ -s "$STATE/files" ] && { echo "already started: $STATE/files exists" >&2; exit 1; }
    named "$RAW" > "$STATE/files"
    xargs -r -d '\n' sed -i "s|$RAW|$LOCAL|g" < "$STATE/files"
    python3 -m http.server "$PORT" --bind 127.0.0.1 \
      --directory "$SUITE/resources/jsonld-contexts" > "$STATE/access.log" 2>&1 &
    echo $! > "$STATE/pid"
    # readiness on the listing, so only a broker fetch counts in `check`
    for _ in $(seq 50); do curl -sf -o /dev/null "$LOCAL" && break; sleep 0.1; done
    echo "${LOCAL}ngsi-ld-test-suite-compound.jsonld"
    ;;
  check)
    left=$(named "$RAW")
    [ -z "$left" ] || { echo "suite still names $RAW in:"; echo "$left"; exit 1; }
    hits=$(grep -c 'GET /ngsi-ld-test-suite' "$STATE/access.log" || true)
    [ "$hits" -gt 0 ] || { echo "no broker fetched the suite context from $LOCAL"; exit 1; }
    echo "suite context served locally: $hits fetches"
    ;;
  stop)
    [ -f "$STATE/pid" ] && kill "$(cat "$STATE/pid")" 2>/dev/null || true
    [ -f "$STATE/files" ] && xargs -r -d '\n' sed -i "s|$LOCAL|$RAW|g" < "$STATE/files"
    rm -f "$STATE/pid" "$STATE/files"
    ;;
  *)
    sed -n '2,19p' "$0"; exit 2 ;;
esac
