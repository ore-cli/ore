#!/usr/bin/env bash

# Installs rcodesign for fork/release/macos-signing/*.sh. Upstream's AKV
# signing flow downloads its rcodesign build from a private az:// blob
# (.github/actions/setup-akv-pkcs11-codesigning/action.yaml) that a fork has
# no credentials to reach; the same project publishes identical binaries on
# GitHub Releases (indygreg/apple-platform-rs, tag apple-codesign/0.29.0).
#
# The tarball is verified against the sha256 LITERALS pinned below — not the
# .tar.gz.sha256 sidecar published in the same release, which is what the
# legacy fork checked. The sidecar travels over the exact channel it is meant
# to guard: whoever can substitute the tarball (a compromised release, a
# poisoned download path) can substitute the sidecar in the same motion, so a
# sidecar check catches transfer corruption but never substitution. A digest
# committed here ties the artifact to a reviewed git commit instead.

set -euo pipefail

# Digests below are per-version; bumping this requires re-pinning both.
RCODESIGN_VERSION="0.29.0"

# ---------------------------------------------------------------------------
# PLACEHOLDER DIGESTS — fill in before first use. From any networked machine:
#
#   v=0.29.0
#   for arch in aarch64 x86_64; do
#     curl -fsSL "https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign%2F${v}/apple-codesign-${v}-${arch}-apple-darwin.tar.gz" \
#       | shasum -a 256
#   done
#
# Cross-check each value against the release's .sha256 sidecar AND at least
# one independent record (a second machine on a different network, or a
# package-manager lockfile that pins the same release) before committing:
# whatever is pasted here becomes the root of trust. Lowercase hex only —
# the placeholder guard below accepts nothing else.
# ---------------------------------------------------------------------------
SHA256_AARCH64="FILL-ME-sha256-of-apple-codesign-0.29.0-aarch64-apple-darwin.tar.gz"
SHA256_X86_64="FILL-ME-sha256-of-apple-codesign-0.29.0-x86_64-apple-darwin.tar.gz"

usage() {
  cat >&2 <<'USAGE'
Usage: install-rcodesign.sh [--dest DIR]

Downloads rcodesign (indygreg/apple-platform-rs) for the current macOS
architecture, verifies the tarball against the sha256 literal pinned in this
script, and installs the rcodesign binary into DIR (default: /usr/local/bin).

Requires no secrets. Fails if the pinned digest is still a placeholder.
USAGE
}

dest="/usr/local/bin"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dest)
      dest="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [[ -z "$dest" ]]; then
  echo "--dest requires a directory." >&2
  exit 2
fi

# Only the <arch>-apple-darwin assets are pinned: the notarize scripts run on
# the macOS signing runners, nowhere else.
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "install-rcodesign.sh pins only <arch>-apple-darwin builds; run it on macOS." >&2
  exit 1
fi

case "$(uname -m)" in
  arm64)
    arch="aarch64"
    expected_sha256="$SHA256_AARCH64"
    ;;
  x86_64)
    arch="x86_64"
    expected_sha256="$SHA256_X86_64"
    ;;
  *)
    echo "Unsupported macOS architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

if [[ ! "$expected_sha256" =~ ^[0-9a-f]{64}$ ]]; then
  {
    echo "The pinned sha256 for ${arch}-apple-darwin is not a valid digest:"
    echo "  ${expected_sha256}"
    echo "Fill SHA256_AARCH64 and SHA256_X86_64 in ${BASH_SOURCE[0]} with the"
    echo "real lowercase digests (fill instructions are in the comment above"
    echo "them) before running this script."
  } >&2
  exit 1
fi

# The release tag is "apple-codesign/0.29.0"; the slash must stay %2F-encoded
# in the download URL.
name="apple-codesign-${RCODESIGN_VERSION}-${arch}-apple-darwin"
url="https://github.com/indygreg/apple-platform-rs/releases/download/apple-codesign%2F${RCODESIGN_VERSION}/${name}.tar.gz"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir" >/dev/null' EXIT

tarball="$tmp_dir/${name}.tar.gz"
curl -fsSL --retry 3 --retry-delay 2 "$url" -o "$tarball"

actual_sha256="$(shasum -a 256 "$tarball" | awk '{ print $1 }')"
if [[ "$actual_sha256" != "$expected_sha256" ]]; then
  {
    echo "sha256 mismatch for ${name}.tar.gz:"
    echo "  expected ${expected_sha256}"
    echo "  actual   ${actual_sha256}"
    echo "Refusing to install."
  } >&2
  exit 1
fi

tar -xzf "$tarball" -C "$tmp_dir"
if [[ ! -f "$tmp_dir/${name}/rcodesign" ]]; then
  echo "${name}.tar.gz did not contain ${name}/rcodesign." >&2
  exit 1
fi

mkdir -p "$dest"
install -m 0755 "$tmp_dir/${name}/rcodesign" "$dest/rcodesign"
"$dest/rcodesign" --version
