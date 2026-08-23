#!/usr/bin/env python3
"""Version and tag invariants (I-VER, scheme C: 1.{upstream_minor}.{ore_patch}).

Tree checks (always):
  a. fork/VERSION matches the ore semver grammar (upstream's release grammar
     with the same alpha/beta prerelease forms).
  b. fork/VERSION == [workspace.package] version in codex-rs/Cargo.toml —
     table-scoped scan, because upstream's own `grep -m1 '^version'` is not
     and can match the wrong table.  Until the version series commit lands the
     workspace still carries the upstream base version; that reports pending.
  c. minor(fork/VERSION) == minor(fork/UPSTREAM tag) — the sync-blocking
     scheme-C check: assemble derives the minor from the base tag, so drift
     here means fork/VERSION was hand-edited against the scheme.

--tag TAG        validates a release tag: grammar ^ore-v<semver>$ and
                 TAG == ore-v<fork/VERSION>.
--binary PATH    runs `<bin> --version` and asserts the ratified two-line
                 format, then SIMULATES the three real consumers of that
                 output (daemon token split, install.sh sed, install.ps1
                 regex) — the format only counts as stable if all three
                 recover the exact version.
--on-main-proof  the 3-grep release-gate proof that the assembled tree ships
                 `ore` (runs automatically on an assembled tree).
--tree-only      skip binary checks even if --binary was given.

Exit codes: 0 ok, 1 fail, 2 could-not-run, 3 only expected-pending findings.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from pathlib import Path

# Upstream's release grammar (rust-release.yml / install.sh / install.ps1 /
# publish_r2_release.py agree on it) with the ore-v prefix.
SEMVER_RE = re.compile(r"^([0-9]+)\.([0-9]+)\.([0-9]+)(-(alpha(\.[0-9]+){0,2}|beta(\.[0-9]+)?))?$")
TAG_RE = re.compile(r"^ore-v([0-9]+\.[0-9]+\.[0-9]+(-(alpha(\.[0-9]+){0,2}|beta(\.[0-9]+)?))?)$")

# Line 2 must end in ')' so no installer's trailing-token regex can ever
# mistake the base SHA for a version (verdict-version-on-wire).
LINE2_RE = re.compile(r"^codex-base: (rust-v[0-9]+\.[0-9]+\.[0-9]+) \(([0-9a-f]{7,40})\)$")

# The three on-main-proof greps: the shipped CLI is `ore` while the cargo bin
# target stays `codex` (packaging-time rename, decision 1).
ON_MAIN_PROOFS = (
    ("scripts/codex_package/targets.py", re.compile(r'executable_stem\s*=\s*"ore"'),
     "packaging renames the shipped entrypoint to ore"),
    ("codex-rs/cli/src/main.rs", re.compile(r'name\s*=\s*"ore"'),
     "clap identity: --version/--help print ore"),
    ("codex-cli/package.json", re.compile(r'"ore":\s*"bin/'),
     "npm bin entry ships the ore command"),
)


class Report:
    def __init__(self):
        self.fails: list[str] = []
        self.pendings: list[str] = []
        self.oks: list[str] = []
        self.notes: list[str] = []

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


def workspace_version(cargo_toml: Path) -> str | None:
    """Table-scoped: only a version line inside [workspace.package] counts."""
    table = None
    for line in cargo_toml.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        m = re.match(r"^\[(.+)\]$", stripped)
        if m:
            table = m.group(1)
            continue
        if table == "workspace.package":
            m = re.match(r'^version\s*=\s*"([^"]+)"', stripped)
            if m:
                return m.group(1)
    return None


# --- the three real consumers of `--version`, simulated exactly ------------

def sim_daemon(output: str) -> str | None:
    # app-server-daemon managed_install.rs: split_whitespace().nth(1) over the
    # WHOLE output — the version must be the second token overall.
    toks = output.split()
    return toks[1] if len(toks) > 1 else None


def sim_install_sh(output: str) -> str | None:
    # install.sh: sed -n 's/.* \([0-9][0-9A-Za-z.+-]*\)$/\1/p' | head -n 1
    # BRE, greedy `.*`, so the capture is the LAST version-shaped token of the
    # first line that has one.
    for line in output.splitlines():
        m = re.match(r"^(.*) ([0-9][0-9A-Za-z.+-]*)$", line)
        if m:
            return m.group(2)
    return None


def sim_install_ps1_line(line: str) -> str | None:
    # install.ps1: `$versionOutput -match '([0-9][0-9A-Za-z.+-]*)$'` — the fork
    # installer feeds it the FIRST line (ps1's $matches is not populated when
    # the left side is a multi-line array; verdict-version-on-wire).
    m = re.search(r"([0-9][0-9A-Za-z.+-]*)$", line)
    return m.group(1) if m else None


def check_binary(bin_path: Path, expect_version: str, expect_tag: str, expect_commit: str, rep: Report) -> None:
    try:
        proc = subprocess.run([str(bin_path), "--version"], capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as err:
        rep.fails.append(f"could not run {bin_path} --version: {err}")
        return
    out = proc.stdout
    lines = out.splitlines()
    if not lines:
        rep.fails.append(f"{bin_path} --version produced no stdout (stderr: {proc.stderr.strip()[:200]!r})")
        return

    line1 = lines[0]
    m = re.match(r"^ore (\S+)$", line1)
    if not m:
        rep.fails.append(f"--version line 1 is {line1!r}; must be exactly two tokens: 'ore <semver>'")
        return
    ver = m.group(1)
    if not SEMVER_RE.match(ver):
        rep.fails.append(f"--version line 1 version {ver!r} does not match the ore semver grammar")
    if ver != expect_version:
        rep.fails.append(
            f"--version reports {ver} but fork/VERSION (== CARGO_PKG_VERSION by check b) is {expect_version} — "
            f"the daemon's IfVersionChanged restart logic string-compares these"
        )
    rep.oks.append(f"--version line 1 = {line1!r}")

    if len(lines) < 2:
        rep.fails.append("--version has no line 2; expected 'codex-base: rust-vX.Y.Z (<sha>)'")
    else:
        m2 = LINE2_RE.match(lines[1])
        if not m2:
            rep.fails.append(
                f"--version line 2 is {lines[1]!r}; must match "
                f"'^codex-base: rust-vX.Y.Z (<7-40 hex>)$' (the trailing ')' keeps installer regexes off it)"
            )
        else:
            if expect_tag and m2.group(1) != expect_tag:
                rep.fails.append(f"line 2 base tag {m2.group(1)} != fork/UPSTREAM tag {expect_tag}")
            if expect_commit and not expect_commit.startswith(m2.group(2)):
                rep.fails.append(f"line 2 base sha {m2.group(2)} is not a prefix of fork/UPSTREAM commit {expect_commit}")
            rep.oks.append(f"--version line 2 = {lines[1]!r}")

    for name, got in (
        ("app-server-daemon split_whitespace().nth(1)", sim_daemon(out)),
        ("install.sh sed last-token-of-first-matching-line", sim_install_sh(out)),
        ("install.ps1 first-line trailing-token match", sim_install_ps1_line(line1)),
    ):
        if got != ver:
            rep.fails.append(f"consumer simulation {name} recovers {got!r}, not {ver!r}")
        else:
            rep.oks.append(f"consumer simulation ok: {name} -> {got}")
    if len(lines) > 1 and sim_install_ps1_line(lines[1]) is not None:
        rep.fails.append(
            f"line 2 unexpectedly ends in a version-shaped token; a naive whole-output "
            f"installer match would grab it: {lines[1]!r}"
        )


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", help="repo root (default: two levels above this script)")
    ap.add_argument("--tag", help="validate a release tag (e.g. ore-v1.149.0)")
    ap.add_argument("--binary", metavar="PATH", help="run PATH --version and check format + consumer sims")
    ap.add_argument("--tree-only", action="store_true", help="skip binary checks")
    ap.add_argument("--on-main-proof", action="store_true",
                    help="force the 3-grep on-main proof (automatic on an assembled tree)")
    args = ap.parse_args(argv)

    root = Path(args.root).resolve() if args.root else Path(__file__).resolve().parents[2]
    rep = Report()

    version_file = root / "fork" / "VERSION"
    if not version_file.is_file():
        print("skip: fork/VERSION does not exist")
        return 2
    fork_version = version_file.read_text(encoding="utf-8").strip()

    try:
        with open(root / "fork" / "UPSTREAM", "rb") as fh:
            meta = tomllib.load(fh)
    except (OSError, tomllib.TOMLDecodeError) as err:
        print(f"skip: fork/UPSTREAM unreadable: {err}")
        return 2
    base_tag = meta.get("tag", "")
    base_commit = meta.get("commit", "")
    assembled = bool(meta.get("assembled_at"))

    # (a) grammar
    m = SEMVER_RE.match(fork_version)
    if not m:
        rep.fails.append(f"fork/VERSION {fork_version!r} does not match the ore semver grammar")
    else:
        rep.oks.append(f"fork/VERSION {fork_version} matches the ore semver grammar")

    # (c) scheme C: minor tracks the upstream base minor
    base_m = re.match(r"^rust-v([0-9]+)\.([0-9]+)\.([0-9]+)$", base_tag)
    if not base_m:
        rep.fails.append(f"fork/UPSTREAM tag {base_tag!r} is not a rust-vX.Y.Z stable tag")
    elif m:
        if m.group(2) != base_m.group(2):
            rep.fails.append(
                f"scheme-C violation (sync-blocking): minor(fork/VERSION)={m.group(2)} but "
                f"minor(base tag {base_tag})={base_m.group(2)} — assemble derives 1.{{upstream_minor}}.0 "
                f"on a new base; fork/VERSION was edited against the scheme"
            )
        else:
            rep.oks.append(f"scheme C holds: minor {m.group(2)} == upstream base minor ({base_tag})")

    # (b) workspace version, table-scoped
    cargo_toml = root / "codex-rs" / "Cargo.toml"
    ws = workspace_version(cargo_toml)
    base_version = base_m and f"{base_m.group(1)}.{base_m.group(2)}.{base_m.group(3)}"
    if ws is None:
        rep.fails.append("no version line found inside [workspace.package] in codex-rs/Cargo.toml")
    elif ws == fork_version:
        rep.oks.append(f"[workspace.package] version == fork/VERSION ({ws})")
    elif base_version and ws == base_version:
        rep.pendings.append(
            f"[workspace.package] version is still the upstream base {ws}; it becomes {fork_version} "
            f"when the version series commit lands (CARGO_PKG_VERSION is wire-visible, so this must "
            f"never ship mismatched)"
        )
    else:
        rep.fails.append(
            f"[workspace.package] version {ws} matches neither fork/VERSION ({fork_version}) "
            f"nor the upstream base ({base_version})"
        )

    # (d) release tag
    #
    # Compare the WHOLE version, prerelease suffix included.
    #
    # This once compared only the base, on the reasoning that comparing the whole
    # thing "rejected every prerelease". It was rejecting them correctly. A tag of
    # ore-v1.149.0-alpha.1 over a tree whose [workspace.package] version is 1.149.0
    # ships a binary reporting `ore 1.149.0`, while scripts/install resolves
    # 1.149.0-alpha.1 from the tag and then refuses what it just downloaded:
    # "Installed ore command did not report expected version". The same mismatch
    # makes the app-server daemon reinstall on every start, because it compares
    # --version's second token against its own CARGO_PKG_VERSION.
    #
    # A prerelease is therefore not a tag-only decoration. To cut ore-vX.Y.Z-alpha.N,
    # fork/VERSION must read X.Y.Z-alpha.N before assembly, so that the workspace
    # version, CARGO_PKG_VERSION, the tag and the installer all agree.
    if args.tag:
        tm = TAG_RE.match(args.tag)
        if not tm:
            rep.fails.append(f"tag {args.tag!r} does not match the ore tag grammar ^ore-v<semver>$")
        elif tm.group(1) != fork_version:
            rep.fails.append(
                f"tag {args.tag} carries {tm.group(1)}, but fork/VERSION is {fork_version} — set "
                f"fork/VERSION to {tm.group(1)} and reassemble before tagging, or the installer "
                f"will reject the binary it just downloaded"
            )
        else:
            rep.oks.append(f"tag {args.tag} matches the grammar and fork/VERSION")

    # (f) on-main proof
    if args.on_main_proof or assembled:
        for rel, rx, why in ON_MAIN_PROOFS:
            path = root / rel
            if not path.is_file():
                rep.fails.append(f"on-main proof: {rel} does not exist")
            elif not rx.search(path.read_text(encoding="utf-8")):
                rep.fails.append(f"on-main proof: {rel} lacks /{rx.pattern}/ ({why})")
            else:
                rep.oks.append(f"on-main proof: {rel} carries /{rx.pattern}/")
    else:
        rep.notes.append("on-main proof skipped on a pre-assembly tree (the greps only hold on generated main)")

    # (e) binary format + consumer simulations
    if args.binary and not args.tree_only:
        p = Path(args.binary)
        if not p.is_file():
            print(f"skip: binary {p} does not exist")
            return 2
        check_binary(p, fork_version, base_tag, base_commit, rep)

    code = rep.finish()
    if code == 0:
        print("ok: version checks clean")
    return code


if __name__ == "__main__":
    sys.exit(main())
