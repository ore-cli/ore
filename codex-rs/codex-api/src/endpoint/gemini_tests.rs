use super::*;
use crate::provider::RetryConfig;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::time::Duration;

fn provider(query_params: Option<HashMap<String, String>>) -> Provider {
    Provider {
        name: "gemini".to_string(),
        base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
        query_params,
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_attempts: 1,
            base_delay: Duration::from_millis(10),
            retry_429: false,
            retry_5xx: true,
            retry_transport: true,
        },
        stream_idle_timeout: Duration::from_secs(1),
    }
}

/// The model rides in the path on this wire, not in the body.
#[test]
fn the_path_names_the_model() {
    assert_eq!(
        stream_path("gemini-3-pro"),
        "models/gemini-3-pro:streamGenerateContent"
    );
}

/// A slug that already spells the resource name would otherwise address
/// `models/models/...`, which 404s.
#[test]
fn an_already_qualified_model_keeps_its_own_prefix() {
    assert_eq!(
        stream_path("models/gemini-3-pro"),
        "models/gemini-3-pro:streamGenerateContent"
    );
    assert_eq!(
        stream_path("/models/gemini-3-pro"),
        "models/gemini-3-pro:streamGenerateContent"
    );
    assert_eq!(
        stream_path("tunedModels/my-model"),
        "tunedModels/my-model:streamGenerateContent"
    );
}

#[test]
fn the_stream_url_selects_sse_framing() {
    let mut url = provider(None).url_for_path(&stream_path("gemini-3-pro"));
    append_alt_sse(&mut url);

    assert_eq!(
        url,
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse"
    );
}

/// A provider carrying its own query params already spent the `?`; a second one
/// makes the whole parameter list unparsable.
#[test]
fn a_provider_query_string_keeps_the_url_parsable() {
    let params = HashMap::from([("key".to_string(), "abc".to_string())]);
    let mut url = provider(Some(params)).url_for_path(&stream_path("gemini-3-pro"));
    append_alt_sse(&mut url);

    assert_eq!(url.matches('?').count(), 1, "{url}");
    assert!(url.ends_with("?key=abc&alt=sse"), "{url}");
    let parsed = url::Url::parse(&url).expect("url");
    let query: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
    assert_eq!(query.get("alt").map(String::as_str), Some("sse"));
    assert_eq!(query.get("key").map(String::as_str), Some("abc"));
}

/// Discovery takes model ids verbatim from arbitrary gateways, so a slug can
/// carry anything. Both of these used to fail silently.
#[test]
fn a_slug_with_url_syntax_cannot_escape_the_path() {
    // `?` ended the path, so `:streamGenerateContent` landed in the QUERY and the
    // request addressed the wrong resource entirely.
    let path = stream_path("gemini?x=1");
    assert!(
        !path.contains('?'),
        "a query character must not survive into the path: {path}"
    );
    assert!(path.ends_with(":streamGenerateContent"), "{path}");

    // `#` started a fragment, so the `alt=sse` appended afterwards was never
    // sent; the server answered with a chunked JSON array that the SSE parser
    // reads as one unterminated frame and drops -- an empty turn, no error.
    let path = stream_path("gemini#frag");
    assert!(
        !path.contains('#'),
        "a fragment character must not survive into the path: {path}"
    );
}

#[test]
fn a_vertex_resource_name_is_not_double_prefixed() {
    // The first version allowlisted only `models/` and `tunedModels/`, so every
    // Vertex deployment addressed `models/projects/...` and 404'd.
    let path = stream_path("projects/p/locations/us/publishers/google/models/gemini-2.5-pro");
    assert!(
        path.starts_with("projects/p/"),
        "a qualified Vertex name must not gain a models/ prefix: {path}"
    );
    assert!(!path.contains("models/projects/"), "{path}");
}

#[test]
fn ordinary_and_namespaced_slugs_are_unchanged() {
    // The encoder must not mangle the ids that already worked.
    assert_eq!(
        stream_path("gemini-2.5-pro"),
        "models/gemini-2.5-pro:streamGenerateContent"
    );
    // A gateway namespace keeps its separator: it is a path, not an escapable
    // character.
    assert_eq!(
        stream_path("openai/gpt-oss-20b"),
        "models/openai/gpt-oss-20b:streamGenerateContent"
    );
    assert_eq!(
        stream_path("models/gemini-2.5-flash"),
        "models/gemini-2.5-flash:streamGenerateContent"
    );
}

#[test]
fn a_vertex_model_version_survives_encoding() {
    // `@` is Vertex's model-version separator and a legal pchar. Escaping it
    // 404'd every versioned Vertex model -- in the same change that added
    // `projects/` to enable Vertex at all.
    assert_eq!(
        stream_path("gemini-2.5-pro@001"),
        "models/gemini-2.5-pro@001:streamGenerateContent"
    );
    assert_eq!(
        stream_path("publishers/google/models/gemini-2.5-pro"),
        "publishers/google/models/gemini-2.5-pro:streamGenerateContent",
        "a publishers/ resource name must not gain a models/ prefix"
    );
}

#[test]
fn an_already_encoded_id_is_not_encoded_twice() {
    // Re-encoding `%` turns an id a gateway gave us into a different id.
    assert_eq!(
        stream_path("gemini%2Dpro"),
        "models/gemini%2Dpro:streamGenerateContent"
    );
}
