#!/usr/bin/env bash
# fork/preflight.sh — what to check before landing a sync.
#
# Run from a checkout of the SERIES (delta, or a sync-delta/* branch), naming the
# candidate commit assemble produced:
#
#   fork/preflight.sh [--tests] <candidate-ref>
#
# --tests additionally runs the CANDIDATE's full nextest suite with the same
# known-failing filter ore-ci uses. It is slow and it needs the prebuilt v8 (see
# "Reproducing CI locally" in CLAUDE.md), and it is worth it: the rust-v0.151.0
# sync took three CI rounds at ~75 minutes each because each round found the next
# batch of known-failing entries. All of them were findable here first.
#
# It exists because the rust-v0.150.1 sync landed green and turned delta red one
# push later. Every check below was available at the time; the reason it was
# missed is that they were run against the wrong tree, or not at once.
#
# The two trees answer different questions and BOTH have to be asked:
#
#   * the SERIES is what delta becomes. version_check --root against it is a
#     different assertion from version_check --tag against the candidate, and it
#     is the one that caught fork/VERSION lagging its own base.
#   * the CANDIDATE is what ships. A substitution contradiction exists only
#     after substitution, so a test that passes on the series proves nothing
#     about it -- the agents-summary home path passed on delta and failed on
#     main for exactly that reason.
set -uo pipefail

fail=0
say()  { printf '\n\033[36m==>\033[0m %s\n' "$*"; }
ok()   { printf '    \033[32mok\033[0m   %s\n' "$*"; }
bad()  { printf '    \033[31mFAIL\033[0m %s\n' "$*"; fail=1; }
# fork/verify/run.py's convention: 0 ok, 1 fail, 2 could-not-run, 3 pending.
# PENDING is not a failure -- version_check reports it on a series tree because
# the workspace version is still upstream's until the assembly writes it, which
# is exactly right before assembly. Treating it as red would make this script
# cry wolf on every clean series, and a preflight nobody believes is worse than
# no preflight.
run()  {
  "$@" >/dev/null 2>&1
  case $? in
    0) ok "$*" ;;
    3) ok "$* (pending, expected pre-assembly)" ;;
    2) printf '    \033[33mskip\033[0m %s (could not run)\n' "$*" ;;
    *) bad "$*" ;;
  esac
}

RUN_TESTS=0
CAND=""
for arg in "$@"; do
  case "$arg" in
    --tests) RUN_TESTS=1 ;;
    -*) echo "preflight: unknown flag '$arg'" >&2; exit 2 ;;
    *)  CAND="$arg" ;;
  esac
done
[[ -n "$CAND" ]] || { echo "usage: fork/preflight.sh [--tests] <candidate-ref>" >&2; exit 2; }
git rev-parse --verify -q "$CAND^{commit}" >/dev/null \
  || { echo "preflight: $CAND is not a commit" >&2; exit 2; }

# A shallow boundary is the failure that looks like someone else's. Fetching an
# upstream ref to compare against -- which this fork does constantly -- can write
# .git/shallow, and every fetch after it inherits truncated history. Objects then
# READ fine, fsck stays clean, and rev-list --objects succeeds, because none of
# those need ancestry. Only SERVING a pack walks parents, so the first symptom is
# a push rejected by the remote for an object you have never had:
#
#   remote: fatal: did not receive expected object <sha>
#   error: remote unpack failed: index-pack failed
#
# That cost a full debugging session. `git fetch --unshallow <remote>` fixes it.
say "repository shape"
if [ -f "$(git rev-parse --git-dir)/shallow" ]; then
  bad "the repository is SHALLOW — run: git fetch --unshallow upstream"
  printf '    a shallow boundary truncates history for everything fetched after it,\n' >&2
  printf '    and the push it breaks reports an object you never had.\n' >&2
else
  ok "not a shallow clone"
fi

# Cheap corroboration: the base tag should reach a plausible amount of history.
# When this bit, the base tag reached 81 commits and its neighbour 9784 -- the
# difference was unmistakable the moment anything actually counted.
#
# Resolve it through the fetch namespace FIRST. Upstream tags are fetched into
# refs/upstream/tags/* and deliberately not as local tags, so the bare name is
# exactly what a correctly-namespaced clone cannot resolve -- which silently
# skipped this whole block and printed nothing, while preflight still said
# clean. It ran at all only in a clone carrying legacy local rust-v* tags.
_base_tag="$(sed -n 's/^tag = "\(.*\)"/\1/p' fork/UPSTREAM 2>/dev/null | head -1)"
_base_ref=""
for _cand in "refs/upstream/tags/$_base_tag" "$_base_tag"; do
  if [ -n "$_base_tag" ] && git rev-parse --verify -q "$_cand^{commit}" >/dev/null 2>&1; then
    _base_ref="$_cand"
    break
  fi
done
if [ -n "$_base_tag" ] && [ -z "$_base_ref" ]; then
  bad "$_base_tag does not resolve — cannot corroborate history depth"
elif [ -n "$_base_ref" ]; then
  _n="$(git rev-list --count "$_base_ref" 2>/dev/null || echo 0)"
  if [ "$_n" -lt 1000 ]; then
    bad "$_base_tag reaches only $_n commit(s) — history looks truncated"
  else
    ok "$_base_tag reaches $_n commits"
  fi
fi

say "series (this checkout) — what delta becomes"
run python3 fork/verify/version_check.py --root .
run bash fork/lint-series.sh
run python3 fork/verify/known_failing_check.py --root .

