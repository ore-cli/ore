#!/usr/bin/env bash
#
# fork/first-push.sh — the one-time sequence that puts ore on GitHub safely.
#
#   first-push.sh --check          # preconditions only, changes nothing (default)
#   first-push.sh --step <n>       # run exactly one step, after --check passes
#   first-push.sh --print          # print the sequence and exit
#
# WHY THIS IS A SCRIPT AND NOT A CHECKLIST.  The order is load-bearing and one
# mistake is expensive rather than annoying: `delta`'s tree carries upstream's
# live workflows, so a push with Actions enabled runs openai/codex's CI from our
# tag tree, and `rust-release-zsh.yml` / `rusty-v8-release.yml` would publish real
# releases under upstream's names.  Encoding the order removes the chance of
# remembering it wrong at 1am.
#
# ORDER, and where it departs from the plan's checklist item 7.  The plan installs
# the tag ruleset while the repo is still private.  On this org that is impossible:
# ore-cli is on the free plan, where rulesets return 403 "Upgrade to GitHub Pro or
# make this repository public".  So the public flip has to come first -- which is
# safe only because the repo is still EMPTY at that point, and because Actions are
# turned off in step 1 before anything else happens.
#
#   1  disable Actions                     (works while private; closes the window)
#   2  make the repository public          (empty repo; unblocks rulesets)
#   3  install the tag ruleset             (blocks creation of upstream-name tags)
#   4  push delta and the assembled main   (through safe_push only)
#   5  set the default branch to main
#   6  disable every non-allowlisted workflow (ref-independent guard)
#   7  re-enable Actions
#
# Steps 1-3 and 5-7 are reversible.  Step 2 is not: making a repository public
# cannot be fully undone, because anything fetched while public stays fetched.
# Step 4 is the first time ore's code leaves this machine.
#
# NEVER pushes tags.  Release tags are cut by a human afterwards, on main.

set -euo pipefail

FORK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$FORK_DIR/lib.sh"

SLUG="${ORE_REPO_SLUG:-ore-cli/ore}"
MODE=check
STEP=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --check) MODE=check; shift ;;
    --step)  MODE=step; STEP="${2:?--step needs a number}"; shift 2 ;;
    --print) sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -h|--help) sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument '$1'" ;;
  esac
done

command -v gh >/dev/null || die "gh CLI is required"
gh auth status >/dev/null 2>&1 || die "gh is not authenticated"

# ---------------------------------------------------------------- preconditions

preconditions() {
  local bad=0

  info "checking preconditions for $SLUG"

  # The series must be clean before it becomes public history.
  "$FORK_DIR/lint-series.sh" >/dev/null 2>&1 \
    || { warn "series lint fails — fix it before publishing"; bad=1; }

  python3 "$FORK_DIR/verify/run.py" --suite static >/dev/null 2>&1 \
    || { warn "static invariant suite fails"; bad=1; }

  # An origin that already has refs means this is not a first push, and the
  # sequence below would be operating on assumptions that no longer hold.
  #
  # Only meaningful up to and including the push itself: steps 5-7 run precisely
  # BECAUSE refs now exist, so asserting emptiness there would make the second
  # half of the sequence unreachable.
  # `git ls-remote` wants a URL, not owner/repo, and pipefail would turn its
  # failure into an exit-128 with no explanation.
  if [[ "$MODE" == "check" || "$STEP" -le 4 ]]; then
    local url refs
    url=$(git remote get-url origin 2>/dev/null || echo "https://github.com/$SLUG.git")
    if refs=$(git ls-remote "$url" 2>/dev/null | wc -l | tr -d ' '); then
      [[ "$refs" == "0" ]] \
        || { warn "origin already has $refs ref(s) — this is not a first push"; bad=1; }
    else
      warn "could not reach $url — cannot confirm origin is empty"; bad=1
    fi
  fi

  # Refuse outright if an upstream-name tag exists anywhere locally that a
  # careless `git push --tags` could carry. safe_push blocks it, but a human
  # bypassing safe_push is exactly the failure this guards.
  local badtags
  badtags=$(git for-each-ref --format='%(refname:short)' \
              'refs/tags/rust-v*' 'refs/tags/codex-zsh-v*' \
              'refs/tags/rusty-v8-v*' 'refs/tags/python-v*' 2>/dev/null | wc -l | tr -d ' ')
  if [[ "$badtags" != "0" ]]; then
    warn "$badtags upstream-name tag(s) exist as LOCAL tags."
    warn "  They must never reach origin. Never run 'git push --tags'."
    warn "  fork/assemble.sh fetches into refs/upstream/tags/* precisely to avoid this."
  fi

  git rev-parse --verify --quiet refs/heads/delta >/dev/null \
    || { warn "no local 'delta' branch"; bad=1; }

  git rev-parse --verify --quiet refs/heads/main >/dev/null \
    || warn "no local 'main' — assemble one first: fork/assemble.sh --tag <tag>"

  if [[ "$bad" == "0" ]]; then info "preconditions ok"; else die "preconditions failed"; fi
}

