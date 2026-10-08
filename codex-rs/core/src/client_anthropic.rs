//! Turn streaming over the Anthropic Messages API.
//!
//! A child module of `client`, like the chat glue beside it, and modelled on
//! today's `stream_responses_api` request loop. Facts the Messages API needs
//! but the shared `ModelInfo` does not carry — the required `max_tokens` cap,
//! the thinking mode, the cache minimum — come from the fork-side facts table
//! in `codex-model-provider`.

use std::sync::Arc;

use codex_api::AnthropicCachePolicy;
use codex_api::AnthropicClient as ApiAnthropicClient;
use codex_api::AnthropicPromptOptions;
use codex_api::ApiError;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider::AnthropicModelFacts;
use codex_model_provider::anthropic_model_facts;
use codex_otel::SessionTelemetry;
use codex_protocol::error::Result;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_response_debug_context::extract_response_debug_context;
use codex_response_debug_context::extract_response_debug_context_from_api_error;
use codex_rollout_trace::InferenceTraceContext;
use codex_tools::create_tools_json_for_anthropic_api;
use tracing::instrument;

use super::AuthRequestTelemetryContext;
use super::ModelClientSession;
use super::PendingUnauthorizedRetry;
use super::RequestRouteTelemetry;
use super::handle_unauthorized;
use super::map_response_stream;
use crate::client_common::Prompt;
use crate::client_common::ResponseStream;

const ANTHROPIC_MESSAGES_ENDPOINT: &str = "/messages";

/// Effort levels the Messages API accepts. The remaining `ReasoningEffort`
/// variants are Responses-only and are a 400 here, so they omit the field.
fn wire_effort(effort: Option<&ReasoningEffort>) -> Option<&'static str> {
    match effort? {
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        ReasoningEffort::XHigh => Some("xhigh"),
        ReasoningEffort::Max => Some("max"),
        ReasoningEffort::None
        | ReasoningEffort::Minimal
        | ReasoningEffort::Ultra
        // Persistent disables reasoning and hands over to the catalog's
        // persistent instructions; it is a mode, not a level, and the Messages
        // API has no spelling for it.
        | ReasoningEffort::Persistent
        | ReasoningEffort::Custom(_) => None,
    }
}

/// Models without adaptive thinking reject it and require a fixed token budget.
fn thinking_enabled(facts: &AnthropicModelFacts, effort: Option<&ReasoningEffort>) -> bool {
    facts.supports_adaptive_thinking && !matches!(effort, Some(ReasoningEffort::None))
}

/// Requests the output cap only while it fits beside the prompt.
///
/// A server that counts input plus `max_tokens` against the window (vLLM, and
/// any gateway in front of it) rejects the request once the sum overflows.
/// Auto-compaction starts at 90% of the window, so any cap above a tenth of it
/// leaves a band of prompt sizes that can neither be sent nor compacted: a
/// 262144-token model asked for 32000 on a 230145-token prompt. The prompt is
/// estimated at three bytes a token, deliberately high, so the clamp errs
/// toward a shorter answer rather than a refused request.
fn output_budget(cap: i64, context_window: Option<i64>, request_bytes: usize) -> i64 {
    const MIN_OUTPUT_TOKENS: i64 = 1_024;
    let Some(context_window) = context_window else {
        return cap;
    };
    let estimated_input = i64::try_from(request_bytes / 3).unwrap_or(i64::MAX);
    cap.min(context_window.saturating_sub(estimated_input))
        .max(MIN_OUTPUT_TOKENS.min(cap))
}

/// The API rejects an effort level the model does not publish.
fn effort_for(model_info: &ModelInfo, effort: Option<&ReasoningEffort>) -> Option<&'static str> {
    let effort = effort?;
    if !model_info
        .supported_reasoning_levels
        .iter()
        .any(|preset| preset.effort == *effort)
    {
        return None;
    }
    wire_effort(Some(effort))
}

