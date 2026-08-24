//! Runtime provider for Google's Gemini `generateContent` API.
//!
//! The catalog is static: Gemini's `/v1beta/models` list carries display names
//! and token limits but none of the rest of `ModelInfo`, and the facts the
//! adapter depends on (thinking budget ranges, whether thinking can be turned
//! off, implicit-cache minimums) are not published by any endpoint — they live
//! in the fork-side facts table below.

mod auth;
mod error;

pub(crate) mod info;

use std::path::PathBuf;
use std::sync::Arc;

use codex_api::ApiError;
use codex_api::SharedAuthProvider;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_models_manager::model_info::BASE_INSTRUCTIONS;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelMessages;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::openai_models::WebSearchToolType;
use codex_protocol::openai_models::default_input_modalities;

use crate::auth::auth_manager_for_provider;
use crate::auth::resolve_provider_auth;
use crate::provider::ModelProvider;
use crate::provider::ModelProviderFuture;
use crate::provider::ProviderAccountResult;
use crate::provider::ProviderAccountState;
use crate::provider::ProviderCapabilities;
use crate::provider::RemoteCompactionSupport;

/// The workhorse rather than the top model, matching the Anthropic provider's
/// choice: these run on every turn, and Pro's latency is felt on all of them.
const GEMINI_APPROVAL_REVIEW_MODEL: &str = "gemini-2.5-flash";
const GEMINI_MEMORY_EXTRACTION_MODEL: &str = "gemini-2.5-flash-lite";
const GEMINI_MEMORY_CONSOLIDATION_MODEL: &str = "gemini-2.5-flash";

/// Inclusive `thinkingConfig.thinkingBudget` bounds, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeminiThinkingBudget {
    /// A budget below this is rejected, so an effort level must never map under it.
    pub min: i64,
    pub max: i64,
}

/// Facts about a Gemini model that no `generativelanguage` endpoint publishes
/// and that the shared `ModelInfo` deliberately does not carry (keeping the
/// protocol structs upstream-identical).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeminiModelFacts {
    /// The cap for `generationConfig.maxOutputTokens`. Unlike Anthropic's
    /// `max_tokens` this field is optional on the wire, but a value ABOVE the
    /// model's own cap is a hard 400, so the adapter has to clamp to it.
    pub max_output_tokens: i64,
    /// `None` for a model with no thinking mode at all: sending `thinkingConfig`
    /// to one is an `INVALID_ARGUMENT`, not a silently ignored field.
    pub thinking_budget: Option<GeminiThinkingBudget>,
    /// Whether `thinkingBudget: 0` is accepted. 2.5 Pro cannot stop thinking —
    /// asking it to is a 400 — while Flash and Flash-Lite can.
    pub can_disable_thinking: bool,
    /// Prefixes shorter than this silently do not hit the implicit cache.
    /// `None` where implicit caching is not offered and only explicit
    /// `cachedContent` works, which is a different request shape entirely.
    pub implicit_cache_min_prefix_tokens: Option<i64>,
    /// Whether `systemInstruction` is honoured. Every 2.x model honours it; the
    /// field exists because a 1.x-era slug pointed at a gateway still 400s on it.
    pub supports_system_instruction: bool,
}

struct KnownModelFacts {
    /// Matched as a slug prefix, so dated snapshots (`-preview-05-20`) and
    /// `-latest` aliases inherit the base model.
    slug_prefix: &'static str,
    facts: GeminiModelFacts,
}

/// For a slug no prefix matches: an unknown model is most likely newer than this
/// table, so it gets thinking with the narrowest range every 2.5 model accepts,
/// no `thinkingBudget: 0` (omitting thinking is always safe; disabling it is
/// not), the largest published cache minimum, and the output cap that is the
/// FLOOR of the whole 2.x line — a value under a model's cap merely leaves
/// headroom unused, while a value over it is a hard 400.
const UNKNOWN_MODEL_FACTS: GeminiModelFacts = GeminiModelFacts {
    max_output_tokens: 8_192,
    // Deliberately None, and asymmetric with max_output_tokens above.
    //
    // Guessing LOW on an output cap costs unused headroom. Guessing at all on
    // thinking costs the whole turn: a model with no thinking mode answers
    // `thinkingConfig` with INVALID_ARGUMENT (every 1.x slug, still served on
    // v1beta), and the 3.x line replaced `thinkingBudget` with `thinkingLevel`
    // and rejects the pair. Sending nothing degrades an unknown model to its own
    // default reasoning; sending a budget can fail every request it makes.
    thinking_budget: None,
    can_disable_thinking: false,
    implicit_cache_min_prefix_tokens: Some(2_048),
    supports_system_instruction: true,
};

