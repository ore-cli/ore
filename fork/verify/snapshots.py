#!/usr/bin/env python3
"""Help/version snapshots (I-SNAP): freeze the user-visible CLI surface.

Captures `--help` for the root command and one level of subcommands (the
ratified starting depth), plus `--version`, into fork/snapshots/.  The
snapshots guard the rebrand surface — program name, `ore <sub>` usage lines,
hidden subcommands staying hidden — so a missed substitution after a sync is
a readable diff, not a silent regression.

Output is normalised before compare/store: the semver becomes {{VERSION}}
(and the base line's tag/SHA {{BASE_TAG}}/{{BASE_SHA}}, so fixtures survive
releases), temp paths become {{TMP}}, trailing whitespace is stripped.
Capture uses a pipe for stdout with COLUMNS unset: clap wraps at its
deterministic non-tty default width.

  snapshots.py --bin PATH           compare against fork/snapshots/
  snapshots.py --bin PATH --update  rewrite the fixtures (review the diff!)

Exit codes: 0 ok, 1 fail, 2 no binary available, 3 no fixtures recorded yet.
"""

from __future__ import annotations

import argparse
import difflib
import os
import re
import subprocess
import sys
from pathlib import Path

VERSION_TOKEN_RE = re.compile(r"\b\d+\.\d+\.\d+(-(?:alpha(?:\.\d+){0,2}|beta(?:\.\d+)?))?\b")
BASE_LINE_RE = re.compile(r"^codex-base: (\S+) \(([0-9a-f]{7,40})\)$", re.M)
TMP_RE = re.compile(r"(/private/var/folders/\S+|/var/folders/\S+|/tmp/\S+)")


def run_help(bin_path: Path, args: list[str]) -> str | None:
    env = {k: v for k, v in os.environ.items() if k != "COLUMNS"}
    try:
        proc = subprocess.run([str(bin_path), *args], capture_output=True, text=True,
                              timeout=60, env=env)
    except (OSError, subprocess.TimeoutExpired):
        return None
    # clap prints help on stdout with exit 0; a subcommand that rejects
    # --help (none do today) would land here as None and be reported.
    return proc.stdout if proc.returncode == 0 and proc.stdout else None


def normalize(text: str) -> str:
    text = BASE_LINE_RE.sub("codex-base: {{BASE_TAG}} ({{BASE_SHA}})", text)
    text = VERSION_TOKEN_RE.sub("{{VERSION}}", text)
    text = TMP_RE.sub("{{TMP}}", text)
    return "\n".join(ln.rstrip() for ln in text.splitlines()) + "\n"


def discover_subcommands(root_help: str) -> list[str]:
    """Parse the Commands: section of clap's help output."""
    subs: list[str] = []
    in_commands = False
    for line in root_help.splitlines():
        if not in_commands:
            if line.strip() == "Commands:":
                in_commands = True
            continue
        if line.strip() == "" or not line.startswith("  "):
            break
        name = line.split()[0].rstrip(",")
        if name and name != "help":
            subs.append(name)
    return subs


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", help="repo root (default: two levels above this script)")
    ap.add_argument("--bin", metavar="PATH", help="built entrypoint binary to snapshot")
    ap.add_argument("--update", action="store_true", help="rewrite fixtures instead of comparing")
    args = ap.parse_args(argv)

    root = Path(args.root).resolve() if args.root else Path(__file__).resolve().parents[2]
    snap_dir = root / "fork" / "snapshots"

    if not args.bin:
        print("skip: no binary available yet — snapshots need a built entrypoint (--bin PATH)")
        return 2
    bin_path = Path(args.bin)
    if not bin_path.is_file():
        print(f"skip: binary {bin_path} does not exist")
        return 2

    captures: dict[str, str] = {}
    root_help = run_help(bin_path, ["--help"])
    if root_help is None:
        print(f"FAIL: {bin_path} --help did not succeed")
        return 1
    captures["help/root.txt"] = normalize(root_help)

    for sub in discover_subcommands(root_help):
        sub_help = run_help(bin_path, [sub, "--help"])
        if sub_help is None:
            print(f"FAIL: {bin_path} {sub} --help did not succeed")
            return 1
        captures[f"help/{sub}.txt"] = normalize(sub_help)

    try:
        version_out = subprocess.run([str(bin_path), "--version"], capture_output=True,
                                     text=True, timeout=60).stdout
    except (OSError, subprocess.TimeoutExpired) as err:
        print(f"FAIL: {bin_path} --version did not run: {err}")
        return 1
    captures["version.txt"] = normalize(version_out)

    if args.update:
        for rel, text in sorted(captures.items()):
            dest = snap_dir / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(text, encoding="utf-8")
            print(f"wrote {dest.relative_to(root)}")
        # Fixtures no longer captured (a subcommand upstream removed) linger
        # otherwise and would fail the next compare.
        expected = set(captures)
        for old in sorted((snap_dir / "help").glob("*.txt")):
            rel = old.relative_to(snap_dir).as_posix()
            if rel not in expected:
                old.unlink()
                print(f"removed stale {old.relative_to(root)}")
        print(f"ok: {len(captures)} fixtures updated — review and commit the diff on delta")
        return 0

    have_fixtures = any((snap_dir / rel).is_file() for rel in captures)
    if not have_fixtures:
        print("pending: no fixtures recorded yet — run snapshots.py --update --bin PATH once a "
              "binary exists, then commit fork/snapshots/ on delta")
        return 3

    fails = 0
    for rel, text in sorted(captures.items()):
        fixture = snap_dir / rel
        if not fixture.is_file():
            print(f"FAIL: fixture {fixture.relative_to(root)} missing (new subcommand?) — "
                  f"run snapshots.py --update")
            fails += 1
            continue
        want = fixture.read_text(encoding="utf-8")
        if want != text:
            fails += 1
            print(f"FAIL: {rel} drifted from its fixture:")
            for ln in list(difflib.unified_diff(want.splitlines(), text.splitlines(),
                                                f"fixture/{rel}", "captured", lineterm=""))[:40]:
                print(f"  {ln}")
    stale = [p for p in (snap_dir / "help").glob("*.txt")
             if p.relative_to(snap_dir).as_posix() not in captures] if (snap_dir / "help").is_dir() else []
    for p in stale:
        print(f"FAIL: stale fixture {p.relative_to(root)} — its subcommand no longer exists; "
              f"run snapshots.py --update")
        fails += 1

    if fails:
        return 1
    print(f"ok: {len(captures)} snapshots match their fixtures")
    return 0


if __name__ == "__main__":
    sys.exit(main())
