#!/usr/bin/env python3
"""Per-sync semantic review: what changed upstream that the fork must care about.

    semantic-review.py --base rust-vA.B.C --tag rust-vX.Y.Z [--out FILE] [--repo DIR]

Reads both tags' trees (never the working tree), classifies the changes against
the fork's data files (fork/egress.yaml, fork/seams.yaml), and writes a
markdown report with an OK / WARN verdict per section so the sync PR body is
skimmable. Deterministic, stdlib-only; an LLM summary may be appended by the
sync workflow but is never the record.

Line-number heuristics are deliberately absent: seam proximity is
anchor-regex-in-changed-hunk, because line ranges drift with every release
while the identifiers a patch anchors on do not.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

FORK_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(FORK_DIR))
import _yamlmin  # noqa: E402

WARN = "⚠️"
OK = "OK"

# The auth fence: byte-identical-to-upstream paths (fork policy). Any upstream
# movement here needs the fence's literal allowlist re-checked before merge.
AUTH_FENCE = [
    "codex-rs/login",
    "codex-rs/protocol/src/auth.rs",
    "codex-rs/model-provider/src/auth.rs",
    "codex-rs/model-provider/src/bearer_auth_provider.rs",
    "codex-rs/model-provider-info/src/lib.rs",
    "codex-rs/agent-identity",
    "codex-rs/workload-identity",
    "codex-rs/http-client/src/chatgpt_hosts.rs",
    "codex-rs/http-client/src/chatgpt_cloudflare_cookies.rs",
    "codex-rs/app-server-protocol",
]

PACKAGING_PATHS = [
    "scripts/install",
    "scripts/codex_package",
    "scripts/stage_npm_packages.py",
    "codex-cli",
    ".github/dotslash-config.json",
    "sdk/typescript/src/exec.ts",
]

# Kept POSIX-ERE-safe (git grep -E shares it): no backslashes or brackets
# inside the class — a `]` there terminates the class early and the pattern
# silently matches nothing.
URL_RE = re.compile(r"https?://[A-Za-z0-9._~:/%#@!$&'()*+,;=?{}-]+")

# RFC 2606/6761 reserved names and loopback hosts can never egress; URLs built
# with format! placeholders are classified by their path fragments (the
# substring lists), not by host.
_FIXTURE_TLDS = {"example", "invalid", "test", "localhost"}
_FIXTURE_HOSTS = {"localhost", "127.0.0.1", "0.0.0.0", "::1", "example.com", "example.org", "example.net"}


def run_git(repo: Path, *args: str) -> str:
    res = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, check=False
    )
    if res.returncode != 0:
        raise SystemExit(f"git {' '.join(args[:3])}... failed: {res.stderr.strip()}")
    return res.stdout


def try_git(repo: Path, *args: str) -> str | None:
    res = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, check=False
    )
    return res.stdout if res.returncode == 0 else None


def resolve_tag(repo: Path, tag: str) -> str:
    """Peeled commit sha; prefers the non-tag fetch namespace (tag objects are
    annotated, so ^{commit} is mandatory everywhere)."""
    for ref in (f"refs/upstream/tags/{tag}", f"refs/tags/{tag}"):
        out = try_git(repo, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}")
        if out and out.strip():
            return out.strip()
    raise SystemExit(f"tag '{tag}' not found (refs/upstream/tags/ or refs/tags/)")


def show(repo: Path, commit: str, path: str) -> str | None:
    return try_git(repo, "show", f"{commit}:{path}")


class Report:
    def __init__(self) -> None:
        self.lines: list[str] = []
        self.verdicts: list[tuple[str, str]] = []

    def section(self, title: str, verdict: str) -> None:
        self.verdicts.append((title, verdict))
        self.lines.append(f"\n## {verdict} {title}\n")

    def add(self, text: str = "") -> None:
        self.lines.append(text)

    def code(self, text: str) -> None:
        if text.strip():
            self.lines.append("```")
            self.lines.append(text.rstrip("\n"))
            self.lines.append("```")


# ---------------------------------------------------------------- sections


def headline(rep: Report, repo: Path, base_c: str, tag_c: str) -> None:
    commits = run_git(repo, "rev-list", "--count", f"{base_c}..{tag_c}").strip()
    authors = run_git(repo, "shortlog", "-sn", f"{base_c}..{tag_c}").splitlines()
    dirstat = run_git(repo, "diff", "--dirstat=lines,3", base_c, tag_c).splitlines()
    files = run_git(repo, "diff", "--name-only", base_c, tag_c).splitlines()
    rep.section("headline", OK)
    rep.add(f"- {commits} commits, {len(files)} files changed, {len(authors)} authors")
    rep.add(f"- top authors: {', '.join(a.strip().split(chr(9))[-1] for a in authors[:8])}")
    rep.add("- dirstat (lines, >=3%):")
    rep.code("\n".join(dirstat[:20]))


def workspace_members(rep: Report, repo: Path, base_c: str, tag_c: str) -> None:
    def members(commit: str) -> set[str]:
        text = show(repo, commit, "codex-rs/Cargo.toml") or ""
        return set(tomllib.loads(text)["workspace"]["members"])

    old, new = members(base_c), members(tag_c)
    added, removed = sorted(new - old), sorted(old - new)
    verdict = WARN if added or removed else OK
    rep.section("workspace members", verdict)
    if not added and not removed:
        rep.add(f"- unchanged ({len(new)} members)")
        return
    for m in added:
        rep.add(f"- ADDED `{m}` — new crate: check for default-on egress and telemetry deps")
    for m in removed:
        rep.add(f"- REMOVED `{m}` — if a series commit patches it, that commit now applies to nothing")


def lock_packages(rep: Report, repo: Path, base_c: str, tag_c: str) -> None:
    def packages(commit: str) -> dict[tuple[str, str], str]:
        text = show(repo, commit, "codex-rs/Cargo.lock") or ""
        out: dict[tuple[str, str], str] = {}
        for pkg in tomllib.loads(text).get("package", []):
            out[(pkg["name"], pkg.get("version", "?"))] = pkg.get("source", "path")
        return out

    old, new = packages(base_c), packages(tag_c)
    # A lock can hold several versions of one crate — compare version SETS per
    # name or every multi-version crate reports a phantom cross-product bump.
    old_v: dict[str, set[str]] = {}
    new_v: dict[str, set[str]] = {}
    for (n, v) in old:
        old_v.setdefault(n, set()).add(v)
    for (n, v) in new:
        new_v.setdefault(n, set()).add(v)
    added = sorted(set(new_v) - set(old_v))
    removed = sorted(set(old_v) - set(new_v))
    bumped = [
        (n, old_v[n], new_v[n])
        for n in sorted(set(old_v) & set(new_v))
        if old_v[n] != new_v[n]
    ]
    verdict = WARN if added else OK
    rep.section("Cargo.lock package set (upstream's own dep changes)", verdict)
    rep.add(f"- {len(old)} -> {len(new)} `[[package]]` entries; {len(bumped)} crates changed version")
    for n in added:
        srcs = {new[(nn, v)] for (nn, v) in new if nn == n}
        git_flag = " **git source**" if any("git+" in s for s in srcs) else ""
        rep.add(f"- NEW crate `{n}` ({', '.join(sorted(srcs))}){git_flag}")
    for n in removed:
        rep.add(f"- removed crate `{n}`")
    if bumped:
        def fmt(n: str, ov: set[str], nv: set[str]) -> str:
            # Only the version-set delta: a crate locked at several versions
            # would otherwise re-list its unchanged ones.
            gone, came = sorted(ov - nv), sorted(nv - ov)
            if gone and came:
                return f"{n} {'/'.join(gone)}->{'/'.join(came)}"
            return f"{n} {'+' if came else '-'}{'/'.join(came or gone)}"

        shown = ", ".join(fmt(n, ov, nv) for n, ov, nv in bumped[:25])
        more = f" (+{len(bumped) - 25} more)" if len(bumped) > 25 else ""
        rep.add(f"- bumps: {shown}{more}")


def load_egress(path: Path) -> dict[str, list[str]]:
    data = _yamlmin.safe_load(path.read_text(encoding="utf-8"))
    return {k: [str(v).lower() for v in data.get(k, [])] for k in ("kill", "decide", "keep")}


def classify(url: str, egress: dict[str, list[str]]) -> str:
    bare = re.sub(r"^https?://", "", url).lower()
    # Longest matching pattern wins across all buckets, ties broken
    # kill > decide > keep — so a broad keep never swallows a specific kill and
    # a broad decide never flags an already-kept specific path.
    best: tuple[int, int, str] | None = None
    for prio, bucket in enumerate(("kill", "decide", "keep")):
        for pat in egress[bucket]:
            if pat in bare and (best is None or (len(pat), -prio) > (best[0], -best[1])):
                best = (len(pat), prio, bucket)
    if best:
        return best[2]
    host = bare.split("/", 1)[0].rsplit("@", 1)[-1].split(":", 1)[0]
    if not host or "{" in host:
        return "fixture"  # format! placeholder — path fragments classify these
    if host in _FIXTURE_HOSTS or host.rsplit(".", 1)[-1] in _FIXTURE_TLDS:
        return "fixture"
    return "unclassified"


def rs_urls(repo: Path, commit: str) -> set[str]:
    # `*.rs` as a git pathspec matches at any depth; -h drops filenames so the
    # output is the raw literal set. git grep exits 1 on zero matches — treat a
    # completely empty result on a tree this size as a broken pattern, not "no
    # URLs" (regression guard for the report's most safety-relevant section).
    out = try_git(repo, "grep", "-h", "-o", "-E", URL_RE.pattern, commit, "--", "*.rs")
    urls: set[str] = set()
    for line in (out or "").splitlines():
        urls.add(line.strip().rstrip("\"'`.,);@{"))
    if not urls:
        raise SystemExit(f"egress scan found ZERO url literals at {commit[:10]} — url regex is broken")
    return urls


def egress_literals(rep: Report, repo: Path, base_c: str, tag_c: str) -> None:
    egress = load_egress(FORK_DIR / "egress.yaml")
    added = sorted(rs_urls(repo, tag_c) - rs_urls(repo, base_c))
    buckets: dict[str, list[str]] = {"kill": [], "decide": [], "keep": [], "fixture": [], "unclassified": []}
    for url in added:
        buckets[classify(url, egress)].append(url)
    verdict = WARN if buckets["kill"] or buckets["decide"] or buckets["unclassified"] else OK
    rep.section("new egress literals in .rs (vs fork/egress.yaml)", verdict)
    rep.add(f"- {len(added)} URL literal(s) are new at {tag_c[:10]}")
    for url in buckets["unclassified"]:
        rep.add(f"- {WARN} UNCLASSIFIED `{url}` — needs-decision: add to fork/egress.yaml in this sync PR")
    for url in buckets["kill"]:
        rep.add(f"- {WARN} KILL-class `{url}` — upstream grew a phone-home; confirm the owning series neutralizes it")
    for url in buckets["decide"]:
        rep.add(f"- {WARN} DECIDE-class `{url}` — standing open decision, re-triage")
    if buckets["keep"]:
        rep.add(f"- keep-class ({len(buckets['keep'])}): " + ", ".join(f"`{u}`" for u in buckets["keep"][:15]))
    if buckets["fixture"]:
        rep.add(f"- test fixtures (reserved hosts / placeholders, {len(buckets['fixture'])}): "
                + ", ".join(f"`{u}`" for u in buckets["fixture"][:15])
                + (" ..." if len(buckets["fixture"]) > 15 else ""))


def auth_fence(rep: Report, repo: Path, base_c: str, tag_c: str) -> None:
    stat = run_git(repo, "diff", "--stat", base_c, tag_c, "--", *AUTH_FENCE)
    verdict = WARN if stat.strip() else OK
    rep.section("auth-fence paths", verdict)
    if not stat.strip():
        rep.add("- fence untouched upstream")
        return
    rep.add(
        "- the fence moved upstream: it stays byte-identical to the NEW tag by construction, "
        "but re-check the literal-level allowlist (model-provider-info is edited by the wire-api "
        "series — its fence is literal-level, not file-level)"
    )
    lines = stat.rstrip("\n").splitlines()
    if len(lines) > 26:
        # The last line is git's summary; keep it visible under the truncation.
        lines = lines[:25] + [f"    ... ({len(lines) - 26} more files)", lines[-1]]
    rep.code("\n".join(lines))


def hunks_of(diff_text: str) -> list[str]:
    """Split one file's unified diff into hunks (header + body)."""
    hunks: list[str] = []
    current: list[str] = []
    for line in diff_text.splitlines():
        if line.startswith("@@"):
            if current:
                hunks.append("\n".join(current))
            current = [line]
        elif current:
            current.append(line)
    if current:
        hunks.append("\n".join(current))
    return hunks


