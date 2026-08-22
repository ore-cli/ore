#!/usr/bin/env python3
"""Dependency gate: the telemetry-crate set in Cargo.lock is frozen.

Two assertions over `codex-rs/Cargo.lock` (parsed with tomllib — a lockfile is
plain TOML with a [[package]] array):

  1. {packages matching the telemetry watch prefixes} == telemetry-baseline.txt,
     exactly.  No additions (new telemetry arriving with a sync) and no silent
     removals (upstream dropping a crate invalidates the seam analysis the
     baseline documents).  Regenerate deliberately with --regen after review.

  2. No package matches the FORBIDDEN_NEW analytics-SDK name net.  Always
     fatal: there is no legitimate reason for any of these to appear.

"None survives" is deliberately NOT the gate: the crates stay linked because
codex-http-client needs opentelemetry at runtime for traceparent injection and
upstream bans workspace features that could compile it out.  Unreachability is
proven behaviourally (telemetry crate tests + release-build egress), not here.

Exit codes: 0 ok, 1 fail, 2 could-not-run.
"""

from __future__ import annotations

import argparse
import sys
import tomllib
from pathlib import Path

WATCH_PREFIXES = ("opentelemetry", "tracing-opentelemetry", "sentry")

# Best-effort name net over analytics/monitoring SDKs.  Substring match on the
# package name; every current Cargo.lock package was checked against this list
# for false positives when it was written.
FORBIDDEN_NEW = (
    "statsig", "posthog", "segment-", "amplitude", "mixpanel", "datadog",
    "dd-trace", "honeycomb", "bugsnag", "rollbar", "newrelic", "appsignal",
    "launchdarkly",
)


def load_lock_names(lock_path: Path) -> set[str]:
    with open(lock_path, "rb") as fh:
        data = tomllib.load(fh)
    return {p["name"] for p in data.get("package", [])}


def load_baseline(path: Path) -> set[str]:
    names = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            names.add(line)
    return names


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", help="repo root (default: two levels above this script)")
    ap.add_argument("--regen", action="store_true",
                    help="rewrite the baseline body from the current Cargo.lock (deliberate, reviewed)")
    args = ap.parse_args(argv)

    root = Path(args.root).resolve() if args.root else Path(__file__).resolve().parents[2]
    lock_path = root / "codex-rs" / "Cargo.lock"
    baseline_path = Path(__file__).resolve().parent / "telemetry-baseline.txt"

    if not lock_path.is_file():
        print(f"skip: {lock_path} does not exist")
        return 2

    names = load_lock_names(lock_path)
    watched = {n for n in names if n.startswith(WATCH_PREFIXES)}

    if args.regen:
        text = baseline_path.read_text(encoding="utf-8")
        header = "".join(line + "\n" for line in text.splitlines() if line.startswith("#"))
        baseline_path.write_text(header + "".join(n + "\n" for n in sorted(watched)), encoding="utf-8")
        print(f"ok: baseline regenerated with {len(watched)} crates — review the diff before committing")
        return 0

    failed = False

    forbidden_hits = sorted(n for n in names if any(f in n for f in FORBIDDEN_NEW))
    if forbidden_hits:
        failed = True
        for n in forbidden_hits:
            print(f"FAIL: forbidden analytics SDK in Cargo.lock: {n}")

    baseline = load_baseline(baseline_path)
    added = sorted(watched - baseline)
    removed = sorted(baseline - watched)
    for n in added:
        print(f"FAIL: telemetry crate added since the frozen baseline: {n} "
              f"(a sync brought new telemetry; review, then deps_gate.py --regen)")
    for n in removed:
        print(f"FAIL: telemetry crate missing from Cargo.lock but frozen in the baseline: {n} "
              f"(upstream dropped it; re-verify the seam analysis, then deps_gate.py --regen)")
    failed = failed or added or removed

    if not failed:
        print(f"ok: telemetry crate set == frozen baseline ({len(baseline)} crates); "
              f"no forbidden analytics SDKs among {len(names)} packages")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
