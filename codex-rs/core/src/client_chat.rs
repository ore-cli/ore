//! Turn streaming over the Chat Completions API.
//!
//! Restored in ore; upstream deleted the chat wire in #10157. This file is a
//! child module of `client` (`#[path]` mod at the bottom of client.rs), so it
//! reaches the same private plumbing `stream_responses_api` uses through
//! `super::` without widening any of it. The request loop is re-derived from
//! today's `stream_responses_api`, not ported from the pre-deletion code: the
//! 401-recovery protocol changed since then.

use std::sync::Arc;

use codex_api::ApiError;
use codex_api::ChatCachePolicy;
use codex_api::ChatClient as ApiChatClient;
use codex_api::TransportError;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider::anthropic_model_facts;
use codex_otel::SessionTelemetry;
use codex_protocol::error::Result;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_response_debug_context::extract_response_debug_context;
use codex_response_debug_context::extract_response_debug_context_from_api_error;
use codex_rollout_trace::InferenceTraceContext;
use codex_tools::ChatToolBindings;
use codex_tools::ChatToolKind;
use codex_tools::create_tools_json_for_chat_completions_api;
use tracing::instrument;
use tracing::warn;

use super::AuthRequestTelemetryContext;
use super::ModelClientSession;
use super::PendingUnauthorizedRetry;
use super::RESPONSE_STREAM_CHANNEL_CAPACITY;
use super::RequestRouteTelemetry;
use super::handle_unauthorized;
use super::map_response_stream;
use crate::client_common::Prompt;
use crate::client_common::ResponseStream;

const CHAT_COMPLETIONS_ENDPOINT: &str = "/chat/completions";

/// Effort spellings the Chat Completions `reasoning_effort` parameter accepts.
/// The remaining `ReasoningEffort` variants are Responses-only, so they clamp
/// to the nearest accepted level or omit the field.
fn chat_wire_effort(effort: Option<&ReasoningEffortConfig>) -> Option<&'static str> {
    match effort? {
        ReasoningEffortConfig::None => Some("none"),
        ReasoningEffortConfig::Minimal => Some("minimal"),
        ReasoningEffortConfig::Low => Some("low"),
        ReasoningEffortConfig::Medium => Some("medium"),
        ReasoningEffortConfig::High
        | ReasoningEffortConfig::XHigh
        | ReasoningEffortConfig::Ultra
        | ReasoningEffortConfig::Max => Some("high"),
        ReasoningEffortConfig::Custom(_) => None,
    }
}

/// Auto prompt caching, keyed by the Anthropic facts table: `cache_control`
/// markers only pay off on gateways fronting Claude models, and the facts table
/// publishes each family's minimum cacheable prefix. Every other model gets no
/// markers, so the stock OpenAI request shape is unchanged.
fn chat_cache_policy(model_info: &ModelInfo) -> Option<ChatCachePolicy> {
    if !model_info.slug.starts_with("claude-") {
        return None;
    }
    Some(ChatCachePolicy {
        min_prefix_tokens: anthropic_model_facts(&model_info.slug).cache_min_prefix_tokens,
    })
}

/// Whether a failed request was rejected for carrying `cache_control`.
///
/// A proxy validating the stock OpenAI schema refuses the marker even when the
/// model behind it caches. Matching needs both the field name and a rejection
/// phrase: a gateway that echoes the offending body would otherwise implicate
/// the marker in every 4xx.
fn cache_control_was_rejected(error: &ApiError) -> bool {
    const REJECTIONS: [&str; 5] = [
        "unexpected propert",
        "unknown propert",
        "unrecognized",
        "additional propert",
        "not permitted",
    ];

    let ApiError::Transport(TransportError::Http { status, body, .. }) = error else {
        return false;
    };
    if !status.is_client_error() {
        return false;
    }
    let Some(body) = body else {
        return false;
    };
    let body = body.to_ascii_lowercase();
    body.contains("cache_control") && REJECTIONS.iter().any(|phrase| body.contains(phrase))
}

