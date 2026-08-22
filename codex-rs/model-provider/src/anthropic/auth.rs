use codex_api::AuthProvider;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::EnvVarError;
use codex_protocol::error::Result;
use http::HeaderMap;
use http::HeaderValue;

use super::info::ANTHROPIC_API_KEY_ENV_VAR;
use super::info::ANTHROPIC_API_KEY_INSTRUCTIONS;

/// Credential header for the Messages API. It does not read `Authorization`.
const ANTHROPIC_API_KEY_HEADER: &str = "x-api-key";

/// Attaches a provider-owned API key as `x-api-key`.
pub(super) struct AnthropicApiKeyAuthProvider {
    api_key: String,
}

/// Opaque: the struct holds an API key that must never reach a log line.
impl std::fmt::Debug for AnthropicApiKeyAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicApiKeyAuthProvider").finish()
    }
}

impl AnthropicApiKeyAuthProvider {
    pub(super) fn new(api_key: String) -> Self {
        Self { api_key }
    }
}

impl AuthProvider for AnthropicApiKeyAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        if let Ok(header) = HeaderValue::from_str(&self.api_key) {
            let _ = headers.insert(ANTHROPIC_API_KEY_HEADER, header);
        }
    }
}

/// Resolves the key a Messages API provider authenticates with. A configured
/// `env_key` or bearer token wins; otherwise the key comes from
/// `ANTHROPIC_API_KEY`. A missing key is a `CodexErr::EnvVar`, not an
/// unauthenticated request.
pub(super) fn anthropic_api_key_auth(
    provider: &ModelProviderInfo,
) -> Result<AnthropicApiKeyAuthProvider> {
    anthropic_api_key_auth_from(provider, |var| std::env::var(var).ok())
}

fn anthropic_api_key_auth_from(
    provider: &ModelProviderInfo,
    env: impl Fn(&str) -> Option<String>,
) -> Result<AnthropicApiKeyAuthProvider> {
    if let Some(api_key) = provider.api_key()? {
        return Ok(AnthropicApiKeyAuthProvider::new(api_key));
    }

    if let Some(token) = provider.experimental_bearer_token.clone() {
        return Ok(AnthropicApiKeyAuthProvider::new(token));
    }

    env(ANTHROPIC_API_KEY_ENV_VAR)
        .filter(|value| !value.trim().is_empty())
        .map(AnthropicApiKeyAuthProvider::new)
        .ok_or_else(|| {
            CodexErr::EnvVar(EnvVarError {
                var: ANTHROPIC_API_KEY_ENV_VAR.to_string(),
                instructions: provider
                    .env_key_instructions
                    .clone()
                    .or_else(|| Some(ANTHROPIC_API_KEY_INSTRUCTIONS.to_string())),
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::error::CodexErrorDetails;
    use pretty_assertions::assert_eq;

    use crate::anthropic::info::create_anthropic_provider;

    /// A `Debug` derive here would put the API key in any `{:?}` log line.
    #[test]
    fn debug_does_not_expose_the_api_key() {
        let provider = AnthropicApiKeyAuthProvider::new("sk-ant-secret".to_string());

        assert!(!format!("{provider:?}").contains("sk-ant-secret"));
    }

    #[test]
    fn api_key_is_attached_as_x_api_key_only() {
        let auth = AnthropicApiKeyAuthProvider::new("sk-ant-test".to_string());

        let headers = auth.to_auth_headers();

        assert_eq!(
            headers
                .get(ANTHROPIC_API_KEY_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("sk-ant-test")
        );
        assert!(
            !headers.contains_key(http::header::AUTHORIZATION),
            "the Messages API ignores Authorization; sending it would leak the key to a proxy"
        );
    }

    /// Upstream's telemetry helper only knows `authorization`; widening it to
    /// name `x-api-key` would be a codex-api edit this fork does not take. The
    /// header is attached to requests regardless — telemetry just cannot name it.
    #[test]
    fn api_key_auth_is_invisible_to_the_bearer_telemetry_helper() {
        let auth = AnthropicApiKeyAuthProvider::new("sk-ant-test".to_string());

        assert_eq!(
            codex_api::auth_header_telemetry(&auth),
            codex_api::AuthHeaderTelemetry {
                attached: false,
                name: None,
            }
        );
    }

    #[test]
    fn the_default_env_var_supplies_the_key() {
        let provider = create_anthropic_provider(/*base_url*/ None);

        let auth = anthropic_api_key_auth_from(&provider, |var| {
            (var == ANTHROPIC_API_KEY_ENV_VAR).then(|| "sk-ant-env".to_string())
        })
        .expect("key resolves");

        assert_eq!(
            auth.to_auth_headers()
                .get(ANTHROPIC_API_KEY_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("sk-ant-env")
        );
    }

    #[test]
    fn a_missing_key_reports_the_variable_and_instructions() {
        let provider = create_anthropic_provider(/*base_url*/ None);

        let err = anthropic_api_key_auth_from(&provider, |_| None)
            .expect_err("a missing API key should not resolve to unauthenticated auth");

        let CodexErrorDetails::EnvVar(err) = err.details() else {
            panic!("expected an env var error, got {err:?}");
        };
        assert_eq!(err.var, ANTHROPIC_API_KEY_ENV_VAR);
        assert_eq!(
            err.instructions, provider.env_key_instructions,
            "the provider's own instructions should reach the user"
        );
    }

    #[test]
    fn a_declared_env_key_that_is_unset_reports_that_variable() {
        let env_key = "CODEX_TEST_ANTHROPIC_KEY_THAT_IS_NEVER_SET";
        let provider = ModelProviderInfo {
            env_key: Some(env_key.to_string()),
            ..create_anthropic_provider(/*base_url*/ None)
        };

        let err = anthropic_api_key_auth(&provider)
            .expect_err("a declared but unset env_key is an error, not a fall-through");

        let CodexErrorDetails::EnvVar(err) = err.details() else {
            panic!("expected an env var error, got {err:?}");
        };
        assert_eq!(err.var, env_key);
    }

    #[test]
    fn a_configured_token_is_used_when_no_env_key_is_declared() {
        let provider = ModelProviderInfo {
            env_key: None,
            experimental_bearer_token: Some("sk-ant-configured".to_string()),
            ..create_anthropic_provider(/*base_url*/ None)
        };

        let auth = anthropic_api_key_auth_from(&provider, |_| None)
            .expect("configured token should resolve");

        assert_eq!(
            auth.to_auth_headers()
                .get(ANTHROPIC_API_KEY_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("sk-ant-configured")
        );
    }
}
