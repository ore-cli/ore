//! End-to-end coverage for `wire_api = "anthropic"`: routing to `/messages`, the
//! request body shape, the credential headers, and translation of the streamed
//! reply into `ResponseEvent`s.

#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use codex_core::ModelClient;
use codex_core::Prompt;
use codex_core::ResponseEvent;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use core_test_support::TestCodexResponsesRequestKind;
use core_test_support::load_default_config_for_test;
use core_test_support::responses_metadata;
use core_test_support::skip_if_no_network;
use futures::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

/// The key the provider table supplies; the adapter must send it as `x-api-key`.
const TEST_API_KEY: &str = "sk-ant-test-key";

fn anthropic_provider(server: &MockServer) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "anthropic-test".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        env_key: None,
        env_key_instructions: None,
        // A configured token keeps the test hermetic: no ANTHROPIC_API_KEY env
        // dependency, and the adapter must still spell it `x-api-key`.
        experimental_bearer_token: Some(TEST_API_KEY.to_string()),
        auth: None,
        aws: None,
        wire_api: WireApi::Anthropic,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
    }
}

/// Streams one turn through the Anthropic path and returns the events produced
/// plus the request the server received.
async fn run_anthropic_turn(sse_body: &'static str) -> (Vec<ResponseEvent>, wiremock::Request) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = anthropic_provider(&server);
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home).await;
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    let effort = config.model_reasoning_effort.clone();
    let model = "claude-sonnet-5".to_string();
    config.model = Some(model.clone());
    let config = Arc::new(config);
    let model_info =
        codex_core::test_support::construct_model_info_offline(model.as_str(), &config);
    let thread_id = ThreadId::new();
    let session_telemetry = SessionTelemetry::new(
        thread_id,
        model.as_str(),
        model_info.slug.as_str(),
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "test".to_string(),
        SessionSource::Exec,
    );

    // The ambient ChatGPT-style credential is deliberately present: the
    // assertions below prove it never reaches the third-party provider.
    let client = ModelClient::new(
        Some(AuthManager::from_auth_for_testing(CodexAuth::from_api_key(
            "openai-api-key-that-must-not-leak",
        ))),
        AgentIdentityAuthPolicy::JwtOnly,
        thread_id,
        provider,
        SessionSource::Exec,
        "test_originator".to_string(),
        config.model_verbosity,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/
        config
            .features
            .enabled(Feature::ConcurrentReasoningSummaries),
        /*attestation_provider*/ None,
        config.http_client_factory(),
    );
    let thread = thread_id.to_string();
    let turn_metadata = responses_metadata(
        "11111111-1111-4111-8111-111111111111",
        &thread,
        &thread,
        /*turn_id*/ None,
        "test-thread:0".to_string(),
        &SessionSource::Exec,
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::Turn,
    );
    let mut client_session = client.new_session();

    let mut prompt = Prompt::default();
    prompt.input.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "hello".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });

    let mut stream = client_session
        .stream(
            &prompt,
            &model_info,
            &session_telemetry,
            effort,
            ReasoningSummary::Auto,
            /*service_tier*/ None,
            &turn_metadata,
            &codex_rollout_trace::InferenceTraceContext::disabled(),
        )
        .await
        .expect("anthropic stream should start");

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("stream event");
        let completed = matches!(event, ResponseEvent::Completed { .. });
        events.push(event);
        if completed {
            break;
        }
    }

    let mut requests = server.received_requests().await.expect("received requests");
    assert_eq!(requests.len(), 1, "expected exactly one request");
    (events, requests.remove(0))
}

const TEXT_TURN: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-test","usage":{"input_tokens":10,"cache_read_input_tokens":4,"cache_creation_input_tokens":2,"output_tokens":0}}}"#,
    "\n\n",
    "event: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi there"}}"#,
    "\n\n",
    "event: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
    "\n\n",
    "event: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_wire_api_posts_a_messages_shaped_body() {
    skip_if_no_network!();

    let (_, request) = run_anthropic_turn(TEXT_TURN).await;
    let body: Value = serde_json::from_slice(&request.body).expect("anthropic request body");

    assert!(
        body.get("max_tokens").is_some(),
        "max_tokens is required by the Messages API: {body}"
    );
    assert!(
        body["system"].is_array(),
        "cache_control attaches to a block, so system is always an array: {body}"
    );
    assert_eq!(body["stream"], Value::Bool(true));
    assert!(
        body.get("stream_options").is_none(),
        "stream_options is a Chat Completions concept: {body}"
    );

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages[0]["role"], "user");
    assert!(
        messages[0]["content"].is_array(),
        "user turns are always block arrays: {body}"
    );
}

/// The Messages API authenticates with `x-api-key` plus a pinned
/// `anthropic-version`; `Authorization` must never appear. The client was
/// constructed with an ambient OpenAI credential, so its absence here is the
/// proof that first-party auth cannot leak to a third-party provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_requests_carry_the_api_key_and_no_openai_auth() {
    skip_if_no_network!();

    let (_, request) = run_anthropic_turn(TEXT_TURN).await;

    assert_eq!(
        request
            .headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
        Some(TEST_API_KEY),
        "the configured key must be sent as x-api-key"
    );
    assert_eq!(
        request
            .headers
            .get("anthropic-version")
            .and_then(|value| value.to_str().ok()),
        Some("2023-06-01"),
        "the Messages API rejects requests without a pinned version"
    );
    assert!(
        request.headers.get("authorization").is_none(),
        "ambient ChatGPT/OpenAI auth must never reach a third-party provider"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_stream_translates_into_response_events() {
    skip_if_no_network!();

    let (events, _) = run_anthropic_turn(TEXT_TURN).await;

    assert!(
        events.iter().any(
            |event| matches!(event, ResponseEvent::OutputTextDelta(text) if text == "hi there")
        ),
        "{events:?}"
    );

    let completed = events
        .iter()
        .filter(|event| matches!(event, ResponseEvent::Completed { .. }))
        .count();
    assert_eq!(
        completed, 1,
        "callers treat a missing Completed as a dropped turn and a second as corruption: {events:?}"
    );

    let ResponseEvent::Completed { token_usage, .. } = events.last().expect("at least one event")
    else {
        panic!("the stream must end with Completed: {events:?}");
    };
    let usage = token_usage.as_ref().expect("usage from message_start");
    assert_eq!(usage.cached_input_tokens, 4);
    assert_eq!(usage.cache_write_input_tokens, 2);
    assert_eq!(usage.output_tokens, 7);
}
