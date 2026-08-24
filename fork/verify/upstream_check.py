#!/usr/bin/env python3
"""Base-manifest check (I-BASE): fork/UPSTREAM describes the tree it is in.

Everything downstream treats fork/UPSTREAM as fact — assemble derives --base
from it, auth_fence diffs the fence against its commit, workflows_check builds
the relocation set from that commit's workflow set, `ore --version` line 2
prints it.  A manifest that disagrees with the tree therefore fails nothing
loudly; it quietly moves every one of those checks onto the wrong baseline.

  a. grammar and internal agreement: `tag` is a stable rust-vX.Y.Z (an alpha or
     beta is never a base), `commit` is a full sha, and — when the tag ref
     resolves locally, from refs/upstream/tags/ or refs/tags/ — the tag object
     peels to exactly `commit`.
  b. `commit` is an ancestor of HEAD.  This is the property assemble relies on
     when it replays BASE..delta onto the next tag.
  c. no NEWER upstream stable tag is reachable from HEAD, i.e. the recorded
     base is the one the tree actually sits on rather than one it has moved
     past.  Hard on an assembled tree; a note on delta, whose copy of the
     manifest is a documented stale-ok placeholder.
  d. merge shape: the commit that carries `commit` as a direct parent must be
     the generated merge — two parents in tag-second order (prev main, tag), or
     one for the bootstrap and --reassemble shapes.  Tag-second is what keeps
     `git log --first-parent main` the fork's own line and what lets finalize
     fast-forward.  With refs/remotes/origin/main present, the published main
     must also still be an ancestor (main is append-only).  A pre-assembly tree
     has no merge to judge and reports pending.
  e. placeholder discipline: branch `delta` must carry the placeholder manifest
     (assembled_at empty) and branch `main` a generated one —
     a generated manifest on delta means assemble output was committed to the
     series.  Detached HEAD (every pull_request checkout) skips this.

Exit codes: 0 ok, 1 fail, 2 could-not-run, 3 only expected-pending findings.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path

STABLE_TAG_RE = re.compile(r"^rust-v[0-9]+\.[0-9]+\.[0-9]+$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")

# refs/upstream/tags/ is where the fork fetches upstream tags (layer 1 of the
# tag defence); refs/tags/ is what a plain clone of upstream has.
TAG_NAMESPACES = ("refs/upstream/tags/", "refs/tags/")


class Report:
    def __init__(self):
        self.fails: list[str] = []
        self.pendings: list[str] = []
        self.notes: list[str] = []
        self.oks: list[str] = []

    def finish(self) -> int:
        for line in self.notes:
            print(f"note: {line}")
        for line in self.oks:
            print(f"ok: {line}")
        for line in self.pendings:
            print(f"pending: {line}")
        for line in self.fails:
            print(f"FAIL: {line}")
        return 1 if self.fails else (3 if self.pendings else 0)


def git(root: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-C", str(root), *args],
                          capture_output=True, text=True, timeout=120)


def resolve_tag_ref(root: Path, tag: str) -> tuple[str, str, str] | None:
    """(ref, tag object sha, peeled commit sha) for the first namespace that has it."""
    for ns in TAG_NAMESPACES:
        ref = ns + tag
        obj = git(root, "rev-parse", "--verify", "--quiet", ref)
        if obj.returncode != 0:
            continue
        peeled = git(root, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}")
        if peeled.returncode == 0:
            return ref, obj.stdout.strip(), peeled.stdout.strip()
    return None


def tag_sort_key(name: str) -> tuple[int, ...]:
    """Version order for a `rust-vX.Y.Z` tag.

    Compared numerically per component: a string compare puts rust-v0.149.10
    before rust-v0.149.9, and an unparseable name sorts lowest so it can never
    be mistaken for a newer base.
    """
    match = STABLE_TAG_RE.match(name)
    if not match:
        return (-1,)
    digits = re.findall(r"\d+", match.group(0))
    return tuple(int(part) for part in digits)


def stable_tag_commits(root: Path) -> dict[str, str]:
    """peeled commit sha -> tag name, over both namespaces.  Stable tags only."""
    patterns = [ns + "rust-v*" for ns in TAG_NAMESPACES]
    proc = git(root, "for-each-ref", "--format=%(refname) %(objectname) %(*objectname)", *patterns)
    out: dict[str, str] = {}
    for line in proc.stdout.splitlines():
        parts = line.split()
        if len(parts) < 2:
            continue
        name = parts[0].rsplit("/", 1)[-1]
        if not STABLE_TAG_RE.match(name):
            continue
        # Field 3 is set for annotated tags (the peeled commit); lightweight
        # tags point straight at the commit.
        out[parts[2] if len(parts) > 2 else parts[1]] = name
    return out


def current_branch(root: Path) -> str | None:
    proc = git(root, "symbolic-ref", "--quiet", "--short", "HEAD")
    return proc.stdout.strip() if proc.returncode == 0 else None


def check_merge_shape(root: Path, commit: str, rep: Report) -> None:
    proc = git(root, "rev-list", "--parents", f"{commit}..HEAD")
    if proc.returncode != 0:
        rep.notes.append(f"merge shape unavailable: git rev-list {commit[:12]}..HEAD failed")
        return
    carriers = []
    for line in proc.stdout.splitlines():
        shas = line.split()
        if commit in shas[1:]:
            carriers.append((shas[0], shas[1:]))

    if not carriers:
        rep.fails.append(
            f"no commit between {commit[:12]} and HEAD has it as a direct parent — the recorded "
            f"base is reachable but nothing attaches to it (a squashed or grafted history)"
        )
        return
    if len(carriers) > 1:
        # Upstream's own children of the tag commit are unreachable from a
        # generated main, which merges the tag commit and nothing after it.
        rep.fails.append(
            "more than one commit claims the recorded base as a direct parent: "
            + ", ".join(sha[:12] for sha, _ in carriers)
        )
        return

    sha, parents = carriers[0]
    if len(parents) == 1:
        rep.oks.append(
            f"merge {sha[:12]} has one parent, the base tag — the bootstrap shape "
            f"(--reassemble keeps prev main as its only parent instead)"
        )
    elif len(parents) == 2:
        if parents[1] != commit:
            rep.fails.append(
                f"merge {sha[:12]} carries the upstream tag as its FIRST parent — main's "
                f"first-parent chain must stay the fork's own line (commit-tree TREE "
                f"-p prev-main -p tag), or `git log --first-parent main` walks into upstream"
            )
        else:
            rep.oks.append(f"merge {sha[:12]} parents = (prev main {parents[0][:12]}, base {commit[:12]})")
    else:
        rep.fails.append(f"merge {sha[:12]} has {len(parents)} parents; the generated merge has at most two")

    prev = git(root, "rev-parse", "--verify", "--quiet", "refs/remotes/origin/main")
    if prev.returncode != 0:
        rep.notes.append("append-only check skipped: no refs/remotes/origin/main (the fork is not pushed yet)")
    elif git(root, "merge-base", "--is-ancestor", prev.stdout.strip(), "HEAD").returncode != 0:
        rep.fails.append(
            f"published main ({prev.stdout.strip()[:12]}) is not an ancestor of HEAD — main is "
            f"append-only and finalize fast-forwards it; this candidate would rewrite it"
        )
    else:
        rep.oks.append("append-only: origin/main is an ancestor of HEAD")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", help="repo root (default: two levels above this script)")
    ap.add_argument("--mode", choices=["auto", "delta", "main"], default="auto",
                    help="auto reads fork/UPSTREAM assembled_at")
    args = ap.parse_args(argv)

    root = Path(args.root).resolve() if args.root else Path(__file__).resolve().parents[2]
    rep = Report()

    try:
        with open(root / "fork" / "UPSTREAM", "rb") as fh:
            meta = tomllib.load(fh)
    except (OSError, tomllib.TOMLDecodeError) as err:
        print(f"skip: fork/UPSTREAM unreadable: {err}")
        return 2
    if git(root, "rev-parse", "--git-dir").returncode != 0:
        print(f"skip: {root} is not a git repository")
        return 2

    tag = meta.get("tag", "")
    commit = meta.get("commit", "")
    tag_object = meta.get("tag_object", "")
    assembled = bool(meta.get("assembled_at")) if args.mode == "auto" else (args.mode == "main")
    rep.notes.append(f"tree mode: {'assembled (main)' if assembled else 'pre-assembly (delta)'}")

    # (a) grammar and internal agreement
    if not STABLE_TAG_RE.match(tag):
        rep.fails.append(f"fork/UPSTREAM tag {tag!r} is not a stable rust-vX.Y.Z tag (alphas are never a base)")
    if not SHA_RE.match(commit):
        rep.fails.append(f"fork/UPSTREAM commit {commit!r} is not a full 40-hex sha — nothing below "
                         f"can be resolved against it")
        return rep.finish()
    if tag_object and not SHA_RE.match(tag_object):
        rep.fails.append(f"fork/UPSTREAM tag_object {tag_object!r} is not a full 40-hex sha")
    series_head = meta.get("series_head", "")
    if series_head and not SHA_RE.match(series_head):
        rep.fails.append(f"fork/UPSTREAM series_head {series_head!r} is not a full 40-hex sha")

    resolved = resolve_tag_ref(root, tag) if STABLE_TAG_RE.match(tag) else None
    if resolved is None:
        rep.notes.append(
            f"tag {tag} resolves in neither {' nor '.join(TAG_NAMESPACES)} — "
            f"tag/commit agreement unverified (fetch: git fetch --no-tags upstream "
            f"'+refs/tags/rust-v*:refs/upstream/tags/rust-v*')"
        )
    else:
        ref, obj, peeled = resolved
        if peeled != commit:
            rep.fails.append(
                f"{ref} peels to {peeled} but fork/UPSTREAM records commit = {commit} — "
                f"every check that diffs against the base is using the wrong tree"
            )
        else:
            rep.oks.append(f"{ref} peels to the recorded commit {commit[:12]}")
        if tag_object and obj != tag_object:
            rep.fails.append(f"{ref} is object {obj} but fork/UPSTREAM records tag_object = {tag_object}")

    if git(root, "cat-file", "-e", f"{commit}^{{commit}}").returncode != 0:
        rep.notes.append(f"commit {commit[:12]} is not in this clone (shallow checkout?) — "
                         f"ancestry, staleness and merge shape unverified")
        return rep.finish()

    # (b) the recorded base is where HEAD's history attaches
    if git(root, "merge-base", "--is-ancestor", commit, "HEAD").returncode != 0:
        rep.fails.append(
            f"recorded base {commit[:12]} ({tag}) is NOT an ancestor of HEAD — fork/UPSTREAM and "
            f"the tree disagree; assemble replays BASE..delta and would replay upstream commits "
            f"as series commits"
        )
        return rep.finish()
    rep.oks.append(f"recorded base {tag} ({commit[:12]}) is an ancestor of HEAD")

    # (c) staleness: a newer stable tag already in HEAD's history
    newer: list[str] = []
    by_commit = stable_tag_commits(root)
    if not by_commit:
        rep.notes.append("no upstream stable tag refs in this clone — staleness unchecked")
    else:
        proc = git(root, "rev-list", f"{commit}..HEAD")
        reachable = {by_commit[sha] for sha in proc.stdout.split() if sha in by_commit}
        # Reachability alone is not order. Upstream's stable tags are NOT a chain:
        # rust-v0.149.1 is not a descendant of rust-v0.149.0 -- they fork from a
        # common ancestor, because the patch release was cut from another branch.
        # So after syncing onto 0.149.1 the OLD base 0.149.0 is unreachable from
        # the new one, and this check reported it as "newer stable tag(s)
        # rust-v0.149.0" than 0.149.1 -- blocking the first real sync with a
        # message that contradicted itself. Compare versions, and let reachability
        # only narrow the candidates.
        newer = sorted(
            (name for name in reachable if tag_sort_key(name) > tag_sort_key(tag)),
            key=tag_sort_key,
        )
        if not newer:
            rep.oks.append(f"no upstream stable tag newer than {tag} is reachable from HEAD")
        elif assembled:
            rep.fails.append(
                f"fork/UPSTREAM records {tag} but HEAD already contains newer stable tag(s) "
                f"{', '.join(newer)} — the manifest describes an older base than the tree"
            )
        else:
            rep.notes.append(
                f"fork/UPSTREAM ({tag}) lags the tree, which already contains {', '.join(newer)}; "
                f"stale-ok on delta by design — assemble takes its base from origin/main"
            )

    # (d) merge shape
    if not assembled:
        rep.pendings.append(
            "no generated merge commit on a pre-assembly tree — assemble builds it with "
            "commit-tree TREE -p prev-main -p tag, so the two-parent shape is provable only on main"
        )
    elif newer:
        rep.notes.append("merge shape not judged: the manifest is stale (above), so the commit it "
                         "names is not the one the merge was built from")
    else:
        check_merge_shape(root, commit, rep)

    # (e) placeholder discipline
    branch = current_branch(root)
    generated = bool(meta.get("assembled_at"))
    if branch == "delta" and generated:
        rep.fails.append(
            "branch delta carries a GENERATED fork/UPSTREAM (assembled_at is set) — assemble "
            "output was committed to the series; delta's copy is a placeholder"
        )
    elif branch == "main" and not generated:
        rep.fails.append(
            "branch main carries the delta placeholder fork/UPSTREAM (assembled_at empty) — "
            "main is generated by assemble, never hand-built"
        )
    elif branch in ("delta", "main"):
        rep.oks.append(f"branch {branch} carries the {'generated' if generated else 'placeholder'} manifest")
    else:
        rep.notes.append("placeholder discipline skipped: HEAD is detached (every pull_request checkout is)")

    code = rep.finish()
    if code == 0:
        print("ok: base manifest agrees with the tree")
    return code


if __name__ == "__main__":
    sys.exit(main())