def seam_proximity(rep: Report, repo: Path, base_c: str, tag_c: str) -> None:
    seams = _yamlmin.safe_load((FORK_DIR / "seams.yaml").read_text(encoding="utf-8"))
    findings: list[str] = []
    quiet: list[str] = []
    for seam in seams:
        path, anchors, series = seam["path"], seam["anchors"], seam["series"]
        diff = run_git(repo, "diff", base_c, tag_c, "--", path)
        if not diff.strip():
            continue
        hit_lines: list[str] = []
        for hunk in hunks_of(diff):
            header = hunk.splitlines()[0]
            matched = [a for a in anchors if re.search(a, hunk)]
            if matched:
                hit_lines.append(f"    - `{header[:80]}` matched {', '.join(f'`{m}`' for m in matched)}")
        if hit_lines:
            findings.append(f"- {WARN} `{path}` (series **{series}**): changed hunks touch its anchors")
            findings.extend(hit_lines)
        else:
            quiet.append(f"- `{path}` changed, but no anchor sits in a changed hunk ({series})")
    verdict = WARN if findings else OK
    rep.section("seam proximity (fork/seams.yaml)", verdict)
    if not findings and not quiet:
        rep.add("- no seam file changed between the tags")
    rep.lines.extend(findings)
    rep.lines.extend(quiet)


