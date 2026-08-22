#!/usr/bin/env python3
"""Kill/keep/forbid string scanner for the ore fork (data: strings.toml).

Scans raw bytes with compiled regexes rather than shelling out to strings(1),
so ELF, Mach-O and PE artifacts are all handled by the same code path.

Modes
  --tree             source tripwire: kill patterns must not appear outside the
                     source_allow set, forbid patterns must not appear at all,
                     keep literals must still exist under codex-rs/ (a missing
                     keep literal means a substitution rewrote a wire id).
  --binary PATH      one shipped binary; treated as the entrypoint (keep set
                     required) unless --not-entrypoint is passed.
  --artifacts DIR    a codex-package dir or a directory of binaries; kill and
                     forbid run on every binary, keep only on the entrypoint.

A kill entry's `removed_by` decides how a hit is judged:
  substitution   removed by the assemble-time substitution pass, so it is
                 EXPECTED in a pre-assembly (delta) tree; enforced on an
                 assembled tree and in binaries.
  series:<slug>  removed by a delta series commit; `pending = true` demotes
                 hits to warnings until that commit lands and drops the flag.

Whether the tree is assembled is read from fork/UPSTREAM's `assembled_at`
(written by fork/assemble.sh; empty on delta).

Exit codes: 0 ok, 1 fail, 2 could-not-run, 3 only expected-pending findings.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import tomllib
from pathlib import Path

# Directories that hold build output, vendored code or scm state — never ours
# to police, and target/ alone is tens of GB.
SKIP_DIRS = {
    ".git", "target", "node_modules", "__pycache__", ".venv", "venv",
    "bazel-out", "bazel-bin", "bazel-testlogs", "dist", "build",
    "vendor", "third_party",
}
# Lockfiles enumerate upstream package names by design.
SKIP_FILES = {
    "Cargo.lock", "MODULE.bazel.lock", "pnpm-lock.yaml", "uv.lock",
    "package-lock.json", "flake.lock",
}
SKIP_SUFFIXES = {
    ".png", ".jpg", ".jpeg", ".gif", ".ico", ".woff", ".woff2",
    ".zip", ".gz", ".zst", ".tar", ".pdf",
}

# Shipped entrypoint names, pre- and post- packaging-time rename.
ENTRYPOINT_NAMES = {"ore", "ore.exe", "codex", "codex.exe"}

# Bytes considered part of one "token" when deciding whether a kill match is
# really an allowlisted identifier family (see source_allow in strings.toml).
TOKEN_BYTES = re.compile(rb"[A-Za-z0-9_@%/.:-]")


def repo_root(cli_root: str | None) -> Path:
    if cli_root:
        return Path(cli_root).resolve()
    return Path(__file__).resolve().parents[2]


def tree_is_assembled(root: Path) -> bool | None:
    """None = fork/UPSTREAM unreadable (treat as pre-assembly, but say so)."""
    upstream = root / "fork" / "UPSTREAM"
    try:
        with open(upstream, "rb") as fh:
            meta = tomllib.load(fh)
    except (OSError, tomllib.TOMLDecodeError):
        return None
    return bool(meta.get("assembled_at"))


def glob_to_re(pattern: str) -> re.Pattern[str]:
    # ** crosses directory separators, * does not — same dialect as the
    # substitution manifest so the two allowlists read identically.
    out, i = [], 0
    while i < len(pattern):
        ch = pattern[i]
        if ch == "*":
            if pattern[i : i + 2] == "**":
                out.append(".*")
                i += 2
                if i < len(pattern) and pattern[i] == "/":
                    i += 1
                continue
            out.append("[^/]*")
        elif ch == "?":
            out.append("[^/]")
        else:
            out.append(re.escape(ch))
        i += 1
    return re.compile("^" + "".join(out) + "$")


class Lists:
    def __init__(self, data: dict):
        self.kill = data.get("kill", [])
        self.keep = data.get("keep", [])
        self.forbid = data.get("forbid", [])
        allow = data.get("source_allow", {})
        self.allow_res = [glob_to_re(p) for p in allow.get("paths", [])]
        self.forbid_allow_res = [
            [glob_to_re(p) for p in e.get("source_allow_paths", [])] for e in self.forbid
        ]
        self.allow_prefixes = [p.encode() for p in allow.get("literal_prefixes", [])]
        self.allow_literals = [l.encode() for l in allow.get("literals", [])]
        for section, entries in (("kill", self.kill), ("keep", self.keep), ("forbid", self.forbid)):
            for e in entries:
                if not e.get("reason"):
                    raise SystemExit(f"strings.toml: {section} entry {e!r} has no reason (mandatory)")
        self.kill_res = [re.compile(e["pattern"].encode()) for e in self.kill]
        self.forbid_res = [re.compile(e["pattern"].encode()) for e in self.forbid]

    def path_allowed(self, rel: str) -> bool:
        return any(r.match(rel) for r in self.allow_res)

    def match_is_allowed_token(self, data: bytes, start: int, end: int) -> bool:
        # Widen the match to the surrounding token; an allowlisted identifier
        # family (CODEX_*, codex-*, …) that happens to contain a kill pattern
        # is not a finding.
        a, b = start, end
        while a > 0 and TOKEN_BYTES.match(data[a - 1 : a]):
            a -= 1
        while b < len(data) and TOKEN_BYTES.match(data[b : b + 1]):
            b += 1
        token = data[a:b]
        if token in self.allow_literals:
            return True
        return any(token.startswith(p) for p in self.allow_prefixes)


def load_lists(root: Path) -> Lists:
    with open(Path(__file__).resolve().parent / "strings.toml", "rb") as fh:
        return Lists(tomllib.load(fh))


def iter_tree(root: Path):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for name in sorted(filenames):
            if name in SKIP_FILES or Path(name).suffix in SKIP_SUFFIXES:
                continue
            path = Path(dirpath) / name
            if path.is_symlink():
                continue
            yield path, path.relative_to(root).as_posix()


class Report:
    def __init__(self):
        self.fails: list[str] = []
        self.pendings: list[str] = []
        self.notes: list[str] = []

    def finish(self) -> int:
        for line in self.notes:
            print(f"note: {line}")
        for line in self.pendings:
            print(f"pending: {line}")
        for line in self.fails:
            print(f"FAIL: {line}")
        if self.fails:
            return 1
        return 3 if self.pendings else 0


def emit_kill_hits(lists: Lists, kill_hits: dict[int, list[str]],
                   assembled: bool, binary: bool, rep: Report) -> None:
    for idx, wheres in sorted(kill_hits.items()):
        entry = lists.kill[idx]
        removed_by = entry.get("removed_by", "")
        pending = bool(entry.get("pending"))
        listing = ", ".join(wheres[:8]) + (f", … +{len(wheres) - 8} more" if len(wheres) > 8 else "")
        what = f"kill '{entry['pattern']}' present in {listing} — {entry['reason']}"
        if pending:
            rep.pendings.append(f"{what} [expected until {removed_by} lands]")
        elif removed_by == "substitution" and not binary and not assembled:
            # Substitution runs at assemble time; on delta these literals are the
            # upstream text the manifest exists to rewrite.
            rep.notes.append(f"{what} [removed by the substitution pass at assemble; expected on delta]")
        else:
            rep.fails.append(what)


def scan_blob(data: bytes, where: str, lists: Lists, *, rel: str | None,
              rep: Report, kill_hits: dict[int, list[str]]) -> None:
    if rel is not None and lists.path_allowed(rel):
        return
    for idx, (entry, rx) in enumerate(zip(lists.kill, lists.kill_res)):
        m = rx.search(data)
        if not m:
            continue
        if rel is not None and lists.match_is_allowed_token(data, m.start(), m.end()):
            continue
        kill_hits.setdefault(idx, []).append(where)
    for entry, rx, allow_res in zip(lists.forbid, lists.forbid_res, lists.forbid_allow_res):
        if not rx.search(data):
            continue
        # A forbid entry may name paths where the literal is documentation rather
        # than behaviour -- a host a user visits in a browser is not a host the
        # binary contacts. Binary scans pass rel=None and so are never exempt.
        if rel is not None and any(r.match(rel) for r in allow_res):
            continue
        rep.fails.append(f"forbid '{entry['pattern']}' present in {where} ({entry['reason']})")


def run_tree(root: Path, lists: Lists, assembled: bool, rep: Report) -> None:
    keep_missing = {e["literal"].encode(): e for e in lists.keep}
    kill_hits: dict[int, list[str]] = {}
    for path, rel in iter_tree(root):
        try:
            data = path.read_bytes()
        except OSError:
            continue
        scan_blob(data, rel, lists, rel=rel, rep=rep, kill_hits=kill_hits)
        if keep_missing and rel.startswith("codex-rs/"):
            for lit in [l for l in keep_missing if l in data]:
                del keep_missing[lit]
    emit_kill_hits(lists, kill_hits, assembled, binary=False, rep=rep)
    for lit, entry in keep_missing.items():
        rep.fails.append(
            f"keep literal {lit.decode()!r} no longer exists under codex-rs/ — "
            f"a substitution or series commit rewrote a wire identifier ({entry['reason']})"
        )


def binaries_in(artifact_dir: Path) -> list[Path]:
    out = []
    for dirpath, dirnames, filenames in os.walk(artifact_dir):
        dirnames[:] = sorted(dirnames)
        for name in sorted(filenames):
            p = Path(dirpath) / name
            if p.is_symlink():
                continue
            # Shipped binaries: anything executable, plus Windows .exe which
            # carries no mode bit worth trusting after archive round-trips.
            if name.endswith(".exe") or os.access(p, os.X_OK):
                out.append(p)
    return out


def run_binary(path: Path, lists: Lists, *, entrypoint: bool, rep: Report) -> None:
    try:
        data = path.read_bytes()
    except OSError as err:
        rep.fails.append(f"cannot read binary {path}: {err}")
        return
    kill_hits: dict[int, list[str]] = {}
    scan_blob(data, str(path), lists, rel=None, rep=rep, kill_hits=kill_hits)
    emit_kill_hits(lists, kill_hits, assembled=True, binary=True, rep=rep)
    if entrypoint:
        for e in lists.keep:
            if e["literal"].encode() not in data:
                rep.fails.append(
                    f"keep literal {e['literal']!r} missing from entrypoint binary {path} ({e['reason']})"
                )


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--tree", action="store_true", help="scan the source tree")
    mode.add_argument("--binary", metavar="PATH", help="scan one shipped binary")
    mode.add_argument("--artifacts", metavar="DIR", help="scan every binary in a package dir")
    ap.add_argument("--root", help="repo root (default: two levels above this script)")
    ap.add_argument("--not-entrypoint", action="store_true",
                    help="with --binary: skip the keep set (binary is not the entrypoint)")
    args = ap.parse_args(argv)

    root = repo_root(args.root)
    lists = load_lists(root)
    rep = Report()

    if args.tree:
        assembled = tree_is_assembled(root)
        if assembled is None:
            rep.notes.append("fork/UPSTREAM unreadable — judging the tree as pre-assembly")
            assembled = False
        rep.notes.append(f"tree mode: {'assembled (main)' if assembled else 'pre-assembly (delta)'}")
        run_tree(root, lists, assembled, rep)
    elif args.binary:
        p = Path(args.binary)
        if not p.is_file():
            print(f"skip: binary {p} does not exist")
            return 2
        run_binary(p, lists, entrypoint=not args.not_entrypoint, rep=rep)
    else:
        d = Path(args.artifacts)
        if not d.is_dir():
            print(f"skip: artifact dir {d} does not exist")
            return 2
        bins = binaries_in(d)
        if not bins:
            print(f"skip: no binaries found under {d}")
            return 2
        entry_seen = False
        for b in bins:
            is_entry = b.name in ENTRYPOINT_NAMES
            entry_seen = entry_seen or is_entry
            run_binary(b, lists, entrypoint=is_entry, rep=rep)
        if not entry_seen:
            rep.fails.append(
                f"no entrypoint binary ({'/'.join(sorted(ENTRYPOINT_NAMES))}) found under {d}; "
                "the keep set was not checked anywhere"
            )
    code = rep.finish()
    if code == 0:
        print("ok: strings check clean")
    return code


if __name__ == "__main__":
    sys.exit(main())
