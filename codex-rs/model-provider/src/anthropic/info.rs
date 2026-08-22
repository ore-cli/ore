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
    ModelProviderInfo {
        name: ANTHROPIC_PROVIDER_NAME.into(),
        base_url: Some(base_url.unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_string())),
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
    fn a_configured_base_url_replaces_the_default() {
        let provider = create_anthropic_provider(Some("https://proxy.example.com/v1".to_string()));

        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://proxy.example.com/v1")
        );
    }
}
