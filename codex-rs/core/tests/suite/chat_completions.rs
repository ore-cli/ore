//! End-to-end coverage for the restored `wire_api = "chat"` path.
//!
//! Upstream deleted Chat Completions support in Feb 2026 (#10157) along with
//! `core/tests/chat_completions_payload.rs` and `chat_completions_sse.rs`. Those
//! files were written against `OtelManager`, `TransportManager` and an older
//! `ModelClient::new`, none of which still exist, so this is a rewrite against
//! the current harness rather than a revert of the originals.
//!
//! The protocol translation itself is covered by unit tests next to the code
//! (`codex-api`). What is verified here is the wiring those cannot reach: that a
//! provider declaring `wire_api = "chat"` actually routes to
//! `/chat/completions`, sends a Chat-shaped body, and has the streamed reply
//! translated back into `ResponseEvent`s.

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

fn chat_provider(server: &MockServer) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "chat-compat".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Chat,
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

/// Runs one turn against the mocked provider and returns its events.
///
/// `model_override` picks the model slug; `None` uses the offline default.
async fn stream_turn_with(
    provider: ModelProviderInfo,
    model_override: Option<&str>,
) -> Vec<ResponseEvent> {
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home).await;
    config.model_provider_id = provider.name.clone();
    config.model_provider = provider.clone();
    let effort = config.model_reasoning_effort.clone();
    let model = model_override
        .map(str::to_string)
        .unwrap_or_else(|| codex_core::test_support::get_model_offline(config.model.as_deref()));
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

    let client = ModelClient::new(
        Some(AuthManager::from_auth_for_testing(CodexAuth::from_api_key(
            "unused-api-key",
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
        .expect("chat completions stream should start");

    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event.expect("stream event");
        let completed = matches!(event, ResponseEvent::Completed { .. });
        events.push(event);
        if completed {
            break;
        }
    }

    events
}

/// Streams one turn through the chat path and returns the events it produced
/// plus the request body the server received.
async fn run_chat_turn(sse_body: &'static str) -> (Vec<ResponseEvent>, Value) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let events = stream_turn_with(chat_provider(&server), /*model_override*/ None).await;

    let requests = server.received_requests().await.expect("received requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("chat request body");
    (events, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_wire_api_posts_chat_shaped_body_to_chat_completions() {
    skip_if_no_network!();

    let (_events, body) =
        run_chat_turn("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n")
            .await;

    // Chat Completions carries the transcript as `messages`, not `input`, and
    // always streams.
    assert!(
        body.get("input").is_none(),
        "unexpected Responses shape: {body}"
    );
    assert_eq!(body["stream"], Value::Bool(true));
    let messages = body["messages"].as_array().expect("messages array");
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "user" && m["content"].to_string().contains("hello")),
        "user message missing: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_wire_api_translates_stream_into_response_events() {
    skip_if_no_network!();

    let (events, _body) =
        run_chat_turn("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n")
            .await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, ResponseEvent::OutputTextDelta(text) if text == "hi")),
        "expected the content delta to surface: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(ResponseEvent::Completed { .. })),
        "expected the stream to complete: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_wire_api_completes_on_bare_done_sentinel() {
    skip_if_no_network!();

    // Some servers emit `DONE` without brackets; the turn must still complete
    // rather than hang waiting for an end-of-stream that never comes.
    let (events, _body) =
        run_chat_turn("data: {\"choices\":[{\"delta\":{}}]}\n\ndata: DONE\n\n").await;

    assert!(
        matches!(events.last(), Some(ResponseEvent::Completed { .. })),
        "expected completion on a bare DONE sentinel: {events:?}"
    );
}

/// Cache markers are keyed by the facts table: a model no Claude family claims
/// must produce the stock OpenAI request shape, with no `cache_control` anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrecognized_models_get_no_cache_markers() {
    skip_if_no_network!();

    let (_events, body) =
        run_chat_turn("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n")
            .await;

    assert!(
        !body.to_string().contains("cache_control"),
        "the OpenAI request shape must not change: {body}"
    );
}

/// A proxy validating the stock OpenAI schema refuses `cache_control` even when
/// the model behind it caches. The catalog reports only the model, so the first
/// request is the probe: it must degrade to an uncached turn rather than fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_cache_marker_retries_the_turn_without_it() {
    skip_if_no_network!();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(422).set_body_raw(
            r#"{"detail":"validation failed","errors":[{"message":"unexpected property","location":"body.messages[0].cache_control"}]}"#,
            "application/json",
        ))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    // A Claude slug turns Auto-mode markers on, and the default base
    // instructions are far past claude-sonnet-5's cache minimum.
    let events = stream_turn_with(chat_provider(&server), Some("claude-sonnet-5")).await;

    let requests = server.received_requests().await.expect("received requests");
    assert_eq!(2, requests.len(), "expected a retry");

    let marked: Value = serde_json::from_slice(&requests[0].body).expect("first body");
    assert!(
        marked.to_string().contains("cache_control"),
        "the probe should carry the marker: {marked}"
    );
    let retried: Value = serde_json::from_slice(&requests[1].body).expect("second body");
    assert!(
        !retried.to_string().contains("cache_control"),
        "the retry must drop the marker: {retried}"
    );
    assert!(
        matches!(events.last(), Some(ResponseEvent::Completed { .. })),
        "the turn should still complete: {events:?}"
    );
}
