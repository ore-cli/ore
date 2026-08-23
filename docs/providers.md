# Model providers

ore can talk to model providers over three wire protocols. Upstream Ore
removed everything except the OpenAI Responses API; ore restores the Chat
Completions adapter and adds a native Anthropic Messages adapter so you can
bring your own key for the provider you actually use.

A provider is selected with `model_provider` and defined in a
`[model_providers.<id>]` table in `config.toml`. The `wire_api` field picks the
protocol.

## `wire_api` values

| `wire_api`    | Protocol                | Endpoint                           | Typical servers                                                                |
| ------------- | ----------------------- | ---------------------------------- | ------------------------------------------------------------------------------ |
| `"responses"` | OpenAI Responses API    | `POST <base_url>/responses`        | OpenAI (the default), Azure OpenAI                                             |
| `"chat"`      | OpenAI Chat Completions | `POST <base_url>/chat/completions` | llama.cpp, vLLM, LM Studio, Ollama, OpenRouter, most OpenAI-compatible proxies |
| `"anthropic"` | Anthropic Messages API  | `POST <base_url>/messages`         | Anthropic, or a gateway that fronts it                                         |

There is no built-in provider for the restored wires: nothing speaks `chat` or
`anthropic` unless your config defines a provider that does.

### Chat Completions example

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

Namespaced tool names are flattened onto this wire (joined with `__`) and
mapped back when the model calls them. For Claude models served through an
OpenAI-compatible gateway, ore places Anthropic-style `cache_control` prompt
markers automatically; if the gateway rejects them, the request is retried once
without markers and the turn proceeds uncached.

## Anthropic

```toml
model = "claude-opus-5"
model_provider = "anthropic"

[model_providers.anthropic]
name = "Anthropic"
wire_api = "anthropic"
# base_url is optional and defaults to https://api.anthropic.com/v1
```

Export your key before starting ore:

```shell
export ANTHROPIC_API_KEY="sk-ant-..."
```

The key is created in the [Anthropic Console](https://console.anthropic.com)
and is sent as the `x-api-key` header, alongside the required
`anthropic-version: 2023-06-01` header. Setting `env_key` on the provider
switches which environment variable is read; the value is still sent as
`x-api-key`, never as a bearer token. A model catalog is bundled, so no
`/models` endpoint is contacted.

### API keys only — subscription sign-in is not supported

Anthropic prohibits using Claude subscription (Pro/Max) OAuth credentials in
third-party tools, and enforces this server-side. This is why ore supports
**API keys only** for Anthropic, with no OAuth or "sign in with Claude" path: a
fork that wired one up would be violating Anthropic's terms of service, and the
server-side enforcement means it would simply stop working. Usage is billed to
your API account, not to a Claude subscription.

## Egress

Each wire API talks to exactly one host per request, and credentials never
cross providers:

- `wire_api = "responses"`: only the provider's `base_url` host. For the
  built-in OpenAI provider that is `chatgpt.com` (ChatGPT sign-in, path
  `/backend-api/codex`) or `api.openai.com` (API key).
- `wire_api = "chat"`: only the `base_url` host your provider table names.
  ore ships no default host for this wire.
- `wire_api = "anthropic"`: `api.anthropic.com`, unless your provider table
  sets `base_url` to a gateway of your choosing.

OpenAI/ChatGPT credentials are attached only to requests on the `responses`
wire; the Anthropic adapter sends `x-api-key` and never an `Authorization`
header. This is covered by tests that fail if an ambient ChatGPT credential
ever appears on a third-party request.
