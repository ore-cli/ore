use codex_api::AuthProvider;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::EnvVarError;
use codex_protocol::error::Result;
use http::HeaderMap;
use http::HeaderValue;

use super::info::GEMINI_API_KEY_ENV_VAR;
use super::info::GEMINI_API_KEY_INSTRUCTIONS;

/// Credential header for the generative-language API. It does not read
/// `Authorization`; the only other spelling it accepts is a `?key=` query
/// parameter, which would put the credential in every proxy access log.
const GEMINI_API_KEY_HEADER: &str = "x-goog-api-key";

/// Attaches a provider-owned API key as `x-goog-api-key`.
pub(super) struct GeminiApiKeyAuthProvider {
    api_key: String,
}

/// Opaque: the struct holds an API key that must never reach a log line.
impl std::fmt::Debug for GeminiApiKeyAuthProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiApiKeyAuthProvider").finish()
    }
}

impl GeminiApiKeyAuthProvider {
    /// Trims, then proves the result can actually be sent.
    ///
    /// Trimming matters because a credential from a `.env` file, a Docker
    /// --env-file or a Kubernetes secret mount keeps its trailing newline.
    /// Validating matters more: `add_auth_headers` returns `()` and the default
    /// `resolve_auth_headers` wraps it in `Ok`, so a key this type accepts but
    /// `HeaderValue` rejects -- an interior newline from a two-line secret file,
    /// a control character from a mangled paste -- would produce an EMPTY header
    /// map and an unauthenticated request that no caller could detect. Gemini
    /// answers such a request with a 400 naming a missing API key, which sends
    /// the user hunting for an unset variable that is in fact set. Rejecting
    /// here turns that into the `CodexErr::EnvVar` this module promises.
    pub(super) fn try_new(
        name: &str,
        api_key: String,
        instructions: Option<String>,
    ) -> Result<Self> {
        let api_key = api_key.trim().to_string();
        let bad = |reason: &str| {
            CodexErr::EnvVar(EnvVarError {
                var: format!("{name} ({reason})"),
                instructions: instructions.clone(),
            })
        };
        if api_key.is_empty() {
            return Err(bad("set but empty"));
        }
        if HeaderValue::from_str(&api_key).is_err() {
            // Deliberately does not echo the value: this is a credential.
            return Err(bad("not a valid HTTP header value"));
        }
        Ok(Self { api_key })
    }
}

impl AuthProvider for GeminiApiKeyAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        // Infallible by construction: `try_new` has already proved this string is
        // a legal header value, so there is no failure branch here to swallow.
        // Logging-and-returning here would still send the request with no
        // credential -- the defect this design exists to prevent.
        match HeaderValue::from_str(&self.api_key) {
            Ok(mut header) => {
                // Keeps the key out of any `{:?}` of this map -- the same call the
                // responses-api proxy makes on its own auth header.
                header.set_sensitive(true);
                let _ = headers.insert(GEMINI_API_KEY_HEADER, header);
            }
            Err(_) => debug_assert!(false, "try_new validated this key"),
        }
    }
}

/// Resolves the key a Gemini provider authenticates with. A configured
/// `env_key` or bearer token wins; otherwise the key comes from
/// `GEMINI_API_KEY`. A missing key is a `CodexErr::EnvVar`, not an
/// unauthenticated request.
pub(super) fn gemini_api_key_auth(
    provider: &ModelProviderInfo,
) -> Result<GeminiApiKeyAuthProvider> {
    gemini_api_key_auth_from(provider, |var| std::env::var(var).ok())
}

