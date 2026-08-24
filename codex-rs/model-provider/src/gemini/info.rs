//! Provider definition for Google's native Gemini API (`generativelanguage`).
//!
//! This file sits beside the other provider definitions in
//! `model-provider-info/src/`, but is compiled into `codex-model-provider`
//! (mounted by `model-provider/src/gemini/mod.rs`): that crate's `lib.rs` is
//! inside the auth fence and carries no fork edits beyond the `WireApi`
//! variants, so it cannot grow a `mod gemini;` line.

use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;

const GEMINI_PROVIDER_NAME: &str = "Google Gemini";
pub const GEMINI_PROVIDER_ID: &str = "gemini";
/// The generative-language endpoint, version segment included: `base_url` is a
/// prefix that `models/{model}:streamGenerateContent` is appended to.
pub const GEMINI_DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const GEMINI_API_KEY_ENV_VAR: &str = "GEMINI_API_KEY";
/// The companion of `GEMINI_API_KEY`. Like `base_url` in `config.toml` this is
/// the prefix `models/...` is appended to, so it carries the version segment
/// (`https://host/v1beta`). Google's own SDKs take a bare host and add the
/// version themselves; [`with_version_segment`] accepts both spellings.
pub const GEMINI_BASE_URL_ENV_VAR: &str = "GEMINI_BASE_URL";
/// The version segment a bare host is missing. `v1beta` rather than `v1`
/// because the thinking, cached-content, and structured-output fields this
/// adapter sends are only served under `v1beta`.
const GEMINI_API_VERSION_SEGMENT: &str = "v1beta";
/// Gemini accepts API keys only. Vertex AI's service-account (OAuth) flow is a
/// different host, a different auth scheme, and a different request shape, so
/// it is not reachable through this provider.
pub(crate) const GEMINI_API_KEY_INSTRUCTIONS: &str = "Create an API key in Google AI Studio and \
export it as GEMINI_API_KEY. Vertex AI service-account credentials are not supported; only AI \
Studio API keys are.";

/// Appends `/v1beta` to a base URL that names only a host.
///
/// ore's `base_url` is a prefix that `models/{model}:...` is appended to, so it
/// must carry the version segment. Google's own SDKs, `google-genai`, and every
/// gateway runbook written for them take a BARE HOST and append the version
/// themselves. Reusing their variable name with the opposite meaning would make
/// `GEMINI_BASE_URL=https://generativelanguage.googleapis.com` -- correct
/// everywhere else -- POST to `/models/...` and 404. This is the same trap
/// `ANTHROPIC_BASE_URL` fell into, fixed the same way.
///
/// Accepting both is better than picking a side: a value with a path is left
/// exactly as written, so a gateway mounted at `/gemini/v1beta` still works, and
/// a bare host gains the segment the SDKs would have added.
fn with_version_segment(base_url: String) -> String {
    // Split BEFORE trimming: trimming the whole string is a no-op when a query
    // follows, so `https://gw/?key=1` kept its slash, `after_scheme` contained
    // one, and the URL was misread as already having a path -- leaving it with no
    // version segment at all and every turn 404ing.
    let (head, suffix) = match base_url.find(['?', '#']) {
        Some(at) => (&base_url[..at], &base_url[at..]),
        None => (base_url.as_str(), ""),
    };
    let trimmed = head.trim_end_matches('/');
    // A query string is not a path; strip it before looking, or
    // `https://gw?key=1` would read as though it had one.
    let after_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);

    if after_scheme.contains('/') {
        format!("{trimmed}{suffix}")
    } else {
        format!("{trimmed}/{GEMINI_API_VERSION_SEGMENT}{suffix}")
    }
}

/// Provider for `generativelanguage.googleapis.com`, or for a proxy that fronts it.
pub fn create_gemini_provider(base_url: Option<String>) -> ModelProviderInfo {
    create_gemini_provider_from(base_url, |var| std::env::var(var).ok())
}

