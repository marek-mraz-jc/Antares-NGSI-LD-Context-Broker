#!/usr/bin/env python3
"""E2/E4 gate (surfaces-conformance.md 2.3, T-3501): the ETSI suite through a
view answers TP by TP as the plain root does (E1), except where the view owes
a refusal.

A refusal is owed, not hand-listed: a TP whose body sent a request the view
does not offer (its operation, from operation-matrix.yaml's NGSI-LD binding,
is not in the view's `operations`; ADR-0023 rule 4), a request no operation
binds, a request naming an NGSILD-Tenant (rule 10) or an entity map (threat
V22), or a Subscription operation in a TP whose fixtures made a Subscription
at the root, which no view serves (rule 9: a view serves its own
Subscriptions alone). The requests come from the run's requests.jsonl
(dev/etsi_setup_url.py records the body's and, marked, the fixtures').

Usage:
  etsi-view-diff.py <view.json> <e1-dir> <e2-dir>

Every other difference fails, unless dev/etsi-view-known.json names the TP
under the view's id with the task that fixes it; a named TP that no longer
differs fails too (remove it). The owed refusals are written to
<e2-dir>/owed-refusals.json. KNOWN overrides the known file (the selftest).
"""
import glob
import json
import os
import sys
from urllib.parse import parse_qs, urlsplit

HERE = os.path.dirname(os.path.abspath(__file__))
MATRIX = f"{HERE}/../docs/analysis/operation-matrix.yaml"
KNOWN = os.environ.get("KNOWN") or f"{HERE}/etsi-view-known.json"
ROOT = "/ngsi-ld/v1"

sys.path.insert(0, HERE)
from importlib import import_module  # noqa: E402

baseline = import_module("etsi-baseline")


def bindings(matrix_path=MATRIX):
    """[(op, methods, segments, query)] from the matrix's NGSI-LD bindings;
    broker-initiated ones (notifications) bind no request the suite sends."""
    import yaml

    out = []
    for op in yaml.safe_load(open(matrix_path))["operations"]:
        b = (op.get("surfaces") or {}).get("ngsi_ld_http") or {}
        text = b.get("binding") or ""
        if b.get("status") != "supported" or "{notification" in text \
                or "notification endpoint" in text:
            continue
        methods, _, paths = text.partition(" ")
        first = paths.split(" and ")[0]
        for path in paths.split(" and "):
            path = path.replace("...", first.rsplit("/attrs", 1)[0]) if path.startswith("...") else path
            path, _, query = path.partition("?")
            segs = path.removeprefix(ROOT).strip("/").split("/")
            out.append((op["id"], set(methods.split("|")), segs, dict(q.split("=") for q in query.split("&") if q)))
    return out


def operation(method, url, binds):
    """The operation a request is, or None. Literal segments and a matched
    query beat placeholders (`/types?details=true` over `/types`)."""
    parts = urlsplit(url)
    path = parts.path
    if ROOT not in path:
        return None
    segs = path.split(ROOT, 1)[1].strip("/").split("/")
    query = {k: v[-1] for k, v in parse_qs(parts.query).items()}
    best, score = None, -1
    for op, methods, pattern, wants in binds:
        if method not in methods or len(pattern) != len(segs):
            continue
        if any(not p.startswith("{") and p != s for p, s in zip(pattern, segs)):
            continue
        if any(query.get(k) != v for k, v in wants.items()):
            continue
        s = sum(not p.startswith("{") for p in pattern) + 2 * len(wants)
        if s > score:
            best, score = op, s
    return best


# The Subscription operations a view runs on its own Subscriptions alone
# (ADR-0023 rule 9, antares-api views::SUBSCRIPTIONS).
OWN_SUBSCRIPTIONS = {"retrieve_subscription", "update_subscription",
                     "delete_subscription", "query_subscriptions"}


def owed(requests, offered, binds, fixtures=()):
    """Why the view refuses some request of a TP's body, or []."""
    why = []
    at_root = any(operation(f["method"], f["url"], binds) == "create_subscription" for f in fixtures)
    for r in requests:
        url, headers = r["url"], set(r.get("headers") or ())
        op = operation(r["method"], url, binds)
        if "ngsild-tenant" in headers:
            why.append(f"{r['method']} {op or url}: NGSILD-Tenant (ADR-0023 rule 10)")
        elif "ngsild-entitymap" in headers or "entityMap" in parse_qs(urlsplit(url).query):
            why.append(f"{r['method']} {op or url}: an entity map (threat V22)")
        elif op is None:
            why.append(f"{r['method']} {urlsplit(url).path}: no operation binds it (ADR-0023 rule 4)")
        elif op not in offered:
            why.append(f"{r['method']} {op}: not offered (ADR-0023 rule 4)")
        elif at_root and op in OWN_SUBSCRIPTIONS:
            why.append(f"{r['method']} {op}: a fixture made the Subscription at the root, "
                       "and a view serves its own alone (ADR-0023 rule 9)")
    return why


def requests_of(d):
    """(body requests by TP, fixture requests by TP and by suite file)."""
    by_tp, fixtures = {}, {}
    for path in glob.glob(f"{d}/**/requests.jsonl", recursive=True):
        for line in open(path, encoding="utf-8"):
            r = json.loads(line)
            if r.get("fixture"):
                fixtures.setdefault(r["test"] or r.get("suite", ""), []).append(r)
            else:
                by_tp.setdefault(r["test"], []).append(r)
    return by_tp, fixtures


def diff(view_path, e1_dir, e2_dir, binds=None):
    view = json.load(open(view_path))
    known = json.load(open(KNOWN)).get(view["id"], {}) if os.path.exists(KNOWN) else {}
    binds = bindings() if binds is None else binds
    e1, e2 = baseline.run_of(e1_dir), baseline.run_of(e2_dir)
    red = [f"{d}: no TP ran (tp-status.csv missing or empty)" for d, r in ((e1_dir, e1), (e2_dir, e2)) if not r]
    if red:
        print("\n".join(red))
        return 1
    reqs, fixtures = requests_of(e2_dir)
    offered = set(view.get("operations") or [])
    refusals, explained = [], 0
    for tp in sorted(e1.keys() | e2.keys()):
        a, b = e1.get(tp, "did not run"), e2.get(tp, "did not run")
        if a == b:
            if tp in known:
                red.append(f"stale: {tp} answers as E1 now; remove it from {os.path.basename(KNOWN)}")
            continue
        made = fixtures.get(tp, []) + fixtures.get(tp.split("::")[0], [])
        why = owed(reqs.get(tp, []), offered, binds, made) if "did not run" not in (a, b) else []
        if why:
            refusals.append({"tp": tp, "e1": a, "view": b, "owed": why})
        elif tp in known:
            explained += 1
            print(f"known ({known[tp]}): E1 {a}, view {b}: {tp}")
        else:
            red.append(f"E1 {a}, view {b}, no refusal owed: {tp}")
    with open(f"{e2_dir}/owed-refusals.json", "w") as fh:
        json.dump({"view": view["id"], "refusals": refusals}, fh, indent=1)
    print(f"{len(e1)} TPs E1, {len(e2)} through view {view['id']}: "
          f"{len(refusals)} owed refusals, {explained} known, {len(red)} red")
    for line in red:
        print(line)
    return 1 if red else 0


if __name__ == "__main__":
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    sys.exit(diff(*sys.argv[1:]))