/// This table is a floor, not a census. It carries the families whose limits
/// were verified against Google's published docs; anything newer resolves to
/// UNKNOWN_MODEL_FACTS, which is deliberately conservative rather than absent.
///
/// It carries no dated provenance claim on purpose: the first version asserted
/// one ("as of <date>") that its own contents contradicted, and a false citation
/// is worse than none because it stops the next reader checking.
/// budget ranges, implicit-cache minimums). Refreshing this table against the
/// current docs is a sync-checklist item.
const KNOWN_MODEL_FACTS: &[KnownModelFacts] = &[
    KnownModelFacts {
        slug_prefix: "gemini-2.5-pro",
        facts: GeminiModelFacts {
            max_output_tokens: 65_536,
            thinking_budget: Some(GeminiThinkingBudget {
                min: 128,
                max: 32_768,
            }),
            // Pro always thinks; `thinkingBudget: 0` is rejected.
            can_disable_thinking: false,
            implicit_cache_min_prefix_tokens: Some(2_048),
            supports_system_instruction: true,
        },
    },
    KnownModelFacts {
        slug_prefix: "gemini-2.5-flash-lite",
        facts: GeminiModelFacts {
            max_output_tokens: 64_000,
            thinking_budget: Some(GeminiThinkingBudget {
                min: 512,
                max: 24_576,
            }),
            can_disable_thinking: true,
            implicit_cache_min_prefix_tokens: Some(1_024),
            supports_system_instruction: true,
        },
    },
    KnownModelFacts {
        slug_prefix: "gemini-2.5-flash",
        facts: GeminiModelFacts {
            max_output_tokens: 65_536,
            thinking_budget: Some(GeminiThinkingBudget {
                min: 0,
                max: 24_576,
            }),
            can_disable_thinking: true,
            implicit_cache_min_prefix_tokens: Some(1_024),
            supports_system_instruction: true,
        },
    },
    KnownModelFacts {
        slug_prefix: "gemini-2.0-flash-lite",
        facts: GeminiModelFacts {
            max_output_tokens: 8_192,
            thinking_budget: None,
            can_disable_thinking: false,
            implicit_cache_min_prefix_tokens: None,
            supports_system_instruction: true,
        },
    },
    KnownModelFacts {
        slug_prefix: "gemini-2.0-flash",
        facts: GeminiModelFacts {
            max_output_tokens: 8_192,
            // The 2.0 family predates thinking; `thinkingConfig` is a 400 here.
            thinking_budget: None,
            can_disable_thinking: false,
            implicit_cache_min_prefix_tokens: None,
            supports_system_instruction: true,
        },
    },
];

/// Facts for `slug`; longest matching prefix wins, so `gemini-2.5-flash-lite`
/// is not swallowed by the `gemini-2.5-flash` row and dated snapshots inherit
/// their base model's.
pub fn gemini_model_facts(slug: &str) -> GeminiModelFacts {
    // A gateway namespaces the model it proxies (`anthropic/claude-opus-5`,
    // `vertex/gemini-2.5-pro`). Discovery keeps that id because it is what the
    // wire accepts, so match on the last segment too rather than treating a
    // proxied model as unknown and handing it the fallback caps.
    let slug = slug.rsplit_once('/').map_or(slug, |(_, suffix)| suffix);
    KNOWN_MODEL_FACTS
        .iter()
        .filter(|known| slug.starts_with(known.slug_prefix))
        .max_by_key(|known| known.slug_prefix.len())
        .map_or(UNKNOWN_MODEL_FACTS, |known| known.facts)
}

/// The whole 2.x line serves a 1M-token context.
const GEMINI_CONTEXT_WINDOW: i64 = 1_048_576;

