#!/usr/bin/env python3
"""Deny-by-default workflow audit (I-WF).

Upstream workflows ride along in the tree and run from the tree of the pushed
ref, so neutralisation has three layers this script audits two of:

  tree   assemble relocates every .github/workflows/*.y{,a}ml whose basename is
         not in fork/workflows.allow into fork/upstream-workflows/ (main only —
         delta deliberately keeps the live upstream files so series rebases see
         pristine context);
  repo   every non-allowlisted workflow is disabled via the GitHub API
         (--api mode; the ref-independent guard, because delta's tree still
         carries self-firing upstream workflows);
  server a tag ruleset blocks upstream-named tag creation (not tree-visible;
         documented in fork/README).

Checks: (a) allowlist membership, (b) allowed upstream files stay
workflow_call/workflow_dispatch-only, (c) no upstream tag-push globs,
(d) no upstream actions/secrets, (e) fork/upstream-workflows/ set-equality
against the base tag, (f) fork-owned workflows never `secrets: inherit`,
(g) --api: enabled workflows == allowlist.

Whether the tree is assembled is read from fork/UPSTREAM's `assembled_at`;
on a pre-assembly tree the to-be-relocated upstream files report as pending,
and anything that is neither allowlisted, fork-owned, nor part of the base
tag's workflow set still fails hard.

Exit codes: 0 ok, 1 fail, 2 could-not-run, 3 only expected-pending findings.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

# Fallback until fork/workflows.allow lands (it is authored by the assemble /
# workflow-neutralisation commit).  Mirrors ratified decision 5: the three
# workflow_call-only upstream files ore-ci reuses; repo-checks.yml is NOT
# allowed (its checks are invoked as plain script steps instead).
DEFAULT_ALLOW = ["blob-size-policy.yml", "cargo-deny.yml", "codespell.yml"]

SAFE_ON_KEYS = {"workflow_call", "workflow_dispatch"}
UPSTREAM_TAG_PREFIXES = ("rust-v", "codex-zsh-v", "rusty-v8-v", "python-v")

# Upstream-only credentials and actions: any reference means an upstream
# release/publish job survived neutralisation.
FORBIDDEN_LITERALS = (
    "openai/codex-action",
    "CODEX_OPENAI_API_KEY",
    "WINGET_PUBLISH_PAT",
    "DEV_WEBSITE_VERCEL_DEPLOY_HOOK_URL",
)
FORBIDDEN_PREFIXES = ("CODEX_R2_", "AKV_", "AZURE_ARTIFACT_SIGNING_")
# blob-size-policy.yml legitimately uses the public openai/fence action.
FENCE_CARVEOUT = ("blob-size-policy.yml", "openai/fence@")

ON_KEY_RE = re.compile(r'^(?:"on"|\'on\'|on):(.*)$')


def load_yamlmin(root: Path):
    spec = importlib.util.spec_from_file_location("_yamlmin", root / "fork" / "_yamlmin.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def strip_comment(line: str) -> str:
    # Good enough for the trigger line itself; full lines go to _yamlmin.
    out, quote = [], None
    for ch in line:
        if quote:
            out.append(ch)
            if ch == quote:
                quote = None
        elif ch in "'\"":
            quote = ch
            out.append(ch)
        elif ch == "#":
            break
        else:
            out.append(ch)
    return "".join(out).rstrip()


def extract_on(text: str, yamlmin) -> dict | None:
    """Narrow to the top-level `on:` block, then parse just that block.

    Real workflow YAML exceeds fork/_yamlmin's subset (anchors, deep nesting in
    jobs:), but the trigger block itself stays inside it.
    """
    lines = text.splitlines()
    for i, raw in enumerate(lines):
        m = ON_KEY_RE.match(raw)
        if not m:
            continue
        inline = strip_comment(m.group(1)).strip()
        if inline:
            value = _trigger_block(yamlmin.safe_load(f"on: {inline}"))
        else:
            block = ["on:"]
            for follow in lines[i + 1:]:
                if follow.strip() == "" or follow.lstrip().startswith("#") or follow[:1] in (" ", "\t"):
                    block.append(follow)
                    continue
                break
            value = _trigger_block(yamlmin.safe_load("\n".join(block) + "\n"))
        if isinstance(value, str):
            return {value: None}
        if isinstance(value, list):
            return {str(k): None for k in value}
        if isinstance(value, dict):
            return value
        return {}
    return None


def glob_can_match_upstream_tag(glob: str) -> bool:
    g = glob.strip("\"'")
    if g.startswith(("*", "?", "[")):
        # A leading wildcard matches upstream-shaped tags too.
        return True
    return any(g.startswith(p) for p in UPSTREAM_TAG_PREFIXES)


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


def read_upstream_meta(root: Path) -> dict:
    try:
        with open(root / "fork" / "UPSTREAM", "rb") as fh:
            return tomllib.load(fh)
    except (OSError, tomllib.TOMLDecodeError):
        return {}


def _trigger_block(doc: dict):
    """Return a workflow's `on:` mapping regardless of which YAML the loader used.

    `on` is a YAML 1.1 boolean, so PyYAML parses the key as True while the
    fork's dependency-free loader keeps it as the string "on". The fork's Macs
    have no PyYAML and ubuntu-24.04's runners do, so this diverged exactly
    between a green laptop and a red CI job.
    """
    for key in ("on", True):
        if key in doc:
            return doc[key]
    raise KeyError("on")


def upstream_workflow_set(root: Path, commit: str, rep: Report) -> set[str] | None:
    try:
        out = subprocess.run(
            ["git", "-C", str(root), "ls-tree", "-r", "--name-only", commit, "--",
             ".github/workflows", ".github/dependabot.yaml", ".github/dependabot.yml"],
            capture_output=True, text=True, check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as err:
        rep.notes.append(f"git ls-tree {commit} failed ({err}); base-tag workflow set unavailable")
        return None
    # dependabot.yaml is relocated alongside the workflows even though it lives one
    # directory up: leaving it in place keeps Dependabot opening version-update PRs
    # against upstream's dependency policy on a tree the fork controls.
    return {Path(p).name for p in out.splitlines() if p.endswith((".yml", ".yaml"))}


def check_api(root: Path, allowed: set[str], rep: Report) -> None:
    url = ""
    try:
        url = subprocess.run(["git", "-C", str(root), "remote", "get-url", "origin"],
                             capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        pass
    m = re.search(r"github\.com[:/]([^/]+/[^/.]+)", url)
    slug = m.group(1) if m else "ore-cli/ore"
    try:
        out = subprocess.run(
            ["gh", "api", f"/repos/{slug}/actions/workflows", "--paginate"],
            capture_output=True, text=True, check=True,
        ).stdout
    except FileNotFoundError:
        rep.notes.append("api check skipped: gh CLI not installed")
        return
    except subprocess.CalledProcessError as err:
        rep.notes.append(
            f"api check skipped: gh api /repos/{slug}/actions/workflows failed "
            f"(unauthenticated, or the repo is not reachable): {err.stderr.strip().splitlines()[:1]}"
        )
        return
    workflows = []
    # --paginate concatenates JSON objects; parse them all.
    dec = json.JSONDecoder()
    idx, out_stripped = 0, out.strip()
    while idx < len(out_stripped):
        obj, end = dec.raw_decode(out_stripped, idx)
        workflows.extend(obj.get("workflows", []))
        idx = end
        while idx < len(out_stripped) and out_stripped[idx] in " \n\r\t":
            idx += 1
    bad = []
    for wf in workflows:
        path = wf.get("path", "")
        # GitHub synthesises `dynamic/dependabot/*` entries on every repository.
        # They are not files in the tree, the disable endpoint rejects them with
        # 422, and they are not upstream workflows -- so counting them here would
        # make the invariant permanently unsatisfiable. What actually governs
        # Dependabot is whether .github/dependabot.yaml exists, and assemble
        # relocates it out of the tree; check (e) already asserts that.
        if path.startswith("dynamic/"):
            continue
        base = Path(path).name
        if base in allowed or base.startswith("ore-"):
            continue
        if wf.get("state") != "disabled_manually":
            bad.append(f"{base} (state={wf.get('state')})")
    if bad:
        rep.fails.append(
            "api: non-allowlisted workflows still enabled on the repo (run `gh workflow disable`): "
            + ", ".join(sorted(bad))
        )
    else:
        rep.oks.append(f"api: every non-allowlisted workflow on {slug} is disabled_manually")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", help="repo root (default: two levels above this script)")
    ap.add_argument("--mode", choices=["auto", "delta", "main"], default="auto",
                    help="auto reads fork/UPSTREAM assembled_at")
    ap.add_argument("--api", action="store_true", help="also assert repo-level workflow disablement via gh")
    args = ap.parse_args(argv)

    root = Path(args.root).resolve() if args.root else Path(__file__).resolve().parents[2]
    rep = Report()
    yamlmin = load_yamlmin(root)
    meta = read_upstream_meta(root)

    assembled = bool(meta.get("assembled_at")) if args.mode == "auto" else (args.mode == "main")
    rep.notes.append(f"tree mode: {'assembled (main)' if assembled else 'pre-assembly (delta)'}")

    allow_file = root / "fork" / "workflows.allow"
    if allow_file.is_file():
        allowed = {ln.strip() for ln in allow_file.read_text(encoding="utf-8").splitlines()
                   if ln.strip() and not ln.strip().startswith("#")}
        allow_is_fallback = False
    else:
        allowed = set(DEFAULT_ALLOW)
        allow_is_fallback = True
        rep.notes.append(
            "fork/workflows.allow not present yet (authored by the workflow-neutralisation commit); "
            f"using the built-in default: {', '.join(DEFAULT_ALLOW)} + ore-*.yml"
        )

    def is_allowed(base: str) -> bool:
        if base in allowed:
            return True
        # Until the allow file exists, fork-owned files pass by prefix; once it
        # exists it is the single authority and must list them too.
        return allow_is_fallback and base.startswith("ore-")

    wf_dir = root / ".github" / "workflows"
    live = sorted(p for p in wf_dir.iterdir() if p.suffix in (".yml", ".yaml")) if wf_dir.is_dir() else []

    upstream_set = None
    if meta.get("commit"):
        upstream_set = upstream_workflow_set(root, meta["commit"], rep)

    # (a) deny-by-default membership
    surviving: list[Path] = []
    awaiting = []
    for p in live:
        base = p.name
        if is_allowed(base):
            surviving.append(p)
        elif not assembled and upstream_set is not None and base in upstream_set:
            awaiting.append(base)
        else:
            rep.fails.append(
                f"workflow {base} is not in fork/workflows.allow"
                + ("" if assembled else " and is not part of the base tag's workflow set")
                + " — deny-by-default: add it to the allowlist deliberately or relocate it"
            )
    if awaiting:
        rep.pendings.append(
            f"{len(awaiting)} upstream workflows await relocation at assemble "
            f"(live on delta by design; repo-level disablement is the guard): "
            + ", ".join(awaiting)
        )

    # (b) allowed upstream files must not self-fire
    for p in surviving:
        if p.name.startswith("ore-"):
            continue
        on = extract_on(p.read_text(encoding="utf-8"), yamlmin)
        if on is None:
            rep.fails.append(f"{p.name}: no top-level on: block found (cannot prove it never self-fires)")
            continue
        extra = set(on) - SAFE_ON_KEYS
        if extra:
            rep.fails.append(
                f"{p.name}: allowlisted upstream workflow has self-firing triggers {sorted(extra)} "
                f"(only workflow_call/workflow_dispatch are safe to keep live)"
            )

    # (c) upstream tag-push globs; (d) upstream actions/secrets; (f) secrets: inherit
    for p in surviving:
        text = p.read_text(encoding="utf-8")
        on = extract_on(text, yamlmin) or {}
        push = on.get("push")
        if isinstance(push, dict):
            for glob in (push.get("tags") or []):
                if glob_can_match_upstream_tag(str(glob)):
                    rep.fails.append(
                        f"{p.name}: on.push.tags glob {glob!r} can match an upstream-named tag "
                        f"({'/'.join(UPSTREAM_TAG_PREFIXES)}) — pushing one runs upstream CI from the tag tree"
                    )
        for lit in FORBIDDEN_LITERALS:
            if lit in text:
                rep.fails.append(f"{p.name}: references upstream-only credential/action {lit!r}")
        for pref in FORBIDDEN_PREFIXES:
            if pref in text:
                rep.fails.append(f"{p.name}: references upstream-only secret family {pref}*")
        for m in re.finditer(r"uses:\s*(openai/\S+)", text):
            if p.name == FENCE_CARVEOUT[0] and m.group(1).startswith(FENCE_CARVEOUT[1]):
                continue  # openai/fence is a public, pinned lint action
            rep.fails.append(f"{p.name}: uses upstream-org action {m.group(1)}")
        if p.name.startswith("ore-") and re.search(r"secrets:\s*inherit\b", text):
            rep.fails.append(
                f"{p.name}: fork-owned workflow uses `secrets: inherit` — release secrets must be "
                f"passed explicitly so no reusable upstream job can ever see them"
            )

    # (e) fork/upstream-workflows/ == (base tag set) − allowlist
    reloc_dir = root / "fork" / "upstream-workflows"
    if upstream_set is None:
        rep.notes.append("relocation set-equality skipped: base tag workflow set unavailable")
    else:
        expected = {b for b in upstream_set if not is_allowed(b)}
        if reloc_dir.is_dir():
            have = {p.name for p in reloc_dir.iterdir() if p.suffix in (".yml", ".yaml")}
            missing = sorted(expected - have)
            stale = sorted(have - expected)
            if missing:
                rep.fails.append(f"fork/upstream-workflows/ missing relocated files: {', '.join(missing)}")
            if stale:
                rep.fails.append(f"fork/upstream-workflows/ has stale copies not in the base tag set: {', '.join(stale)}")
            if not missing and not stale:
                rep.oks.append(f"fork/upstream-workflows/ equals base-tag set minus allowlist ({len(expected)} files)")
        elif assembled:
            rep.fails.append(
                f"fork/upstream-workflows/ does not exist on an assembled tree "
                f"({len(expected)} files should have been relocated)"
            )
        else:
            rep.pendings.append(
                f"fork/upstream-workflows/ not created yet (assemble relocates {len(expected)} files)"
            )

    if args.api:
        check_api(root, allowed, rep)

    code = rep.finish()
    if code == 0:
        print("ok: workflow neutralisation checks clean")
    return code


if __name__ == "__main__":
    sys.exit(main())
