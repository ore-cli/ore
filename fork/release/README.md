# `fork/release/` — release scripts upstream deleted or never had

Nothing here runs on its own: the fork release workflow (a later series commit) calls these.
They live under `fork/` — not `.github/scripts/` — so upstream churn in its own script
directory can never conflict with them.

> **Unproven at 0.149 coordinates.** This exact rcodesign signing/notarization path shipped
> the legacy fork's ore-v0.146.2–0.146.5, but nothing has exercised it against the 0.149 tree
> (zsh-manifest SHA pin, `CODEX_REPO_ROOT`-requiring packaging, AKV-era signing scripts
> alongside — all new since 0.146). Treat the macOS path as unproven until the first
> `ore-v1.149.0-alpha.1` shakedown release runs it end to end.

## Inventory

### `macos-signing/notarize_macos_binary_with_rcodesign.sh`, `macos-signing/notarize_macos_dmg_with_rcodesign.sh`

Vendored byte-identical (below the provenance header) from upstream
`.github/scripts/macos-signing/` at `rust-v0.146.0`, the last tag to carry them. Upstream
replaced them in `0c07c7ee47` "Use Azure Key Vault for macOS notarization (#37154)"
(2026-08-05) with an AKV flow that signs through a PKCS#11 provider and downloads its
rcodesign build from a private `az://` blob
(`.github/actions/setup-akv-pkcs11-codesigning/action.yaml`) — a fork has no credentials for
either. The rcodesign path is fully public, so ore keeps it.

The binary variant submits the binary in a ZIP (standalone binaries cannot carry a stapled
ticket) and retains the notarization log; the DMG variant notarizes with `--staple`. Both
need `rcodesign` on PATH and the `APPLE_NOTARIZATION_*` environment below. Signing itself
still uses upstream's `.github/scripts/macos-signing/sign_macos_code.sh`, whose native
`codesign` backend survived the AKV change — only notarization needed vendoring.

Sync checklist: if upstream reintroduces public notarize scripts, prefer theirs and delete
these.

### `install-rcodesign.sh`

Installs rcodesign 0.29.0 from the public GitHub release (`indygreg/apple-platform-rs`, tag
`apple-codesign/0.29.0`, asset `apple-codesign-0.29.0-<arch>-apple-darwin.tar.gz`) and
verifies the tarball against a sha256 **literal pinned in the script**. The legacy fork
verified against the `.tar.gz.sha256` sidecar fetched from the same release — a weak pin,
since anything able to substitute the tarball can substitute the sidecar in the same motion.
The two digests are committed as placeholders (the script refuses to run while they are);
fill instructions sit directly above them in the script.

### `render_homebrew_formula.sh`

Adapted from the legacy fork's `.github/scripts/render-homebrew-formula.sh`; upstream has
never had Homebrew automation in-repo (its `codex` cask lives in Homebrew/homebrew-cask).
Renders `Formula/ore.rb` (class `Ore`) for the `ore-cli/homebrew-tap` tap
(`brew install ore-cli/tap/ore`) from a published `ore-v<version>` release:

- Reads the four `codex-package-<target>.tar.gz` digests from the release's `SHA256SUMS`
  and hard-fails if any is missing — never ship 3 of 4 platforms. ore releases publish both
  `SHA256SUMS` (fork-added, covers every asset — what this reads) and
  `codex-package_SHA256SUMS` (upstream's format, consumed by `scripts/install/install.sh`).
- The formula installs the whole codex-package bundle into `libexec` (the binary resolves
  its rg/zsh siblings relative to itself), symlinks `bin.install_symlink libexec/"bin/ore"`,
  and generates shell completions from the installed executable. Its `test` block asserts
  `ore --version` reports the formula version — which holds because line 1 of `--version`
  is exactly `ore <fork/VERSION>`.

## Secret / env contract

The release workflow loads secrets with `1password/load-secrets-action`
(`export-env: false`) from the `ore-cli` vault. The item and field names below were
carried over from the predecessor fork's vault and verified against the live one
with `fork/release/check-secrets.sh`:

| 1Password ref | Environment variable | Consumed by |
|---|---|---|
| `op://ore-cli/apple-developer-id/p12_base64` | `MACOS_CERTIFICATE_P12` | keychain-import step (Developer ID Application cert, base64 `.p12`) |
| `op://ore-cli/apple-developer-id/password` | `MACOS_CERTIFICATE_PASSWORD` | keychain-import step |
| `op://ore-cli/apple-developer-id/identity` | `MACOS_SIGNING_IDENTITY` | `.github/scripts/macos-signing/sign_macos_code.sh --identity` |
| `op://ore-cli/apple-notarization/p8_base64` | `APPLE_NOTARIZATION_KEY_P8` | both notarize scripts (base64 App Store Connect `.p8` key) |
| `op://ore-cli/apple-notarization/key_id` | `APPLE_NOTARIZATION_KEY_ID` | both notarize scripts |
| `op://ore-cli/apple-notarization/issuer_id` | `APPLE_NOTARIZATION_ISSUER_ID` | both notarize scripts |
| `op://ore-cli/ore-cli-github-homebrew-tap/credential` | `HOMEBREW_TAP_TOKEN` | checkout + push of `ore-cli/homebrew-tap` |

Every script here answers `--help` (and validates its arguments) without any secret set;
missing credentials fail loudly at run time, never silently skip.