impl ModelClientSession {
    /// Streams a turn via the Chat Completions API.
    ///
    /// Modelled on `stream_responses_api` and sharing its auth-recovery loop,
    /// so an expired token is refreshed and retried the same way. The chat wire
    /// has no server-side conversation state, so there is no turn-state header
    /// and no websocket path.
    #[instrument(
        name = "model_client.stream_chat_completions",
        level = "info",
        skip_all,
        fields(
            model = %model_info.slug,
            wire_api = %self.client.state.provider.info().wire_api,
            transport = "chat_http",
            http.method = "POST",
            api.path = "chat/completions"
        )
    )]
    pub(super) async fn stream_chat_completions(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        inference_trace: &InferenceTraceContext,
    ) -> Result<ResponseStream> {
        let auth_manager = self.client.state.provider.auth_manager();
        let mut auth_recovery = auth_manager
            .as_ref()
            .map(AuthManager::unauthorized_recovery);
        let mut provider_auth_recovery_attempted = false;
        let mut pending_retry = PendingUnauthorizedRetry::default();
        // In-turn only: the next turn probes again, so a transient gateway
        // rejection does not disable caching for the rest of the session.
        let mut cache_markers_rejected = false;

        let chat_tools = create_tools_json_for_chat_completions_api(&prompt.tools)?;
        let tools_json = chat_tools.json;
        let mut input = prompt.input.clone();
        self.client.prepare_response_items_for_request(&mut input);

        loop {
            let client_setup = self.client.current_client_setup().await?;
            let transport = self
                .client
                .build_api_transport(&client_setup.api_provider, CHAT_COMPLETIONS_ENDPOINT)?;
            let request_auth_context = AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                client_setup.agent_identity_telemetry.clone(),
                pending_retry,
            );
            let (request_telemetry, sse_telemetry) = Self::build_streaming_telemetry(
                session_telemetry,
                request_auth_context,
                RequestRouteTelemetry::for_endpoint(CHAT_COMPLETIONS_ENDPOINT),
                self.client.state.auth_env_telemetry.clone(),
            );

            let inference_trace_attempt = inference_trace.start_attempt();
            let client =
                ApiChatClient::new(transport, client_setup.api_provider, client_setup.api_auth)
                    .with_telemetry(Some(request_telemetry), Some(sse_telemetry));

            let cache_policy = if cache_markers_rejected {
                None
            } else {
                chat_cache_policy(model_info)
            };
            let stream_result = client
                .stream_prompt(
                    &model_info.slug,
                    &prompt.base_instructions.text,
                    &input,
                    &tools_json,
                    Some(self.client.state.thread_id.to_string()),
                    Some(self.client.state.session_source.clone()),
                    prompt.output_schema.as_ref(),
                    prompt.output_schema_strict,
                    /*max_tokens*/ None,
                    chat_wire_effort(effort.as_ref()),
                    cache_policy,
                )
                .await;

            match stream_result {
                Ok(stream) => {
                    let stream = resolve_chat_tool_calls(stream, chat_tools.bindings.clone());
                    let (stream, _) = map_response_stream(
                        stream,
                        session_telemetry.clone(),
                        inference_trace_attempt,
                        Arc::clone(&self.client.state.provider),
                    );
                    return Ok(stream);
                }
                Err(ApiError::Transport(unauthorized_transport))
                    if self
                        .client
                        .state
                        .provider
                        .is_recoverable_auth_error(&unauthorized_transport) =>
                {
                    let response_debug_context =
                        extract_response_debug_context(&unauthorized_transport);
                    inference_trace_attempt.record_failed(
                        &unauthorized_transport,
                        response_debug_context.request_id.as_deref(),
                        /*output_items*/ &[],
                    );
                    pending_retry = PendingUnauthorizedRetry::from_recovery(
                        handle_unauthorized(
                            unauthorized_transport,
                            &mut auth_recovery,
                            &mut provider_auth_recovery_attempted,
                            session_telemetry,
                            &self.client.state.provider,
                        )
                        .await?,
                    );
                    continue;
                }
                Err(err)
                    if cache_policy.is_some()
                        && cache_control_was_rejected(&err)
                        && !cache_markers_rejected =>
                {
                    cache_markers_rejected = true;
                    warn!(
                        "endpoint rejected cache_control; retrying this turn without \
                         prompt-cache markers"
                    );
                    let response_debug_context =
                        extract_response_debug_context_from_api_error(&err);
                    inference_trace_attempt.record_failed(
                        &err,
                        response_debug_context.request_id.as_deref(),
                        /*output_items*/ &[],
                    );
                    continue;
                }
                Err(err) => {
                    let response_debug_context =
                        extract_response_debug_context_from_api_error(&err);
                    let err = self.client.state.provider.map_api_error(err);
                    inference_trace_attempt.record_failed(
                        &err,
                        response_debug_context.request_id.as_deref(),
                        /*output_items*/ &[],
                    );
                    return Err(err);
                }
            }
        }
    }
}

/// Maps chat function names back to the tools they were encoded from; without
/// this the router sees a name it does not know. Shared with the Anthropic
/// glue, which advertises tools through the same flattening encoder.
pub(super) fn resolve_chat_tool_calls(
    stream: codex_api::ResponseStream,
    bindings: ChatToolBindings,
) -> codex_api::ResponseStream {
    if bindings
        .iter()
        .all(|(flat, b)| b.kind == ChatToolKind::Function && *flat == b.tool_name.name)
    {
        // Nothing was rewritten on the way out.
        return stream;
    }

    let codex_api::ResponseStream {
        mut rx_event,
        upstream_request_id,
    } = stream;
    let (tx, rx) = tokio::sync::mpsc::channel(RESPONSE_STREAM_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        while let Some(event) = rx_event.recv().await {
            let event = match event {
                Ok(codex_api::ResponseEvent::OutputItemDone(item)) => Ok(
                    codex_api::ResponseEvent::OutputItemDone(rebind_tool_call(item, &bindings)),
                ),
                other => other,
            };
            if tx.send(event).await.is_err() {
                return;
            }
        }
    });
    codex_api::ResponseStream {
        rx_event: rx,
        upstream_request_id,
    }
}

