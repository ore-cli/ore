#!/usr/bin/env python3
"""Undo the timing a loaded machine wrote into regenerated snapshots.

    settle-snapshots.py --root WORKTREE [--base-ref REF]

assemble's snapshot regen records whatever each test renders, and many tui
renders show elapsed time and the spinner frame: "• Working (0s • esc to
interrupt)" on an idle machine, "◦ Working (27s ..." on a busy one. The regen
exists to carry the rebrand into snapshots, and the rebrand changes letters.
So a regenerated line that differs from its pre-regen version only in elapsed
seconds, the spinner glyph and the padding around them is put back as it was.

Lines are paired within same-length replaced blocks, so a snapshot whose
layout the rebrand reflowed keeps every reflowed line. Stdlib-only; prints one
line per file it settles and exits 0.
"""

from __future__ import annotations

import argparse
import difflib
import re
import subprocess
import sys
from pathlib import Path

SPINNER = str.maketrans({"◦": "•"})


ELAPSED = re.compile(r"\b\d+s\b")


def settled_form(line: str) -> str:
    """The line with everything a slow render can change made constant.

    Only elapsed seconds ("27s") are normalized, not every number: a version
    string the rebrand changed ("v0.0.0" to "v1.161.0") must survive.
    """
    line = ELAPSED.sub("0s", line.translate(SPINNER))
    return re.sub(r" +", " ", line).rstrip()


def settle(old: list[str], new: list[str]) -> tuple[list[str], int]:
    out: list[str] = []
    reverted = 0
    matcher = difflib.SequenceMatcher(a=old, b=new, autojunk=False)
    for op, a0, a1, b0, b1 in matcher.get_opcodes():
        if op == "replace" and a1 - a0 == b1 - b0:
            for before, after in zip(old[a0:a1], new[b0:b1]):
                if settled_form(before) == settled_form(after):
                    out.append(before)
                    reverted += 1
                else:
                    out.append(after)
        else:
            out.extend(new[b0:b1])
    return out, reverted


def git(root: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(root), *args], check=True, capture_output=True, text=True
    ).stdout


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--base-ref", default="HEAD")
    args = parser.parse_args()

    changed = git(
        args.root, "diff", "--name-only", args.base_ref, "--", "*.snap"
    ).splitlines()
    for path in changed:
        try:
            before = git(args.root, "show", f"{args.base_ref}:{path}")
        except subprocess.CalledProcessError:
            continue
        target = args.root / path
        if not target.is_file():
            continue
        after = target.read_text()
        lines, reverted = settle(
            before.splitlines(keepends=True), after.splitlines(keepends=True)
        )
        if reverted:
            target.write_text("".join(lines))
            print(f"settled {reverted} line(s): {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