def ci_surface(rep: Report, repo: Path, base_c: str, tag_c: str) -> None:
    paths = [".github/workflows", ".github/actions", ".github/scripts", ".github/dependabot.yaml"]
    ns = run_git(repo, "diff", "--name-status", base_c, tag_c, "--", *paths).splitlines()
    new_workflows = [
        line.split("\t")[1]
        for line in ns
        if line.startswith("A") and re.match(r"A\t\.github/workflows/.*\.ya?ml$", line)
    ]
    release_touched = [
        line
        for line in ns
        if re.search(r"\.github/workflows/rust-release(-windows|-zsh)?\.yml$", line)
    ]
    verdict = WARN if new_workflows or release_touched else OK
    rep.section("workflow / actions / scripts surface", verdict)
    if not ns:
        rep.add("- no .github changes")
        return
    for wf in new_workflows:
        rep.add(
            f"- {WARN} NEW workflow `{wf}` — relocation neutralizes it on the generated main, but a "
            "new workflow file arrives ENABLED at the repository level: disable it via the Actions "
            "API before merge (the sync workflow's enabled-workflows == allowlist check enforces this)"
        )
    for line in release_touched:
        rep.add(f"- {WARN} `{line.split(chr(9))[-1]}` changed — read the diff: port to ore-release*?")
    rep.add(f"- full change list ({len(ns)} entries):")
    rep.code("\n".join(ns[:60]) + ("\n..." if len(ns) > 60 else ""))


