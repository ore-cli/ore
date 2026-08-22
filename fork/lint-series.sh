#!/usr/bin/env bash
#
# fork/lint-series.sh — trailer lint for the delta series.
#
#   lint-series.sh [--base <ref>] [--head <ref>]
#
# Every commit between the base tag and the series head must carry the intent
# header the sync machinery depends on: `Fork-Patch:` slugs give commits a
# stable identity across rebases (the apply-log's dropped-slug diff is how an
# upstream-absorbed commit is noticed), `Invariant:`/`Verify:` are what a
# reviewer consults when a rebase drops or shrinks a commit, and
# `Conflict-notes:` (optional) is what the conflict agent reads at a stop.
#
# Forbidden paths exist because the generated passes own them: a series commit
# touching Cargo.lock would guarantee rebase conflicts (upstream's lock churns
# constantly) and break the "lock is always regenerated from upstream's"
# policy; fork/upstream-workflows/ exists only on generated main; schema
# fixtures are generated output — committing them alone means the generator
# inputs and outputs have diverged (together with a .rs change they are fine:
# that keeps delta CI green between syncs).
#
# Defaults: --head HEAD; --base = the tag recorded in fork/UPSTREAM at --head,
# resolved via refs/upstream/tags/* first, local tags second.

set -euo pipefail

FORK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$FORK_DIR/lib.sh"

BASE="" HEAD_REF="HEAD"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --base) BASE="${2:?--base needs a value}"; shift 2 ;;
    --head) HEAD_REF="${2:?--head needs a value}"; shift 2 ;;
    -h|--help) sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument '$1'" ;;
  esac
done

git rev-parse --verify --quiet "$HEAD_REF" >/dev/null || die "head ref '$HEAD_REF' does not exist"

if [[ -z "$BASE" ]]; then
  tag=$(git show "$HEAD_REF:fork/UPSTREAM" 2>/dev/null | sed -n 's/^tag = "\(.*\)"$/\1/p')
  [[ -n "$tag" ]] || die "cannot read the base tag from $HEAD_REF:fork/UPSTREAM; pass --base"
  if BASE=$(resolve_upstream_tag "$tag"); then
    BASE_COMMIT=$(peel_tag "$BASE")
  else
    # No tag ref -- the normal state in CI, because upstream-name tags are never
    # pushed to origin. fork/UPSTREAM records the peeled commit for exactly this,
    # and it is reachable: the series is built on top of it.
    BASE_COMMIT=$(git show "$HEAD_REF:fork/UPSTREAM" 2>/dev/null | sed -n 's/^commit = "\(.*\)"$/\1/p')
    [[ -n "$BASE_COMMIT" ]] \
      || die "base tag '$tag' is not fetched and fork/UPSTREAM records no commit; pass --base"
    git cat-file -e "${BASE_COMMIT}^{commit}" 2>/dev/null \
      || die "base commit $BASE_COMMIT (from fork/UPSTREAM) is not in this repository; fetch more history"
    BASE="$BASE_COMMIT"
  fi
else
  BASE_COMMIT=$(peel_tag "$BASE")
fi

git merge-base --is-ancestor "$BASE_COMMIT" "$HEAD_REF" \
  || die "base $BASE is not an ancestor of $HEAD_REF"

# Paths the series must never touch; schema fixtures are conditional (see below).
FORBIDDEN_ALWAYS='^(codex-rs/Cargo\.lock|MODULE\.bazel\.lock|pnpm-lock\.yaml|fork/upstream-workflows/)'
SCHEMA_FIXTURES='^(codex-rs/core/config\.schema\.json|codex-rs/app-server-protocol/schema/|codex-rs/hooks/schema/)'
SLUG_RE='^[a-z0-9]+(-[a-z0-9]+)*$'
MAX_UPSTREAM_LINES=400

errors=0 warnings=0 count=0
declare -a seen_slugs=()
err() { printf '\033[31mFAIL\033[0m %s: %s\n' "$1" "$2"; errors=$((errors + 1)); }

commits=$(git rev-list --reverse --first-parent "$BASE_COMMIT..$HEAD_REF")
if [[ -z "$commits" ]]; then
  info "lint-series: empty series ($BASE..$HEAD_REF) — nothing to lint"
  exit 0
fi

for c in $commits; do
  count=$((count + 1))
  short=$(git log -1 --format='%h %s' "$c")

  # --parse normalizes trailers to `Key: value` one per line (folded
  # continuations), which makes the presence checks plain greps.
  trailers=$(git log -1 --format=%B "$c" | git interpret-trailers --parse)

  for key in Fork-Patch Purpose Invariant Verify; do
    value=$(sed -n "s/^$key:[[:space:]]*//p" <<<"$trailers" | head -1)
    if [[ -z "$value" ]]; then
      err "$short" "missing or empty trailer '$key:'"
    fi
  done

  slug=$(sed -n 's/^Fork-Patch:[[:space:]]*//p' <<<"$trailers" | head -1)
  if [[ -n "$slug" ]]; then
    [[ "$slug" =~ $SLUG_RE ]] || err "$short" "Fork-Patch slug '$slug' is not kebab-case"
    for s in ${seen_slugs[@]+"${seen_slugs[@]}"}; do
      [[ "$s" == "$slug" ]] && err "$short" "Fork-Patch slug '$slug' is not unique in the series"
    done
    seen_slugs+=("$slug")
  fi

  files=$(git diff-tree --no-commit-id --name-only -r "$c")

  bad=$(grep -E "$FORBIDDEN_ALWAYS" <<<"$files" || true)
  [[ -n "$bad" ]] && err "$short" "touches forbidden path(s): $(tr '\n' ' ' <<<"$bad")"

  fixtures=$(grep -E "$SCHEMA_FIXTURES" <<<"$files" || true)
  if [[ -n "$fixtures" ]]; then
    # Generator inputs are the Rust types the schemas are derived from.
    if ! grep -qE '^codex-rs/.*\.rs$' <<<"$files"; then
      err "$short" "changes schema fixtures without their generator inputs: $(tr '\n' ' ' <<<"$fixtures")"
    fi
  fi

  # Size warning: modifications to upstream-owned files should stay small
  # (fork-owned fork/** files and newly added files are exempt — additive is
  # the whole point).
  new_files=$(git diff-tree --no-commit-id --name-status -r "$c" | awk -F'\t' '$1 == "A" { print $2 }')
  changed=$(git diff-tree --no-commit-id --numstat -r "$c" \
    | awk -F'\t' '
        NR == FNR      { added[$0] = 1; next }
        $3 ~ /^fork\// { next }
        ($3 in added)  { next }
        $1 != "-"      { total += $1 + $2 }
        END { print total + 0 }' <(printf '%s\n' "$new_files") -)
  if [[ "$changed" -gt "$MAX_UPSTREAM_LINES" ]]; then
    warn "$short: $changed changed lines in upstream files (> $MAX_UPSTREAM_LINES; consider splitting)"
    warnings=$((warnings + 1))
  fi
done

if [[ "$errors" -gt 0 ]]; then
  printf '\033[31mlint-series: %d error(s)\033[0m in %d commit(s)\n' "$errors" "$count" >&2
  exit 1
fi
info "lint-series: $count commit(s) clean ($warnings warning(s))"
