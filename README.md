<div align="center">
<img src="https://raw.githubusercontent.com/ore-cli/ore/main/.github/ore-splash.gif" width="870" alt="ore" />
<h1>ore</h1>
<p><em><strong>ore</strong>, n. (Mining) A native metal or its compound with the rock in<br />
which it occurs, after it has been picked over to throw out what is worthless.</em></p>

<p><sub>— Webster's Revised Unabridged Dictionary, 1913</sub></p>
<p>
<a href="https://github.com/ore-cli/ore/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/ore-cli/ore?label=release&color=c9a227"></a>
<a href="https://discord.gg/awN2xANFMW"><img alt="Discord" src="https://img.shields.io/badge/discord-join-5865F2?logo=discord&logoColor=white"></a>
<a href="https://github.com/ore-cli/ore/blob/main/LICENSE"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blue"></a>
</p>
</div>

A coding agent for the terminal. Ore is a fork of [OpenAI Codex](https://github.com/openai/codex) with no telemetry, and it talks to Anthropic, Gemini and OpenAI-compatible Chat Completions endpoints directly.

## Install

Run the following on macOS or Linux to install Ore:

```shell
curl -fsSL https://github.com/ore-cli/ore/releases/latest/download/install.sh | sh
```

Run the following on Windows to install Ore:

```shell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/ore-cli/ore/releases/latest/download/install.ps1 | iex"
```

Ore can also be installed via the following package managers:

```shell
# Install using npm
npm install -g @ore-cli/ore
```

```shell
# Install using Homebrew
brew install ore-cli/tap/ore
```

macOS, Linux and Windows, on ARM64 and x86_64.

The shell installer puts `ore` in `~/.local/bin`, the PowerShell installer in `%LOCALAPPDATA%\Programs\Ore\bin`. Set `CODEX_INSTALL_DIR` to override either. Windows binaries are not signed yet, so SmartScreen warns on first run.

Then run `ore`. To check the install:

```console
$ ore --version
ore 1.149.1
codex-base: rust-v0.149.1 (ff29a44391)
```

## Providers

`openai`, `anthropic`, `gemini`, `ollama`, `lmstudio`, `amazon-bedrock` and `amazon-bedrock-runtime` are built in. Set one in `~/.ore/config.toml` and export its key:

```toml
model_provider = "anthropic"
```

Anthropic reads `ANTHROPIC_API_KEY`, Gemini `GEMINI_API_KEY`. `ANTHROPIC_BASE_URL` and `GEMINI_BASE_URL` point either at a gateway, unless a table under that id replaces the built-in. `ORE_MODEL_PROVIDER` and `ORE_MODEL` select from the environment when no config layer has set them.

For anything else, add a provider table. `wire_api` picks the protocol — `"responses"`, `"chat"`, `"anthropic"` or `"gemini"`. Most local servers and OpenAI-compatible proxies want `"chat"`:

```toml
model_provider = "local"

[model_providers.local]
base_url = "http://localhost:8080/v1"
wire_api = "chat"
```

The built-in `ollama` and `lmstudio` providers speak `"responses"`, and a table under an existing id is ignored. To reach either over Chat Completions, define a provider under a different id.

## Configuration

Config lives in `~/.ore`. `ORE_HOME` moves it. An existing `~/.codex/config.toml` is merged in underneath, with `~/.ore` winning where the two disagree; setting `CODEX_HOME` instead pins that directory as the only home and reads no layer beneath it. A project-local `.codex/` directory is still read under that name.

## Sign in

Run `ore` and select **Sign in with ChatGPT** to use your Plus, Pro, Business or Enterprise plan, or sign in with an API key. The browser page that completes sign-in says Codex.

## What changed from Codex

| Area        | Codex                                | Ore                                            |
| ----------- | ------------------------------------ | ---------------------------------------------- |
| Telemetry   | metrics, analytics, feedback reports | none                                           |
| Config home | `~/.codex`                           | `~/.ore`                                       |
| Wire APIs   | Responses                            | Responses, Chat Completions, Anthropic, Gemini |

## Build from source

```bash
git clone https://github.com/ore-cli/ore.git
cd ore/codex-rs
cargo build --release --bin codex
```

The binary is `target/release/codex`; rename it to `ore`. `--bin codex` builds the agent alone — drop the flag to build `codex-code-mode-host` as well. Packaged installs also bundle ripgrep, and a source build falls back to `rg` on your `PATH`.

## Docs

[Providers](https://github.com/ore-cli/ore/blob/main/docs/providers.md) is Ore's. The rest of `docs/` is inherited from Codex and still describes it. [Getting started](https://github.com/ore-cli/ore/blob/main/docs/getting-started.md) · [Configuration](https://github.com/ore-cli/ore/blob/main/docs/config.md) · [Providers](https://github.com/ore-cli/ore/blob/main/docs/providers.md) · [Authentication](https://github.com/ore-cli/ore/blob/main/docs/authentication.md) · [Sandbox](https://github.com/ore-cli/ore/blob/main/docs/sandbox.md) · [Installing & building](https://github.com/ore-cli/ore/blob/main/docs/install.md) · [Contributing](https://github.com/ore-cli/ore/blob/main/docs/contributing.md) · [Security](https://github.com/ore-cli/ore/blob/main/SECURITY.md) · [How the fork is built](https://github.com/ore-cli/ore/blob/main/fork/README.md)

## License

[Apache-2.0](https://github.com/ore-cli/ore/blob/main/LICENSE). Ore is a fork of [OpenAI Codex](https://github.com/openai/codex), Copyright 2025 OpenAI. Files throughout are modified from the originals; see [NOTICE](https://github.com/ore-cli/ore/blob/main/NOTICE). Ore is not affiliated with, endorsed by, or sponsored by OpenAI.