# ---------------------------------------------------------------------- steps

step_1_disable_actions() {
  info "1/7 disabling Actions on $SLUG"
  gh api -X PUT "/repos/$SLUG/actions/permissions" -F enabled=false
  info "    Actions disabled — pushing delta can no longer fire upstream workflows"
}

step_2_make_public() {
  warn "2/7 making $SLUG PUBLIC — this cannot be fully undone"
  gh api -X PATCH "/repos/$SLUG" -f visibility=public
  info "    public; rulesets are now available on the free plan"
}

step_3_tag_ruleset() {
  info "3/7 installing the upstream-tag ruleset"
  # Empty bypass_actors is deliberate and was ratified: ore's own sidecar tags are
  # ore-prefixed, so nothing legitimate needs an exemption here.
  gh api -X POST "/repos/$SLUG/rulesets" --input - <<'JSON'
{
  "name": "block upstream-name tags",
  "target": "tag",
  "enforcement": "active",
  "bypass_actors": [],
  "conditions": {
    "ref_name": {
      "include": [
        "refs/tags/rust-v*",
        "refs/tags/codex-zsh-v*",
        "refs/tags/rusty-v8-v*",
        "refs/tags/python-v*",
        "refs/tags/artifact-runtime-v*",
        "refs/tags/codex-rs-*"
      ],
      "exclude": []
    }
  },
  "rules": [{ "type": "creation" }]
}
JSON
  info "    upstream-name tags can no longer be created on origin"
}

step_4_push() {
  info "4/7 pushing delta and main through safe_push"
  safe_push refs/heads/delta
  git rev-parse --verify --quiet refs/heads/main >/dev/null && safe_push refs/heads/main
  info "    pushed; no tags were pushed and none ever should be by hand"
}

step_5_default_branch() {
  info "5/7 setting the default branch to main"
  gh api -X PATCH "/repos/$SLUG" -f default_branch=main
}

step_6_disable_upstream_workflows() {
  info "6/7 disabling every workflow not in fork/workflows.allow"
  local allow
  allow=$(grep -vE '^\s*(#|$)' "$FORK_DIR/workflows.allow")
  gh api "/repos/$SLUG/actions/workflows" --paginate \
    --jq '.workflows[] | [.id, .path, (.path|split("/")|last), .state] | @tsv' \
  | while IFS=$'\t' read -r id path name state; do
      # dynamic/dependabot/* is synthesised by GitHub, is not a file, and the
      # disable endpoint answers 422 for it. Dependabot is governed by whether
      # .github/dependabot.yaml exists, which assemble relocates away.
      if [[ "$path" == dynamic/* ]]; then
        info "    skip    $path (synthesised by GitHub; not a workflow file)"
      elif grep -qxF "$name" <<<"$allow"; then
        info "    keep    $name"
      elif [[ "$state" == "disabled_manually" ]]; then
        info "    already $name"
      else
        info "    disable $name"
        gh api -X PUT "/repos/$SLUG/actions/workflows/$id/disable"
      fi
    done
}

step_7_reenable_actions() {
  info "7/7 re-enabling Actions with only the fork's workflows live"
  gh api -X PUT "/repos/$SLUG/actions/permissions" -F enabled=true
  info "    verifying the enabled set matches the allowlist"
  python3 "$FORK_DIR/verify/workflows_check.py" --api
}

# ------------------------------------------------------------------- dispatch

preconditions
[[ "$MODE" == "check" ]] && { info "check only; re-run with --step <1..7> to act"; exit 0; }

case "$STEP" in
  1) step_1_disable_actions ;;
  2) step_2_make_public ;;
  3) step_3_tag_ruleset ;;
  4) step_4_push ;;
  5) step_5_default_branch ;;
  6) step_6_disable_upstream_workflows ;;
  7) step_7_reenable_actions ;;
  *) die "step must be 1..7" ;;
esac
