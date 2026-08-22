# ore on Windows

ore ships native Windows binaries for x64 and ARM64. They are **not code-signed**: the project has no code-signing certificate yet, so Windows will warn about them. This page describes what you will see, why, and how to check a download properly instead of clicking through the warning.

## Install

```powershell
irm https://github.com/ore-cli/ore/releases/latest/download/install.ps1 | iex
```

The installer downloads the release package for your architecture, verifies its SHA-256 against both the digest in the GitHub release metadata and the `codex-package_SHA256SUMS` manifest, and refuses to install on a mismatch. It unpacks under your ore home, points a directory junction at the unpacked release so an upgrade retargets the junction rather than replacing a live directory, and puts `ore.exe` on your user PATH through `%LOCALAPPDATA%\Programs\ore\bin`.

Set these before running it to change what it does:

| Variable                | Effect                                                 |
| ----------------------- | ------------------------------------------------------ |
| `CODEX_RELEASE`         | install an exact version instead of the latest release |
| `CODEX_INSTALL_DIR`     | use a different directory for the PATH entry           |
| `CODEX_NON_INTERACTIVE` | never prompt; answer no to every question              |

The variable names are upstream's and are kept deliberately, so a script written against the upstream installer keeps working.

## What Windows will show you

**SmartScreen.** Downloading and running an unsigned executable produces "Windows protected your PC", with **More info → Run anyway** as the way through. Unsigned binaries carry no publisher name and no SmartScreen reputation, and reputation is per-file, so every new release starts from zero and prompts again.

**An "Unknown publisher" UAC prompt.** The first time ore runs a sandboxed command it has to provision the Windows sandbox. `codex-windows-sandbox-setup.exe` ships an `asInvoker` manifest, so it never elevates on its own; ore launches it with the `runas` verb at that one moment and Windows asks for consent. Because the helper is unsigned the dialog is the yellow variant with `Publisher: Unknown`. This is the only place ore asks for administrator rights. Declining is safe — the sandbox setup fails, and with it the command that needed the sandbox.

**Mark of the Web.** A release archive saved by a browser is tagged as downloaded from the internet, which some tools refuse to run from. `Unblock-File` clears the tag.

**Antivirus heuristics.** Unsigned, statically linked Rust binaries occasionally trip heuristic detection. A checksum match (below) settles whether the file is the one ore published; it does not settle whether your scanner will keep complaining.

## Verify the download instead

A signature would let Windows answer "did ore really publish this?" on your behalf. Without one, the check that carries real weight is the checksum, and the release publishes two manifests for it:

```powershell
Get-FileHash -Algorithm SHA256 .\ore-x86_64-pc-windows-msvc.exe
```

`SHA256SUMS` covers every asset on the release; `codex-package_SHA256SUMS` covers the package archives specifically and is the one the installer itself checks. Compare the hash against the matching line and stop if they differ.

ore also cosigns its **Linux** binaries keylessly against Sigstore and publishes a `.sigstore` bundle beside each. There is no equivalent for the Windows assets — cosign runs only on the Linux build rows — so there is nothing to `cosign verify-blob` here.

## WinGet is deferred

There is no WinGet manifest and none is planned until ore has a code-signing identity. A package in the WinGet catalogue reads as vetted, and shipping an unsigned installer there would deliver the warnings above to people who had every reason to expect none. The release workflow already has a signing step behind the `ORE_WINDOWS_SIGNING` repository variable, but no signing backend is wired up to it yet. Install with the PowerShell one-liner or from npm in the meantime.

## Known limits

- Everything above follows from being unsigned, and stays true until an identity is in place.
- ARM64 binaries are cross-compiled on x64 runners. The release smoke test runs the x64 entrypoint only, so ARM64 is built but not exercised in CI.
- If the warnings are a blocker, the Linux build under WSL2 is signed against Sigstore and installs with `install.sh`.

## Uninstall

Remove `%LOCALAPPDATA%\Programs\ore\bin` from your user PATH, delete `%LOCALAPPDATA%\Programs\ore`, and delete `packages\standalone` under your ore home to reclaim the downloaded releases.