/// Resolves the base URL from an argument, then `GEMINI_BASE_URL`, then the
/// default. The env lookup is injected so the precedence is testable without
/// mutating process environment, which is shared across a test binary's threads.
fn create_gemini_provider_from(
    base_url: Option<String>,
    env: impl Fn(&str) -> Option<String>,
) -> ModelProviderInfo {
    let base_url = base_url
        .or_else(|| {
            env(GEMINI_BASE_URL_ENV_VAR)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .map(with_version_segment)
        .unwrap_or_else(|| GEMINI_DEFAULT_BASE_URL.to_string());

    ModelProviderInfo {
        name: GEMINI_PROVIDER_NAME.into(),
        base_url: Some(base_url),
        // Deliberately unset: a declared `env_key` routes the key onto
        // `Authorization: Bearer`, and the generative-language API reads
        // `x-goog-api-key` — attached by the provider's `api_auth()` override
        // instead. A bearer token here is not merely ignored; it is forwarded to
        // whatever proxy sits in front, which is a credential leak for nothing.
        env_key: None,
        env_key_instructions: Some(GEMINI_API_KEY_INSTRUCTIONS.to_string()),
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Gemini,
        // The key rides in a header, never in `?key=`: a query parameter lands in
        // proxy access logs and in `RUST_LOG` request traces.
        query_params: None,
        // Gemini has no required version header; the version lives in the path.
        http_headers: None,
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
    fn the_provider_speaks_the_gemini_wire_and_skips_openai_auth() {
        let provider = create_gemini_provider(/*base_url*/ None);

        assert_eq!(provider.wire_api, WireApi::Gemini);
        assert!(!provider.requires_openai_auth);
        assert_eq!(provider.base_url.as_deref(), Some(GEMINI_DEFAULT_BASE_URL));
        assert_eq!(
            provider.env_key, None,
            "a declared env_key would route the key onto Authorization: Bearer, which Gemini \
             ignores and a fronting proxy would log"
        );
        assert_eq!(
            provider.query_params, None,
            "the key must not be spelled ?key=; query strings reach access logs"
        );
        assert_eq!(
            provider.env_key_instructions.as_deref(),
            Some(GEMINI_API_KEY_INSTRUCTIONS)
        );
    }

    #[test]
    fn the_env_var_sets_the_base_url_when_no_argument_is_given() {
        let provider = create_gemini_provider_from(/*base_url*/ None, |var| {
            (var == GEMINI_BASE_URL_ENV_VAR).then(|| "https://gw.internal/gemini/v1beta".into())
        });

        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://gw.internal/gemini/v1beta"),
            "GEMINI_BASE_URL is the companion of GEMINI_API_KEY; a key you can set from the \
             environment and a URL you cannot is most of the way to useless for a gateway"
        );
    }

    #[test]
    fn an_explicit_base_url_outranks_the_env_var() {
        let provider = create_gemini_provider_from(
            Some("https://explicit.example/v1beta".to_string()),
            |_| Some("https://from-env.example/v1beta".to_string()),
        );

        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://explicit.example/v1beta")
        );
    }

    #[test]
    fn a_blank_env_var_falls_back_to_the_default() {
        for blank in ["", "   ", "\t\n"] {
            let provider = create_gemini_provider_from(None, |_| Some(blank.to_string()));
            assert_eq!(
                provider.base_url.as_deref(),
                Some(GEMINI_DEFAULT_BASE_URL),
                "an exported-but-empty {GEMINI_BASE_URL_ENV_VAR} must not blank the endpoint"
            );
        }
    }

    #[test]
    fn the_env_var_is_trimmed() {
        let provider =
            create_gemini_provider_from(None, |_| Some("  https://gw.internal/v1beta  ".into()));

        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://gw.internal/v1beta")
        );
    }

    #[test]
    fn a_configured_base_url_replaces_the_default() {
        let provider = create_gemini_provider(Some("https://proxy.example.com/v1beta".to_string()));

        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://proxy.example.com/v1beta")
        );
    }
}

#[cfg(test)]
mod base_url_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_bare_host_gains_the_version_segment_the_sdks_would_add() {
        for bare in [
            "https://generativelanguage.googleapis.com",
            "https://generativelanguage.googleapis.com/",
            "https://gw.internal:8443",
            "http://127.0.0.1:1234",
        ] {
            let provider = create_gemini_provider_from(None, |_| Some(bare.to_string()));
            assert!(
                provider
                    .base_url
                    .as_deref()
                    .is_some_and(|url| url.ends_with("/v1beta")),
                "{bare} should gain /v1beta, got {:?}",
                provider.base_url
            );
        }
    }

    #[test]
    fn a_url_that_already_has_a_path_is_left_alone() {
        for pathed in [
            "https://generativelanguage.googleapis.com/v1beta",
            "https://gw.internal/gemini/v1beta",
            // A user who deliberately pins the stable version must keep it.
            "https://generativelanguage.googleapis.com/v1",
            "https://gw.internal/gemini/v1beta/",
        ] {
            let provider = create_gemini_provider_from(None, |_| Some(pathed.to_string()));
            assert_eq!(
                provider.base_url.as_deref(),
                Some(pathed.trim_end_matches('/')),
                "a configured path must survive verbatim"
            );
        }
    }

    #[test]
    fn a_query_string_is_not_mistaken_for_a_path() {
        let provider =
            create_gemini_provider_from(None, |_| Some("https://gw.internal?tenant=1".to_string()));
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://gw.internal/v1beta?tenant=1"),
            "the version segment belongs in the PATH; appending it after the query \
             left the HTTP path as `/` and buried the version, model and method \
             inside the query string, so every turn 404'd"
        );
    }

    #[test]
    fn an_explicit_argument_is_normalized_too() {
        let provider =
            create_gemini_provider_from(Some("https://explicit.example".to_string()), |_| None);
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://explicit.example/v1beta")
        );
    }
}
