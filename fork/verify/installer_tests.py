#!/usr/bin/env python3
"""Run the installer suites, deselecting cases the rebrand makes inapplicable.

scripts/install/test_install_sh.py is UPSTREAM's file and stays upstream's: the
fork rebrands it alongside the script it exercises rather than editing it, so the
policy about which cases apply lives here instead of as decorators in their tree.

The `test_releases_*` cases drive install.sh's releases.openai.com path. The
substitution `installer-releases-base-url` empties RELEASES_BASE_URL, so on an
assembled tree that path cannot be taken -- `download_text` fails on a relative
URL and the installer falls through to GitHub Releases, which is the point. Those
cases assert a flow the shipped installer does not have.

On an UNRENDERED tree they are upstream's own tests against upstream's own
script, and they must pass; nothing is deselected there.

Every deselection is printed with its reason. A suite that quietly runs less than
you think is the failure this repository keeps meeting.

Exit codes: 0 ok, 1 fail, 2 could-not-run.
"""

from __future__ import annotations

import argparse
import sys
import unittest
from pathlib import Path

# Cases that exercise a path the rebrand disables. Prefix match on the method.
RENDERED_DESELECT = {"test_install_sh": ("test_releases_",)}


def rendered(root: Path) -> bool:
    """True once fork/substitute.py has run over the installer."""
    sh = root / "scripts" / "install" / "install.sh"
    return 'RELEASES_BASE_URL=""' in sh.read_text(encoding="utf-8")


def collect(suite, out):
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            collect(item, out)
        else:
            out.append(item)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", default=".", help="repository root")
    args = ap.parse_args()
    root = Path(args.root).resolve()
    start = root / "scripts" / "install"
    if not start.is_dir():
        print(f"skip: {start} does not exist", file=sys.stderr)
        return 2

    is_rendered = rendered(root)
    print(f"tree: {'rendered (assembled)' if is_rendered else 'unrendered (series)'}")

    sys.path.insert(0, str(start))
    discovered = unittest.defaultTestLoader.discover(
        str(start), pattern="test_install_*.py"
    )
    tests: list[unittest.TestCase] = []
    collect(discovered, tests)
    if not tests:
        # Discovering nothing is the silent-green case, not a pass.
        print("FAIL: discovered no installer tests at all", file=sys.stderr)
        return 1

    selected, dropped = [], []
    for t in tests:
        module = type(t).__module__.rsplit(".", 1)[-1]
        method = t.id().rsplit(".", 1)[-1]
        prefixes = RENDERED_DESELECT.get(module, ()) if is_rendered else ()
        if any(method.startswith(p) for p in prefixes):
            dropped.append(t.id())
        else:
            selected.append(t)

    for d in sorted(dropped):
        print(f"  deselected: {d}")
    if dropped:
        print(f"  reason: the rebrand empties RELEASES_BASE_URL, so install.sh never")
        print(
            f"          takes the releases.openai.com path these {len(dropped)} case(s) drive"
        )
    print(f"running {len(selected)} of {len(tests)} installer test(s)")

    result = unittest.TextTestRunner(verbosity=2).run(unittest.TestSuite(selected))
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
