# Model providers

Ore speaks four wire APIs. Select a provider with `model_provider` and define it
in a `[model_providers.<id>]` table in `~/.ore/config.toml`. The `wire_api` field
picks the protocol, and defaults to `"responses"` when a table leaves it out.

## `wire_api` values

| `wire_api`    | Protocol                       | Endpoint                                               | Typical servers                                                                      |
| ------------- | ------------------------------ | ------------------------------------------------------ | ------------------------------------------------------------------------------------ |
| `"responses"` | OpenAI Responses API           | `POST <base_url>/responses`                            | OpenAI (the default), Azure OpenAI, the built-in `ollama` and `lmstudio`             |
| `"chat"`      | OpenAI Chat Completions        | `POST <base_url>/chat/completions`                     | llama.cpp, vLLM, LM Studio, older Ollama, OpenRouter, most OpenAI-compatible proxies |
| `"anthropic"` | Anthropic Messages API         | `POST <base_url>/messages`                             | Anthropic, or a gateway that fronts it                                               |
| `"gemini"`    | Google generative-language API | `POST <base_url>/models/{model}:streamGenerateContent` | Google Gemini, or a gateway that fronts it                                           |

`anthropic` and `gemini` are built in. `model_provider = "anthropic"` with
`ANTHROPIC_API_KEY` exported works with no `[model_providers.*]` table at all,
and so does `gemini` with `GEMINI_API_KEY`. No built-in provider speaks `chat`;
nothing speaks it unless your config defines a provider that does.

## Claiming a built-in id

`anthropic` and `gemini` stand aside when your config claims their id, so a table
under either name replaces the built-in definition. `amazon-bedrock` and
`amazon-bedrock-runtime` accept overrides of `base_url`, `auth`, `http_headers`
and the `aws` settings. Under any other built-in id (`openai`, `ollama`,
`lmstudio`) the built-in definition wins and your table is discarded without an
error.

The built-in `ollama` and `lmstudio` providers speak `"responses"`. Reaching
either server over Chat Completions takes a provider under an id of your own,
such as `[model_providers.ollama-local]`. `ollama-chat` was removed and now
errors.

## Model discovery

At session start each provider is asked `GET <base_url>/models`. That list
decides which models the picker offers; the bundled catalog supplies the context
window, reasoning support and the other facts known about each id. An id absent
from the bundled catalog is still selectable and inherits the provider's default
model as its template. When the request fails, times out, or answers with
something other than a model list, the picker falls back to the bundled catalog
alone.

Discovery runs on all four wire APIs. It is skipped for the first-party OpenAI
path, for the Amazon Bedrock providers, and whenever `model_catalog_json` pins a
catalog in config.

## Chat Completions example

```toml
model = "llama3.3-70b"
model_provider = "local-llama"

[model_providers.local-llama]
name = "Local llama.cpp"
base_url = "http://localhost:8080/v1"
wire_api = "chat"
# Optional: sent as `Authorization: Bearer <value of the variable>`.
env_key = "LLAMA_API_KEY"
```

On the `chat`, `anthropic` and `gemini` wire APIs a namespaced tool is advertised
under a single flattened name joined with `__`, so an MCP tool reads as
`mcp__server__tool` in transcripts and in provider-side logs.

For Claude models served through an OpenAI-compatible gateway, Ore places
Anthropic-style `cache_control` prompt markers automatically. If the gateway
rejects them, the turn is retried once without markers.

## Anthropic

Create a key in the [Anthropic Console](https://console.anthropic.com) and export
it:

```shell
export ANTHROPIC_API_KEY="sk-ant-..."
```

```toml
model = "claude-opus-5"
model_provider = "anthropic"
```

Requests go to `https://api.anthropic.com/v1`, carrying the key in an `x-api-key`
header alongside the `anthropic-version: 2023-06-01` header the Messages API
requires. Sign-in with an Anthropic subscription (Pro/Max) is not supported; the
provider authenticates with an API key, and usage bills to your API account.

`ANTHROPIC_BASE_URL` points the provider at a gateway:

```shell
export ANTHROPIC_BASE_URL="https://gateway.example.com/anthropic/v1"
```

A value naming only a host gains `/v1`, so `https://gateway.example.com` resolves
the same way `https://gateway.example.com/v1` does. A value carrying a path is
used as written.

A `[model_providers.anthropic]` table does the same job and outranks the
variable, which Ore reads only while no table claims the id:

```toml
[model_providers.anthropic]
name = "Anthropic"
wire_api = "anthropic"
base_url = "https://gateway.example.com/anthropic/v1"
env_key = "GATEWAY_TOKEN"
```

`env_key` names the variable the key is read from. Its value still travels in
`x-api-key`.

## Gemini

Create a key in [Google AI Studio](https://aistudio.google.com) and export it:

```shell
export GEMINI_API_KEY="..."
```

```toml
model = "gemini-2.5-pro"
model_provider = "gemini"
```

Requests go to `https://generativelanguage.googleapis.com/v1beta`, carrying the
key in an `x-goog-api-key` header, which keeps it out of request URLs and proxy
access logs. Vertex AI service-account credentials are not supported; the
provider authenticates with an AI Studio API key.

`GEMINI_BASE_URL` redirects the provider the way `ANTHROPIC_BASE_URL` does, with
`/v1beta` as the segment a bare host gains:

```shell
export GEMINI_BASE_URL="https://gateway.example.com/gemini/v1beta"
```

A `[model_providers.gemini]` table outranks the variable in the same way, and
must set `wire_api = "gemini"`.