/// Gemini's reasoning control is a continuous token budget, not a set of named
/// tiers, so the tiers here are the fork's own. `XHigh` is left out
/// deliberately: it exists upstream to distinguish two Anthropic thinking modes,
/// and inventing a fifth budget between High and Max would give the user a
/// choice with no observable difference.
fn effort_presets() -> Vec<ReasoningEffortPreset> {
    [
        (ReasoningEffort::Low, "Fastest, least thorough"),
        (ReasoningEffort::Medium, "Balances speed and depth"),
        (ReasoningEffort::High, "Thorough reasoning"),
        (ReasoningEffort::Max, "Maximum thinking budget, slowest"),
    ]
    .into_iter()
    .map(|(effort, description)| ReasoningEffortPreset {
        effort,
        description: description.to_string(),
    })
    .collect()
}

/// `priority` follows catalog order — the first visible entry becomes the
/// default model.
fn gemini_model(
    slug: &str,
    display_name: &str,
    description: &str,
    context_window: i64,
    supported_reasoning_levels: Vec<ReasoningEffortPreset>,
    priority: i32,
) -> ModelInfo {
    let default_reasoning_level =
        (!supported_reasoning_levels.is_empty()).then_some(ReasoningEffort::High);
    ModelInfo {
        slug: slug.to_string(),
        display_name: display_name.to_string(),
        description: Some(description.to_string()),
        default_reasoning_level,
        supported_reasoning_levels,
        shell_type: ConfigShellToolType::Default,
        visibility: ModelVisibility::List,
        supported_in_api: true,
        priority,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
        availability_nux: None,
        upgrade: None,
        model_messages: Some(ModelMessages {
            instructions_template: Some(BASE_INSTRUCTIONS.to_string()),
            instructions_variables: None,
            approvals: None,
            collaboration_modes: None,
            auto_review: None,
            permissions: None,
            multi_agent: None,
            token_budget: None,
            guardian_v2: None,
        }),
        include_skills_usage_instructions: false,
        include_plugin_usage_instructions: false,
        include_apps_usage_instructions: false,
        // Responses API parameter; Gemini returns thought summaries through
        // `thinkingConfig.includeThoughts`, which is not a summary style.
        supports_reasoning_summary_parameter: false,
        default_reasoning_summary: ReasoningSummary::Auto,
        support_verbosity: false,
        default_verbosity: None,
        apply_patch_tool_type: None,
        web_search_tool_type: WebSearchToolType::Text,
        truncation_policy: TruncationPolicyConfig::bytes(/*limit*/ 10_000),
        supports_image_detail_original: false,
        context_window: Some(context_window),
        max_context_window: Some(context_window),
        auto_compact_token_limit: None,
        comp_hash: None,
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: default_input_modalities(),
        used_fallback_model_metadata: false,
        supports_search_tool: false,
        use_responses_lite: false,
        node_repl_auto_review_required: false,
        node_repl_disabled: false,
        auto_review_model_override: None,
        model_specialty: None,
        tool_mode: None,
        multi_agent_version: None,
    }
}

fn static_model_catalog() -> ModelsResponse {
    ModelsResponse {
        models: vec![
            gemini_model(
                "gemini-2.5-pro",
                "Gemini 2.5 Pro",
                "Strongest on deep reasoning, long context, and agentic coding",
                GEMINI_CONTEXT_WINDOW,
                effort_presets(),
                /*priority*/ 0,
            ),
            gemini_model(
                "gemini-2.5-flash",
                "Gemini 2.5 Flash",
                "Best combination of speed and intelligence",
                GEMINI_CONTEXT_WINDOW,
                effort_presets(),
                /*priority*/ 1,
            ),
            gemini_model(
                "gemini-2.5-flash-lite",
                "Gemini 2.5 Flash-Lite",
                "Fastest and most cost-effective for simple, high-volume tasks",
                GEMINI_CONTEXT_WINDOW,
                effort_presets(),
                /*priority*/ 2,
            ),
            gemini_model(
                "gemini-2.0-flash",
                "Gemini 2.0 Flash",
                "Previous generation; no thinking mode",
                GEMINI_CONTEXT_WINDOW,
                // The 2.0 family has no thinking mode, so offering effort levels
                // would render a control that changes nothing on the wire.
                /*supported_reasoning_levels*/
                Vec::new(),
                /*priority*/ 3,
            ),
            gemini_model(
                "gemini-2.0-flash-lite",
                "Gemini 2.0 Flash-Lite",
                "Previous generation; smallest and cheapest",
                GEMINI_CONTEXT_WINDOW,
                /*supported_reasoning_levels*/ Vec::new(),
                /*priority*/ 4,
            ),
        ],
    }
}

