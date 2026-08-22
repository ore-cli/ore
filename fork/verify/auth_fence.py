#!/usr/bin/env python3
"""Auth-fence check (I-AUTH): the ChatGPT sign-in path stays byte-identical.

ore has no OAuth client registration of its own; the flow only works because
every wire-visible byte of the login path is upstream's.  The legacy fork
rebranded inside the login crate (CODEX_API_KEY -> ORE_API_KEY, "Codex Auth"
-> "ore Auth", ...) and broke exactly this — the fence exists so that class of
regression is structurally impossible.

Three layers:
  a. `git diff <fork/UPSTREAM commit> -- <fence paths>` (working tree), normalised
     (index lines dropped), must equal fork/verify/allowed-fence.diff.
  b. Wire literals OUTSIDE the fence must still appear verbatim at their home
     sites — a substitution rule or series commit rewriting one of them
     changes what the backend sees without touching a fenced file.
  c. Binary keep/forbid sets, delegated to strings_check.py (--binary).

Exit codes: 0 ok, 1 fail, 2 could-not-run.
"""

from __future__ import annotations

import argparse
import importlib.util
import re
import subprocess
import sys
import tomllib
from pathlib import Path

# The ratified fence (plan, auth.md §2/§5): the login crate plus every
# wire-literal carrier, including app-server-protocol.
FENCE_PATHS = (
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
)

# Generated output inside a fenced path. `app-server-protocol/schema/` is written
# by assemble's schema-regen pass -- the JSON, TypeScript and precomputed exports
# are derived from the Rust types, and the first full assembly rewrote 28 of them
# because the WireApi variants changed. Diffing them here would report a fence
# violation for every assembly that regenerates them correctly.
#
# Nothing is lost by excluding them: the wire identifiers this fence protects
# live in the crate's .rs sources, which remain fenced, and upstream ships
# exact-bytes fixture tests that fail if the generated output does not match the
# types it came from.
FENCE_EXCLUDE = (":(exclude)codex-rs/app-server-protocol/schema/**",)

# (file, literal, why) — wire identifiers that live outside the fence.
# All verified present at rust-v0.149.0.
OUTSIDE_LITERALS = (
    ("codex-rs/exec/src/lib.rs", '"codex_exec"',
     "originator for the exec binary; the backend keys behaviour off it"),
    ("codex-rs/tui/src/lib.rs", 'client_name: "codex-tui"',
     "wire client_name sent by the TUI"),
    ("codex-rs/app-server/src/request_processors/initialize_processor.rs", '"codex_app_server_daemon"',
     "non-originating client name the app-server recognises"),
    ("codex-rs/app-server/src/request_processors/initialize_processor.rs", '"codex-backend"',
     "non-originating client name the app-server recognises"),
    ("codex-rs/chatgpt/src/chatgpt_client.rs", 'CODEX_PRODUCT_SKU: &str = "codex"',
     "OAI-Product-Sku header value on ChatGPT-backend requests"),
    ("codex-rs/backend-client/src/client.rs", '"codex-cli"',
     "User-Agent fallback for the backend client"),
)


def normalize_diff(text: str) -> list[str]:
    # `index <hash>..<hash> <mode>` lines churn with every rebase and carry no
    # review value; everything else in the diff is the contract.
    return [ln for ln in text.splitlines() if not ln.startswith("index ")]


def load_allowed(path: Path) -> list[str]:
    return [ln for ln in path.read_text(encoding="utf-8").splitlines()
            if ln and not ln.startswith("#")]


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", help="repo root (default: two levels above this script)")
    ap.add_argument("--binary", metavar="PATH",
                    help="also run the binary keep/forbid sets via strings_check.py")
    args = ap.parse_args(argv)

    root = Path(args.root).resolve() if args.root else Path(__file__).resolve().parents[2]
    here = Path(__file__).resolve().parent
    fails: list[str] = []

    try:
        with open(root / "fork" / "UPSTREAM", "rb") as fh:
            meta = tomllib.load(fh)
    except (OSError, tomllib.TOMLDecodeError) as err:
        print(f"skip: fork/UPSTREAM unreadable: {err}")
        return 2
    base = meta.get("commit") or meta.get("tag")
    if not base:
        print("skip: fork/UPSTREAM carries neither commit nor tag")
        return 2

    # (a) fence diff == allowed-fence.diff
    #
    # Diffed against the WORKING TREE, not against HEAD. `git diff base HEAD`
    # compares two commits and is blind to the tree it is certifying: a red-team
    # run rewrote the ChatGPT originator in codex-rs/login/ and this check
    # reported the fence intact, because the edit was not committed. In CI the
    # two are the same; the cases where they differ -- an uncommitted edit, or a
    # generated pass that reached into a fenced path -- are exactly the ones
    # worth catching.
    try:
        proc = subprocess.run(
            ["git", "-C", str(root), "diff", base, "--"] + list(FENCE_PATHS) + list(FENCE_EXCLUDE),
            capture_output=True, text=True, check=True,
        )
        untracked = subprocess.run(
            ["git", "-C", str(root), "ls-files", "--others", "--exclude-standard", "--"]
            + list(FENCE_PATHS) + list(FENCE_EXCLUDE),
            capture_output=True, text=True, check=True,
        ).stdout.split()
    except (OSError, subprocess.CalledProcessError) as err:
        print(f"skip: git diff against {base} failed: {err}")
        return 2
    # A new file inside a fenced path is not a diff hunk, so it would otherwise
    # be invisible to a check whose whole job is "these paths are upstream's".
    for path in untracked:
        fails.append(f"untracked file inside the auth fence: {path}")
    actual = normalize_diff(proc.stdout)
    allowed = load_allowed(here / "allowed-fence.diff")
    if actual != allowed:
        import difflib
        delta = list(difflib.unified_diff(allowed, actual, "allowed-fence.diff", "git diff (normalised)", lineterm=""))
        fails.append(
            f"fence diff against {base} does not equal allowed-fence.diff "
            f"({len(actual)} vs {len(allowed)} lines) — either an unauthorised change touched the auth "
            f"fence, or a deliberate series change forgot to regenerate the reference"
        )
        for ln in delta[:40]:
            print(f"  {ln}")
        if len(delta) > 40:
            print(f"  … {len(delta) - 40} more lines")
    else:
        print(f"ok: fence diff vs {base[:12]} equals allowed-fence.diff ({len(allowed)} allowed lines)")

    # (b) wire literals outside the fence
    for rel, literal, why in OUTSIDE_LITERALS:
        path = root / rel
        if not path.is_file():
            fails.append(f"{rel} no longer exists — the wire literal {literal!r} moved; re-derive the fence ({why})")
            continue
        if literal not in path.read_text(encoding="utf-8"):
            fails.append(
                f"{rel} no longer contains {literal!r} verbatim — a substitution or series commit "
                f"rewrote a wire identifier ({why})"
            )
        else:
            print(f"ok: {rel} still carries {literal!r}")

    # (c) binary keep/forbid, shared engine
    if args.binary:
        spec = importlib.util.spec_from_file_location("strings_check", here / "strings_check.py")
        sc = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(sc)
        code = sc.main(["--binary", args.binary, "--root", str(root)])
        if code == 1:
            fails.append("binary keep/forbid sets failed (strings_check --binary above)")
        elif code == 2:
            print(f"skip: binary {args.binary} not scannable")
            return 2
    else:
        print("note: binary keep/forbid deferred to the strings-binary check (no --binary given)")

    for f in fails:
        print(f"FAIL: {f}")
    if not fails:
        print("ok: auth fence intact")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
