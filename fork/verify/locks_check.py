#!/usr/bin/env python3
"""Lock integrity (I-LOCK): nothing enters the fork through a lockfile.

Series commits may not touch a lockfile at all (fork/lint-series.sh) and
assemble regenerates codex-rs/Cargo.lock with `cargo update --workspace`, so
every lock difference from the base tag is either the fork's own crates or
something that arrived unreviewed.

  a. new-dependency tripwire: {[[package]] names in codex-rs/Cargo.lock} minus
     the same set at the base tag must be a subset of fork/expected-deps.txt.
     A crate with a `git+` source is called out separately: an unpinned
     dependency is a different class of risk from a registry one.
  b. the other way round — every name in fork/expected-deps.txt must actually
     be in the lock on an assembled tree.  On delta it is legitimately absent
     (the series cannot touch the lock), which reports pending, not failure.
  c. every workspace member resolves in the lock.  This is the hermetic half of
     `cargo metadata --locked`: the failure it exists to catch is a fork crate
     that never made it into the regenerated lock.  A member missing from the
     lock that is NOT one of the fork's own crates fails on either branch —
     that is a broken lock, not the delta policy.
  d. MODULE.bazel.lock and pnpm-lock.yaml are byte-equal to the base tag.
     Nothing in the pipeline regenerates them and substitute.py skips them, so
     any change is a hand edit or a bad merge.

`cargo metadata --locked` itself is the authoritative form of (c) and is run
where a toolchain is guaranteed: assemble.sh's version+lock pass, and — more
strictly — ore-ci's check-clean-worktree, which proves a full cargo build did
not need to rewrite the lock at all.  --cargo-metadata runs it here too, for
an operator holding a candidate tree; it is deliberately not wired into
manifest.toml, whose static suite must stay toolchain-free.

Exit codes: 0 ok, 1 fail, 2 could-not-run, 3 only expected-pending findings.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import tomllib
from pathlib import Path

# Byte-equal to the tag, on both branches.
FROZEN_LOCKS = ("MODULE.bazel.lock", "pnpm-lock.yaml")
CARGO_LOCK = "codex-rs/Cargo.lock"


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


def lock_packages(text: str) -> dict[str, dict]:
    """name -> the last [[package]] entry with that name (versions may repeat)."""
    data = tomllib.loads(text)
    return {p["name"]: p for p in data.get("package", [])}


def member_names(root: Path, rep: Report) -> dict[str, str]:
    """workspace member path -> [package] name, globs expanded."""
    ws_path = root / "codex-rs" / "Cargo.toml"
    try:
        with open(ws_path, "rb") as fh:
            members = tomllib.load(fh)["workspace"]["members"]
    except (OSError, KeyError, tomllib.TOMLDecodeError) as err:
        rep.notes.append(f"workspace members unreadable ({err}); member coverage unchecked")
        return {}
    out: dict[str, str] = {}
    for entry in members:
        dirs = sorted(p.parent for p in (root / "codex-rs").glob(f"{entry}/Cargo.toml")) \
            if "*" in entry else [root / "codex-rs" / entry]
        for d in dirs:
            try:
                with open(d / "Cargo.toml", "rb") as fh:
                    name = tomllib.load(fh)["package"]["name"]
            except (OSError, KeyError, tomllib.TOMLDecodeError):
                rep.fails.append(f"workspace member {entry} has no readable [package] name")
                continue
            out[str(d.relative_to(root))] = name
    return out


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", help="repo root (default: two levels above this script)")
    ap.add_argument("--mode", choices=["auto", "delta", "main"], default="auto",
                    help="auto reads fork/UPSTREAM assembled_at")
    ap.add_argument("--cargo-metadata", action="store_true",
                    help="also run `cargo metadata --locked` (needs a toolchain; assembled trees only)")
    args = ap.parse_args(argv)

    root = Path(args.root).resolve() if args.root else Path(__file__).resolve().parents[2]
    rep = Report()

    try:
        with open(root / "fork" / "UPSTREAM", "rb") as fh:
            meta = tomllib.load(fh)
    except (OSError, tomllib.TOMLDecodeError) as err:
        print(f"skip: fork/UPSTREAM unreadable: {err}")
        return 2
    base = meta.get("commit", "")
    if not base:
        print("skip: fork/UPSTREAM records no base commit")
        return 2
    if git(root, "cat-file", "-e", f"{base}^{{commit}}").returncode != 0:
        print(f"skip: base commit {base[:12]} is not in this clone (shallow checkout?)")
        return 2

    assembled = bool(meta.get("assembled_at")) if args.mode == "auto" else (args.mode == "main")
    rep.notes.append(f"tree mode: {'assembled (main)' if assembled else 'pre-assembly (delta)'}")

    expected_path = root / "fork" / "expected-deps.txt"
    if expected_path.is_file():
        expected = {ln.strip() for ln in expected_path.read_text(encoding="utf-8").splitlines()
                    if ln.strip() and not ln.strip().startswith("#")}
    else:
        expected = set()
        rep.notes.append("fork/expected-deps.txt is absent — treating the allowed-addition set as "
                         "empty (deny-by-default: any package the base lock lacks then fails)")

    # The worktree lock, not the committed blob: this is what cargo resolves
    # against, and on delta the first cargo invocation legitimately rewrites it
    # (ore-ci tolerates exactly that one dirty path there).
    lock_path = root / CARGO_LOCK
    if not lock_path.is_file():
        print(f"skip: {CARGO_LOCK} does not exist")
        return 2
    try:
        here = lock_packages(lock_path.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as err:
        print(f"FAIL: {CARGO_LOCK} does not parse as TOML: {err}")
        return 1
    show = git(root, "show", f"{base}:{CARGO_LOCK}")
    if show.returncode != 0:
        print(f"skip: {CARGO_LOCK} not readable at base {base[:12]}")
        return 2
    theirs = lock_packages(show.stdout)

    # (a) additions
    added = sorted(set(here) - set(theirs))
    unexpected = [n for n in added if n not in expected]
    for name in unexpected:
        pkg = here[name]
        source = pkg.get("source", "workspace-local path dependency")
        origin = "git dependency (unpinned upstream, not a registry release): " if source.startswith("git+") else ""
        rep.fails.append(
            f"{CARGO_LOCK} carries {name} {pkg.get('version', '?')}, which the base tag's lock does "
            f"not — {origin}{source}. Review it, then add the name to fork/expected-deps.txt"
        )
    if added and not unexpected:
        rep.oks.append(f"lock additions over the base tag are all declared: {', '.join(added)}")
    elif not added:
        rep.oks.append(f"{CARGO_LOCK} adds nothing to the base tag's {len(theirs)} package names")
    removed = sorted(set(theirs) - set(here))
    if removed:
        rep.notes.append(f"{len(removed)} package(s) in the base lock are gone from this one: "
                         + ", ".join(removed[:8]) + (" …" if len(removed) > 8 else ""))

    # (b) the fork's declared crates are actually in the lock
    missing_expected = sorted(n for n in expected if n not in here)
    if missing_expected:
        if assembled:
            rep.fails.append(
                "declared in fork/expected-deps.txt but absent from the assembled lock: "
                + ", ".join(missing_expected) + " — assemble's `cargo update --workspace` did not "
                "run, or the crate left the workspace without its line here leaving too"
            )
        else:
            rep.pendings.append(
                "the lock does not yet carry " + ", ".join(missing_expected) + " — expected on delta, "
                "where series commits may not touch Cargo.lock; assemble regenerates it"
            )
    elif expected:
        rep.oks.append(f"every declared fork dependency is in the lock ({', '.join(sorted(expected))})")

    # (c) member coverage — the hermetic half of `cargo metadata --locked`
    members = member_names(root, rep)
    absent = sorted(name for name in members.values() if name not in here)
    broken = [n for n in absent if n not in expected]
    if broken:
        rep.fails.append(
            "workspace member(s) missing from the lock and undeclared: " + ", ".join(broken)
            + " — cargo cannot resolve this workspace against this lock (`cargo metadata --locked` "
              "would refuse it)"
        )
    elif members:
        rep.oks.append(f"{len(members) - len(absent)} of {len(members)} workspace members resolve in the lock"
                       + (f"; the rest are declared: {', '.join(absent)}" if absent else ""))

    # (d) the locks nothing regenerates
    diff = git(root, "diff", "--name-only", base, "HEAD", "--", *FROZEN_LOCKS)
    if diff.returncode != 0:
        rep.notes.append(f"git diff against {base[:12]} failed; frozen-lock equality unchecked")
    elif diff.stdout.split():
        for path in diff.stdout.split():
            rep.fails.append(
                f"{path} differs from the base tag — assemble never regenerates it and substitute.py "
                f"skips it, so this is a hand edit or a bad merge; restore it with "
                f"`git checkout {base[:12]} -- {path}`"
            )
    else:
        rep.oks.append(f"{' and '.join(FROZEN_LOCKS)} are byte-equal to the base tag")

    # `cargo metadata --locked`, for a caller that has a toolchain
    if args.cargo_metadata and not assembled:
        rep.notes.append("--cargo-metadata skipped on a pre-assembly tree: delta's lock lacks the "
                         "fork's crates by policy, so --locked is expected to refuse it")
    elif args.cargo_metadata:
        proc = subprocess.run(["cargo", "metadata", "--locked", "--format-version", "1"],
                              cwd=root / "codex-rs", capture_output=True, text=True, timeout=900)
        if proc.returncode != 0:
            for line in proc.stderr.strip().splitlines()[:10]:
                print(f"  {line}")
            rep.fails.append("cargo metadata --locked refused this workspace (stderr above); when it "
                             "names the lock, the tree was assembled without `cargo update --workspace`")
        else:
            rep.oks.append("cargo metadata --locked resolves the workspace against this lock")

    code = rep.finish()
    if code == 0:
        print("ok: lock integrity clean")
    return code


if __name__ == "__main__":
    sys.exit(main())