def config_schema(rep: Report, repo: Path, base_c: str, tag_c: str) -> None:
    def props(commit: str) -> tuple[set[str], set[str]]:
        text = show(repo, commit, "codex-rs/core/config.schema.json")
        if not text:
            return set(), set()
        data = json.loads(text)
        return set(data.get("properties", {})), set(data.get("definitions", {}))

    old_p, old_d = props(base_c)
    new_p, new_d = props(tag_c)
    added_p, removed_p = sorted(new_p - old_p), sorted(old_p - new_p)
    added_d, removed_d = sorted(new_d - old_d), sorted(old_d - new_d)
    verdict = WARN if added_p or removed_p else OK
    rep.section("config.toml schema surface", verdict)
    for k in added_p:
        rep.add(f"- {WARN} NEW top-level config key `{k}` — new behavior to triage (default-on egress?)")
    for k in removed_p:
        rep.add(f"- removed top-level config key `{k}`")
    if added_d or removed_d:
        rep.add(f"- definitions: +{len(added_d)} ({', '.join(added_d[:10])}) -{len(removed_d)}")
    if not (added_p or removed_p or added_d or removed_d):
        rep.add("- schema property set unchanged")


def packaging(rep: Report, repo: Path, base_c: str, tag_c: str) -> None:
    stat = run_git(repo, "diff", "--stat", base_c, tag_c, "--", *PACKAGING_PATHS)
    verdict = WARN if stat.strip() else OK
    rep.section("packaging / installers", verdict)
    if not stat.strip():
        rep.add("- untouched")
        return
    rep.add("- changes here usually need a mirrored change in ore's release workflow or install docs:")
    rep.code(stat)


