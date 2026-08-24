//! Turn streaming over the Gemini `generateContent` API.
//!
//! A child module of `client`, modelled on the Anthropic glue beside it. Facts
//! this wire needs but the shared `ModelInfo` does not carry — the output cap,
//! whether the model thinks at all and within which token budget — come from
//! the fork-side facts table in `codex-model-provider`.

use std::sync::Arc;

use codex_api::ApiError;
use codex_api::GeminiClient as ApiGeminiClient;
use codex_api::GeminiPromptOptions;
use codex_api::GeminiThinkingConfig;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider::GeminiModelFacts;
use codex_model_provider::GeminiThinkingBudget;
use codex_model_provider::gemini_model_facts;
use codex_otel::SessionTelemetry;
use codex_protocol::error::Result;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_response_debug_context::extract_response_debug_context;
use codex_response_debug_context::extract_response_debug_context_from_api_error;
use codex_rollout_trace::InferenceTraceContext;
use codex_tools::create_tools_json_for_gemini_api;
use tracing::instrument;

use super::AuthRequestTelemetryContext;
use super::ModelClientSession;
use super::PendingUnauthorizedRetry;
use super::RequestRouteTelemetry;
use super::handle_unauthorized;
use super::map_response_stream;
use crate::client_common::Prompt;
use crate::client_common::ResponseStream;

/// Stands in for the real request path, which names the model
/// (`models/{slug}:streamGenerateContent`) and so cannot be a constant. The
/// endpoint client builds that path itself; this label only classifies the
/// outbound route and names the span, both of which want one stable string per
/// endpoint rather than one per model.
const GEMINI_STREAM_ROUTE: &str = "models:streamGenerateContent";

/// The `thinkingBudget` an effort level buys, as a share of the model's own
/// ceiling.
///
/// Gemini's reasoning control is a continuous token count, not a named tier, so
/// unlike the Anthropic wire there is no value the API rejects for being
/// unpublished — only one outside the model's range, which the clamp rules out.
fn budget_tokens(range: GeminiThinkingBudget, effort: &ReasoningEffort) -> i64 {
    // A facts row with min > max would make `i64::clamp` PANIC, aborting the turn
    // on a typo in a table. Normalising here costs nothing and cannot abort.
    let min = range.min.min(range.max);
    let max = range.max.max(range.min);

    // Interpolate across the model's ACTUAL span rather than taking fractions of
    // the ceiling. Fractions of `max` collapse against a high floor: 2.5 Pro-like
    // bounds of min=20000/max=24576 sent every tier below XHigh to 20000, so four
    // visibly distinct settings produced one identical wire value and the user saw
    // no difference between Minimal and High. Spanning min..max keeps the tiers
    // distinct for any range the table can hold.
    let span = max - min;
    let share = |numerator: i64, denominator: i64| min + span * numerator / denominator;
    let tokens = match effort {
        ReasoningEffort::Minimal => min,
        ReasoningEffort::Low => share(1, 4),
        ReasoningEffort::Medium => share(1, 2),
        ReasoningEffort::High => share(3, 4),
        // The tiers above High have no budget of their own left to spend: the
        // ceiling is the most this model will think under any name.
        ReasoningEffort::XHigh | ReasoningEffort::Max | ReasoningEffort::Ultra => max,
        // Handled by the caller, which has a config to return rather than a
        // number.
        ReasoningEffort::None | ReasoningEffort::Custom(_) => share(1, 2),
    };
    tokens.clamp(min, max)
}

