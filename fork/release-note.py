#!/usr/bin/env python3
"""Build the fixed-format release announcement for a stable ore release.

Almost all of this is DATA, not prose, and deliberately so. fork/assemble.sh
writes the upstream tag and the full `<hash> <Fork-Patch slug> <subject>` list
into every generated main commit, so "what is in this release that was not in the
last one" is derivable from the two release tags alone -- no model, no guessing,
and no way for a summary to claim something the tree does not contain.

The only free text is the upstream-highlights block, which is passed in with
--upstream-bullets and validated before it is used: upstream ships dozens of
commits per tag whose subjects are noisy, and that is the one place a summary
earns its keep. If the bullets fail validation the section is dropped and the
rest of the announcement still goes out -- a malformed summary must never take
the announcement down with it, and must never be posted unchecked.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

SLUG_LINE = re.compile(r"^\s{2}([0-9a-f]{7,40})\s+(\S+)\s+(.*\S)\s*$")
UPSTREAM_LINE = re.compile(r"^Upstream:\s+(rust-v[0-9.]+)\s")
STABLE_TAG = re.compile(r"^ore-v([0-9]+)\.([0-9]+)\.([0-9]+)$")

MAX_BULLETS = 4
MAX_BULLET_CHARS = 160
MAX_ORE_LINES = 12

# Areas that exist only to keep the fork running: the assembly pipeline, the
# invariant suite, the sync machinery, CI. A user of the CLI never sees them, so
# they are counted rather than listed. `release:` is NOT here -- "build Windows
# on tag" lives under it and is exactly what someone reads a release note for.
INTERNAL_AREAS = {"verify", "sync", "fork", "ci", "assemble", "docs", "chore"}


def is_internal(subject: str) -> bool:
    area, sep, _ = subject.partition(":")
    return bool(sep) and area.strip() in INTERNAL_AREAS


def git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, check=True
    ).stdout


def tag_message(repo: Path, tag: str) -> str:
    return git(repo, "log", "-1", "--format=%B", f"{tag}^{{commit}}")


def parse_release(repo: Path, tag: str) -> tuple[str | None, dict[str, str]]:
    """-> (upstream tag, {slug: subject}) for one release tag."""
    upstream: str | None = None
    slugs: dict[str, str] = {}
    for line in tag_message(repo, tag).splitlines():
        m = UPSTREAM_LINE.match(line)
        if m:
            upstream = m.group(1)
            continue
        m = SLUG_LINE.match(line)
        if m:
            _, slug, subject = m.groups()
            # A commit with no Fork-Patch trailer collapses the two fields; its
            # "slug" is really the subject's first word and always carries a
            # colon, which a slug never does.
            if ":" in slug:
                continue
            slugs[slug] = subject
    return upstream, slugs


def stable_tags(repo: Path) -> list[str]:
    out = git(repo, "tag", "-l", "ore-v*", "--sort=version:refname").split()
    return [t for t in out if STABLE_TAG.match(t)]


def clean_bullets(raw: str) -> list[str]:
    """Keep only bullets that pass every rule; drop the section if any fail."""
    lines = [ln.strip().lstrip("-*• ").strip() for ln in raw.splitlines()]
    lines = [ln for ln in lines if ln]
    if not lines or len(lines) > MAX_BULLETS:
        return []
    for ln in lines:
        if len(ln) > MAX_BULLET_CHARS:
            return []
        # No pings, no embedded links, no control characters, no markdown that
        # could restructure the message.
        if "@" in ln or "http" in ln or "```" in ln:
            return []
        if any(ord(c) < 32 for c in ln):
            return []
    return lines


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--tag", required=True, help="release tag being announced (ore-vX.Y.Z)")
    ap.add_argument("--prev", help="previous stable tag (default: the one before --tag)")
    ap.add_argument("--repo", default=".", help="repository root")
    ap.add_argument("--upstream-bullets", help="file of candidate upstream-highlight bullets")
    ap.add_argument("--url", default="", help="release URL")
    args = ap.parse_args()

    repo = Path(args.repo).resolve()
    m = STABLE_TAG.match(args.tag)
    if not m:
        print(f"refusing to announce {args.tag!r}: not a stable ore-vX.Y.Z tag", file=sys.stderr)
        return 2
    version = args.tag[len("ore-v") :]

    prev = args.prev
    if not prev:
        tags = stable_tags(repo)
        prev = tags[tags.index(args.tag) - 1] if args.tag in tags and tags.index(args.tag) > 0 else None

    up_now, slugs_now = parse_release(repo, args.tag)
    up_prev, slugs_prev = (None, {})
    if prev:
        up_prev, slugs_prev = parse_release(repo, prev)

    new = {s: t for s, t in slugs_now.items() if s not in slugs_prev}

    out: list[str] = []
    out.append(f"**ore {version}**")
    out.append("")
    if up_prev and up_now and up_prev != up_now:
        out.append(f"**Upstream** · `{up_prev}` → `{up_now}`")
    elif up_now:
        out.append(f"**Upstream** · `{up_now}`")
    if prev:
        out.append(f"**Since** · `{prev}` — {len(new)} new change(s) in ore")
    out.append("")

    shown = [t for t in new.values() if not is_internal(t)]
    internal = len(new) - len(shown)

    if shown:
        out.append("**New in ore**")
        for subject in shown[:MAX_ORE_LINES]:
            out.append(f"• {subject}")
        if len(shown) > MAX_ORE_LINES:
            out.append(f"• …and {len(shown) - MAX_ORE_LINES} more")
    elif new:
        out.append("**New in ore** · internal changes only")
    # Counted, never silently dropped: a reader can tell the difference between
    # "nothing else happened" and "the rest was plumbing".
    if internal:
        out.append(f"_+{internal} internal change(s) to the fork's own tooling_")
    if shown or new:
        out.append("")

    if args.upstream_bullets:
        p = Path(args.upstream_bullets)
        bullets = clean_bullets(p.read_text(encoding="utf-8")) if p.is_file() else []
        if bullets:
            out.append("**From upstream**")
            out.extend(f"• {b}" for b in bullets)
            out.append("")
        else:
            # Said out loud rather than silently omitted: a missing section that
            # looks like "upstream changed nothing" would be a lie.
            out.append("**From upstream** · summary unavailable for this release")
            out.append("")

    out.append("**Install**")
    out.append(f"`curl -fsSL https://github.com/ore-cli/ore/releases/download/{args.tag}/install.sh | sh -s -- --release {version}`")
    if args.url:
        out.append("")
        out.append(f"<{args.url}>")

    print("\n".join(out))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
