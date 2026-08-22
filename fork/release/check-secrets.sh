#!/usr/bin/env bash
#
# fork/release/check-secrets.sh — prove the release secret contract before a tag.
#
#   check-secrets.sh [--vault ore-cli]
#
# Every op:// reference in .github/workflows/ore-release.yml is resolved through
# `op read`, and ONLY the exit status is inspected -- no secret value is printed,
# stored, or passed to another process.
#
# This exists because the failure it prevents is slow and expensive: a wrong item
# name is invisible until a release is 90 minutes into its matrix and the signing
# job cannot load a certificate. The vault item for the Homebrew tap really is
# named `ore-cli-github-homebrew-tap`, not `github-homebrew-tap`; that one
# character of drift is exactly the class of thing this catches in five seconds.
#
# Requires `op` signed in to the account owning the vault. CI does not run this:
# there the same references are resolved by 1password/load-secrets-action inside
# the `release` environment.

set -euo pipefail

FORK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "$FORK_DIR/.." && pwd)"
# shellcheck source=../lib.sh
. "$FORK_DIR/lib.sh"

WORKFLOW="$REPO_ROOT/.github/workflows/ore-release.yml"
[[ -f "$WORKFLOW" ]] || die "cannot find $WORKFLOW"
command -v op >/dev/null || die "the 1Password CLI (op) is required"

mapfile -t REFS < <(grep -oE 'op://[A-Za-z0-9._-]+/[A-Za-z0-9._-]+/[A-Za-z0-9._-]+' "$WORKFLOW" | sort -u)
[[ "${#REFS[@]}" -gt 0 ]] || die "no op:// references found in $(basename "$WORKFLOW")"

info "resolving ${#REFS[@]} secret reference(s) from $(basename "$WORKFLOW")"
missing=0
for ref in "${REFS[@]}"; do
  if op read "$ref" >/dev/null 2>&1; then
    printf '  \033[32mok\033[0m    %s\n' "$ref"
  else
    printf '  \033[31mMISS\033[0m  %s\n' "$ref"
    missing=$((missing + 1))
  fi
done

if [[ "$missing" -ne 0 ]]; then
  die "$missing reference(s) do not resolve — the release would fail in the signing job"
fi
info "every release secret resolves"
