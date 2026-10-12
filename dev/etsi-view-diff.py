#!/usr/bin/env python3
"""E2/E4 gate (surfaces-conformance.md 2.3, T-3501): the ETSI suite through a
view answers TP by TP as the plain root does (E1), except where the view owes
a refusal.

A refusal is owed, not hand-listed: a TP whose body sent a request the view
does not offer (its operation, from operation-matrix.yaml's NGSI-LD binding,
is not in the view's `operations`; ADR-0023 rule 4), a request no operation
binds, a request naming an NGSILD-Tenant (rule 10) or an entity map (threat
V22), and, for a view that does not show every name, a request naming a type
outside its selector (rule 3) or an Attribute no rule shows (rule 2) in its
`type`, `attrs`, `pick`, `omit` or `q`, or in the JSON body it sent (T-3787).
Names are expanded as the suite sends them: its own @context, then the core
one, then the default vocabulary. The body requests come from the run's
requests.jsonl (dev/etsi_setup_url.py records them).

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
import re
import sys
from urllib.parse import parse_qs, urlsplit

HERE = os.path.dirname(os.path.abspath(__file__))
MATRIX = f"{HERE}/../docs/analysis/operation-matrix.yaml"
KNOWN = os.environ.get("KNOWN") or f"{HERE}/etsi-view-known.json"
ROOT = "/ngsi-ld/v1"
CORE_CONTEXT = f"{HERE}/../crates/antares-jsonld/contexts/core-v1.9.jsonld"
DEFAULT_VOCAB = "https://uri.etsi.org/ngsi-ld/default-context/"
# ngsi-ld-test-suite resources/jsonld-contexts/ngsi-ld-test-suite.jsonld, the
# terms of the @context every TP sends (through -compound.jsonld, before core)
SUITE_CONTEXT = {t: f"https://ngsi-ld-test-suite/context#{t}" for t in (
    "Building", "Vehicle", "OffStreetParking", "airQualityLevel", "almostFull",
    "availableSpotsNumber", "brandName", "fuelLevel", "isParked", "name",
    "reliability", "source", "speed", "totalSpotsNumber", "subCategory")}
# members of an Entity or a request body that are no Attribute (CIM 009 5.2)
NOT_ATTRIBUTES = {"id", "@id", "type", "@type", "@context", "scope", "createdAt",
                  "modifiedAt", "deletedAt", "expiresAt"}
# request bodies that name entities and attributes rather than being one
SELECTORS = {"Query", "Subscription", "ContextSourceRegistration", "CSourceRegistration"}

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


def core_terms(path=CORE_CONTEXT):
    """term -> IRI of the core @context, its compact IRIs resolved."""
    ctx = json.load(open(path))["@context"]
    plain = {k: v if isinstance(v, str) else v.get("@id") if isinstance(v, dict) else None
             for k, v in ctx.items()}
    out = {}
    for term, iri in plain.items():
        if not iri or term.startswith("@"):
            continue
        prefix, sep, rest = iri.partition(":")
        out[term] = plain[prefix] + rest if sep and prefix in plain and not rest.startswith("//") else iri
    return out


def expand(name, core):
    """The IRI the broker reads `name` as under the suite's @context."""
    if "://" in name or name.startswith("urn:"):
        return name
    return SUITE_CONTEXT.get(name) or core.get(name) or DEFAULT_VOCAB + name


def q_attributes(q):
    """The Attribute names a `q` (CIM 009 4.9) reads: the first path segment
    of every term, before its operator."""
    names = set()
    for term in re.split(r"[;|()]", q or ""):
        m = re.match(r"\s*!?\s*([^=!<>~\[.\s]+)", term)
        if m and not m.group(1).startswith(("'", '"')):
            names.add(m.group(1))
    return names


def named(url, body):
    """(types, attributes) a request names, in its query and its body."""
    query = parse_qs(urlsplit(url).query)
    first = lambda key: ",".join(query.get(key, []))
    types = {t for t in re.split(r"[,;|()]", first("type")) if t.strip()}
    attrs = {a for key in ("attrs", "pick", "omit") for a in first(key).split(",") if a.strip()}
    attrs |= q_attributes(first("q"))
    for item in (body if isinstance(body, list) else [body]):
        if not isinstance(item, dict):
            continue
        kind = item.get("type")
        if kind in SELECTORS:
            for e in item.get("entities") or []:
                if isinstance(e, dict) and isinstance(e.get("type"), str):
                    types |= {t for t in re.split(r"[,;|()]", e["type"]) if t.strip()}
            for key in ("attrs", "watchedAttributes"):
                attrs |= {a for a in item.get(key) or [] if isinstance(a, str)}
            attrs |= {a for a in (item.get("notification") or {}).get("attributes") or [] if isinstance(a, str)}
            attrs |= q_attributes(item.get("q") if isinstance(item.get("q"), str) else "")
        else:
            kinds = kind if isinstance(kind, list) else [kind]
            types |= {t for t in kinds if isinstance(t, str)}
            attrs |= {k for k in item if k not in NOT_ATTRIBUTES}
    return {t.strip() for t in types}, {a.strip() for a in attrs}


def hidden_by(view, core):
    """A function naming what of a request the view hides, or None for a view
    that shows every name."""
    selector = set(view.get("selector") or [])
    shown = {a for rule in view.get("rules") or [] for a in rule.get("attributes") or []}
    if "*" in selector and "*" in shown:
        return None

    def hidden(url, body):
        types, attrs = named(url, body)
        why = [f"type {t} outside the view (ADR-0023 rule 3)"
               for t in sorted(types) if "*" not in selector and expand(t, core) not in selector]
        why += [f"Attribute {a} the view does not show (ADR-0023 rule 2)"
                for a in sorted(attrs) if "*" not in shown and expand(a, core) not in shown]
        return why
    return hidden


def owed(requests, offered, binds, hidden=None):
    """Why the view refuses some request of a TP's body, or []."""
    why = []
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
        elif hidden:
            why += [f"{r['method']} {op}: {w}" for w in hidden(url, r.get("body"))]
    return why


def requests_of(d):
    by_tp = {}
    for path in glob.glob(f"{d}/**/requests.jsonl", recursive=True):
        for line in open(path, encoding="utf-8"):
            r = json.loads(line)
            by_tp.setdefault(r["test"], []).append(r)
    return by_tp


def diff(view_path, e1_dir, e2_dir, binds=None):
    view = json.load(open(view_path))
    known = json.load(open(KNOWN)).get(view["id"], {}) if os.path.exists(KNOWN) else {}
    binds = bindings() if binds is None else binds
    e1, e2 = baseline.run_of(e1_dir), baseline.run_of(e2_dir)
    red = [f"{d}: no TP ran (tp-status.csv missing or empty)" for d, r in ((e1_dir, e1), (e2_dir, e2)) if not r]
    if red:
        print("\n".join(red))
        return 1
    reqs = requests_of(e2_dir)
    offered = set(view.get("operations") or [])
    hidden = hidden_by(view, core_terms())
    refusals, explained = [], 0
    for tp in sorted(e1.keys() | e2.keys()):
        a, b = e1.get(tp, "did not run"), e2.get(tp, "did not run")
        if a == b:
            if tp in known:
                red.append(f"stale: {tp} answers as E1 now; remove it from {os.path.basename(KNOWN)}")
            continue
        why = owed(reqs.get(tp, []), offered, binds, hidden) if "did not run" not in (a, b) else []
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