def toolchain(rep: Report, repo: Path, base_c: str, tag_c: str) -> None:
    findings: list[str] = []
    for path in ("codex-rs/rust-toolchain.toml", "codex-rs/.config/nextest.toml"):
        d = run_git(repo, "diff", base_c, tag_c, "--", path)
        if d.strip():
            findings.append(f"- {WARN} `{path}` changed:")
            findings.append("```")
            findings.append("\n".join(d.splitlines()[4:20]))
            findings.append("```")

    def gates(commit: str) -> set[str]:
        out = try_git(repo, "grep", "-h", "-o", r"\"minimal_client_version\": \"[0-9.]*\"", commit) or ""
        return set(out.splitlines())

    old_g, new_g = gates(base_c), gates(tag_c)
    if old_g != new_g:
        findings.append(
            f"- {WARN} `minimal_client_version` gates changed: {sorted(old_g)} -> {sorted(new_g)} "
            "(wire-visible: ore's scheme-C version must clear every published gate)"
        )
    rep.section("toolchain / version prerequisites", WARN if findings else OK)
    rep.lines.extend(findings or ["- toolchain, nextest config and client-version gates unchanged"])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[1])
    parser.add_argument("--base", required=True, help="previous upstream tag (rust-vA.B.C)")
    parser.add_argument("--tag", required=True, help="target upstream tag (rust-vX.Y.Z)")
    parser.add_argument("--out", help="write markdown here (default: stdout)")
    parser.add_argument("--repo", default=str(FORK_DIR.parent), help="repository root")
    args = parser.parse_args()

    repo = Path(args.repo).resolve()
    base_c = resolve_tag(repo, args.base)
    tag_c = resolve_tag(repo, args.tag)

    rep = Report()
    headline(rep, repo, base_c, tag_c)
    workspace_members(rep, repo, base_c, tag_c)
    lock_packages(rep, repo, base_c, tag_c)
    egress_literals(rep, repo, base_c, tag_c)
    auth_fence(rep, repo, base_c, tag_c)
    seam_proximity(rep, repo, base_c, tag_c)
    ci_surface(rep, repo, base_c, tag_c)
    config_schema(rep, repo, base_c, tag_c)
    packaging(rep, repo, base_c, tag_c)
    toolchain(rep, repo, base_c, tag_c)

    warn_count = sum(1 for _, v in rep.verdicts if v == WARN)
    header = [
        f"# semantic review: {args.base} -> {args.tag}",
        "",
        f"base `{base_c}` -> tag `{tag_c}`; {warn_count} of {len(rep.verdicts)} sections flagged.",
        "",
        "| section | verdict |",
        "|---|---|",
    ]
    header += [f"| {title} | {verdict} |" for title, verdict in rep.verdicts]
    text = "\n".join(header + rep.lines) + "\n"

    if args.out:
        Path(args.out).write_text(text, encoding="utf-8")
        print(f"semantic-review: wrote {args.out} ({warn_count} flagged section(s))")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
