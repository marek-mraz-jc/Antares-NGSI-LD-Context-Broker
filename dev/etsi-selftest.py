#!/usr/bin/env python3
"""Self-check for the ETSI resource-sampling pair and the per-TP baseline gate.

The two pieces with real logic in them: the sampler's /proc parsing, and the
report's sample↔TP interval join. Both fail silently if they break — an empty
CSV column looks exactly like "nothing happened" — so they get one assert-based
check. std-only, no framework.

Run: python3 dev/etsi-selftest.py
"""
import csv
import json
import os
import subprocess
import sys
import tempfile
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import importlib.util

spec = importlib.util.spec_from_file_location("sampler", f"{HERE}/etsi-sampler.py")
sampler = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sampler)


def check_proc_parsing():
    """cpu_ticks/rss_mib must read THIS process, and CPU must advance."""
    me = os.getpid()
    rss = sampler.rss_mib(me)
    assert rss and rss > 0, f"rss_mib({me}) returned {rss!r} — /proc parsing broke"

    t0 = sampler.cpu_ticks(me)
    assert t0 is not None, "cpu_ticks returned None for a live process"
    x = 0
    for i in range(3_000_000):  # burn measurable CPU
        x += i
    t1 = sampler.cpu_ticks(me)
    assert t1 > t0, f"cpu ticks did not advance across real work ({t0} -> {t1})"

    assert sampler.cpu_ticks(2**30) is None, "a dead pid must yield None, not raise"
    assert sampler.rss_mib(2**30) is None, "a dead pid must yield None, not raise"
    print(f"  proc parsing ok (self rss {rss:.0f} MiB, ticks {t0}->{t1})")


def check_join():
    """A sample inside a TP gets that TP; one in the gap gets (between tests);
    one outside the suite entirely stays unlabelled."""
    base = datetime(2026, 8, 4, 12, 0, 0)
    t0 = base.timestamp()

    def iso(offset):
        return datetime.fromtimestamp(t0 + offset).strftime("%Y-%m-%dT%H:%M:%S.%f")

    with tempfile.TemporaryDirectory() as tmp:
        os.mkdir(f"{tmp}/DemoSuite")
        with open(f"{tmp}/DemoSuite/output.xml", "w") as fh:
            fh.write(f"""<?xml version="1.0"?>
<robot>
<suite id="s1" name="DemoSuite">
  <test id="s1-t1" name="001_01 First"><status status="PASS" start="{iso(0)}" elapsed="2.0"/></test>
  <test id="s1-t2" name="002_01 Second"><status status="FAIL" start="{iso(10)}" elapsed="2.0">boom went the payload</status></test>
  <status status="FAIL" start="{iso(0)}" elapsed="12.0"/>
</suite>
<statistics><total><stat pass="1" fail="1" skip="0">All Tests</stat></total></statistics>
</robot>""")

        with open(f"{tmp}/resource-samples.csv", "w") as fh:
            fh.write("ts,iso,container,phase,cpu_pct,rss_mib\n")
            for offset in (1, 6, 11, 500):  # in t1, gap, in t2, far outside
                fh.write(f"{t0 + offset:.0f},{iso(offset)[:19]},antares1,DemoSuite,10.0,100.0\n")

        env = {**os.environ, "RESULTS": tmp, "STORE": "memory",
               "MEM_LIMIT_MB": "350", "IMAGE_BYTES": "0"}
        subprocess.run([sys.executable, f"{HERE}/etsi-report.py"],
                       env=env, check=True, capture_output=True)

        got = {int(float(r["ts"]) - t0): r["test"]
               for r in csv.DictReader(open(f"{tmp}/resource-samples.csv"))}
        assert got[1] == "001_01 First", f"in-test sample mislabelled: {got[1]!r}"
        assert got[11] == "002_01 Second", f"in-test sample mislabelled: {got[11]!r}"
        assert got[6] == "(between tests)", f"gap sample mislabelled: {got[6]!r}"
        assert got[500] == "", f"sample outside the run must stay unlabelled: {got[500]!r}"

        fails = list(csv.DictReader(open(f"{tmp}/failures.csv")))
        assert len(fails) == 1 and fails[0]["test"] == "002_01 Second", fails
        assert "boom went the payload" in fails[0]["message"], fails

        rollup = list(csv.DictReader(open(f"{tmp}/resource-by-test.csv")))
        assert any(r["test"] == "001_01 First" and r["samples"] == "1" for r in rollup), rollup

        tps = {r["test"]: r["status"] for r in csv.DictReader(open(f"{tmp}/tp-status.csv"))}
        assert tps == {"001_01 First": "PASS", "002_01 Second": "FAIL"}, tps

        summary = open(f"{tmp}/run-summary.md").read()
        assert "002_01 Second" in summary, "the failure must reach the summary"
        assert open(f"{tmp}/gate-status.txt").read().strip() == "FAIL", \
            "a failing suite must fail the gate"
        print(f"  join ok ({len(got)} samples labelled, gate FAIL on a red suite)")