/// The `thinkingConfig` for a model and effort, or `None` to omit the field.
fn thinking_config(
    facts: &GeminiModelFacts,
    effort: Option<&ReasoningEffort>,
) -> Option<GeminiThinkingConfig> {
    // A model with no thinking mode answers `thinkingConfig` with an
    // `INVALID_ARGUMENT`, so the field has to be absent rather than zeroed.
    let range = facts.thinking_budget?;

    // No budget means the model's own dynamic thinking. `includeThoughts` still
    // has to be set: without it the model thinks but streams no thought parts,
    // so the reasoning is billed and never shown.
    let dynamic = GeminiThinkingConfig {
        budget_tokens: None,
        include_thoughts: true,
    };
    let Some(effort) = effort else {
        return Some(dynamic);
    };

    match effort {
        // Zero is a special value rather than a point in the range, so it is
        // legal even where the floor is above it — but only where the family
        // accepts it at all; 2.5 Pro answers it with a 400.
        ReasoningEffort::None if facts.can_disable_thinking => Some(GeminiThinkingConfig {
            budget_tokens: Some(0),
            include_thoughts: false,
        }),
        // A model that cannot stop thinking gets the smallest legal budget,
        // which is as close to the user's ask as this wire goes.
        ReasoningEffort::None => Some(GeminiThinkingConfig {
            budget_tokens: Some(range.min),
            include_thoughts: true,
        }),
        // A free-form level names nothing on a numeric control, so the model
        // keeps its own default rather than being handed an invented number.
        ReasoningEffort::Custom(_) => Some(dynamic),
        effort => Some(GeminiThinkingConfig {
            budget_tokens: Some(budget_tokens(range, effort)),
            include_thoughts: true,
        }),
    }
}

