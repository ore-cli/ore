#!/usr/bin/env bash
#
# fork/ci/autoland-gate.sh — may this sync land and tag without a human?
#
#   autoland-gate.sh <pr-number> <candidate-sha>
#
# Prints one `reason=` line per rejection and exits 1, or exits 0 having printed
# `boring=yes`. Every gate is a REASON TO STOP, never a reason to proceed: a
# check that cannot be evaluated fails closed.
#
# The gate answers a narrow question -- "did anything here need judgement?" --
# and nothing else. It does not judge the upstream diff; ore-ci does that. What
# it refuses is a candidate whose SERIES changed, because every series change so
# far has been a decision someone had to defend: a known-failing entry, a
# substitution rule, a regenerated fence reference. Those are exactly the
# changes that go silently green when they are wrong.
#
# `review` is deliberately NOT consulted. It reported success on 2026-09-05
# while SKIPPING (a PR touching the workflow fails the action's own validation,
# and a skip is a success), and it has reported failure since 2026-09-03 for
# reasons unrelated to any candidate. A signal that is green when it did nothing
# is worse than no signal in a gate that merges code.
set -euo pipefail

pr="${1:?usage: autoland-gate.sh <pr-number> <candidate-sha>}"
sha="${2:?usage: autoland-gate.sh <pr-number> <candidate-sha>}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set}"
rc=0
reject() { echo "reason=$1"; rc=1; }

# 1. The required check, by SHA. `gh pr checks` reports the previous head after
#    a force-push, which is how a stale green nearly got read as current.
required="$(gh api "repos/$repo/commits/$sha/check-runs?per_page=100" \
  --jq '[.check_runs[] | select(.name == "required")] | .[0] | "\(.status)/\(.conclusion // "pending")"' 2>/dev/null || echo "")"
[ "$required" = "completed/success" ] \
  || reject "the 'required' check on $sha is '${required:-absent}', not completed/success"

# 2/3. What assemble reported about the rebase, as written into the PR body.
body="$(gh pr view "$pr" --repo "$repo" --json body --jq .body 2>/dev/null || echo "")"
[ -n "$body" ] || reject "could not read PR #$pr's body"
case "$body" in
  *"## DROPPED"*)
    reject "commits went empty during the rebase (upstream absorbed fork work) -- each dropped slug's Invariant needs a human verdict" ;;
esac
case "$body" in
  *"agent-resolved"*)
    reject "conflicts were resolved by an agent -- a human confirms those before they ship" ;;
esac

# 4. The judgement surface. These files encode decisions, not mechanics; a
#    candidate that changes one is a candidate someone argued for.
#
#    This script is on its own list on purpose. A candidate that widens what may
#    auto-land must not be able to auto-land through the widened gate: the change
#    gets reviewed under the OLD rules, which is the only ordering that lets a
#    human see what is being given away.
judgement="
fork/verify/known-failing
fork/verify/known-failing-upstream
fork/verify/allowed-fence.diff
fork/verify/strings.toml
fork/verify/allowed-signers
fork/substitutions.yaml
fork/egress.yaml
fork/seams.yaml
fork/workflows.allow
fork/ci/autoland-gate.sh
"
changed="$(git diff --name-only refs/remotes/origin/main.."$sha" 2>/dev/null || echo "GITFAIL")"
[ "$changed" != "GITFAIL" ] || reject "could not diff origin/main..$sha"
while read -r f; do
  [ -n "$f" ] || continue
  if printf '%s\n' "$changed" | grep -qxF "$f"; then
    reject "$f changed -- that file records a decision and the decision needs a reviewer"
  fi
done <<<"$judgement"

# 5. The version must have moved. assemble derives both halves now; a candidate
#    carrying main's version would publish a second tree under a live number,
#    which is the collision that cost three releases before it was automated.
old_v="$(git show refs/remotes/origin/main:fork/VERSION 2>/dev/null | tr -d '[:space:]' || echo "")"
new_v="$(git show "$sha":fork/VERSION 2>/dev/null | tr -d '[:space:]' || echo "")"
[ -n "$old_v" ] && [ -n "$new_v" ] || reject "could not read fork/VERSION on both sides"
[ "$old_v" != "$new_v" ] || reject "fork/VERSION is still $old_v -- assemble did not derive a new version"

if [ "$rc" -eq 0 ]; then
  echo "boring=yes"
  echo "version=$new_v"
else
  echo "boring=no"
fi
exit "$rc"
