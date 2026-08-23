//! Provider definition for Anthropic's native Messages API.
//!
//! This file sits beside the other provider definitions in
//! `model-provider-info/src/`, but is compiled into `codex-model-provider`
//! (mounted by `model-provider/src/anthropic/mod.rs`): this crate's `lib.rs`
//! is inside the auth fence and carries no fork edits beyond the `WireApi`
//! variants, so it cannot grow a `mod anthropic;` line.

use std::collections::HashMap;

use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;

const ANTHROPIC_PROVIDER_NAME: &str = "Anthropic";
pub const ANTHROPIC_PROVIDER_ID: &str = "anthropic";
pub const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const ANTHROPIC_API_KEY_ENV_VAR: &str = "ANTHROPIC_API_KEY";
/// The companion of `ANTHROPIC_API_KEY`, and the name Anthropic's own SDKs
/// use. Note the value means what `base_url` means in `config.toml`: the
/// prefix `messages` is appended to, so it carries the version segment
/// (`https://host/v1`). The SDKs take a bare host and add `/v1` themselves.
pub const ANTHROPIC_BASE_URL_ENV_VAR: &str = "ANTHROPIC_BASE_URL";
/// Anthropic accepts API keys only: subscription (OAuth) sign-in is reserved
/// for Anthropic's own clients and is enforced server-side.
pub(crate) const ANTHROPIC_API_KEY_INSTRUCTIONS: &str = "Create an API key in the Anthropic \
Console and export it as ANTHROPIC_API_KEY. Anthropic subscription sign-in is not supported; \
only API keys are.";
/// The Messages API rejects a request that omits this header.
pub(crate) const ANTHROPIC_VERSION_HEADER: &str = "anthropic-version";
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Provider for `api.anthropic.com`, or for a proxy that fronts it.
pub fn create_anthropic_provider(base_url: Option<String>) -> ModelProviderInfo {
    create_anthropic_provider_from(base_url, |var| std::env::var(var).ok())
}

/// Resolves the base URL from an argument, then `ANTHROPIC_BASE_URL`, then the
/// default. The env lookup is injected so the precedence is testable without
/// mutating process environment, which is shared across a test binary's threads.
fn create_anthropic_provider_from(
    base_url: Option<String>,
    env: impl Fn(&str) -> Option<String>,
) -> ModelProviderInfo {
    let base_url = base_url
        .or_else(|| {
            env(ANTHROPIC_BASE_URL_ENV_VAR)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_string());

    ModelProviderInfo {
        name: ANTHROPIC_PROVIDER_NAME.into(),
        base_url: Some(base_url),
        // Deliberately unset: a declared `env_key` routes the key onto
        // `Authorization: Bearer`, and the Messages API only reads `x-api-key`
        // — attached by the provider's `api_auth()` override instead.
        env_key: None,
        env_key_instructions: Some(ANTHROPIC_API_KEY_INSTRUCTIONS.to_string()),
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Anthropic,
        query_params: None,
        http_headers: Some(HashMap::from([(
            ANTHROPIC_VERSION_HEADER.to_string(),
            ANTHROPIC_VERSION.to_string(),
        )])),
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_provider_speaks_the_anthropic_wire_and_skips_openai_auth() {
        let provider = create_anthropic_provider(/*base_url*/ None);

        assert_eq!(provider.wire_api, WireApi::Anthropic);
        assert!(!provider.requires_openai_auth);
        assert_eq!(
            provider.base_url.as_deref(),
            Some(ANTHROPIC_DEFAULT_BASE_URL)
        );
        assert_eq!(
            provider.env_key, None,
            "a declared env_key would route the key onto Authorization: Bearer"
        );
        assert_eq!(
            provider
                .http_headers
                .as_ref()
                .and_then(|headers| headers.get(ANTHROPIC_VERSION_HEADER))
                .map(String::as_str),
            Some(ANTHROPIC_VERSION),
        );
    }

    #[test]
    fn the_env_var_sets_the_base_url_when_no_argument_is_given() {
        let provider = create_anthropic_provider_from(/*base_url*/ None, |var| {
            (var == ANTHROPIC_BASE_URL_ENV_VAR).then(|| "https://gw.internal/anthropic/v1".into())
        });

        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://gw.internal/anthropic/v1"),
            "ANTHROPIC_BASE_URL is the companion of ANTHROPIC_API_KEY; a key you can set from \
             the environment and a URL you cannot is most of the way to useless for a gateway"
        );
    }

    #[test]
    fn an_explicit_base_url_outranks_the_env_var() {
        let provider =
            create_anthropic_provider_from(Some("https://explicit.example/v1".to_string()), |_| {
                Some("https://from-env.example/v1".to_string())
            });

        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://explicit.example/v1")
        );
    }

    #[test]
    fn a_blank_env_var_falls_back_to_the_default() {
        for blank in ["", "   ", "\t\n"] {
            let provider = create_anthropic_provider_from(None, |_| Some(blank.to_string()));
            assert_eq!(
                provider.base_url.as_deref(),
                Some(ANTHROPIC_DEFAULT_BASE_URL),
                "an exported-but-empty {ANTHROPIC_BASE_URL_ENV_VAR} must not blank the endpoint"
            );
        }
    }

    #[test]
    fn the_env_var_is_trimmed() {
        let provider =
            create_anthropic_provider_from(None, |_| Some("  https://gw.internal/v1  ".into()));

        assert_eq!(provider.base_url.as_deref(), Some("https://gw.internal/v1"));
    }

    #[test]
    fn a_configured_base_url_replaces_the_default() {
        let provider = create_anthropic_provider(Some("https://proxy.example.com/v1".to_string()));

        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://proxy.example.com/v1")
        );
    }
}
