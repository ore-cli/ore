#!/usr/bin/env bash
#
# fork/ci/file-sync-blocked.sh — file (or decline to duplicate) a sync-blocked issue.
#
#   file-sync-blocked.sh <title> <body-file>
#
# Extracted from ore-sync.yml's assembly handler so that BOTH failure paths file
# issues identically. They did not before: the handler was gated on
# `steps.assemble.outputs.rc != '0'`, so a sync whose assembly SUCCEEDED and
# whose later verify step failed filed nothing at all -- no issue, therefore no
# ore-sync-blocked-triage, and the watchdog reads the sync run's conclusion but
# never tests it. rust-v0.153.3 failed that way and three layers stayed quiet.
#
# Dedupe is keyed on the TAG PAIR in the title, not on "any open sync-blocked
# issue". The old rule meant one stale issue silenced every later failure,
# including a different sync failing for a different reason -- the same
# swallow-the-second-signal shape this script exists to fix. Same pair, same
# issue: a nightly that keeps failing the same way still does not spam.
set -euo pipefail

title="${1:?usage: file-sync-blocked.sh <title> <body-file>}"
body_file="${2:?usage: file-sync-blocked.sh <title> <body-file>}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set}"

# The pair is everything between "sync blocked: " and " (" — `rust-v0.153.2 ->
# rust-v0.153.3`. Falls back to the whole title so a retitled caller still
# dedupes against itself rather than against everything.
pair="$(printf '%s' "$title" | sed -n 's/^sync blocked: \(.*\) (.*/\1/p')"
[ -n "$pair" ] || pair="$title"

gh label create sync-blocked --repo "$repo" \
  --description "a sync stopped and cannot land until it is resolved" \
  --color B60205 2>/dev/null || true

found="$(gh issue list --repo "$repo" --state open --label sync-blocked \
  --json number,title --jq "[.[] | select(.title | contains(\"$pair\"))] | .[0] // empty")"
existing="$(printf '%s' "$found" | jq -r '.number // empty' 2>/dev/null || true)"
if [ -n "$existing" ]; then
  # Same pair, but not necessarily the same failure. rust-v0.154.0 filed as
  # "rebase conflicts", the conflict agent then resolved all four, and the next
  # run failed on compile errors instead -- with the dedupe silently swallowing
  # that, the issue kept describing a cause that no longer existed. Suppressing
  # a duplicate is right; suppressing NEW INFORMATION is how an issue becomes a
  # stale diagnosis someone acts on.
  prev_title="$(printf '%s' "$found" | jq -r '.title // empty' 2>/dev/null || true)"
  if [ "$prev_title" != "$title" ]; then
    {
      echo "The failure for \`$pair\` changed."
      echo
      echo "- was: $prev_title"
      echo "- now: $title"
      echo
      [ -n "${RUN_URL:-}" ] && { echo "Run: $RUN_URL"; echo; }
      echo "<details><summary>latest failure detail</summary>"
      echo
      cat "$body_file"
      echo
      echo "</details>"
    } >"${TMPDIR:-/tmp}/sync-blocked-update.$$"
    gh issue comment "$existing" --repo "$repo" \
      --body-file "${TMPDIR:-/tmp}/sync-blocked-update.$$" >/dev/null 2>&1 \
      && echo "the failure changed; commented the new cause on #$existing"
    gh issue edit "$existing" --repo "$repo" --title "$title" >/dev/null 2>&1 \
      && echo "retitled #$existing to the current failure"
    rm -f "${TMPDIR:-/tmp}/sync-blocked-update.$$"
  else
    echo "an open sync-blocked issue for $pair already exists (#$existing), same failure; not filing a duplicate"
  fi
  echo "issue=$existing"
  return 0 2>/dev/null || exit 0
fi

url="$(gh issue create --repo "$repo" --label sync-blocked \
  --title "$title" --body-file "$body_file")"
num="${url##*/}"

# The label is what the watchdog alarms on, and --label on create silently
# produced an unlabelled issue the first time this ran -- so the issue existed
# and the alarm still did not. Apply it again and confirm, because an unlabelled
# sync-blocked issue is exactly as invisible as no issue at all.
if [ -z "$(gh issue view "$num" --repo "$repo" --json labels \
     --jq '[.labels[].name] | map(select(. == "sync-blocked")) | .[]')" ]; then
  if gh issue edit "$num" --repo "$repo" --add-label sync-blocked 2>&1; then
    echo "::warning::sync-blocked did not stick on create; applied it to #$num afterwards"
  else
    echo "::error::could not label #$num sync-blocked. The ore-sync App needs Issues: Read and write (App settings -> Permissions, then approve the update on the installation). Until then the watchdog cannot see this issue and ore-sync-blocked-triage will not start; label it by hand to start triage."
  fi
fi
echo "filed #$num"
echo "issue=$num"
