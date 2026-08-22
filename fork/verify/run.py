#!/usr/bin/env python3
"""Orchestrator for the ore invariant suite (registry: manifest.toml).

  run.py --suite {static,crate,artifact,egress,all} [--artifact-dir DIR]
         [--bin PATH] [--root DIR] [--api] [--assume-release] [--release-gate]

Runs each selected check in manifest order and prints one status line per
check.  Statuses:

  OK       the invariant holds
  PENDING  the check ran and found only expected-not-yet-true conditions —
           states a later series commit or the assemble pass makes true.
           NOT a failure; NOT silence either, so nobody mistakes it for proof.
  WARN     a warn-severity check failed; printed, never fatal
  SKIP     the check could not run (no binary/artifacts supplied, missing
           tool); fatal for nothing, visible always
  FAIL     the invariant is broken (or a hard check's script is absent)

Exit status is non-zero iff any hard check FAILed.

Checks marked release_required are REFUSED a debug binary: upstream compiles
the Statsig/analytics transmission paths and the update checker out under
cfg!(debug_assertions), so a debug binary passes those checks whether or not
the fork's patches are present — a green debug run proves nothing (fork/README
"Why the invariants must test --release builds").  Point --bin at a
target/release/ build, or pass --assume-release for a binary whose path does
not reveal its profile (never for one under target/debug/).

--release-gate says this run IS the pre-publish gate: every check carrying
release_rerun gets that argv appended, which is how a check that is honest in
debug but has one debug-degenerate assertion (the crate suite's Statsig route
strip) gets its release-profile re-run without becoming release_required.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import tomllib
from pathlib import Path

SUITES = ("static", "crate", "artifact", "egress", "all")
SEVERITIES = ("hard", "warn")

STATUS_OK, STATUS_FAIL, STATUS_SKIP, STATUS_PENDING = 0, 1, 2, 3


def load_manifest(here: Path) -> list[dict]:
    with open(here / "manifest.toml", "rb") as fh:
        data = tomllib.load(fh)
    checks = data.get("check", [])
    seen: set[str] = set()
    for c in checks:
        cid = c.get("id")
        if not cid or cid in seen:
            raise SystemExit(f"manifest.toml: missing or duplicate check id: {cid!r}")
        seen.add(cid)
        for field in ("suite", "severity", "reason"):
            if not c.get(field):
                raise SystemExit(f"manifest.toml: check {cid}: {field} is mandatory")
        if bool(c.get("script")) == bool(c.get("command")):
            raise SystemExit(
                f"manifest.toml: check {cid}: exactly one of script (a file under fork/verify/) "
                "or command (argv run in the cargo workspace) is mandatory"
            )
        if c["suite"] not in SUITES:
            raise SystemExit(f"manifest.toml: check {cid}: suite {c['suite']!r} not in {SUITES}")
        if c["severity"] not in SEVERITIES:
            raise SystemExit(f"manifest.toml: check {cid}: severity {c['severity']!r} not in {SEVERITIES}")
        for field in ("needs_v8", "release_required"):
            if field not in c:
                raise SystemExit(f"manifest.toml: check {cid}: {field} is mandatory")
    return checks


def binary_profile(bin_path: Path) -> str:
    """'debug' | 'release' | 'unknown', judged from the cargo target layout."""
    parts = bin_path.resolve().parts
    for i, part in enumerate(parts[:-1]):
        if part == "target" or (i and parts[i - 1] == "target"):
            if "debug" in parts[i:]:
                return "debug"
            if "release" in parts[i:] or any(p.endswith("_release") for p in parts[i:]):
                return "release"
    return "unknown"


def substitute(args: list[str], mapping: dict[str, str]) -> list[str]:
    return [a.format(**mapping) for a in args]


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--suite", required=True, choices=SUITES)
    ap.add_argument("--artifact-dir", help="codex-package dir of shipped binaries (release by construction)")
    ap.add_argument("--bin", help="built entrypoint binary")
    ap.add_argument("--root", help="repo root (default: two levels above this script)")
    ap.add_argument("--api", action="store_true",
                    help="let checks that support it query the GitHub API (workflow disablement)")
    ap.add_argument("--assume-release", action="store_true",
                    help="vouch that --bin is a release build when its path does not show it")
    ap.add_argument("--release-gate", action="store_true",
                    help="this run is the pre-publish gate: append each check's release_rerun argv")
    args = ap.parse_args(argv)

    here = Path(__file__).resolve().parent
    root = Path(args.root).resolve() if args.root else here.parents[1]
    checks = load_manifest(here)

    selected = [c for c in checks
                if args.suite == "all" or c["suite"] == "all" or c["suite"] == args.suite]

    mapping = {"root": str(root), "bin": args.bin or "", "artifact_dir": args.artifact_dir or ""}
    profile = None
    if args.bin:
        profile = "release" if args.assume_release else binary_profile(Path(args.bin))
        if args.assume_release and binary_profile(Path(args.bin)) == "debug":
            profile = "debug"  # a debug path is never vouchable

    counts = {"OK": 0, "PENDING": 0, "WARN": 0, "SKIP": 0, "FAIL": 0}
    print(f"ore invariant suite — suite={args.suite}, root={root}")

    for check in selected:
        cid = check["id"]
        severity = check["severity"]
        pending_check = bool(check.get("pending"))

        def record(status: str, reason: str = "") -> None:
            counts[status] += 1
            print(f"{status} {cid}" + (f": {reason}" if reason else ""))

        script = here / check["script"] if check.get("script") else None
        if script is not None and not script.is_file():
            if pending_check:
                record("PENDING", f"{check['script']} not present yet (lands with a later fork-verify commit)")
            elif severity == "warn":
                record("WARN", f"{check['script']} missing")
            else:
                record("FAIL", f"{check['script']} missing — the suite is incomplete, not passing")
            continue

        requires = check.get("requires", "")
        use_artifacts = bool(args.artifact_dir and check.get("args_artifacts"))
        use_bin = bool(args.bin and check.get("args_bin")) and not use_artifacts
        if requires == "bin" and not args.bin:
            record("SKIP", "needs --bin (no binary supplied)")
            continue
        if requires == "bin_or_artifacts" and not (args.bin or args.artifact_dir):
            record("SKIP", "needs --bin or --artifact-dir")
            continue

        if check["release_required"] and use_bin and profile != "release":
            record(
                "FAIL",
                f"refusing to run against a {profile} binary: upstream suppresses the Statsig "
                f"exporter and the update checker under cfg!(debug_assertions), so this check "
                f"passes on a debug build of a COMPLETELY UNPATCHED tree — a debug-green run "
                f"proves nothing. Build with --release"
                + ("" if profile == "debug" else ", or pass --assume-release to vouch for it"),
            )
            continue

        cwd = None
        if script is None:
            # A command row is a cargo invocation; cargo picks its workspace from the cwd,
            # and run.py is invoked from the repo root, which carries no Cargo.toml.
            argv_check = substitute(check["command"], mapping)
            cwd = root / "codex-rs"
        else:
            argv_check = [sys.executable, str(script)] if script.suffix == ".py" else [str(script)]
        argv_check += substitute(check.get("args", []), mapping)
        if use_artifacts:
            argv_check += substitute(check["args_artifacts"], mapping)
        elif use_bin:
            argv_check += substitute(check["args_bin"], mapping)
        if args.release_gate:
            argv_check += substitute(check.get("release_rerun", []), mapping)
        if args.api and check.get("accepts_api"):
            argv_check.append("--api")

        try:
            proc = subprocess.run(argv_check, capture_output=True, text=True, timeout=1800, cwd=cwd)
        except (OSError, subprocess.TimeoutExpired) as err:
            record("FAIL", f"could not execute: {err}")
            continue
        output = (proc.stdout + proc.stderr).rstrip()
        for line in output.splitlines():
            print(f"    {line}")

        if proc.returncode == STATUS_OK:
            record("OK")
        elif proc.returncode == STATUS_SKIP:
            reason = next((l.removeprefix("skip: ") for l in output.splitlines() if l.startswith("skip:")), "")
            record("SKIP", reason)
        elif proc.returncode == STATUS_PENDING:
            n = sum(1 for l in output.splitlines() if l.startswith("pending:"))
            record("PENDING", f"{n} expected-not-yet-true finding(s) — see lines above, nothing failed")
        else:
            reason = next((l.removeprefix("FAIL: ") for l in output.splitlines() if l.startswith("FAIL")),
                          f"exit code {proc.returncode}")
            if pending_check:
                record("PENDING", f"expected until its series commit lands: {reason}")
            elif severity == "warn":
                record("WARN", reason)
            else:
                record("FAIL", reason)

    print(
        f"summary: {counts['OK']} ok, {counts['PENDING']} pending, {counts['WARN']} warn, "
        f"{counts['SKIP']} skipped, {counts['FAIL']} failed"
    )
    if counts["FAIL"]:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
