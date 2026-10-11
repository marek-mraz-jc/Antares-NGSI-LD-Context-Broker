#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""The surface features of antares-broker, and the binary size each one costs
(ADR-0027 sections 4 and 6, T-3493).

A surface feature is one that pulls in a crate of role `surface`
(`dep:<crate>`, module rule M1), plus `full` once the broker has it; a new
surface joins by being declared, without a YAML edit.

  dev/surface-features.py list         comma-separated, for cargo hack
  dev/surface-features.py sizes        build vanilla and each surface feature
                                       release, report, gate against the budget
  dev/surface-features.py --self-test  the gate is red on a synthetic overrun

Budget: the "binary size" row of ADR-0027 section 6, read from the ADR so
the gate cannot drift from it (`full` <= vanilla + 6 MiB, each feature alone
<= vanilla + 2 MiB, `mcp` <= vanilla + 4.5 MiB since T-3662), measured as
the size of the stripped release binary (the release profile strips
symbols). The report is a Markdown table on stdout and, in GitHub Actions,
in the step summary.
"""

import json
import os
import pathlib
import re
import subprocess
import sys

MIB = 1024 * 1024
BROKER = "antares-broker"
ADR = pathlib.Path(__file__).resolve().parent.parent / "docs/adr/ADR-0027-surfaces-architecture.md"


def budgets(text):
    """`(named, default)` in bytes from the ADR's `| binary size |` row: a
    feature in backticks followed by `≤ vanilla + N MiB`, and `each feature
    alone ≤ vanilla + N MiB` for the rest."""
    row = next((line for line in text.splitlines() if line.startswith("| binary size |")), None)
    if row is None:
        sys.exit("ADR-0027 has no `| binary size |` row: no budget to gate against")
    rule = row.split("|")[2]
    named = {f: float(n) * MIB for f, n in re.findall(r"`([a-z0-9-]+)`[^`≤]*≤ vanilla \+ ([\d.]+) MiB", rule)}
    default = re.search(r"each feature alone ≤ vanilla \+ ([\d.]+) MiB", rule)
    if default is None or "full" not in named:
        sys.exit(f"ADR-0027's binary size row does not read as a budget: {rule.strip()}")
    return named, float(default.group(1)) * MIB


def metadata():
    out = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--no-deps"],
        check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def surface_features(meta):
    role = {
        p["name"]: ((p.get("metadata") or {}).get("antares") or {}).get("role")
        for p in meta["packages"]
    }
    broker = next(p for p in meta["packages"] if p["name"] == BROKER)
    found = sorted(
        name
        for name, enables in broker["features"].items()
        if any(e.startswith("dep:") and role.get(e[4:]) == "surface" for e in enables)
    )
    if "full" in broker["features"]:
        found.append("full")
    if not found:
        sys.exit(f"{BROKER} declares no surface feature: the gate would pass vacuously")
    return found


def verdicts(vanilla, sizes, budget):
    """One row per feature: (feature, size, over vanilla, budget, within)."""
    named, default = budget
    rows = []
    for feature, size in sizes.items():
        budget = named.get(feature, default)
        rows.append((feature, size, size - vanilla, budget, size - vanilla <= budget))
    return rows


def report(vanilla, rows):
    lines = [
        "## Binary size per surface feature (ADR-0027 section 6)",
        "",
        "| build | bytes | over vanilla | budget | |",
        "|---|---:|---:|---:|---|",
        f"| vanilla | {vanilla} | | | |",
    ]
    for feature, size, over, budget, ok in rows:
        lines.append(
            f"| `{feature}` | {size} | {over / MIB:+.2f} MiB | {budget / MIB:g} MiB | {'ok' if ok else 'OVER'} |"
        )
    return "\n".join(lines) + "\n"


def build(target_dir, features):
    cmd = ["cargo", "build", "--release", "--locked", "-p", BROKER]
    if features:
        cmd += ["--features", features]
    subprocess.run(cmd, check=True)
    return pathlib.Path(target_dir, "release", "antares").stat().st_size


def sizes():
    meta = metadata()
    target = meta["target_directory"]
    vanilla = build(target, None)
    budget = budgets(ADR.read_text())
    rows = verdicts(vanilla, {f: build(target, f) for f in surface_features(meta)}, budget)
    text = report(vanilla, rows)
    print(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(text)
    over = [r[0] for r in rows if not r[4]]
    if over:
        sys.exit(f"over the ADR-0027 size budget: {', '.join(over)}")


def self_test():
    vanilla = 30 * MIB
    budget = budgets(ADR.read_text())
    if budget != ({"full": 6 * MIB, "mcp": 4.5 * MIB, "odata": 3.5 * MIB}, 2 * MIB):
        sys.exit(f"self-test: the ADR's budget reads as {budget}")
    rows = verdicts(vanilla, {"views": vanilla + 2 * MIB, "ws": vanilla + 2 * MIB + 1,
                              "odata": vanilla + 3.5 * MIB + 1,
                              "mcp": vanilla + 4.5 * MIB, "full": vanilla + 6 * MIB + 1}, budget)
    got = {r[0]: r[4] for r in rows}
    if got != {"views": True, "ws": False, "odata": False, "mcp": True, "full": False}:
        sys.exit(f"self-test: verdicts {got}")
    if "4.5 MiB" not in report(vanilla, rows):
        sys.exit("self-test: a fractional budget is not reported as it is")
    if "OVER" not in report(vanilla, rows):
        sys.exit("self-test: an overrun is not marked in the report")
    meta = {"packages": [
        {"name": BROKER, "features": {
            "default": ["mqtt"], "mqtt": ["antares-api/mqtt"], "views": ["dep:antares-ops", "dep:antares-views"],
            "mcp": ["views", "dep:antares-mcp"], "plugin-example": ["dep:antares-plugin-example"],
        }},
        {"name": "antares-views", "metadata": {"antares": {"role": "surface"}}},
        {"name": "antares-mcp", "metadata": {"antares": {"role": "surface"}}},
        {"name": "antares-ops", "metadata": {"antares": {"role": "ops"}}},
        {"name": "antares-plugin-example", "metadata": {"antares": {"role": "addon"}}},
    ]}
    if surface_features(meta) != ["mcp", "views"]:
        sys.exit(f"self-test: surface features {surface_features(meta)}")
    meta["packages"][0]["features"]["full"] = ["mcp"]
    if surface_features(meta) != ["mcp", "views", "full"]:
        sys.exit("self-test: full is not picked up")
    print("surface-features: self-test passed")


if __name__ == "__main__":
    mode = sys.argv[1:] or ["list"]
    if mode == ["--self-test"]:
        self_test()
    elif mode == ["sizes"]:
        sizes()
    elif mode == ["list"]:
        print(",".join(surface_features(metadata())))
    else:
        sys.exit(__doc__)