/// Runtime provider for the Gemini `generateContent` API.
#[derive(Clone, Debug)]
pub struct GeminiModelProvider {
    info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
}

impl GeminiModelProvider {
    pub fn new(provider_info: ModelProviderInfo, auth_manager: Option<Arc<AuthManager>>) -> Self {
        let mut info = provider_info;
        // A user-defined `[model_providers.gemini]` table need not restate the
        // endpoint. Unlike Anthropic there is no required version header: the
        // version is a path segment, carried by `base_url` itself.
        if info.base_url.is_none() {
            info.base_url = Some(info::GEMINI_DEFAULT_BASE_URL.to_string());
        }
        // A first-party credential is never valid against Gemini, and carrying it
        // makes the catalog path treat this provider as the Codex backend.
        let auth_manager = if info.has_command_auth() {
            auth_manager_for_provider(auth_manager, &info)
        } else {
            None
        };
        Self { info, auth_manager }
    }

    async fn auth(&self) -> Option<CodexAuth> {
        match self.auth_manager.as_ref() {
            Some(auth_manager) => auth_manager.auth().await,
            None => None,
        }
    }

    async fn api_auth(&self) -> Result<SharedAuthProvider> {
        // A command-backed token keeps the shared bearer path: that is a
        // gateway's own auth scheme, not Google's.
        if self.info.has_command_auth() {
            let auth = self.auth().await;
            return resolve_provider_auth(auth.as_ref(), &self.info);
        }
        Ok(Arc::new(auth::gemini_api_key_auth(&self.info)?))
    }

    fn static_models_manager(
        &self,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        Arc::new(StaticModelsManager::new(
            /*auth_manager*/ None,
            config_model_catalog.unwrap_or_else(static_model_catalog),
        ))
    }
}

