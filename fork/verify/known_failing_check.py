#!/usr/bin/env python3
"""Every known-failing entry must still name a test that exists.

fork/verify/known-failing is a nextest filterset: consumers OR the expressions
together and exclude the result from the required-green set.  An expression that
matches nothing excludes nothing, and reports success either way -- so an entry
whose test upstream renamed or deleted decays into a comment that looks like
coverage.  That is the same silent-green shape as a filter matching zero tests,
which this repository has shipped more than once.

The strict check is `cargo nextest list`, which needs a full build and a v8
artifact.  This is the cheap half: every `test(NAME)` must appear somewhere in
codex-rs as source text.  It cannot prove the filterset selects the test, but it
does catch the case that actually happens -- upstream renames a test and the
entry silently stops covering anything.

A comment line `# since: rust-vX.Y.Z` applies to the entry that follows it: the
test arrives with that upstream tag. On a tree whose fork/UPSTREAM base is older
the entry is reported pending, not dead -- and the test must still be absent
there, or the directive is wrong. Without it an entry for a test the next tag
introduces could only ever travel on a staging branch.

Exit codes: 0 ok, 1 fail, 2 could-not-run, 3 pending.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path

# test(foo), test(=exact::path), test(~substring)
TEST_ARG = re.compile(r"test\(\s*([=~]?)([^)]+?)\s*\)")
SINCE = re.compile(r"^#\s*since:\s*(rust-v\d+\.\d+\.\d+)\s*$")
STABLE_TAG = re.compile(r"^rust-v(\d+)\.(\d+)\.(\d+)$")


def tag_key(tag: str | None) -> tuple[int, int, int] | None:
    m = STABLE_TAG.match(tag or "")
    return (int(m[1]), int(m[2]), int(m[3])) if m else None


def base_tag(root: Path) -> str | None:
    try:
        with open(root / "fork" / "UPSTREAM", "rb") as fh:
            return tomllib.load(fh).get("tag")
    except (OSError, tomllib.TOMLDecodeError):
        return None


def grep(root: Path, needle: str) -> bool:
    """True if `needle` appears anywhere in codex-rs as source text."""
    hit = subprocess.run(
        ["git", "-C", str(root), "grep", "-l", "-F", needle, "--", "codex-rs"],
        capture_output=True,
        text=True,
    )
    return hit.returncode == 0 and bool(hit.stdout.strip())


def parameterised(root: Path, needle: str) -> bool:
    """True if `needle` names a test carrying #[test_case] attributes.

    nextest reports such a test as `path::fn::case`, which an exact `test(=fn)`
    filter never matches. Read as source text, deliberately: this check is the
    cheap half and must not need a build.
    """
    hit = subprocess.run(
        [
            "git",
            "-C",
            str(root),
            "grep",
            "-n",
            "-B8",
            "-F",
            f"fn {needle}",
            "--",
            "codex-rs",
        ],
        capture_output=True,
        text=True,
    )
    return "test_case" in hit.stdout


def entries(path: Path) -> list[tuple[int, str, str | None]]:
    out = []
    since: str | None = None
    for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        s = line.strip()
        if not s:
            continue
        if s.startswith("#"):
            m = SINCE.match(s)
            if m:
                since = m[1]
            continue
        out.append((n, s, since))
        since = None
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", default=".", help="repository root")
    args = ap.parse_args()
    root = Path(args.root).resolve()
    # BOTH filesets, not just one. CI ORs them together into a single exclusion,
    # so an entry decaying in either hides the same tests -- but this check read
    # only `known-failing` from the day it was written, leaving
    # `known-failing-upstream` unvalidated. rust-v0.152.0 found the gap: an
    # exact-match entry there stopped covering a test upstream parameterised, and
    # nothing noticed until the shard went red.
    src = root / "codex-rs"
    listings = [
        p
        for p in (
            root / "fork" / "verify" / "known-failing",
            root / "fork" / "verify" / "known-failing-upstream",
        )
        if p.is_file()
    ]
    if not listings or not src.is_dir():
        print(f"skip: no known-failing listing, or {src} is missing", file=sys.stderr)
        return 2

    base = base_tag(root)
    base_key = tag_key(base)
    dead: list[str] = []
    pending: list[str] = []
    checked = 0
    for listing, lineno, expr, since in (
        (l, n, e, s) for l in listings for n, e, s in entries(l)
    ):
        names = TEST_ARG.findall(expr)
        if not names:
            # A package()-only entry excludes a whole crate; nothing to resolve.
            continue
        for _prefix, name in names:
            # `=exact::path::name` and `~substring` both reduce to a literal the
            # source must contain; the last path segment is the function name.
            needle = name.split("::")[-1]
            checked += 1
            # A `#[test_case(...; "some description")]` case is written with
            # spaces in the source and reported by nextest with underscores, so
            # the strict form legitimately misses it. Fall back to the spaced
            # form rather than calling a live test dead -- a false "dead" here
            # blocks CI on an entry that is doing its job, which is worse than
            # the slightly looser match this costs. Only reached when the exact
            # literal already missed.
            candidates = [needle]
            if "_" in needle:
                candidates.append(needle.replace("_", " "))
            exists = any(grep(root, c) for c in candidates)
            since_key = tag_key(since)
            if since_key and base_key and base_key < since_key:
                if exists:
                    dead.append(
                        f"{listing.name}:{lineno}: {needle!r} already exists at {base}, "
                        f"so `# since: {since}` is wrong — {expr}"
                    )
                else:
                    pending.append(
                        f"{listing.name}:{lineno}: {needle!r} arrives with {since}; "
                        f"this tree's base is {base} — {expr}"
                    )
                continue
            if not exists:
                dead.append(
                    f"{listing.name}:{lineno}: no test named {needle!r} exists — {expr}"
                )
            elif _prefix == "=" and parameterised(root, needle):
                # An exact filter cannot match a parameterised test: nextest
                # reports those as `path::to::fn::case_name`, and `test(=fn)`
                # matches the bare name only. The function is still in the
                # source, so the liveness grep above is satisfied and the entry
                # looks healthy while covering nothing. rust-v0.152.0 reached
                # exactly this by adding #[test_case] to a test that had been
                # excluded by `=` since rust-v0.149.0.
                dead.append(
                    f"{listing.name}:{lineno}: {needle!r} is parameterised, so the exact "
                    f"filter `test(=...)` matches none of its cases — drop the "
                    f"`=` to match them all — {expr}"
                )

    if dead:
        print(
            f"\n{len(dead)} known-failing entr(ies) name a test that no longer exists:",
            file=sys.stderr,
        )
        for d in dead:
            print(f"  - {d}", file=sys.stderr)
        print(
            "\n  An entry that matches nothing excludes nothing and still reports",
            file=sys.stderr,
        )
        print(
            "  success. Upstream probably renamed or removed the test: drop the",
            file=sys.stderr,
        )
        print(
            "  entry, or re-point it and re-verify the cause still applies.",
            file=sys.stderr,
        )
        return 1
    if pending:
        for p in pending:
            print(f"pending: {p}")
        print(
            f"ok: {checked - len(pending)} known-failing test name(s) exist in codex-rs, "
            f"{len(pending)} pending a later base"
        )
        return 3
    print(f"ok: all {checked} known-failing test name(s) still exist in codex-rs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