fn rebind_tool_call(item: ResponseItem, bindings: &ChatToolBindings) -> ResponseItem {
    let ResponseItem::FunctionCall {
        id,
        name,
        namespace,
        arguments,
        encrypted_function_args,
        call_id,
        internal_chat_message_metadata_passthrough,
    } = item
    else {
        return item;
    };
    let Some(binding) = bindings.get(&name) else {
        return ResponseItem::FunctionCall {
            id,
            name,
            namespace,
            arguments,
            encrypted_function_args,
            call_id,
            internal_chat_message_metadata_passthrough,
        };
    };

    match binding.kind {
        ChatToolKind::Function => ResponseItem::FunctionCall {
            id,
            name: binding.tool_name.name.clone(),
            namespace: binding.tool_name.namespace.clone(),
            arguments,
            encrypted_function_args,
            call_id,
            internal_chat_message_metadata_passthrough,
        },
        // The body was carried as `{"input": "<body>"}`; the handler wants it raw.
        ChatToolKind::Freeform => {
            let input = serde_json::from_str::<serde_json::Value>(&arguments)
                .ok()
                .and_then(|v| {
                    v.get("input")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or(arguments);
            ResponseItem::CustomToolCall {
                id,
                status: None,
                call_id,
                name: binding.tool_name.name.clone(),
                namespace: binding.tool_name.namespace.clone(),
                input,
                internal_chat_message_metadata_passthrough,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use pretty_assertions::assert_eq;

    #[test]
    fn responses_only_efforts_clamp_or_omit_on_the_chat_wire() {
        for (effort, expected) in [
            (ReasoningEffortConfig::None, Some("none")),
            (ReasoningEffortConfig::Minimal, Some("minimal")),
            (ReasoningEffortConfig::Low, Some("low")),
            (ReasoningEffortConfig::Medium, Some("medium")),
            (ReasoningEffortConfig::High, Some("high")),
            (ReasoningEffortConfig::XHigh, Some("high")),
            (ReasoningEffortConfig::Ultra, Some("high")),
            (ReasoningEffortConfig::Max, Some("high")),
            (ReasoningEffortConfig::Custom("bespoke".to_string()), None),
        ] {
            assert_eq!(chat_wire_effort(Some(&effort)), expected, "{effort:?}");
        }

        assert_eq!(chat_wire_effort(None), None);
    }

    fn model(slug: &str) -> ModelInfo {
        codex_models_manager::model_info::model_info_from_slug(slug)
    }

    /// The facts table keys Auto mode: only Claude slugs get markers, and the
    /// minimum prefix comes from the model family's published cache minimum.
    #[test]
    fn cache_markers_are_reserved_for_models_the_facts_table_knows() {
        assert!(chat_cache_policy(&model("gpt-5.3-codex")).is_none());
        assert!(chat_cache_policy(&model("kimi-k2")).is_none());

        let policy =
            chat_cache_policy(&model("claude-sonnet-5")).expect("claude models get markers");
        assert_eq!(policy.min_prefix_tokens, 1_024);
    }

    fn http_error(status: StatusCode, body: Option<&str>) -> ApiError {
        ApiError::Transport(TransportError::Http {
            status,
            url: None,
            headers: None,
            body: body.map(str::to_string),
        })
    }

    #[test]
    fn only_a_cache_control_rejection_reads_as_one() {
        let rejected = http_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            Some(r#"{"detail":"unexpected property: messages[0].cache_control"}"#),
        );
        assert!(cache_control_was_rejected(&rejected));

        // A gateway that echoes the request body mentions the marker in every
        // 4xx; without a rejection phrase that is not evidence.
        let echoed = http_error(
            StatusCode::BAD_REQUEST,
            Some(r#"{"error":"too long","request":{"cache_control":{"type":"ephemeral"}}}"#),
        );
        assert!(!cache_control_was_rejected(&echoed));

        let server_error = http_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some("cache_control unexpected property"),
        );
        assert!(!cache_control_was_rejected(&server_error));

        let no_body = http_error(StatusCode::BAD_REQUEST, None);
        assert!(!cache_control_was_rejected(&no_body));

        assert!(!cache_control_was_rejected(&ApiError::Stream(
            "cache_control unexpected property".to_string()
        )));
    }
}