def check_baseline():
    """dev/etsi-baseline.py: a TP that flips red or stops running fails the
    gate and is named; a new passing TP only asks for a baseline update."""
    tps = ["TP/a.robot::001_01 First", "TP/a.robot::002_01 Second"]
    with tempfile.TemporaryDirectory() as tmp:
        base = f"{tmp}/baseline.json"
        with open(base, "w") as fh:
            json.dump({"tps": tps, "mqtt_tps": tps[1:]}, fh)

        def gate(rows, stores="memory", mqtt_off=""):
            cell = f"{tmp}/cells/ETSI-cell-memory"
            os.makedirs(cell, exist_ok=True)
            with open(f"{cell}/tp-status.csv", "w", newline="") as fh:
                w = csv.writer(fh)
                w.writerow(["suite", "source", "test", "status"])
                for key, status in rows:
                    source, name = key.split("::")
                    w.writerow(["Demo", source, name, status])
            env = {**os.environ, "BASELINE": base, "STORES": stores, "MQTT_OFF": mqtt_off}
            return subprocess.run([sys.executable, f"{HERE}/etsi-baseline.py", "check",
                                   f"{tmp}/cells"], env=env, capture_output=True, text=True)

        green = gate([(t, "PASS") for t in tps])
        assert green.returncode == 0, green.stdout
        flipped = gate([(tps[0], "PASS"), (tps[1], "FAIL")])
        assert flipped.returncode != 0, "a flipped TP must fail the gate"
        assert "FAIL: TP/a.robot::002_01 Second" in flipped.stdout, flipped.stdout
        gone = gate([(tps[0], "PASS")])
        assert gone.returncode != 0, "a TP that stopped running must fail the gate"
        assert "did not run: TP/a.robot::002_01 Second" in gone.stdout, gone.stdout
        new = gate([(t, "PASS") for t in tps] + [("TP/b.robot::003_01 Third", "PASS")])
        assert new.returncode == 0, new.stdout
        assert "ratchet: update baseline: TP/b.robot::003_01 Third" in new.stdout, new.stdout
        # a cell running with MQTT=0 may lack the MQTT TPs, and only those
        assert gate([(tps[0], "PASS")], mqtt_off="memory").returncode == 0
        assert gate([(tps[1], "PASS")], mqtt_off="memory").returncode != 0
        absent = gate([(t, "PASS") for t in tps], stores="memory file")
        assert absent.returncode != 0 and "file: no tp-status.csv" in absent.stdout, absent.stdout
    print("  baseline ok (flip and vanish fail, a new pass asks for a ratchet)")


def check_compare():
    """dev/etsi-baseline.py compare: a TP whose verdict differs between the
    vanilla and the full run, or that ran in one only, fails and is named;
    the same failure in both is not a difference; an empty run fails."""
    with tempfile.TemporaryDirectory() as tmp:

        def run(name, rows):
            d = f"{tmp}/{name}"
            os.makedirs(d, exist_ok=True)
            with open(f"{d}/tp-status.csv", "w", newline="") as fh:
                w = csv.writer(fh)
                w.writerow(["suite", "source", "test", "status"])
                for key, status in rows:
                    source, test = key.split("::")
                    w.writerow(["IOP", source, test, status])
            return d

        def compare(a, b):
            return subprocess.run([sys.executable, f"{HERE}/etsi-baseline.py", "compare", a, b],
                                  capture_output=True, text=True)

        one, two = "IOP_TP/a.robot::IOP_001 One", "IOP_TP/a.robot::IOP_002 Two"
        vanilla = run("vanilla", [(one, "PASS"), (two, "FAIL")])
        same = compare(vanilla, run("same", [(one, "PASS"), (two, "FAIL")]))
        assert same.returncode == 0, same.stdout
        flipped = compare(vanilla, run("flipped", [(one, "FAIL"), (two, "FAIL")]))
        assert flipped.returncode != 0 and f"vanilla PASS, full FAIL: {one}" in flipped.stdout, flipped.stdout
        gone = compare(vanilla, run("gone", [(two, "FAIL")]))
        assert gone.returncode != 0 and f"vanilla PASS, full did not run: {one}" in gone.stdout, gone.stdout
        empty = compare(vanilla, run("empty", []))
        assert empty.returncode != 0 and "no TP ran" in empty.stdout, empty.stdout
        missing = compare(vanilla, f"{tmp}/nowhere")
        assert missing.returncode != 0, missing.stdout
    print("  compare ok (a flip or a TP in one run only fails, a shared failure does not)")


