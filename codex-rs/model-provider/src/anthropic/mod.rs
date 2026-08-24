//! Runtime provider for the Anthropic Messages API.
//!
//! The catalog is static: Anthropic's `/v1/models` list does not carry the
//! metadata `ModelInfo` needs, and the facts the adapter depends on
//! (output caps, thinking mode, cache minimums) are not published by any
//! endpoint — they live in the fork-side facts table below.

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

const ANTHROPIC_APPROVAL_REVIEW_MODEL: &str = "claude-sonnet-5";
const ANTHROPIC_MEMORY_EXTRACTION_MODEL: &str = "claude-haiku-4-5";
const ANTHROPIC_MEMORY_CONSOLIDATION_MODEL: &str = "claude-sonnet-5";

/// Facts about a Messages API model that no Anthropic endpoint publishes and
/// that the shared `ModelInfo` deliberately does not carry (keeping the
/// protocol structs upstream-identical).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnthropicModelFacts {
    /// The output cap, which the Messages API requires callers to state up
    /// front as `max_tokens`.
    pub max_output_tokens: i64,
    /// Newer families take `thinking: {"type": "adaptive"}`; older ones reject
    /// it and take a fixed `budget_tokens` instead.
    pub supports_adaptive_thinking: bool,
    /// Prefixes shorter than this silently do not cache.
    pub cache_min_prefix_tokens: i64,
    /// Whether `role: "system"` messages may appear mid-conversation.
    pub supports_mid_conversation_system: bool,
}

struct KnownModelFacts {
    /// Matched as a slug prefix, so dated snapshots inherit the base model.
    slug_prefix: &'static str,
    facts: AnthropicModelFacts,
}

/// For a slug no prefix matches: an unknown model is most likely newer than
/// this table, so it gets the current thinking API, the most conservative
/// published cache minimum, and a modest output cap.
const UNKNOWN_MODEL_FACTS: AnthropicModelFacts = AnthropicModelFacts {
    max_output_tokens: 32_000,
    supports_adaptive_thinking: true,
    cache_min_prefix_tokens: 4_096,
    supports_mid_conversation_system: false,
};

/// Sources: Anthropic model docs as of 2026-06 (context windows, output caps,
/// thinking modes, per-model cache minimums, mid-conversation system support).
/// Refreshing this table against the current docs is a sync-checklist item.
const KNOWN_MODEL_FACTS: &[KnownModelFacts] = &[
    KnownModelFacts {
        slug_prefix: "claude-fable-5",
        facts: AnthropicModelFacts {
            max_output_tokens: 128_000,
            supports_adaptive_thinking: true,
            cache_min_prefix_tokens: 512,
            supports_mid_conversation_system: true,
        },
    },
    KnownModelFacts {
        slug_prefix: "claude-mythos-5",
        facts: AnthropicModelFacts {
            max_output_tokens: 128_000,
            supports_adaptive_thinking: true,
            cache_min_prefix_tokens: 512,
            supports_mid_conversation_system: true,
        },
    },
    KnownModelFacts {
        slug_prefix: "claude-opus-5",
        facts: AnthropicModelFacts {
            max_output_tokens: 128_000,
            supports_adaptive_thinking: true,
            cache_min_prefix_tokens: 512,
            supports_mid_conversation_system: true,
        },
    },
    KnownModelFacts {
        slug_prefix: "claude-opus-4-8",
        facts: AnthropicModelFacts {
            max_output_tokens: 128_000,
            supports_adaptive_thinking: true,
            cache_min_prefix_tokens: 1_024,
            supports_mid_conversation_system: true,
        },
    },
    KnownModelFacts {
        slug_prefix: "claude-opus-4-7",
        facts: AnthropicModelFacts {
            max_output_tokens: 128_000,
            supports_adaptive_thinking: true,
            cache_min_prefix_tokens: 2_048,
            supports_mid_conversation_system: false,
        },
    },
    KnownModelFacts {
        slug_prefix: "claude-opus-4-6",
        facts: AnthropicModelFacts {
            max_output_tokens: 128_000,
            supports_adaptive_thinking: true,
            cache_min_prefix_tokens: 4_096,
            supports_mid_conversation_system: false,
        },
    },
    KnownModelFacts {
        slug_prefix: "claude-opus-4-5",
        facts: AnthropicModelFacts {
            max_output_tokens: 64_000,
            supports_adaptive_thinking: false,
            cache_min_prefix_tokens: 4_096,
            supports_mid_conversation_system: false,
        },
    },
    KnownModelFacts {
        slug_prefix: "claude-sonnet-5",
        facts: AnthropicModelFacts {
            max_output_tokens: 128_000,
            supports_adaptive_thinking: true,
            cache_min_prefix_tokens: 1_024,
            supports_mid_conversation_system: false,
        },
    },
    KnownModelFacts {
        slug_prefix: "claude-sonnet-4-6",
        facts: AnthropicModelFacts {
            max_output_tokens: 128_000,
            supports_adaptive_thinking: true,
            cache_min_prefix_tokens: 1_024,
            supports_mid_conversation_system: false,
        },
    },
    KnownModelFacts {
        slug_prefix: "claude-sonnet-4-5",
        facts: AnthropicModelFacts {
            max_output_tokens: 64_000,
            supports_adaptive_thinking: false,
            cache_min_prefix_tokens: 1_024,
            supports_mid_conversation_system: false,
        },
    },
    KnownModelFacts {
        slug_prefix: "claude-haiku-4-5",
        facts: AnthropicModelFacts {
            max_output_tokens: 64_000,
            supports_adaptive_thinking: false,
            cache_min_prefix_tokens: 4_096,
            supports_mid_conversation_system: false,
        },
    },
];

