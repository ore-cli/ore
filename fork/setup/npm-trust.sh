#!/usr/bin/env bash
#
# fork/setup/npm-trust.sh — configure npm trusted publishing for every ore package.
#
#   fork/setup/npm-trust.sh            # configure all seven
#   fork/setup/npm-trust.sh --list     # show what is configured today
#
# Run this yourself: it cannot be automated, and that is the control working.
# npm requires 2FA for changing who may publish a package and is actively
# restricting tokens that bypass it (E403, "npm tokens that bypass 2FA are being
# restricted"). A granular token that could silently re-point a package's trusted
# publisher would be worth more to an attacker than a publish token.
#
# So this uses YOUR login session, not a token. `npm login` defaults to
# auth-type=web, which opens a browser and accepts a passkey; the resulting
# session carries the 2FA assertion that `npm trust` demands. No credential is
# read from 1Password here: ORE-CLI-BOOTSTRAP cannot perform this operation at
# all. It is still needed for the FIRST publish of each package, though -- see
# the ordering below -- and only becomes redundant once trust is in place.
#
# ORDER MATTERS: the package must exist first. `npm trust github` POSTs to
# /-/package/<pkg>/trust, which 404s for a package the registry has never seen.
# A --dry-run does NOT catch this: it never contacts the registry, so it reports
# success for a package that does not exist. The sequence is
#
#   1. a release run with ORE_NPM_STAGE=true builds the seven tarballs
#   2. publish them once by hand, with ORE-CLI-BOOTSTRAP, --access public
#   3. this script, handing publishing to CI's OIDC
#   4. revoke the token: from here on nothing reads one
set -euo pipefail

REPO="ore-cli/ore"
WORKFLOW="ore-release.yml"
ENVIRONMENT="release"
REGISTRY="https://registry.npmjs.org"
EXPECT_SCOPE="@ore-cli"

PACKAGES=(
  "@ore-cli/ore"
  "@ore-cli/ore-darwin-arm64"
  "@ore-cli/ore-darwin-x64"
  "@ore-cli/ore-linux-arm64"
  "@ore-cli/ore-linux-x64"
  "@ore-cli/ore-win32-arm64"
  "@ore-cli/ore-win32-x64"
)

command -v npm >/dev/null || { echo "npm is required" >&2; exit 1; }

# `npm trust` landed in 11.5.1.
ver="$(npm --version)"
case "$ver" in
  1[1-9].*|[2-9][0-9].*) ;;
  *) echo "npm $ver is too old for \`npm trust\` (needs >= 11.5.1)" >&2; exit 1 ;;
esac

# Run from a directory with no project .npmrc: this repo's sets pnpm-only keys
# that npm warns about on every call and that have nothing to do with auth.
cd "$(mktemp -d)"

if ! who="$(npm whoami --registry "$REGISTRY" 2>/dev/null)"; then
  echo "Not logged in. Running \`npm login\` -- a browser will open; use your passkey."
  npm login --registry "$REGISTRY"
  who="$(npm whoami --registry "$REGISTRY")"
fi
printf 'logged in as %s\n' "$who"

if [[ "${1:-}" == "--list" ]]; then
  for pkg in "${PACKAGES[@]}"; do
    printf '\n== %s\n' "$pkg"
    if ! curl -fsS -o /dev/null "https://registry.npmjs.org/${pkg/\//%2f}" 2>/dev/null; then
      printf '  not published\n'
      continue
    fi
    npm trust list "$pkg" --registry "$REGISTRY" 2>&1 | grep -viE '^npm warn' || true
  done
  exit 0
fi

printf '\nConfiguring trusted publishing for %d package(s):\n' "${#PACKAGES[@]}"
printf '  repository  %s\n  workflow    %s\n  environment %s\n\n' "$REPO" "$WORKFLOW" "$ENVIRONMENT"

failed=()
unpublished=()
for pkg in "${PACKAGES[@]}"; do
  [[ "$pkg" == "$EXPECT_SCOPE/"* ]] || { echo "refusing out-of-scope package $pkg" >&2; exit 1; }
  # Fail fast and legibly. Without this the run yields seven identical E404s
  # whose cause -- "this package has never been published" -- appears nowhere in
  # the message npm prints.
  if ! curl -fsS -o /dev/null "https://registry.npmjs.org/${pkg/\//%2f}" 2>/dev/null; then
    printf '\n== %s\n  SKIPPED: not on the registry; trust is configured ON a package\n' "$pkg"
    unpublished+=("$pkg")
    continue
  fi
  printf '\n== %s\n' "$pkg"
  # No redirection. npm answers a 2FA challenge interactively -- it either
  # prompts for a code or opens a browser for a WebAuthn assertion -- and it can
  # only do that against a live terminal. Capturing its output produced a bare
  # EOTP on every package: the prompt had nowhere to go.
  if npm trust github "$pkg" \
       --file "$WORKFLOW" --repo "$REPO" --env "$ENVIRONMENT" \
       --yes --registry "$REGISTRY"; then
    printf '  ok\n'
  else
    printf '  FAILED\n'
    failed+=("$pkg")
  fi
done

if ((${#unpublished[@]})); then
  printf '\n%d package(s) are not published yet:\n  %s\n\n' "${#unpublished[@]}" "${unpublished[*]}" >&2
  printf 'Publish them first, then re-run:\n' >&2
  printf '  1. release with ORE_NPM_STAGE=true to build the tarballs\n' >&2
  printf '  2. npm publish <tarball> --access public   (per package)\n' >&2
  exit 1
fi

if ((${#failed[@]})); then
  printf '\nfailed: %s\n' "${failed[*]}" >&2
  printf 'EOTP means npm wanted a 2FA challenge it could not present. Either the\n' >&2
  printf 'terminal was not interactive, or the account has no factor npm can\n' >&2
  printf 'challenge from the CLI. A passkey alone may not be enough: adding an\n' >&2
  printf 'authenticator app at npmjs.com (Account -> Two-Factor Authentication)\n' >&2
  printf 'gives a code npm accepts via --otp, alongside the passkey.\n' >&2
  exit 1
fi
printf '\nAll %d package(s) configured. Verify: fork/setup/npm-trust.sh --list\n' "${#PACKAGES[@]}"
printf 'ORE-CLI-BOOTSTRAP is now unnecessary -- revoke it on npmjs.com.\n'
