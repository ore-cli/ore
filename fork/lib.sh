#!/usr/bin/env bash
# fork/lib.sh — shared helpers for the fork tooling. Source, don't execute.
#
# Everything here is defensive plumbing for one fact: upstream's workflows ride
# along in the tree and GitHub runs the workflow file AT THE PUSHED REF, so a
# single upstream-named tag reaching origin executes upstream-authored CI that
# no edit on main/delta can neutralize (rust-release.yml has no repository
# guard; rust-release-zsh.yml can publish a REAL GitHub Release with only
# GITHUB_TOKEN). Hence four layers, of which safe_push() is the second:
#
#   1. fetch namespace  — upstream tags are fetched into refs/upstream/tags/*,
#                         never as local tags, so `git push --tags` has nothing
#                         upstream-shaped to push
#   2. safe_push()      — the single choke point below; nothing outside the
#                         allowlist regex ever reaches `git push`
#   3. verify + repo settings — fork/verify's workflow check plus repo-level
#                         `gh workflow disable` (ref-independent: delta's tree
#                         still carries live upstream workflow files)
#   4. server tag ruleset — restrict creation of rust-v*, codex-zsh-v*,
#                         rusty-v8-v*, python-v*, artifact-runtime-v*,
#                         codex-rs-* with an EMPTY bypass list
#
# Only ore-v*, ore-zsh-v*, ore-rusty-v8-v* are ever pushed — all match the
# refs/tags/ore- prefix in ORE_PUSH_ALLOW.

# shellcheck disable=SC2034  # consumed by sourcing scripts
ORE_STABLE_TAG_RE='^rust-v[0-9]+\.[0-9]+\.[0-9]+$'
ORE_PUSH_ALLOW='^refs/heads/(main|delta|sync/.+|sync-delta/.+|rerere-cache)$|^refs/tags/ore-'

die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
info() { printf '\033[36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }

# A dirty tree turns a rebase or a generated pass into a mess. Refuse early.
# Untracked files are tolerated (scratch notes, editor droppings) — every
# generated pass runs in a fresh worktree anyway.
require_clean_tree() {
  local root="${1:-.}"
  [[ -z "$(git -C "$root" status --porcelain --untracked-files=no)" ]] \
    || die "working tree at '$root' has uncommitted changes; commit or stash them first"
}

# Upstream rust-v* tags are annotated tag OBJECTS (git cat-file -t rust-v0.149.0
# -> "tag"), so `git rev-parse <tag>` yields the tag object sha, not the commit.
# Every plumbing use (commit-tree parents, merge-base, rebase --onto) needs the
# peeled commit; route them all through here.
peel_tag() {
  git rev-parse --verify --quiet "$1^{commit}" \
    || die "cannot peel '$1' to a commit (unknown ref?)"
}

# Resolve an upstream tag name to a local ref: prefer the non-tag fetch
# namespace (layer 1 above); fall back to a local tag ref for clones that
# fetched upstream tags before the fork tooling existed. Prints the REF, not
# the sha — callers peel with peel_tag.
resolve_upstream_tag() {
  local tag="$1"
  if git rev-parse --verify --quiet "refs/upstream/tags/$tag" >/dev/null; then
    printf 'refs/upstream/tags/%s\n' "$tag"
  elif git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null; then
    printf 'refs/tags/%s\n' "$tag"
  else
    return 1
  fi
}

# safe_push [--force-with-lease[=..]] [--delete] [--dry-run] [--atomic] <refspec>...
#
# Always pushes to origin. Every destination ref must be FULLY QUALIFIED
# (refs/heads/... or refs/tags/...) and match ORE_PUSH_ALLOW. Flags are
# deny-by-default: only the four above pass, so --tags/--mirror/--all/
# --follow-tags/--prune can never sneak a tag out, and '+refspec' force pushes
# are refused in favor of the lease-guarded flag form.
safe_push() {
  local args=("$@") spec dst nspecs=0
  for spec in "${args[@]}"; do
    case "$spec" in
      --force-with-lease|--force-with-lease=*|--delete|--dry-run|--atomic)
        continue ;;
      --*|-*)
        die "safe_push: refusing flag '$spec' (allowed: --force-with-lease, --delete, --dry-run, --atomic)" ;;
    esac
    [[ "$spec" == +* ]] && die "safe_push: refusing forced refspec '$spec'; use --force-with-lease"
    dst="${spec##*:}"
    [[ "$dst" == refs/* ]] \
      || die "safe_push: destination '$dst' is not fully qualified (use refs/heads/... or refs/tags/...)"
    [[ "$dst" =~ $ORE_PUSH_ALLOW ]] \
      || die "safe_push: REFUSED '$dst' — only main/delta/sync/*/sync-delta/*/rerere-cache branches and ore-* tags are ever pushed"
    nspecs=$((nspecs + 1))
  done
  # A flags-only call would degrade to `git push origin` and push the current
  # branch un-vetted — require at least one validated refspec.
  [[ "$nspecs" -gt 0 ]] || die "safe_push: no refspec given"
  git push origin "${args[@]}"
}