say "candidate $CAND — what ships"
WT="$(mktemp -d)/candidate"
git worktree add -q --detach "$WT" "$CAND" || { echo "preflight: could not check out $CAND" >&2; exit 2; }
trap 'git worktree remove --force "$WT" >/dev/null 2>&1 || true' EXIT

run python3 "$WT/fork/verify/run.py" --suite static --root "$WT"
if python3 "$WT/fork/verify/version_check.py" --tag "ore-v$(tr -d '[:space:]' <"$WT/fork/VERSION")" --root "$WT" >/dev/null 2>&1; then
  ok "version_check --tag ore-v$(tr -d '[:space:]' <"$WT/fork/VERSION")"
else
  bad "version_check --tag ore-v$(tr -d '[:space:]' <"$WT/fork/VERSION")"
fi

# Append-only spine: a candidate that does not descend from main cannot be
# fast-forwarded onto it, and finding that out during finalize is too late.
if git merge-base --is-ancestor refs/remotes/origin/main "$CAND" 2>/dev/null; then
  ok "origin/main is an ancestor of the candidate"
else
  bad "origin/main is NOT an ancestor of the candidate — rebuild against current main"
fi

# The candidate's own suite, filtered exactly as ore-ci filters it. This is the
# check whose absence made rust-v0.151.0 cost three CI rounds: round one found 31
# entries, round two found two more plus one contended test, round three was
# green. Rounds two and three were both reachable from here.
if [[ "$RUN_TESTS" -eq 1 ]]; then
  say "candidate test suite — the round trip this saves is ~75 minutes"
  if [[ -z "${RUSTY_V8_ARCHIVE:-}" || -z "${RUSTY_V8_SRC_BINDING_PATH:-}" ]]; then
    printf '    \033[33mskip\033[0m tests (RUSTY_V8_ARCHIVE / RUSTY_V8_SRC_BINDING_PATH unset)\n'
    printf '    the workspace cannot link v8 without them; see "Reproducing CI locally".\n'
  elif ! command -v cargo-nextest >/dev/null 2>&1; then
    printf '    \033[33mskip\033[0m tests (cargo-nextest is not installed)\n'
  else
    _merged="$(mktemp)"
    : >"$_merged"
    for _f in "$WT/fork/verify/known-failing-upstream" "$WT/fork/verify/known-failing"; do
      [[ -f "$_f" ]] && cat "$_f" >>"$_merged"
    done
    _args=(--workspace --no-fail-fast)
    _filter="$(grep -v '^[[:space:]]*#' "$_merged" | grep -v '^[[:space:]]*$' \
               | paste -sd'|' - | sed 's/|/ or /g')"
    # An empty file must not become `not ()`, which nextest rejects.
    [[ -n "$_filter" ]] && _args+=(-E "not ($_filter)")
    _log="$(mktemp)"
    ( cd "$WT/codex-rs" && RUST_MIN_STACK=8388608 cargo nextest run "${_args[@]}" ) \
      >"$_log" 2>&1
    _rc=$?
    if [[ "$_rc" -eq 0 ]]; then
      ok "candidate suite green (known-failing excluded)"
    elif [[ "$_rc" -ne 100 ]]; then
      bad "candidate suite did not build (nextest exit $_rc) — nothing was tested"
      tail -20 "$_log" >&2
    else
      # NOT `bad`. This runs on the maintainer's machine, and ore-ci runs Linux:
      # the first real run of this pass reported 12 failures on a candidate whose
      # four CI shards were green, and every one was platform-local -- seatbelt
      # sandbox tests, brew_is_detected_on_macos_prefixes, and exec approval tests
      # that pass on Linux. Failing preflight on those would make it red on every
      # clean sync, and the header of this script already says why that is worse
      # than no preflight. The value here is spotting the entries CI would find
      # hours later, not gating on a list only Linux can judge.
      printf '    \033[33mnote\033[0m candidate suite has failures — compare against ore-ci\n'
      printf '    a test red here and green there is platform-local, not a sync problem.\n'
      # Reuse the extractor rather than grepping: it knows TMT is an outcome and
      # LEAK is a pass, and it reconciles its total against nextest's own summary.
      python3 fork/ci_failures.py --log "$_log" 2>&1 | sed 's/^/    /' >&2
    fi
    printf '    full log: %s\n' "$_log"
    rm -f "$_merged"
  fi
fi

say "reminders that are not automatable here"
cat <<'NOTE'
    - Substitution-sensitive tests must run on the CANDIDATE, not the series.
      Local builds need the prebuilt v8; see "Reproducing CI locally" in CLAUDE.md.
    - Build with --all-targets, never just --lib. A struct field or enum variant
      added upstream breaks TEST code that destructures it long after the library
      compiles: rust-v0.151.0 passed --workspace --lib here and failed clippy and
      all four shards in CI on two match patterns.
    - A CI failure set that changes between runs is contention, not a regression.
      One that repeats is real. Re-run once before triaging.
    - Read that failure set with fork/ci_failures.py --run <id>, not a grep.
      nextest reports a timeout as TMT, not FAIL; grepping one status word
      reported 31 failures for a run that had 33 at rust-v0.151.0. The script
      reconciles its own total against nextest's Summary line and refuses to
      print a number it cannot justify.
    - Before adding a known-failing entry, show the cause: pass at the commit
      before the seam and fail at the tip, or fail on a pristine upstream
      checkout. Resemblance to an existing entry is not evidence -- twice at
      rust-v0.150.1 it pointed at the wrong cause.
NOTE

if [[ "$fail" -ne 0 ]]; then
  printf '\n\033[31mpreflight: not ready to land\033[0m\n'
  exit 1
fi
printf '\n\033[32mpreflight: clean\033[0m\n'
