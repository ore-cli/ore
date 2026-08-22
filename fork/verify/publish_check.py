#!/usr/bin/env python3
"""Refuse to publish material that is not the fork's to publish.

  publish_check.py [--root DIR] [--base <commit-ish>]

Scope: files the FORK added or rewrote -- everything absent from the upstream
base tag, plus the handful of root files the fork owns outright.  Upstream's own
tree is not policed here; its test fixtures are full of invented home-directory
paths that are upstream's business and are already public.

Describing those fixtures without quoting one is deliberate: this file is itself
in scope, and a literal example would make the check fail on its own docstring.

This check exists because of an incident, and the incident is the argument for
it.  `fork/reference/` was published to a public repository carrying verbatim
copies of a private predecessor repository -- its clone URL, branch, commit SHAs
and commit messages -- alongside 1.3 MB of internal working notes written
against the maintainer's local filesystem paths, one of which was a close
reading of a third party's authentication flow assembled into a single document.

Nothing in it was secret.  All of it was avoidable, and none of it was noticed,
because every other property this fork cares about had a check and this one did
not.  The red-team's lesson applies exactly: a check whose subject is what ships
has to read what ships.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

# Each pattern is (regex, what it means, why it must not ship).
FORBIDDEN = [
    (re.compile(r"/Users/[a-z][a-z0-9._-]*/", re.I),
     "a macOS home directory",
     "names the maintainer's account and local layout"),
    (re.compile(r"/home/(?!user/|runner/|ubuntu/|linuxbrew/)[a-z][a-z0-9._-]*/", re.I),
     "a Linux home directory",
     "names the maintainer's account and local layout"),
    (re.compile(r"\bcore-editor\b"),
     "the predecessor organisation",
     "names a private repository that is not this project's to disclose"),
    (re.compile(r"\bcore-ci\b"),
     "the predecessor secret vault",
     "names private infrastructure"),
]

# Directories the fork must never carry, whatever they contain.
FORBIDDEN_DIRS = [
    ("fork/reference/",
     "copies of a predecessor repository and internal audit notes; the reasoning "
     "that mattered lives in the commit messages, fork/README.md and docs/"),
]

TEXT_SUFFIXES = {
    ".rs", ".py", ".sh", ".md", ".toml", ".yaml", ".yml", ".json", ".txt",
    ".ps1", ".js", ".ts", ".diff", ".allow", "",
}


def upstream_files(root: Path, base: str) -> set[str]:
    try:
        out = subprocess.run(
            ["git", "-C", str(root), "ls-tree", "-r", "--name-only", base],
            capture_output=True, text=True, check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError):
        return set()
    return set(out.splitlines())


def tracked_files(root: Path) -> list[str]:
    out = subprocess.run(
        ["git", "-C", str(root), "ls-files"], capture_output=True, text=True, check=True
    ).stdout
    return out.splitlines()


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", default=".", help="repository root")
    ap.add_argument("--base", default="", help="upstream base (default: fork/UPSTREAM's commit)")
    args = ap.parse_args(argv)

    root = Path(args.root).resolve()
    base = args.base
    if not base:
        try:
            import tomllib
            with open(root / "fork" / "UPSTREAM", "rb") as fh:
                base = tomllib.load(fh).get("commit", "")
        except (OSError, ValueError):
            base = ""
    if not base:
        print("skip: no upstream base recorded; cannot tell fork files from upstream's")
        return 2

    upstream = upstream_files(root, base)
    if not upstream:
        print(f"skip: base {base} not available locally")
        return 2

    fails: list[str] = []

    for bad_dir, why in FORBIDDEN_DIRS:
        if (root / bad_dir).is_dir():
            fails.append(f"{bad_dir} must not exist — {why}")

    scanned = 0
    for rel in tracked_files(root):
        if rel in upstream:
            continue  # upstream's own file; not this check's business
        path = root / rel
        if path.suffix not in TEXT_SUFFIXES or not path.is_file():
            continue
        try:
            text = path.read_text()
        except (UnicodeDecodeError, OSError):
            continue
        scanned += 1
        for rx, what, why in FORBIDDEN:
            m = rx.search(text)
            if m:
                line = text[: m.start()].count("\n") + 1
                fails.append(f"{rel}:{line} contains {what} ({m.group(0)!r}) — {why}")

    if fails:
        for f in fails:
            print(f"FAIL: {f}")
        print("FAIL: the fork publishes only what is its own to publish")
        return 1

    print(f"ok: {scanned} fork-authored file(s) carry no local paths and no predecessor references")
    return 0


if __name__ == "__main__":
    sys.exit(main())
