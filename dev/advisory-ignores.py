#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""The `cargo audit` ignore flags, read from `deny.toml` (surfaces-threat-model
11.1): the advisories cargo-deny is told to ignore are the only ones
cargo-audit ignores, so the two tools cannot drift.

  dev/advisory-ignores.py [deny.toml]   one `--ignore <id>` pair per entry
  dev/advisory-ignores.py --self-test   a deny.toml with two ignores gives two

Entries are `{ id = "...", reason = "..." }` tables or bare id strings, as
cargo-deny takes them. An entry without an id is an error, not a skip: a
silently dropped ignore would turn into a red audit nobody can explain.
A reason that says `remove by YYYY-MM-DD` expires: after that day (UTC) the
entry is an error, so a temporary ignore cannot outlive its date (T-7259).
"""

import datetime
import pathlib
import re
import sys
import tempfile
import tomllib


REMOVE_BY = re.compile(r"remove by (\S+?)[,;.)]?(?:\s|$)")


def ignores(path, today=None):
    today = today or datetime.datetime.now(datetime.timezone.utc).date()
    with open(path, "rb") as f:
        listed = tomllib.load(f).get("advisories", {}).get("ignore", [])
    out = []
    for entry in listed:
        rid = entry.get("id") if isinstance(entry, dict) else entry
        if not isinstance(rid, str) or not rid:
            sys.exit(f"{path}: an [advisories] ignore entry has no id: {entry!r}")
        reason = entry.get("reason", "") if isinstance(entry, dict) else ""
        for raw in REMOVE_BY.findall(reason):
            try:
                due = datetime.date.fromisoformat(raw)
            except ValueError:
                sys.exit(f"{path}: {rid}: 'remove by {raw}' is not a YYYY-MM-DD date")
            if today > due:
                sys.exit(f"{path}: {rid}: ignore expired on {due} (today {today}); "
                         "upgrade or migrate off the crate and drop the entry")
        out += ["--ignore", rid]
    return out


def expires(path, today):
    """The SystemExit message ignores() raises for `today`, or None."""
    try:
        ignores(path, datetime.date.fromisoformat(today))
    except SystemExit as e:
        return str(e)
    return None


def self_test():
    toml = """[advisories]
ignore = [
    { id = "RUSTSEC-2024-0001", reason = "a table" },
    "RUSTSEC-2024-0002",
]
[bans]
deny = [{ name = "openssl" }]
"""
    with tempfile.TemporaryDirectory() as d:
        p = pathlib.Path(d, "deny.toml")
        p.write_text(toml)
        got = ignores(p)
        want = ["--ignore", "RUSTSEC-2024-0001", "--ignore", "RUSTSEC-2024-0002"]
        if got != want:
            sys.exit(f"self-test: {got} != {want}")
        p.write_text("[advisories]\n")
        if ignores(p) != []:
            sys.exit("self-test: no ignore list must give no flags")
        cases = {
            "remove by 2027-02-18 or when T-1 lands": ("2027-02-19", True),
            "remove by 2027-02-18, or earlier": ("2027-02-18", False),
            "remove by 2027-02-18.": ("2026-11-18", False),
            "drop when the crate releases": ("2030-01-01", False),
            "remove by 2027-02-30 or when T-1 lands": ("2026-11-18", True),
        }
        for reason, (today, red) in cases.items():
            p.write_text(f'[advisories]\nignore = [{{ id = "RUSTSEC-2024-0001", reason = "{reason}" }}]\n')
            if bool(expires(p, today)) != red:
                sys.exit(f"self-test: {reason!r} on {today} must be {'red' if red else 'green'}")
    print("advisory-ignores: self-test passed")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    else:
        print(" ".join(ignores(sys.argv[1] if len(sys.argv) > 1 else "deny.toml")))
