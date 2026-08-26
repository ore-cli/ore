#!/usr/bin/env python3
"""Helper binaries shipped beside `ore` must not gain telemetry reach unreviewed.

ore's telemetry is forced off at ONE seam: `analytics_enabled: Some(false)` in
codex-core's config loader, asserted layer-by-layer by the fork-invariants
telemetry suite.  That seam is in the process that loads codex-core's config.

The packaged installs also place helper binaries next to `ore` --
`codex-code-mode-host`, and on Windows `codex-command-runner.exe` and
`codex-windows-sandbox-setup.exe` -- and those are separate processes with their
own `main`.  They never load codex-core's config, so nothing about that seam
reaches them.  A helper that grows an OTLP exporter is outside every guarantee
the fork makes, and no existing check notices: deps_gate.py watches the
LOCKFILE's telemetry crate SET, which does not change when one more workspace
member depends on another.

So this freezes the dependency EDGE instead, transitively.  It is a tripwire,
not a proof: the verdict it wants is a human one.  Upstream added `codex-otel`
to code-mode-host on main after rust-v0.149.1 (opt-in `--otel-trace-exporter`
and a loopback `--otel-trace-listen`, no default endpoint), so this fires on the
sync that brings it.  That is the point -- the fork rules on it in that PR
rather than discovering it later.  Flip the entry, and say why.

`codex-app-server` is deliberately NOT baselined although it ships as its own
package: it already depends on codex-otel, and unlike these it DOES go through
codex-core's config loader, so the analytics seam covers it.

Reach means normal dependencies only, walked through first-party crates.
dev- and build-dependencies do not link into a shipped binary; they are reported
separately and never change the verdict.

Exit codes: 0 ok, 1 fail, 2 could-not-run.
"""

from __future__ import annotations

import argparse
import sys
import tomllib
from pathlib import Path

OTEL_CRATE = "codex-otel"
NORMAL = "dependencies"
NON_SHIPPING = ("dev-dependencies", "build-dependencies")

# crate directory under codex-rs/ -> does it reach codex-otel today?
#
# `windows-sandbox-rs` is True because it already did when this was written; it
# is listed so its REMOVAL is noticed too, the way deps_gate refuses silent
# removals.
BASELINE: dict[str, bool] = {
    "code-mode-host": False,
    "responses-api-proxy": False,
    "windows-sandbox-rs": True,
}


class Failure(Exception):
    """A condition that must stop a human, not be skipped past."""


def load(manifest: Path) -> dict:
    try:
        return tomllib.loads(manifest.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise Failure(f"{manifest} could not be read as TOML: {exc}") from exc


def dep_names(table: dict) -> set[str]:
    """Crate names in one dependency table, resolving `package = ` renames."""
    names = set()
    for key, spec in table.items():
        if isinstance(spec, dict) and "package" in spec:
            names.add(spec["package"])
        else:
            names.add(key)
    return names


def direct(data: dict, kind: str) -> set[str]:
    names = dep_names(data.get(kind, {}))
    for target in data.get("target", {}).values():
        names |= dep_names(target.get(kind, {}))
    return names


def first_party(root: Path) -> dict[str, Path]:
    """Workspace member name -> its Cargo.toml, from [workspace.dependencies]."""
    data = load(root / "codex-rs" / "Cargo.toml")
    out: dict[str, Path] = {}
    for name, spec in data.get("workspace", {}).get("dependencies", {}).items():
        if isinstance(spec, dict) and "path" in spec:
            real = spec.get("package", name)
            out[real] = root / "codex-rs" / spec["path"] / "Cargo.toml"
    return out


def reach(crate: str, manifest: Path, members: dict[str, Path]) -> list[str] | None:
    """Path from `crate` to codex-otel through normal deps, or None."""
    seen: set[str] = set()

    def walk(name: str, path: Path, trail: list[str]) -> list[str] | None:
        if name in seen:
            return None
        seen.add(name)
        for dep in sorted(direct(load(path), NORMAL)):
            if dep == OTEL_CRATE:
                return trail + [dep]
            if dep in members:
                found = walk(dep, members[dep], trail + [dep])
                if found:
                    return found
        return None

    return walk(crate, manifest, [crate])


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", default=".", help="repository root")
    args = ap.parse_args()
    root = Path(args.root).resolve()

    failures: list[str] = []
    try:
        members = first_party(root)
    except Failure as exc:
        print(f"skip: {exc}", file=sys.stderr)
        return 2

    for crate, expected in sorted(BASELINE.items()):
        manifest = root / "codex-rs" / crate / "Cargo.toml"
        if not manifest.is_file():
            # A rename is not a pass, and it is not a skip either: run.py treats
            # exit 2 as SKIP, which is fatal for nothing. Upstream moving one of
            # these must stop a human, so it is a FAIL.
            failures.append(
                f"FAIL: codex-rs/{crate}/Cargo.toml is missing. Upstream renamed or moved a\n"
                f"    baselined helper; re-point BASELINE at its new home and re-read what it\n"
                f"    links before doing so."
            )
            continue
        try:
            path = reach(crate, manifest, members)
            extra = sorted(
                d for k in NON_SHIPPING for d in direct(load(manifest), k) if d == OTEL_CRATE
            )
        except Failure as exc:
            failures.append(f"FAIL: {exc}")
            continue

        actual = path is not None
        if extra:
            # Never part of the verdict: neither table links into the binary.
            print(f"note: {crate} names {OTEL_CRATE} in {'/'.join(NON_SHIPPING)} (does not ship)")
        if actual == expected:
            via = " -> ".join(path) if path else "no normal-dependency path"
            print(f"ok: {crate}: {via}")
            continue
        if actual:
            failures.append(
                f"FAIL: {crate} now reaches {OTEL_CRATE} via {' -> '.join(path)}.\n"
                f"    It ships beside `ore` as its own process and never loads codex-core's\n"
                f"    config, so the seam that forces analytics off does not apply to it. Rule\n"
                f"    on what it exports by default, then set BASELINE[{crate!r}] = True."
            )
        else:
            failures.append(
                f"FAIL: {crate} no longer reaches {OTEL_CRATE}. Good news, probably, but the\n"
                f"    baseline documents the analysis -- set BASELINE[{crate!r}] = False."
            )

    if failures:
        print(f"\n{len(failures)} helper telemetry change(s) need a verdict:", file=sys.stderr)
        for f in failures:
            print(f"  - {f}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