/// Facts for `slug`; longest matching prefix wins, so dated snapshots inherit
/// their base model's row.
pub fn anthropic_model_facts(slug: &str) -> AnthropicModelFacts {
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

const CLAUDE_CONTEXT_WINDOW: i64 = 1_000_000;
const CLAUDE_HAIKU_CONTEXT_WINDOW: i64 = 200_000;

fn effort_presets(include_xhigh: bool) -> Vec<ReasoningEffortPreset> {
    [
        (ReasoningEffort::Low, "Fastest, least thorough", true),
        (ReasoningEffort::Medium, "Balances speed and depth", true),
        (ReasoningEffort::High, "Thorough reasoning", true),
        (
            ReasoningEffort::XHigh,
            "Best for coding and agentic work",
            include_xhigh,
        ),
        (ReasoningEffort::Max, "Maximum depth, slowest", true),
    ]
    .into_iter()
    .filter(|(_, _, included)| *included)
    .map(|(effort, description, _)| ReasoningEffortPreset {
        effort,
        description: description.to_string(),
    })
    .collect()
}

/// `priority` follows catalog order — the first visible entry becomes the
/// default model.
fn anthropic_model(
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
        // Responses API parameter; the Messages API has no equivalent.
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
            anthropic_model(
                "claude-opus-5",
                "Claude Opus 5",
                "Strongest on deep reasoning and long-horizon agentic coding",
                CLAUDE_CONTEXT_WINDOW,
                effort_presets(/*include_xhigh*/ true),
                /*priority*/ 0,
            ),
            anthropic_model(
                "claude-sonnet-5",
                "Claude Sonnet 5",
                "Best combination of speed and intelligence",
                CLAUDE_CONTEXT_WINDOW,
                effort_presets(/*include_xhigh*/ true),
                /*priority*/ 1,
            ),
            anthropic_model(
                "claude-haiku-4-5",
                "Claude Haiku 4.5",
                "Fastest and most cost-effective for simple tasks",
                CLAUDE_HAIKU_CONTEXT_WINDOW,
                /*supported_reasoning_levels*/ Vec::new(),
                /*priority*/ 2,
            ),
            anthropic_model(
                "claude-fable-5",
                "Claude Fable 5",
                "Most capable; for the most demanding reasoning",
                CLAUDE_CONTEXT_WINDOW,
                effort_presets(/*include_xhigh*/ true),
                /*priority*/ 3,
            ),
            anthropic_model(
                "claude-opus-4-8",
                "Claude Opus 4.8",
                "Most capable model of the Opus 4 series",
                CLAUDE_CONTEXT_WINDOW,
                effort_presets(/*include_xhigh*/ true),
                /*priority*/ 4,
            ),
            anthropic_model(
                "claude-opus-4-7",
                "Claude Opus 4.7",
                "Previous-generation Opus",
                CLAUDE_CONTEXT_WINDOW,
                effort_presets(/*include_xhigh*/ true),
                /*priority*/ 5,
            ),
            anthropic_model(
                "claude-opus-4-6",
                "Claude Opus 4.6",
                "Older Opus",
                CLAUDE_CONTEXT_WINDOW,
                effort_presets(/*include_xhigh*/ false),
                /*priority*/ 6,
            ),
            anthropic_model(
                "claude-sonnet-4-6",
                "Claude Sonnet 4.6",
                "Previous-generation Sonnet",
                CLAUDE_CONTEXT_WINDOW,
                effort_presets(/*include_xhigh*/ false),
                /*priority*/ 7,
            ),
        ],
    }
}

/// Runtime provider for the Anthropic Messages API.
#[derive(Clone, Debug)]
pub(crate) struct AnthropicModelProvider {
    info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
}