def check_view_diff():
    """dev/etsi-view-diff.py: a TP that differs through the view passes only
    when its body sent a request the view does not offer, an entity map or a
    Tenant, or the known file names it; a known TP that answers as E1 again,
    a deviation nobody owes and a TP that did not run fail."""
    vd = importlib.util.spec_from_file_location("view_diff", f"{HERE}/etsi-view-diff.py")
    view_diff = importlib.util.module_from_spec(vd)
    vd.loader.exec_module(view_diff)
    binds = view_diff.bindings()
    root = "http://h/x/views/v/ngsi-ld/v1"
    for method, url, op in [
        ("GET", f"{root}/entities/urn:a", "retrieve_entity"),
        ("GET", f"{root}/types?details=true", "retrieve_entity_type_details"),
        ("GET", f"{root}/types", "retrieve_entity_types"),
        ("GET", f"{root}/entities/urn:a/attrs/p/value", "retrieve_attribute_value"),
        ("POST", f"{root}/entityOperations/upsert?options=update", "batch_upsert"),
        ("POST", f"{root}/entities/", "create_entity"),
        ("GET", f"{root}/nothing", None),
        ("GET", "http://h/q/health", None),
    ]:
        assert view_diff.operation(method, url, binds) == op, (method, url)

    a, b, c, d, e, f = (f"TP/s.robot::00{i} T" for i in range(6))
    plain = "http://h/ngsi-ld/v1"
    with tempfile.TemporaryDirectory() as tmp:

        def run(name, rows, requests=()):
            path = f"{tmp}/{name}"
            os.makedirs(path)
            with open(f"{path}/tp-status.csv", "w", newline="") as fh:
                w = csv.writer(fh)
                w.writerow(["suite", "source", "test", "status"])
                for key, status in rows:
                    w.writerow(["s", *key.split("::"), status])
            with open(f"{path}/requests.jsonl", "w") as fh:
                for test, method, url, headers, *fixture in requests:
                    line = {"test": test, "method": method, "url": url, "headers": headers}
                    if fixture:
                        line.update(fixture=True, suite=fixture[0])
                    fh.write(json.dumps(line) + "\n")
            return path

        view = f"{tmp}/view.json"
        json.dump({"id": "v", "operations": ["retrieve_entity", "create_entity",
                                             "create_subscription", "retrieve_subscription"]},
                  open(view, "w"))
        known = f"{tmp}/known.json"
        json.dump({"v": {d: "T-0000"}}, open(known, "w"))
        view_diff.KNOWN = known
        e1 = run("e1", [(a, "PASS"), (b, "PASS"), (c, "PASS"), (d, "PASS"), (e, "PASS")])
        owed = run("owed", [(a, "FAIL"), (b, "FAIL"), (c, "FAIL"), (d, "FAIL"), (e, "FAIL")], [
            (a, "POST", f"{root}/entityOperations/upsert", []),
            (b, "GET", f"{root}/entities/urn:x?entityMap=true", []),
            (c, "GET", f"{root}/entities/urn:x", ["ngsild-tenant"]),
            # rule 9: the suite's setup made the Subscription at the root
            (None, "POST", f"{plain}/subscriptions/", [], "TP/s.robot"),
            (e, "GET", f"{root}/subscriptions/urn:s", []),
        ])
        assert view_diff.diff(view, e1, owed, binds) == 0
        refusals = json.load(open(f"{owed}/owed-refusals.json"))["refusals"]
        assert [r["tp"] for r in refusals] == [a, b, c, e], refusals
        # a Subscription the TP made through the view owes nothing
        own = run("own", [(a, "PASS"), (b, "PASS"), (c, "PASS"), (d, "FAIL"), (e, "FAIL")], [
            (e, "POST", f"{root}/subscriptions/", []),
            (e, "GET", f"{root}/subscriptions/urn:s", []),
        ])
        assert view_diff.diff(view, e1, own, binds) == 1, "a view's own Subscription owes nothing"
        stale = run("stale", [(a, "PASS"), (b, "PASS"), (c, "PASS"), (d, "PASS"), (e, "PASS")])
        assert view_diff.diff(view, e1, stale, binds) == 1, "a known TP answering as E1 must fail"
        offered = run("offered", [(a, "FAIL"), (b, "PASS"), (c, "PASS"), (d, "FAIL"), (e, "PASS")],
                      [(a, "POST", f"{root}/entities", [])])
        assert view_diff.diff(view, e1, offered, binds) == 1, "an offered operation owes nothing"
        gone = run("gone", [(b, "PASS"), (c, "PASS"), (d, "FAIL"), (e, "PASS")])
        assert view_diff.diff(view, e1, gone, binds) == 1, "a TP that did not run must fail"
        assert view_diff.diff(view, e1, f"{tmp}/nowhere", binds) == 1, "an empty run must fail"
    print("  view diff ok (owed refusals pass, a deviation, a stale known entry or a missing TP fails)")


if __name__ == "__main__":
    print("etsi sampling self-check")
    check_proc_parsing()
    check_join()
    check_baseline()
    check_compare()
    check_view_diff()
    print("OK")