impl ModelClientSession {
    /// Streams a turn via the Anthropic Messages API.
    #[instrument(
        name = "model_client.stream_anthropic_messages",
        level = "info",
        skip_all,
        fields(
            model = %model_info.slug,
            wire_api = %self.client.state.provider.info().wire_api,
            transport = "anthropic_http",
            http.method = "POST",
            api.path = "messages"
        )
    )]
    pub(super) async fn stream_anthropic_messages(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffort>,
        inference_trace: &InferenceTraceContext,
    ) -> Result<ResponseStream> {
        let auth_manager = self.client.state.provider.auth_manager();
        let mut auth_recovery = auth_manager
            .as_ref()
            .map(AuthManager::unauthorized_recovery);
        let mut provider_auth_recovery_attempted = false;
        let mut pending_retry = PendingUnauthorizedRetry::default();

        let tools = create_tools_json_for_anthropic_api(&prompt.tools)?;
        let mut input = prompt.input.clone();
        self.client.prepare_response_items_for_request(&mut input);

        let facts = anthropic_model_facts(&model_info.slug);
        let request_bytes = prompt.base_instructions.text.len()
            + serde_json::to_string(&input).map_or(0, |json| json.len())
            + serde_json::to_string(&tools.json).map_or(0, |json| json.len());
        let max_tokens = output_budget(
            facts.max_output_tokens,
            model_info.resolved_context_window(),
            request_bytes,
        );

        loop {
            let client_setup = self
                .client
                .current_client_setup(super::ClientRouting::ConfiguredProvider)
                .await?;
            let transport = self.client.build_api_transport(
                &client_setup.api_provider,
                ANTHROPIC_MESSAGES_ENDPOINT,
                client_setup.redirect_policy,
            )?;
            let request_auth_context = AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                client_setup.agent_identity_telemetry.clone(),
                pending_retry,
            );
            let (request_telemetry, sse_telemetry) = Self::build_streaming_telemetry(
                session_telemetry,
                request_auth_context,
                RequestRouteTelemetry::for_endpoint(ANTHROPIC_MESSAGES_ENDPOINT),
                self.client.state.auth_env_telemetry.clone(),
            );

            let inference_trace_attempt = inference_trace.start_attempt();
            let client = ApiAnthropicClient::new(
                transport,
                client_setup.api_provider,
                client_setup.api_auth,
            )
            .with_telemetry(Some(request_telemetry), Some(sse_telemetry));

            let stream_result = client
                .stream_prompt(
                    &model_info.slug,
                    &prompt.base_instructions.text,
                    &input,
                    &tools.json,
                    AnthropicPromptOptions {
                        max_tokens,
                        effort: effort_for(model_info, effort.as_ref()),
                        thinking_enabled: thinking_enabled(&facts, effort.as_ref()),
                        supports_inline_system: facts.supports_mid_conversation_system,
                        output_schema: prompt.output_schema.as_ref(),
                        cache_policy: Some(AnthropicCachePolicy {
                            min_prefix_tokens: facts.cache_min_prefix_tokens,
                        }),
                        conversation_id: Some(self.client.state.thread_id.to_string()),
                        session_source: Some(self.client.state.session_source.clone()),
                    },
                )
                .await;

            match stream_result {
                Ok(stream) => {
                    let stream =
                        super::chat::resolve_chat_tool_calls(stream, tools.bindings.clone());
                    let (stream, _) = map_response_stream(
                        stream,
                        session_telemetry.clone(),
                        inference_trace_attempt,
                        Arc::clone(&self.client.state.provider),
                        // These wires carry no Responses client_metadata for contributors to fill.
                        Vec::new(),
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
                    // handle_unauthorized's turn id carries the Responses turn
                    // metadata, which this wire does not have, so the recovery
                    // event reaches the UI untagged.
                    pending_retry = PendingUnauthorizedRetry::from_recovery(
                        handle_unauthorized(
                            unauthorized_transport,
                            &mut auth_recovery,
                            &mut provider_auth_recovery_attempted,
                            session_telemetry,
                            &self.client.state.provider,
                            self.client.event_sender.as_ref(),
                            /*turn_id*/ None,
                        )
                        .await?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// The request that a 262144-token vLLM model refused: 32000 requested on
    /// a prompt of about 230145 tokens.
    #[test]
    fn the_output_cap_shrinks_to_the_room_the_prompt_leaves() {
        let prompt_bytes = 230_145 * 3;
        assert_eq!(
            output_budget(32_000, Some(262_144), prompt_bytes),
            262_144 - 230_145
        );
    }

    #[test]
    fn the_output_cap_is_kept_while_it_fits() {
        assert_eq!(output_budget(32_000, Some(262_144), 30_000), 32_000);
        assert_eq!(output_budget(32_000, None, usize::MAX), 32_000);
    }

    /// A full window still asks for something; the server's refusal is then a
    /// context-window error, which compacts the next turn.
    #[test]
    fn an_exhausted_window_still_requests_a_minimal_answer() {
        assert_eq!(output_budget(32_000, Some(262_144), 262_144 * 3), 1_024);
        assert_eq!(output_budget(512, Some(1_000), 3_000), 512);
    }

    /// The Messages API rejects effort values the Responses wire accepts.
    #[test]
    fn only_effort_levels_the_messages_api_accepts_reach_the_wire() {
        for (effort, expected) in [
            (ReasoningEffort::Low, Some("low")),
            (ReasoningEffort::Medium, Some("medium")),
            (ReasoningEffort::High, Some("high")),
            (ReasoningEffort::XHigh, Some("xhigh")),
            (ReasoningEffort::Max, Some("max")),
            (ReasoningEffort::None, None),
            (ReasoningEffort::Minimal, None),
            (ReasoningEffort::Ultra, None),
            (ReasoningEffort::Custom("bespoke".to_string()), None),
        ] {
            assert_eq!(wire_effort(Some(&effort)), expected, "{effort:?}");
        }

        assert_eq!(wire_effort(None), None);
    }

    /// Older model families reject the adaptive thinking form.
    #[test]
    fn adaptive_thinking_is_only_sent_to_models_that_accept_it() {
        let adaptive = anthropic_model_facts("claude-opus-5");
        let budgeted = anthropic_model_facts("claude-haiku-4-5");
        assert!(adaptive.supports_adaptive_thinking);
        assert!(!budgeted.supports_adaptive_thinking);

        assert!(thinking_enabled(&adaptive, Some(&ReasoningEffort::Low)));
        assert!(thinking_enabled(&adaptive, None));
        assert!(!thinking_enabled(&adaptive, Some(&ReasoningEffort::None)));

        assert!(!thinking_enabled(&budgeted, Some(&ReasoningEffort::Low)));
        assert!(!thinking_enabled(&budgeted, None));
    }

    fn model(levels: Vec<ReasoningEffort>) -> ModelInfo {
        let mut info = codex_models_manager::model_info::model_info_from_slug("claude-test");
        info.supported_reasoning_levels = levels
            .into_iter()
            .map(
                |effort| codex_protocol::openai_models::ReasoningEffortPreset {
                    effort,
                    description: String::new(),
                },
            )
            .collect();
        info
    }

    /// A model publishing no effort levels rejects `output_config.effort`.
    #[test]
    fn effort_is_withheld_from_models_that_publish_none() {
        let with_levels = model(vec![ReasoningEffort::High]);
        let without = model(vec![]);

        assert_eq!(
            effort_for(&with_levels, Some(&ReasoningEffort::High)),
            Some("high")
        );
        assert_eq!(effort_for(&without, Some(&ReasoningEffort::High)), None);
    }
}