impl AnthropicModelProvider {
    pub(crate) fn new(
        provider_info: ModelProviderInfo,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        let mut info = provider_info;
        // A user-defined `[model_providers.anthropic]` table need not restate
        // the endpoint or the required version header.
        if info.base_url.is_none() {
            info.base_url = Some(info::ANTHROPIC_DEFAULT_BASE_URL.to_string());
        }
        info.http_headers
            .get_or_insert_default()
            .entry(info::ANTHROPIC_VERSION_HEADER.to_string())
            .or_insert_with(|| info::ANTHROPIC_VERSION.to_string());
        // A first-party credential is never valid against the Messages API, and
        // carrying it makes the catalog path treat this provider as the Codex backend.
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
        // gateway's own auth scheme, not Anthropic's.
        if self.info.has_command_auth() {
            let auth = self.auth().await;
            return resolve_provider_auth(auth.as_ref(), &self.info);
        }
        Ok(Arc::new(auth::anthropic_api_key_auth(&self.info)?))
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

impl ModelProvider for AnthropicModelProvider {
    fn info(&self) -> &ModelProviderInfo {
        &self.info
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            // Namespaced tools are flattened onto the wire, so they stay usable.
            namespace_tools: true,
            // OpenAI-hosted tools with no Messages API counterpart.
            image_generation: false,
            web_search: false,
            external_web_access: false,
            remote_compaction: RemoteCompactionSupport::Unsupported,
        }
    }

    fn approval_review_preferred_model(&self) -> &'static str {
        ANTHROPIC_APPROVAL_REVIEW_MODEL
    }

    fn memory_extraction_preferred_model(&self) -> &'static str {
        ANTHROPIC_MEMORY_EXTRACTION_MODEL
    }

    fn memory_consolidation_preferred_model(&self) -> &'static str {
        ANTHROPIC_MEMORY_CONSOLIDATION_MODEL
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.auth_manager.clone()
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        Box::pin(AnthropicModelProvider::auth(self))
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
        Box::pin(AnthropicModelProvider::api_auth(self))
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

    fn provider() -> AnthropicModelProvider {
        AnthropicModelProvider::new(
            info::create_anthropic_provider(/*base_url*/ None),
            /*auth_manager*/ None,
        )
    }

    #[test]
    fn capabilities_disable_tools_the_messages_api_cannot_serve() {
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
    fn preferred_models_are_anthropic_slugs() {
        let provider = provider();

        for model in [
            provider.approval_review_preferred_model(),
            provider.memory_extraction_preferred_model(),
            provider.memory_consolidation_preferred_model(),
        ] {
            assert!(
                model.starts_with("claude-"),
                "{model} is not servable by the Messages API"
            );
        }
    }

    #[tokio::test]
    async fn openai_auth_is_not_exposed_to_anthropic() {
        let provider = AnthropicModelProvider::new(
            info::create_anthropic_provider(/*base_url*/ None),
            Some(AuthManager::from_auth_for_testing(CodexAuth::from_api_key(
                "openai-api-key",
            ))),
        );

        assert!(provider.auth_manager().is_none());
        assert_eq!(AnthropicModelProvider::auth(&provider).await, None);
        assert_eq!(
            provider.account_state(),
            Ok(ProviderAccountState {
                account: None::<ProviderAccount>,
                requires_openai_auth: false,
            })
        );
    }

    /// A minimal `[model_providers.anthropic]` table gets the endpoint and the
    /// required version header filled in.
    #[test]
    fn a_sparse_user_config_is_normalized() {
        let sparse = ModelProviderInfo {
            wire_api: codex_model_provider_info::WireApi::Anthropic,
            ..ModelProviderInfo::default()
        };

        let provider = AnthropicModelProvider::new(sparse, /*auth_manager*/ None);

        assert_eq!(
            provider.info().base_url.as_deref(),
            Some(info::ANTHROPIC_DEFAULT_BASE_URL)
        );
        assert_eq!(
            provider
                .info()
                .http_headers
                .as_ref()
                .and_then(|headers| headers.get(info::ANTHROPIC_VERSION_HEADER))
                .map(String::as_str),
            Some(info::ANTHROPIC_VERSION),
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
        assert_eq!(slugs[0], "claude-opus-5", "the first entry is the default");
        assert!(slugs.contains(&"claude-sonnet-5"));
        assert!(slugs.contains(&"claude-haiku-4-5"));
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

    /// Every cataloged slug must resolve to a real facts row — the adapter
    /// derives `max_tokens` and the thinking mode from it.
    #[test]
    fn every_catalog_model_has_facts() {
        for model in static_model_catalog().models {
            let facts = anthropic_model_facts(&model.slug);
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
            anthropic_model_facts("claude-opus-5-20260115"),
            anthropic_model_facts("claude-opus-5"),
        );
        assert!(
            !anthropic_model_facts("claude-haiku-4-5").supports_adaptive_thinking,
            "Haiku 4.5 takes a fixed thinking budget, not the adaptive form"
        );
        assert_eq!(
            anthropic_model_facts("claude-sonnet-5").cache_min_prefix_tokens,
            1_024
        );
        assert_eq!(
            anthropic_model_facts("some-unknown-model"),
            UNKNOWN_MODEL_FACTS
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