impl ModelClientSession {
    /// Streams a turn via the Gemini `generateContent` API.
    #[instrument(
        name = "model_client.stream_gemini_messages",
        level = "info",
        skip_all,
        fields(
            model = %model_info.slug,
            wire_api = %self.client.state.provider.info().wire_api,
            transport = "gemini_http",
            http.method = "POST",
            api.path = "models:streamGenerateContent"
        )
    )]
    pub(super) async fn stream_gemini_messages(
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

        let tools = create_tools_json_for_gemini_api(&prompt.tools)?;
        let mut input = prompt.input.clone();
        self.client.prepare_response_items_for_request(&mut input);

        let facts = gemini_model_facts(&model_info.slug);
        let thinking = thinking_config(&facts, effort.as_ref());

        loop {
            let client_setup = self.client.current_client_setup().await?;
            let transport = self
                .client
                .build_api_transport(&client_setup.api_provider, GEMINI_STREAM_ROUTE)?;
            let request_auth_context = AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                client_setup.agent_identity_telemetry.clone(),
                pending_retry,
            );
            let (request_telemetry, sse_telemetry) = Self::build_streaming_telemetry(
                session_telemetry,
                request_auth_context,
                RequestRouteTelemetry::for_endpoint(GEMINI_STREAM_ROUTE),
                self.client.state.auth_env_telemetry.clone(),
            );

            let inference_trace_attempt = inference_trace.start_attempt();
            let client =
                ApiGeminiClient::new(transport, client_setup.api_provider, client_setup.api_auth)
                    .with_telemetry(Some(request_telemetry), Some(sse_telemetry));

            let stream_result = client
                .stream_prompt(
                    &model_info.slug,
                    &prompt.base_instructions.text,
                    &input,
                    &tools.function_declarations,
                    GeminiPromptOptions {
                        // A cap above the model's own is a hard 400, so the
                        // facts table decides it rather than the wire default.
                        max_output_tokens: Some(facts.max_output_tokens),
                        // Left to the model: this wire has no sampling setting
                        // ore exposes.
                        temperature: None,
                        thinking,
                        output_schema: prompt.output_schema.as_ref(),
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

    /// Sending `thinkingConfig` to a model with no thinking mode is an
    /// `INVALID_ARGUMENT`, so the field has to disappear rather than go out zeroed.
    #[test]
    fn a_model_without_thinking_gets_no_thinking_field() {
        let legacy = gemini_model_facts("gemini-2.0-flash");
        assert_eq!(legacy.thinking_budget, None);

        for effort in [
            None,
            Some(ReasoningEffort::None),
            Some(ReasoningEffort::High),
        ] {
            assert!(
                thinking_config(&legacy, effort.as_ref()).is_none(),
                "{effort:?}"
            );
        }
    }

    /// Absent effort must still ask for thought parts, or the reasoning is
    /// billed and never shown.
    #[test]
    fn no_effort_means_dynamic_thinking_with_visible_thoughts() {
        let config =
            thinking_config(&gemini_model_facts("gemini-2.5-pro"), None).expect("pro thinks");

        assert_eq!(config.budget_tokens, None);
        assert!(config.include_thoughts);
    }

    /// Flash can be quieted outright; Pro answers `thinkingBudget: 0` with a 400
    /// and so gets its floor instead.
    #[test]
    fn thinking_is_only_zeroed_where_the_family_accepts_zero() {
        let flash = thinking_config(
            &gemini_model_facts("gemini-2.5-flash"),
            Some(&ReasoningEffort::None),
        )
        .expect("flash thinks");
        assert_eq!(flash.budget_tokens, Some(0));
        assert!(!flash.include_thoughts);

        let pro_facts = gemini_model_facts("gemini-2.5-pro");
        let pro = thinking_config(&pro_facts, Some(&ReasoningEffort::None)).expect("pro thinks");
        assert_eq!(
            pro.budget_tokens,
            pro_facts.thinking_budget.map(|range| range.min)
        );
    }

    /// A budget outside the model's published range is rejected, so every tier
    /// has to land inside it and the tiers have to stay ordered.
    #[test]
    fn every_effort_lands_inside_the_models_budget_range() {
        for slug in [
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.5-flash-lite",
        ] {
            let facts = gemini_model_facts(slug);
            let range = facts.thinking_budget.expect("a 2.5 model thinks");

            let budgets: Vec<i64> = [
                ReasoningEffort::Minimal,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ]
            .iter()
            .map(|effort| {
                thinking_config(&facts, Some(effort))
                    .and_then(|config| config.budget_tokens)
                    .unwrap_or_else(|| panic!("{slug} sent no budget for {effort:?}"))
            })
            .collect();

            for budget in &budgets {
                assert!(
                    (range.min..=range.max).contains(budget),
                    "{slug} would send {budget}, outside {range:?}"
                );
            }
            assert!(
                budgets.windows(2).all(|pair| pair[0] <= pair[1]),
                "{slug} spends more on a lower effort: {budgets:?}"
            );
            assert_eq!(
                budgets.last(),
                Some(&range.max),
                "{slug} never reaches its ceiling"
            );
        }
    }

    /// A free-form level names no token count, so the model keeps its default
    /// rather than being handed an invented number.
    #[test]
    fn a_custom_effort_falls_back_to_dynamic_thinking() {
        let config = thinking_config(
            &gemini_model_facts("gemini-2.5-flash"),
            Some(&ReasoningEffort::Custom("bespoke".to_string())),
        )
        .expect("flash thinks");

        assert_eq!(config.budget_tokens, None);
        assert!(config.include_thoughts);
    }
}

#[cfg(test)]
mod budget_curve_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn tiers(min: i64, max: i64) -> Vec<i64> {
        let range = GeminiThinkingBudget { min, max };
        [
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
        ]
        .iter()
        .map(|effort| budget_tokens(range, effort))
        .collect()
    }

    /// The shipped test asserted in-range, non-strict monotonicity and
    /// `last == max` — all of which a total collapse satisfies. A review proved
    /// it survived both deleting the clamp and flattening every tier to one
    /// value, so it constrained nothing.
    #[test]
    fn a_high_floor_does_not_collapse_the_tiers_into_one_value() {
        // Fractions of the CEILING collapsed here: max/4 = 6144 is below
        // min = 20000, so Minimal, Low, Medium and High all clamped to 20000 and
        // four distinct settings produced one wire value.
        let budgets = tiers(20_000, 24_576);
        let distinct: std::collections::BTreeSet<_> = budgets.iter().collect();
        assert!(
            distinct.len() >= 4,
            "a high floor must not flatten the tiers: {budgets:?}"
        );
    }

    #[test]
    fn every_tier_is_strictly_increasing_across_a_normal_range() {
        let budgets = tiers(128, 32_768);
        for pair in budgets.windows(2) {
            assert!(
                pair[0] < pair[1],
                "tiers must be strictly distinct, got {budgets:?}"
            );
        }
    }

    #[test]
    fn every_tier_stays_inside_the_models_range() {
        for (min, max) in [(128, 32_768), (0, 24_576), (512, 512), (20_000, 24_576)] {
            for budget in tiers(min, max) {
                assert!(
                    (min..=max).contains(&budget),
                    "budget {budget} escaped [{min},{max}]"
                );
            }
        }
    }

    #[test]
    fn a_degenerate_range_yields_that_single_value() {
        assert_eq!(tiers(512, 512), vec![512; 5]);
    }

    #[test]
    fn an_inverted_range_does_not_panic() {
        // i64::clamp panics when min > max, so a typo in the facts table used to
        // abort the turn rather than degrade.
        let budgets = tiers(24_576, 512);
        for budget in &budgets {
            assert!((512..=24_576).contains(budget), "{budgets:?}");
        }
    }
}
