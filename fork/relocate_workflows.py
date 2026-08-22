#!/usr/bin/env python3
"""Deny-by-default relocation of upstream workflows into fork/upstream-workflows/.

Every ``.github/workflows/*.yml|*.yaml`` whose basename is not listed in
``fork/workflows.allow`` is moved to ``fork/upstream-workflows/`` — plus
``.github/dependabot.yaml``, because Dependabot version updates are enabled by
that file's mere presence and disabled by removing it.

This is a deterministic REBUILD, not a one-shot: assembly re-runs it on every
sync so a workflow upstream adds next month is relocated with zero
configuration, and a second run on an already-relocated tree is a no-op.  The
relocated copies are byte-identical to upstream's (excluded from substitutions)
and exist only on the generated ``main`` — the series never touches them, so
`git rerere`'s delete/modify blind spot never comes into play.

Non-YAML files in .github/workflows/ (README.md, Dockerfile.bazel, zstd) are
left alone; ``zstd`` is load-bearing — build-codex-package-archive.sh:174-175
puts .github/workflows on PATH to find it.

Exit codes: 0 = nothing to do / relocation done; 1 = --check found files out of
place, or a basename is both allowlisted and already relocated (allowlist and
tree disagree — a human must reconcile).
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

FORK_DIR = Path(__file__).resolve().parent
DEFAULT_ROOT = FORK_DIR.parent
WORKFLOWS_DIR = Path(".github/workflows")
DEST_DIR = Path("fork/upstream-workflows")
DEPENDABOT = Path(".github/dependabot.yaml")


def load_allowlist(path: Path) -> set[str]:
    allow: set[str] = set()
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.split("#", 1)[0].strip()
        if line:
            allow.add(line)
    if not allow:
        raise SystemExit(f"relocate: allowlist {path} is empty — refusing to relocate everything blind")
    return allow


def is_workflow_yaml(path: Path) -> bool:
    return path.is_file() and path.suffix in (".yml", ".yaml")


def plan(root: Path, allow: set[str]) -> list[tuple[Path, Path]]:
    """(src, dst) pairs, repo-relative, in deterministic order."""
    moves: list[tuple[Path, Path]] = []
    wf = root / WORKFLOWS_DIR
    if wf.is_dir():
        for entry in sorted(wf.iterdir()):
            if is_workflow_yaml(entry) and entry.name not in allow:
                moves.append((WORKFLOWS_DIR / entry.name, DEST_DIR / entry.name))
    if (root / DEPENDABOT).is_file():
        moves.append((DEPENDABOT, DEST_DIR / DEPENDABOT.name))
    return moves


def stale_relocations(root: Path, allow: set[str]) -> list[Path]:
    """Allowlisted basenames sitting in fork/upstream-workflows/.

    A basename cannot be both live and relocated: it breaks verify's
    set-equality check (relocated set == upstream set − allowlist).  This state
    only arises when a human allowlists a workflow after a relocation without
    cleaning up — refuse and make them decide.
    """
    dest = root / DEST_DIR
    if not dest.is_dir():
        return []
    return [DEST_DIR / e.name for e in sorted(dest.iterdir()) if is_workflow_yaml(e) and e.name in allow]


def git_mv(root: Path, src: Path, dst: Path) -> bool:
    """git mv keeps the rename visible to the index; -f because a re-arrived
    upstream file must overwrite any older relocated copy (the tag's bytes are
    authoritative)."""
    res = subprocess.run(
        ["git", "-C", str(root), "mv", "-f", str(src), str(dst)],
        capture_output=True,
        text=True,
    )
    return res.returncode == 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", default=str(DEFAULT_ROOT), help="tree to operate on (default: repo root)")
    parser.add_argument(
        "--allow",
        default=str(FORK_DIR / "workflows.allow"),
        help="allowlist file (default: fork/workflows.allow)",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="report what would move; exit 1 if anything is out of place",
    )
    args = parser.parse_args(argv)

    root = Path(args.root).resolve()
    allow = load_allowlist(Path(args.allow))
    moves = plan(root, allow)
    stale = stale_relocations(root, allow)

    for path in stale:
        print(f"relocate: CONFLICT {path}: basename is allowlisted but sits in {DEST_DIR}", file=sys.stderr)

    if args.check:
        for src, dst in moves:
            print(f"relocate: would move {src} -> {dst}")
        if moves or stale:
            print(f"relocate --check: {len(moves)} file(s) out of place, {len(stale)} conflict(s)", file=sys.stderr)
            return 1
        print("relocate --check: clean")
        return 0

    if stale:
        return 1
    if not moves:
        print("relocate: nothing to move")
        return 0

    (root / DEST_DIR).mkdir(parents=True, exist_ok=True)
    for src, dst in moves:
        if not git_mv(root, src, dst):
            # Outside a git repo (or the file is untracked): plain rename.
            os.replace(root / src, root / dst)
        print(f"relocate: {src} -> {dst}")
    print(f"relocate: moved {len(moves)} file(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