fn gemini_api_key_auth_from(
    provider: &ModelProviderInfo,
    env: impl Fn(&str) -> Option<String>,
) -> Result<GeminiApiKeyAuthProvider> {
    let instructions = provider
        .env_key_instructions
        .clone()
        .or_else(|| Some(GEMINI_API_KEY_INSTRUCTIONS.to_string()));

    // Each source names ITSELF in the error. Reporting GEMINI_API_KEY when the
    // bad value came from a configured `env_key` or from
    // `experimental_bearer_token` sends the user to edit the wrong thing.
    if let Some(api_key) = provider.api_key()? {
        let name = provider
            .env_key
            .as_deref()
            .unwrap_or(GEMINI_API_KEY_ENV_VAR);
        return GeminiApiKeyAuthProvider::try_new(name, api_key, instructions);
    }

    if let Some(token) = provider.experimental_bearer_token.clone() {
        // Blank-filtered like the other two sources: a token of "   " trims to ""
        // and `HeaderValue::from_str("")` is Ok, which would send an empty
        // `x-goog-api-key` header with no error anywhere.
        return GeminiApiKeyAuthProvider::try_new("experimental_bearer_token", token, instructions);
    }

    match env(GEMINI_API_KEY_ENV_VAR) {
        Some(value) => {
            GeminiApiKeyAuthProvider::try_new(GEMINI_API_KEY_ENV_VAR, value, instructions)
        }
        None => Err(CodexErr::EnvVar(EnvVarError {
            var: GEMINI_API_KEY_ENV_VAR.to_string(),
            instructions,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::error::CodexErrorDetails;
    use pretty_assertions::assert_eq;

    use crate::gemini::info::create_gemini_provider;

    #[test]
    fn a_key_that_cannot_be_a_header_is_an_error_not_an_anonymous_request() {
        // Each of these survives trim() but HeaderValue::from_str rejects it. A
        // log-and-return would leave the request to go out with an EMPTY header
        // map -- indistinguishable from having no key at all.
        // Only CONTROL characters are rejected by HeaderValue: bytes 0x80-0xFF are
        // legal obs-text in an HTTP field value, so a non-ASCII key is accepted and
        // forwarded. That is correct -- Google, not this client, decides whether
        // such a key is real.
        for bad in [
            "AIzaSyA\nAIzaSyB", // a two-line secret file or a multi-line `op read`
            "AIzaSy\u{0}b",     // interior NUL
            "AIzaSy\rb",        // bare CR
        ] {
            let result =
                GeminiApiKeyAuthProvider::try_new(GEMINI_API_KEY_ENV_VAR, bad.to_string(), None);
            assert!(
                result.is_err(),
                "{bad:?} must be rejected at resolve time, not silently dropped"
            );
        }
    }

    #[test]
    fn a_blank_credential_is_an_error_from_every_source() {
        for blank in ["", "   ", "\t\n"] {
            assert!(
                GeminiApiKeyAuthProvider::try_new(
                    "experimental_bearer_token",
                    blank.to_string(),
                    None
                )
                .is_err(),
                "{blank:?} trimmed to empty; an empty x-goog-api-key header is not a credential"
            );
        }
    }

    #[test]
    fn the_error_names_the_source_the_bad_value_came_from() {
        let err =
            GeminiApiKeyAuthProvider::try_new("MY_GATEWAY_TOKEN", "bad\nvalue".to_string(), None)
                .expect_err("invalid");
        let rendered = format!("{err:?}");
        assert!(
            rendered.contains("MY_GATEWAY_TOKEN"),
            "the error must name the variable the user has to fix, got: {rendered}"
        );
        assert!(
            !rendered.contains("bad\nvalue"),
            "the credential itself must never appear in an error"
        );
    }

    #[test]
    fn a_key_with_a_trailing_newline_still_authenticates() {
        // `GEMINI_API_KEY=$(op read ...)` in a .env file, a Docker --env-file, or
        // a Kubernetes secret mount all keep the newline.
        let auth = GeminiApiKeyAuthProvider::try_new(
            GEMINI_API_KEY_ENV_VAR,
            "AIzaSyTest\n".to_string(),
            None,
        )
        .expect("a trimmed key is valid");
        let mut headers = HeaderMap::new();
        auth.add_auth_headers(&mut headers);
        assert_eq!(
            headers
                .get(GEMINI_API_KEY_HEADER)
                .map(|value| value.to_str().unwrap_or("")),
            Some("AIzaSyTest"),
            "a trailing newline must be trimmed, not drop the credential entirely"
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let auth = GeminiApiKeyAuthProvider::try_new(
            GEMINI_API_KEY_ENV_VAR,
            "  AIzaSyTest \t".to_string(),
            None,
        )
        .expect("a trimmed key is valid");

        assert_eq!(
            auth.to_auth_headers()
                .get(GEMINI_API_KEY_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("AIzaSyTest")
        );
    }

    #[test]
    fn the_key_header_is_marked_sensitive() {
        let auth = GeminiApiKeyAuthProvider::try_new(
            GEMINI_API_KEY_ENV_VAR,
            "AIzaSySecret".to_string(),
            None,
        )
        .expect("valid");
        let mut headers = HeaderMap::new();
        auth.add_auth_headers(&mut headers);

        let value = headers.get(GEMINI_API_KEY_HEADER).expect("header present");
        assert!(
            value.is_sensitive(),
            "the key must not render in a header-map Debug"
        );
        assert!(
            !format!("{headers:?}").contains("AIzaSySecret"),
            "Debug of the header map leaked the key"
        );
    }

    /// A `Debug` derive here would put the API key in any `{:?}` log line.
    #[test]
    fn debug_does_not_expose_the_api_key() {
        let provider = GeminiApiKeyAuthProvider::try_new(
            GEMINI_API_KEY_ENV_VAR,
            "AIzaSySecret".to_string(),
            None,
        )
        .expect("valid");

        assert!(!format!("{provider:?}").contains("AIzaSySecret"));
    }

    #[test]
    fn api_key_is_attached_as_x_goog_api_key_only() {
        let auth = GeminiApiKeyAuthProvider::try_new(
            GEMINI_API_KEY_ENV_VAR,
            "AIzaSyTest".to_string(),
            None,
        )
        .expect("valid");

        let headers = auth.to_auth_headers();

        assert_eq!(
            headers
                .get(GEMINI_API_KEY_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("AIzaSyTest")
        );
        assert!(
            !headers.contains_key(http::header::AUTHORIZATION),
            "Gemini ignores Authorization; sending it would leak the key to a proxy"
        );
    }

    /// Upstream's telemetry helper only knows `authorization`; widening it to
    /// name `x-goog-api-key` would be a codex-api edit this fork does not take.
    /// The header is attached to requests regardless — telemetry just cannot
    /// name it.
    #[test]
    fn api_key_auth_is_invisible_to_the_bearer_telemetry_helper() {
        let auth = GeminiApiKeyAuthProvider::try_new(
            GEMINI_API_KEY_ENV_VAR,
            "AIzaSyTest".to_string(),
            None,
        )
        .expect("valid");

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
        let provider = create_gemini_provider(/*base_url*/ None);

        let auth = gemini_api_key_auth_from(&provider, |var| {
            (var == GEMINI_API_KEY_ENV_VAR).then(|| "AIzaSyEnv".to_string())
        })
        .expect("key resolves");

        assert_eq!(
            auth.to_auth_headers()
                .get(GEMINI_API_KEY_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("AIzaSyEnv")
        );
    }

    #[test]
    fn a_missing_key_reports_the_variable_and_instructions() {
        let provider = create_gemini_provider(/*base_url*/ None);

        let err = gemini_api_key_auth_from(&provider, |_| None)
            .expect_err("a missing API key should not resolve to unauthenticated auth");

        let CodexErrorDetails::EnvVar(err) = err.details() else {
            panic!("expected an env var error, got {err:?}");
        };
        assert_eq!(err.var, GEMINI_API_KEY_ENV_VAR);
        assert_eq!(
            err.instructions, provider.env_key_instructions,
            "the provider's own instructions should reach the user"
        );
    }

    #[test]
    fn a_declared_env_key_that_is_unset_reports_that_variable() {
        let env_key = "CODEX_TEST_GEMINI_KEY_THAT_IS_NEVER_SET";
        let provider = ModelProviderInfo {
            env_key: Some(env_key.to_string()),
            ..create_gemini_provider(/*base_url*/ None)
        };

        let err = gemini_api_key_auth(&provider)
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
            experimental_bearer_token: Some("AIzaSyConfigured".to_string()),
            ..create_gemini_provider(/*base_url*/ None)
        };

        let auth =
            gemini_api_key_auth_from(&provider, |_| None).expect("configured token should resolve");

        assert_eq!(
            auth.to_auth_headers()
                .get(GEMINI_API_KEY_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("AIzaSyConfigured"),
            "a token configured for a gateway must still ride the header Gemini reads"
        );
    }
}
