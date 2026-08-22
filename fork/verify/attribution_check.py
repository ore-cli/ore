#!/usr/bin/env python3
"""Verify ore credits OpenAI correctly, and never claims to be OpenAI.

Port of the proven legacy checker (ore-legacy/scripts/attribution-check.py),
mode-aware for the delta/main split.  ore is a fork of openai/codex under
Apache 2.0, which imposes obligations that are easy to satisfy once and then
lose silently on a later sync.  Both directions are checked:

  * Attribution present — LICENSE and NOTICE exist, still carry OpenAI's
    copyright, and NOTICE states modification and disclaims affiliation
    (Apache 2.0 sections 4(b)/(c)/(d), plus trademark hygiene under §6).

  * Attribution not overclaimed — nothing says ore comes from OpenAI, and no
    page hotlinks OpenAI-hosted assets.  Upstream keeps reintroducing "a
    coding agent from OpenAI" and the rebrand rewrites the product name but
    not the claim, so this half needs a machine.

On a pre-assembly (delta) tree, upstream's own prose ("Codex CLI is a coding
agent from OpenAI") is true as written and is rewritten at assemble by the
substitution pass — those unanchored hits report as pending there and fail on
an assembled tree.  Claims anchored on ore as the subject fail everywhere.

Exit codes: 0 ok, 1 fail, 2 could-not-run, 3 only expected-pending findings.
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

# (pattern, why, anchored-on-ore).  Unanchored patterns describe upstream's
# own prose and are judged by tree mode; anchored ones are always failures.
# Deliberately narrow: ore legitimately mentions OpenAI constantly (the OpenAI
# provider, OpenAI-compatible endpoints, OpenAI-style auth) and none of those
# are claims of authorship — a check that fires on true statements gets
# ignored, and an ignored check is worse than no check.
OVERCLAIM_PATTERNS = (
    (r"coding agent from OpenAI", "claims OpenAI authorship", False),
    (r"\bore\b[ ,:]{0,3}(?:is )?OpenAI's (?:command-line )?(?:coding )?agent",
     "claims OpenAI authorship", True),
    (r"\bore\b[ ,:]{0,3}(?:is )?an OpenAI\b", "claims to be an OpenAI product", True),
    (r"open source project led by OpenAI", "claims OpenAI stewardship", False),
    (r"(?:developed|created|maintained|built) by OpenAI\b(?!.*\bCodex\b)",
     "claims OpenAI authorship", False),
    (r"github\.com/openai/codex/blob/[^\s\"')]+\.(?:png|jpg|gif|svg)",
     "hotlinks an OpenAI-hosted image", False),
)

# Crediting upstream is these files' whole job.
ATTRIBUTION_FILES = {"LICENSE", "NOTICE"}

SKIP_DIRS = {
    ".git", "target", "node_modules", "__pycache__", ".venv", "venv",
    "bazel-out", "bazel-bin", "bazel-testlogs", "dist", "build",
    "vendor", "third_party",
}
SKIP_FILE_NAMES = {
    "Cargo.lock", "MODULE.bazel.lock", "pnpm-lock.yaml", "uv.lock",
    "package-lock.json", "flake.lock",
}
SCAN_SUFFIXES = {".md", ".json", ".toml", ".rs", ".ts", ".js", ".py", ".yaml", ".yml", ".sh"}

# .github talks about the openai/codex repo because that is where the files
# came from; fork/ names these phrases as data (this script, the manifest,
# reference audits) — neither is a user-facing claim about ore.
SKIP_PREFIXES = (".github/", "fork/", ".devcontainer/")

# Model-facing prompt text stays byte-identical by ratified decision 11: the
# prompts describe the upstream agent to OpenAI models ("You are Codex …") and
# rebranding them would change model behaviour.  They are the ONE place the
# upstream authorship prose is shipped on purpose, so the overclaim scan must
# not police them (they are also substitution skip_paths).
PROMPT_EXEMPT_PREFIXES = (
    "codex-rs/protocol/src/prompts/",
    "codex-rs/models-manager/",
)
PROMPT_EXEMPT_RE = __import__("re").compile(r"^codex-rs/core/[^/]*prompt[^/]*\.md$")


def tree_is_assembled(root: Path) -> bool:
    try:
        with open(root / "fork" / "UPSTREAM", "rb") as fh:
            return bool(tomllib.load(fh).get("assembled_at"))
    except (OSError, tomllib.TOMLDecodeError):
        return False


def check_attribution_present(root: Path, fails: list[str], pendings: list[str]) -> None:
    license_path = root / "LICENSE"
    if not license_path.is_file():
        fails.append("LICENSE is missing")
    else:
        text = license_path.read_text(encoding="utf-8")
        if "Apache License" not in text:
            fails.append("LICENSE is no longer the Apache License")
        if "OpenAI" not in text:
            fails.append("LICENSE no longer carries OpenAI's copyright line")

    notice_path = root / "NOTICE"
    if not notice_path.is_file():
        # NOTICE is fork-authored (repo-docs series commit); absence before it
        # lands is expected, absence after is a licence violation.
        pendings.append("NOTICE not present yet (authored by the repo-docs series commit)")
        return
    notice = notice_path.read_text(encoding="utf-8")
    if "OpenAI" not in notice:
        fails.append("NOTICE no longer credits OpenAI")
    if not re.search(r"\bcodex\b", notice, re.IGNORECASE):
        fails.append("NOTICE does not name the upstream project (Codex)")
    if not re.search(r"\bmodified\b", notice, re.IGNORECASE):
        fails.append("NOTICE does not state that files were modified (Apache 2.0 section 4(b))")
    if not re.search(r"not affiliated|not endorsed", notice, re.IGNORECASE):
        fails.append("NOTICE does not disclaim affiliation or endorsement")


def iter_files(root: Path):
    import os
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for name in sorted(filenames):
            path = Path(dirpath) / name
            if path.is_symlink() or name in SKIP_FILE_NAMES or name in ATTRIBUTION_FILES:
                continue
            if path.suffix not in SCAN_SUFFIXES:
                continue
            rel = path.relative_to(root).as_posix()
            if rel.startswith(SKIP_PREFIXES) or rel.startswith(PROMPT_EXEMPT_PREFIXES):
                continue
            if PROMPT_EXEMPT_RE.match(rel):
                continue
            yield path, rel


def check_no_overclaim(root: Path, assembled: bool, fails: list[str], pendings: list[str]) -> None:
    compiled = [(re.compile(p), why, anchored) for p, why, anchored in OVERCLAIM_PATTERNS]
    for path, rel in iter_files(root):
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        for lineno, line in enumerate(text.splitlines(), 1):
            for regex, why, anchored in compiled:
                if not regex.search(line):
                    continue
                hit = f"{rel}:{lineno} ({why})"
                if anchored or assembled:
                    fails.append(hit)
                else:
                    pendings.append(f"{hit} — upstream's own prose, rewritten by the substitution pass at assemble")
                break


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", help="repo root (default: two levels above this script)")
    args = ap.parse_args(argv)
    root = Path(args.root).resolve() if args.root else Path(__file__).resolve().parents[2]

    assembled = tree_is_assembled(root)
    print(f"note: tree mode: {'assembled (main)' if assembled else 'pre-assembly (delta)'}")

    fails: list[str] = []
    pendings: list[str] = []
    check_attribution_present(root, fails, pendings)
    check_no_overclaim(root, assembled, fails, pendings)

    for p in pendings:
        print(f"pending: {p}")
    for f in fails:
        print(f"FAIL: {f}")
    if fails:
        print("FAIL: ore is a fork — crediting upstream is required, claiming to be upstream is not allowed")
        return 1
    if pendings:
        return 3
    print("ok: attribution present, nothing claims ore comes from OpenAI")
    return 0


if __name__ == "__main__":
    sys.exit(main())