impl ModelProvider for GeminiModelProvider {
    fn info(&self) -> &ModelProviderInfo {
        &self.info
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            // Namespaced tools are flattened onto the wire, so they stay usable.
            namespace_tools: true,
            // OpenAI-hosted tools with no `generateContent` counterpart. Gemini
            // does ground on Search, but only through its own `googleSearch`
            // tool, which has no ore `ToolSpec` spelling and which the tool
            // encoder drops — advertising it here would promise a capability the
            // wire never carries.
            image_generation: false,
            web_search: false,
            external_web_access: false,
            remote_compaction: RemoteCompactionSupport::Unsupported,
        }
    }

    fn approval_review_preferred_model(&self) -> &'static str {
        GEMINI_APPROVAL_REVIEW_MODEL
    }

    fn memory_extraction_preferred_model(&self) -> &'static str {
        GEMINI_MEMORY_EXTRACTION_MODEL
    }

    fn memory_consolidation_preferred_model(&self) -> &'static str {
        GEMINI_MEMORY_CONSOLIDATION_MODEL
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.auth_manager.clone()
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        Box::pin(GeminiModelProvider::auth(self))
    }

    fn account_state(&self) -> ProviderAccountResult {
        Ok(ProviderAccountState {
            account: None,
            requires_openai_auth: self.info.requires_openai_auth,
        })
    }

    fn map_api_error(&self, error: ApiError) -> CodexErr {
        error::map_api_error(error)
    }

    fn api_auth(&self) -> ModelProviderFuture<'_, Result<SharedAuthProvider>> {
        Box::pin(GeminiModelProvider::api_auth(self))
    }

    fn models_manager(
        &self,
        _codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        self.static_models_manager(config_model_catalog)
    }

    fn models_manager_without_cache(
        &self,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        self.static_models_manager(config_model_catalog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_login::CodexAuth;
    use codex_protocol::account::ProviderAccount;
    use pretty_assertions::assert_eq;

    fn provider() -> GeminiModelProvider {
        GeminiModelProvider::new(
            info::create_gemini_provider(/*base_url*/ None),
            /*auth_manager*/ None,
        )
    }

    #[test]
    fn capabilities_disable_tools_gemini_cannot_serve() {
        assert_eq!(
            provider().capabilities(),
            ProviderCapabilities {
                namespace_tools: true,
                image_generation: false,
                web_search: false,
                external_web_access: false,
                remote_compaction: RemoteCompactionSupport::Unsupported,
            }
        );
    }

    #[test]
    fn preferred_models_are_gemini_slugs_that_exist_in_the_catalog() {
        let provider = provider();
        let catalog = static_model_catalog();

        for model in [
            provider.approval_review_preferred_model(),
            provider.memory_extraction_preferred_model(),
            provider.memory_consolidation_preferred_model(),
        ] {
            assert!(
                model.starts_with("gemini-"),
                "{model} is not servable by generateContent"
            );
            assert!(
                catalog.models.iter().any(|entry| entry.slug == model),
                "{model} is not in the catalog, so it can never be selected"
            );
        }
    }

    #[tokio::test]
    async fn openai_auth_is_not_exposed_to_gemini() {
        let provider = GeminiModelProvider::new(
            info::create_gemini_provider(/*base_url*/ None),
            Some(AuthManager::from_auth_for_testing(CodexAuth::from_api_key(
                "openai-api-key",
            ))),
        );

        assert!(provider.auth_manager().is_none());
        assert_eq!(GeminiModelProvider::auth(&provider).await, None);
        assert_eq!(
            provider.account_state(),
            Ok(ProviderAccountState {
                account: None::<ProviderAccount>,
                requires_openai_auth: false,
            })
        );
    }

    /// A minimal `[model_providers.gemini]` table gets the endpoint filled in.
    #[test]
    fn a_sparse_user_config_is_normalized() {
        let sparse = ModelProviderInfo {
            wire_api: codex_model_provider_info::WireApi::Gemini,
            ..ModelProviderInfo::default()
        };

        let provider = GeminiModelProvider::new(sparse, /*auth_manager*/ None);

        assert_eq!(
            provider.info().base_url.as_deref(),
            Some(info::GEMINI_DEFAULT_BASE_URL)
        );
    }

    /// A user-configured endpoint must not be replaced by the default.
    #[test]
    fn a_configured_base_url_survives_normalization() {
        let configured = ModelProviderInfo {
            base_url: Some("https://gw.internal/gemini/v1beta".to_string()),
            wire_api: codex_model_provider_info::WireApi::Gemini,
            ..ModelProviderInfo::default()
        };

        let provider = GeminiModelProvider::new(configured, /*auth_manager*/ None);

        assert_eq!(
            provider.info().base_url.as_deref(),
            Some("https://gw.internal/gemini/v1beta")
        );
    }

    #[test]
    fn catalog_lists_current_models_in_priority_order() {
        let catalog = static_model_catalog();

        let slugs: Vec<&str> = catalog
            .models
            .iter()
            .map(|model| model.slug.as_str())
            .collect();
        assert_eq!(slugs[0], "gemini-2.5-pro", "the first entry is the default");
        assert!(slugs.contains(&"gemini-2.5-flash"));
        assert!(slugs.contains(&"gemini-2.0-flash"));
        for (index, model) in catalog.models.iter().enumerate() {
            assert_eq!(model.priority, i32::try_from(index).expect("small index"));
            assert_eq!(model.visibility, ModelVisibility::List);
            assert!(model.supported_in_api);
            assert!(
                model
                    .model_messages
                    .as_ref()
                    .is_some_and(|messages| messages.instructions_template.is_some()),
                "{} would render empty model instructions",
                model.slug
            );
        }
    }

    /// Offering an effort control for a model with no thinking mode would render
    /// a setting that changes nothing on the wire.
    #[test]
    fn only_thinking_models_offer_reasoning_levels() {
        for model in static_model_catalog().models {
            let facts = gemini_model_facts(&model.slug);
            assert_eq!(
                facts.thinking_budget.is_some(),
                !model.supported_reasoning_levels.is_empty(),
                "{} disagrees with its facts row about thinking",
                model.slug
            );
            assert_eq!(
                model.default_reasoning_level.is_some(),
                !model.supported_reasoning_levels.is_empty(),
                "{} has a default effort it does not list",
                model.slug
            );
        }
    }

    /// Every cataloged slug must resolve to a real facts row — the adapter
    /// derives the output cap and the thinking config from it.
    #[test]
    fn every_catalog_model_has_facts() {
        for model in static_model_catalog().models {
            let facts = gemini_model_facts(&model.slug);
            assert_ne!(
                facts, UNKNOWN_MODEL_FACTS,
                "{} fell through to the unknown-model fallback",
                model.slug
            );
        }
    }

    #[test]
    fn facts_match_by_longest_prefix_so_snapshots_inherit() {
        assert_eq!(
            gemini_model_facts("gemini-2.5-pro-preview-06-05"),
            gemini_model_facts("gemini-2.5-pro"),
        );
        assert_eq!(
            gemini_model_facts("some-unknown-model"),
            UNKNOWN_MODEL_FACTS
        );
    }

    /// `gemini-2.5-flash-lite` starts with `gemini-2.5-flash`; a shortest-match
    /// lookup would hand it Flash's budget floor of 0, and a `thinkingBudget: 0`
    /// that Flash-Lite's 512 minimum rejects.
    #[test]
    fn flash_lite_is_not_swallowed_by_the_flash_row() {
        let lite = gemini_model_facts("gemini-2.5-flash-lite");
        let flash = gemini_model_facts("gemini-2.5-flash");

        assert_ne!(lite, flash);
        assert_eq!(lite.thinking_budget.map(|budget| budget.min), Some(512));
        assert_eq!(flash.thinking_budget.map(|budget| budget.min), Some(0));
        assert_eq!(
            gemini_model_facts("gemini-2.0-flash-lite").thinking_budget,
            None
        );
    }

    /// Sending `thinkingBudget: 0` to Pro is a 400, not a quieter model.
    #[test]
    fn pro_thinks_unconditionally_and_the_two_zero_family_does_not_think_at_all() {
        let pro = gemini_model_facts("gemini-2.5-pro");
        assert!(pro.thinking_budget.is_some());
        assert!(!pro.can_disable_thinking);

        let legacy = gemini_model_facts("gemini-2.0-flash");
        assert_eq!(legacy.thinking_budget, None);
        assert_eq!(legacy.implicit_cache_min_prefix_tokens, None);
    }

    /// An over-cap `maxOutputTokens` is a hard 400, so the fallback must be the
    /// floor of the line, never a guess above it.
    #[test]
    fn the_unknown_model_output_cap_is_the_family_floor() {
        let floor = KNOWN_MODEL_FACTS
            .iter()
            .map(|known| known.facts.max_output_tokens)
            .min()
            .expect("the table is not empty");

        assert_eq!(UNKNOWN_MODEL_FACTS.max_output_tokens, floor);
        assert!(
            !gemini_model_facts("gemini-9-unreleased").can_disable_thinking,
            "omitting thinking is always safe for an unknown model; disabling it is not"
        );
    }

    #[test]
    fn the_models_manager_serves_the_static_catalog() {
        let provider = provider();

        let manager =
            provider.models_manager(std::env::temp_dir(), /*config_model_catalog*/ None);

        let models = manager
            .try_get_remote_models()
            .expect("static manager never blocks");
        assert_eq!(models.len(), static_model_catalog().models.len());
    }
}

#[cfg(test)]
mod unknown_facts_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Names the value literally rather than comparing the constant to itself.
    ///
    /// The sibling assertion is `gemini_model_facts("unknown") == UNKNOWN_MODEL_FACTS`,
    /// which holds however the constant is defined: a review restored the old
    /// `Some(128..24576)` and all 190 tests still passed.
    #[test]
    fn an_unrecognised_model_is_sent_no_thinking_config() {
        assert_eq!(
            UNKNOWN_MODEL_FACTS.thinking_budget, None,
            "guessing a budget costs the whole turn: a model with no thinking mode \
             answers thinkingConfig with INVALID_ARGUMENT, and the 3.x line replaced \
             thinkingBudget with thinkingLevel and rejects the pair. Sending nothing \
             only costs reasoning."
        );
        assert_eq!(gemini_model_facts("gemini-1.5-pro").thinking_budget, None);
        assert_eq!(
            gemini_model_facts("gemini-3-pro-preview").thinking_budget,
            None
        );
    }
}
