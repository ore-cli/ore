#!/usr/bin/env python3
"""Extract a CI run's real test failures, and refuse to guess.

Reading failures out of a nextest log by grepping one status word is how this
repository reported "31 failures" for a run that had 33: the grep matched
`TRY 3 FAIL` and silently dropped `TRY 3 TMT`, the status a test gets when it is
killed at the slow-timeout ceiling. Two analytics tests sat in that blind spot
for a full CI cycle. A filter that drops an outcome class looks exactly like a
clean result.

So this does two things a grep does not:

  * it collects EVERY terminal outcome class, not one, and treats a test that
    later passes on retry as flaky rather than failed;
  * it reconciles its own total against nextest's `Summary` line, which states
    `N failed, M timed out` independently. If the two disagree it reports the
    disagreement and exits non-zero rather than printing a number it cannot
    justify. An extractor that cannot be wrong quietly is the whole point.

  ci_failures.py --run <id> [--repo owner/name]   # fetch with gh
  ci_failures.py --log <file>                     # already-downloaded log

Exit codes: 0 ok (list printed, possibly empty), 1 reconciliation failed,
2 could not run.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

# nextest prints one line per attempt. The leading `TRY n` appears only when
# retries are configured, so it is optional; the status word is what matters.
OUTCOME = re.compile(
    r"(?:TRY (?P<try>\d+) )?"
    # LEAK-FAIL before LEAK: alternation is first-match, and the longer one must
    # win or `LEAK-FAIL` parses as a plain LEAK.
    r"(?P<status>PASS|FAIL|TMT|LEAK-FAIL|LEAK|SIGSEGV|SIGABRT|SIGILL|SIGBUS"
    # ABRT is nextest's own spelling for a signal-terminated test, and it is
    # what a stack overflow produces. Missing it cost a second reconciliation
    # failure -- 1 extracted against 5 declared -- on the first probe run that
    # hit one. ABORT is kept for older output.
    r"|ABRT|ABORT)"
    r"\s+\[\s*[^\]]*\]\s*\([^)]*\)\s+(?P<binary>\S+)\s+(?P<test>\S+)"
)
# `Summary [  549.048s] 4132 tests run: 4129 passed (3 slow, 1 flaky), 1 failed,
#  2 timed out, 12739 skipped` -- the counts are absent when zero.
SUMMARY = re.compile(r"Summary \[[^\]]*\]\s+(?P<run>\d+) tests? run:")
SUM_FAILED = re.compile(r"(\d+) failed")
SUM_TIMEOUT = re.compile(r"(\d+) timed out")

# LEAK is a PASS that leaked a handle: nextest counts it inside `N passed
# (... 3 leaky)`, not in `N failed`. Treating it as a failure is what made the
# first real run of this script over-count 12 as 15 -- caught by the
# reconciliation below, which is the entire reason that check exists. LEAK-FAIL
# is the opposite: a leak the profile promotes to a failure.
PASSING = {"PASS", "LEAK"}


def strip_ansi(text: str) -> str:
    return re.sub(r"\x1b\[[0-9;]*m", "", text)


def fetch(run: str, repo: str | None) -> str:
    cmd = ["gh", "run", "view", run, "--log-failed"]
    if repo:
        cmd += ["--repo", repo]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0 or not proc.stdout.strip():
        print(f"could not fetch run {run}: {proc.stderr.strip()[:400]}", file=sys.stderr)
        raise SystemExit(2)
    return proc.stdout


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--run", help="GitHub Actions run id, fetched with gh")
    src.add_argument("--log", type=Path, help="a log file already on disk")
    ap.add_argument("--repo", help="owner/name, when --run is not in this repo's default")
    args = ap.parse_args()

    text = strip_ansi(args.log.read_text(encoding="utf-8", errors="replace")
                      if args.log else fetch(args.run, args.repo))

    # Per test, every attempt's status in order. A test that ever reaches PASS
    # succeeded on retry: nextest counts it flaky, and so do we.
    attempts: dict[tuple[str, str], list[str]] = {}
    for m in OUTCOME.finditer(text):
        attempts.setdefault((m.group("binary"), m.group("test")), []).append(m.group("status"))

    failed = sorted(
        f"{binary} {test} [{outcomes[-1]}]"
        for (binary, test), outcomes in attempts.items()
        if outcomes and not any(o in PASSING for o in outcomes)
    )

    # Independent count, straight from nextest.
    declared = 0
    summaries = 0
    for line in text.splitlines():
        if not SUMMARY.search(line):
            continue
        summaries += 1
        f = SUM_FAILED.search(line)
        t = SUM_TIMEOUT.search(line)
        declared += (int(f.group(1)) if f else 0) + (int(t.group(1)) if t else 0)

    for entry in failed:
        print(entry)

    if summaries == 0:
        print("\nno nextest Summary line found: cannot reconcile, so this list is "
              "unverified. Check that the log covers the test jobs.", file=sys.stderr)
        return 1
    if len(failed) != declared:
        print(f"\nRECONCILIATION FAILED: extracted {len(failed)} failing test(s), but "
              f"{summaries} nextest Summary line(s) declare {declared}.", file=sys.stderr)
        print("  The extractor is missing an outcome class, or the log is truncated.",
              file=sys.stderr)
        print("  Do not report either number until they agree.", file=sys.stderr)
        return 1
    print(f"\nok: {len(failed)} failing test(s), reconciled against {summaries} "
          f"nextest summary line(s)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
