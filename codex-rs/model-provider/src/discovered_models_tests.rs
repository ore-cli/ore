//! Tests for gateway model discovery.
//!
//! These are written to fail against the behavior they replaced: a picker that
//! serves the static catalog no matter what the provider says. Where a weaker
//! assertion (a `contains`, a length check) would also pass against the static
//! catalog, the assertion here is on the exact merged list, in order.

use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use codex_http_client::OutboundProxyPolicy;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;
use wiremock::matchers::query_param_is_missing;

use super::*;
use crate::provider::create_model_provider;

fn factory() -> HttpClientFactory {
    HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
}

fn discovered(id: &str) -> DiscoveredModel {
    DiscoveredModel {
        id: id.to_string(),
        display_name: None,
    }
}

/// A static entry whose every interesting field differs from what synthesis
/// would produce, so "kept the static metadata" cannot pass by coincidence.
fn static_entry(slug: &str, priority: i32) -> ModelInfo {
    ModelInfo {
        display_name: format!("Curated {slug}"),
        description: Some(format!("the catalog description of {slug}")),
        default_reasoning_level: Some(ReasoningEffort::High),
        supported_reasoning_levels: vec![ReasoningEffortPreset {
            effort: ReasoningEffort::High,
            description: "Thorough reasoning".to_string(),
        }],
        visibility: ModelVisibility::List,
        priority,
        context_window: Some(400_000),
        max_context_window: Some(400_000),
        used_fallback_model_metadata: false,
        ..model_info_from_slug(slug)
    }
}

/// The slugs a user is OFFERED.
///
/// A static entry the gateway did not list stays in the catalog carrying its
/// metadata, but hidden -- deleting it made `get_model_info` fall through to a
/// slug-shaped guess and hand a pinned model a context window its provider does
/// not have. These assertions are about the picker, so they filter to what the
/// picker shows.
fn slugs(models: &[ModelInfo]) -> Vec<&str> {
    models
        .iter()
        .filter(|model| model.visibility != ModelVisibility::Hide)
        .map(|model| model.slug.as_str())
        .collect()
}

/// Every slug in the catalog, visible or not — for assertions about metadata
/// retention rather than about the picker.
fn all_slugs(models: &[ModelInfo]) -> Vec<&str> {
    models.iter().map(|model| model.slug.as_str()).collect()
}

#[derive(Debug, Clone)]
enum FakeOutcome {
    Models(Vec<DiscoveredModel>),
    Failure,
}

#[derive(Debug)]
struct FakeDiscovery {
    outcome: FakeOutcome,
    calls: AtomicUsize,
}

impl FakeDiscovery {
    fn serving(ids: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            outcome: FakeOutcome::Models(ids.iter().map(|id| discovered(id)).collect()),
            calls: AtomicUsize::new(0),
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            outcome: FakeOutcome::Failure,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ModelListDiscovery for FakeDiscovery {
    fn discover(
        &self,
        _http_client_factory: HttpClientFactory,
    ) -> DiscoveryFuture<'_, Result<Vec<DiscoveredModel>, DiscoveryError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let outcome = self.outcome.clone();
        Box::pin(async move {
            match outcome {
                FakeOutcome::Models(models) => Ok(models),
                FakeOutcome::Failure => Err(DiscoveryError::new("gateway said no")),
            }
        })
    }
}

fn manager_over(
    static_models: Vec<ModelInfo>,
    discovery: Arc<dyn ModelListDiscovery>,
) -> DiscoveringModelsManager {
    DiscoveringModelsManager::new(
        Arc::new(StaticModelsManager::new(
            /*auth_manager*/ None,
            ModelsResponse {
                models: static_models,
            },
        )),
        discovery,
        /*auth_manager*/ None,
    )
}

#[test]
fn a_discovered_slug_that_matches_a_static_entry_keeps_the_static_metadata() {
    let known = static_entry("gpt-5.3-codex", 0);

    let merged = merge_catalog(vec![known.clone()], &[discovered("gpt-5.3-codex")]);

    // Whole-struct equality: reasoning levels, context window, display name,
    // description and priority all have to survive discovery untouched.
    assert_eq!(merged, vec![known]);
}

#[test]
fn a_discovered_slug_the_catalog_does_not_know_is_still_selectable() {
    let merged = merge_catalog(
        vec![static_entry("gpt-5.3-codex", 0)],
        &[DiscoveredModel {
            id: "qwen3-coder-480b".to_string(),
            display_name: Some("Qwen3 Coder 480B".to_string()),
        }],
    );

    assert_eq!(slugs(&merged), vec!["qwen3-coder-480b"]);
    let synthesized = &merged[0];
    assert_eq!(synthesized.display_name, "Qwen3 Coder 480B");
    assert_eq!(
        synthesized.visibility,
        ModelVisibility::List,
        "a discovered model that is hidden from the picker was not discovered at all"
    );
    assert!(synthesized.supported_in_api);
    assert!(
        synthesized
            .model_messages
            .as_ref()
            .is_some_and(|messages| messages.instructions_template.is_some()),
        "a model with no instruction template would run with empty instructions"
    );
    assert_eq!(
        synthesized.description, None,
        "inheriting the template's description would describe a different model"
    );
    assert!(
        synthesized.supported_reasoning_levels.is_empty()
            && synthesized.default_reasoning_level.is_none(),
        "an unknown model must not be sent a reasoning parameter it may reject"
    );
    assert!(synthesized.used_fallback_model_metadata);
}

#[test]
fn a_static_model_the_provider_does_not_serve_is_dropped() {
    let merged = merge_catalog(
        vec![static_entry("gpt-5.3-codex", 0), static_entry("gpt-5.1", 1)],
        &[discovered("gpt-5.1")],
    );

    assert_eq!(
        slugs(&merged),
        vec!["gpt-5.1"],
        "a curated gateway's omission is the whole signal"
    );
}

#[test]
fn known_models_keep_catalog_order_and_unknown_ones_follow() {
    let merged = merge_catalog(
        vec![static_entry("gpt-5.3-codex", 0), static_entry("gpt-5.1", 1)],
        // Deliberately listed with the unknown model first and the catalog's
        // default model last.
        &[
            discovered("local-llama"),
            discovered("gpt-5.1"),
            discovered("gpt-5.3-codex"),
        ],
    );

    assert_eq!(
        slugs(&merged),
        vec!["gpt-5.3-codex", "gpt-5.1", "local-llama"]
    );
}

#[test]
fn a_namespaced_gateway_id_inherits_the_metadata_of_the_model_it_proxies() {
    let known = static_entry("gpt-5.3-codex", 0);

    let merged = merge_catalog(vec![known.clone()], &[discovered("openai/gpt-5.3-codex")]);

    assert_eq!(
        merged,
        vec![ModelInfo {
            // The wire needs the gateway's id...
            slug: "openai/gpt-5.3-codex".to_string(),
            // ...and everything else stays the metadata we trust.
            ..known
        }]
    );
}

#[test]
fn a_repeated_id_is_listed_once() {
    let merged = merge_catalog(
        vec![static_entry("gpt-5.1", 0)],
        &[discovered("gpt-5.1"), discovered("gpt-5.1")],
    );

    assert_eq!(slugs(&merged), vec!["gpt-5.1"]);
}

#[tokio::test]
async fn an_offline_listing_never_reaches_the_provider() {
    let discovery = FakeDiscovery::serving(&["gateway-only"]);
    let manager = manager_over(
        vec![static_entry("gpt-5.1", 0)],
        Arc::clone(&discovery) as _,
    );

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Offline, factory())
        .await;

    assert_eq!(
        discovery.calls(),
        0,
        "offline promised not to send anything"
    );
    assert_eq!(slugs(&catalog.models), vec!["gpt-5.1"]);
}

#[tokio::test]
async fn an_online_listing_serves_what_the_provider_lists() {
    let discovery = FakeDiscovery::serving(&["gpt-5.1", "gateway-only"]);
    let manager = manager_over(
        vec![static_entry("gpt-5.1", 0)],
        Arc::clone(&discovery) as _,
    );

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(discovery.calls(), 1);
    assert_eq!(slugs(&catalog.models), vec!["gpt-5.1", "gateway-only"]);
}

#[tokio::test]
async fn a_failed_discovery_yields_exactly_the_static_catalog() {
    let static_models = vec![static_entry("gpt-5.3-codex", 0), static_entry("gpt-5.1", 1)];
    let manager = manager_over(static_models.clone(), FakeDiscovery::failing());

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(catalog.models, static_models);
}

#[tokio::test]
async fn a_provider_that_lists_nothing_yields_the_static_catalog() {
    let static_models = vec![static_entry("gpt-5.1", 0)];
    let manager = manager_over(static_models.clone(), FakeDiscovery::serving(&[]));

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(
        catalog.models, static_models,
        "an empty picker is worse than a stale one"
    );
}

#[tokio::test]
async fn a_later_failure_does_not_leave_the_picker_disagreeing_with_itself() {
    let static_models = vec![static_entry("gpt-5.1", 0)];
    let discovery = Arc::new(SequencedDiscovery::new(vec![
        FakeOutcome::Models(vec![discovered("gateway-only")]),
        FakeOutcome::Failure,
    ]));
    let manager = manager_over(static_models.clone(), Arc::clone(&discovery) as _);

    let merged = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;
    assert_eq!(slugs(&merged.models), vec!["gateway-only"]);

    let fallback = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(fallback.models, static_models);
    assert_eq!(
        manager.get_remote_models().await,
        static_models,
        "metadata lookups must see the same catalog the picker does"
    );
}

/// Discovery that answers differently on each call, for state-over-time checks.
#[derive(Debug)]
struct SequencedDiscovery {
    outcomes: Mutex<std::collections::VecDeque<FakeOutcome>>,
}

impl SequencedDiscovery {
    fn new(outcomes: Vec<FakeOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into()),
        }
    }
}

impl ModelListDiscovery for SequencedDiscovery {
    fn discover(
        &self,
        _http_client_factory: HttpClientFactory,
    ) -> DiscoveryFuture<'_, Result<Vec<DiscoveredModel>, DiscoveryError>> {
        let outcome = self
            .outcomes
            .lock()
            .expect("sequenced discovery lock should not be poisoned")
            .pop_front()
            .unwrap_or(FakeOutcome::Failure);
        Box::pin(async move {
            match outcome {
                FakeOutcome::Models(models) => Ok(models),
                FakeOutcome::Failure => Err(DiscoveryError::new("gateway said no")),
            }
        })
    }
}

#[tokio::test]
async fn a_cached_listing_does_not_ask_twice() {
    let discovery = FakeDiscovery::serving(&["gateway-only"]);
    let manager = manager_over(
        vec![static_entry("gpt-5.1", 0)],
        Arc::clone(&discovery) as _,
    );

    for _ in 0..3 {
        let catalog = manager
            .raw_model_catalog(RefreshStrategy::OnlineIfUncached, factory())
            .await;
        assert_eq!(slugs(&catalog.models), vec!["gateway-only"]);
    }

    assert_eq!(discovery.calls(), 1);
}

#[tokio::test]
async fn the_picker_lists_discovered_models() {
    let discovery = FakeDiscovery::serving(&["gpt-5.1", "gateway-only"]);
    let manager = manager_over(vec![static_entry("gpt-5.1", 0)], discovery);

    let presets = manager
        .list_models(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(
        presets
            .iter()
            .map(|preset| preset.model.as_str())
            .collect::<Vec<_>>(),
        vec!["gpt-5.1", "gateway-only"],
        "the merged catalog has to survive preset filtering to be selectable"
    );
}

fn gateway_provider_info(base_url: &str) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "litellm".to_string(),
        base_url: Some(base_url.to_string()),
        wire_api: WireApi::Chat,
        requires_openai_auth: false,
        experimental_bearer_token: Some("gateway-token".to_string()),
        ..ModelProviderInfo::default()
    }
}

fn anthropic_gateway_info(base_url: &str) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "anthropic".to_string(),
        base_url: Some(base_url.to_string()),
        wire_api: WireApi::Anthropic,
        requires_openai_auth: false,
        experimental_bearer_token: Some("gateway-token".to_string()),
        ..ModelProviderInfo::default()
    }
}

/// The catalog the provider would serve with no discovery at all: `Offline`
/// never opens a socket, so this is the static list by construction.
async fn static_catalog_of(manager: &SharedModelsManager) -> Vec<ModelInfo> {
    manager
        .raw_model_catalog(RefreshStrategy::Offline, factory())
        .await
        .models
}

#[tokio::test]
async fn an_openai_compatible_gateway_list_decides_which_models_are_offered() {
    let server = MockServer::start().await;
    let provider = create_model_provider(
        gateway_provider_info(&server.uri()),
        /*auth_manager*/ None,
    );
    let manager = provider.models_manager_without_cache(/*config_model_catalog*/ None);
    let static_models = static_catalog_of(&manager).await;
    let known = static_models
        .first()
        .map(|model| model.slug.clone())
        .expect("the bundled catalog is not empty");

    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer gateway-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [
                {"id": "gateway-only-model", "object": "model"},
                {"id": known, "object": "model"},
                {"id": "text-embedding-3-large", "object": "embedding"},
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(
        slugs(&catalog.models),
        vec![known.as_str(), "gateway-only-model"],
        "non-model entries are ignored and the gateway's list is the catalog"
    );
    assert_eq!(
        catalog.models.first(),
        static_models.first(),
        "a listed model keeps its catalog metadata"
    );
}

#[tokio::test]
async fn an_anthropic_gateway_list_is_read_with_the_credential_that_wire_expects() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        // The Messages API does not read `Authorization`.
        .and(header("x-api-key", "gateway-token"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "claude-opus-5", "display_name": "Claude Opus 5", "type": "model"},
                {"id": "claude-neue-9", "display_name": "Claude Neue 9", "type": "model"},
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = create_model_provider(
        anthropic_gateway_info(&server.uri()),
        /*auth_manager*/ None,
    );
    let manager = provider.models_manager_without_cache(/*config_model_catalog*/ None);
    let static_models = static_catalog_of(&manager).await;

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(
        slugs(&catalog.models),
        vec!["claude-opus-5", "claude-neue-9"],
        "the shipped Claude catalog is longer than this; only what the gateway serves is offered"
    );
    assert!(
        static_models.len() > slugs(&catalog.models).len(),
        "this gateway is curated, so discovery must have narrowed what is OFFERED. \
         The unlisted entries remain in the catalog, hidden, so a model the user \
         pinned keeps its real metadata instead of falling through to a \
         slug-shaped guess."
    );
    assert_eq!(
        catalog.models[0].display_name, "Claude Opus 5",
        "a listed model keeps its catalog metadata"
    );
    assert!(
        !catalog.models[0].supported_reasoning_levels.is_empty(),
        "the static reasoning levels are the point of keeping static metadata"
    );
    assert_eq!(catalog.models[1].display_name, "Claude Neue 9");
}

#[tokio::test]
async fn a_gateway_without_a_models_endpoint_keeps_the_static_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;

    let provider = create_model_provider(
        gateway_provider_info(&server.uri()),
        /*auth_manager*/ None,
    );
    let manager = provider.models_manager_without_cache(/*config_model_catalog*/ None);
    let static_models = static_catalog_of(&manager).await;

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(catalog.models, static_models);
    assert!(!catalog.models.is_empty());
}

#[tokio::test]
async fn a_gateway_that_answers_with_nonsense_keeps_the_static_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("<html><body>login required</body></html>"),
        )
        .mount(&server)
        .await;

    let provider = create_model_provider(
        gateway_provider_info(&server.uri()),
        /*auth_manager*/ None,
    );
    let manager = provider.models_manager_without_cache(/*config_model_catalog*/ None);
    let static_models = static_catalog_of(&manager).await;

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(catalog.models, static_models);
}

#[tokio::test]
async fn a_json_body_that_is_not_a_model_list_keeps_the_static_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"error": "unauthorized"})))
        .mount(&server)
        .await;

    let provider = create_model_provider(
        gateway_provider_info(&server.uri()),
        /*auth_manager*/ None,
    );
    let manager = provider.models_manager_without_cache(/*config_model_catalog*/ None);
    let static_models = static_catalog_of(&manager).await;

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(catalog.models, static_models);
}

#[tokio::test]
async fn a_gateway_that_never_answers_keeps_the_static_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_json(json!({"data": [{"id": "never-arrives", "object": "model"}]})),
        )
        .mount(&server)
        .await;

    let provider = create_model_provider(
        gateway_provider_info(&server.uri()),
        /*auth_manager*/ None,
    );
    let static_models = static_catalog_of(&provider.models_manager_without_cache(None)).await;
    let manager = DiscoveringModelsManager::new(
        Arc::new(StaticModelsManager::new(
            /*auth_manager*/ None,
            ModelsResponse {
                models: static_models.clone(),
            },
        )),
        Arc::new(ProviderModelListDiscovery::with_timeout(
            provider,
            Duration::from_millis(250),
        )),
        /*auth_manager*/ None,
    );

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(catalog.models, static_models);
}

#[tokio::test]
async fn a_catalog_pinned_in_config_is_not_second_guessed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gateway-only-model", "object": "model"}]
        })))
        .expect(0)
        .mount(&server)
        .await;

    let provider = create_model_provider(
        gateway_provider_info(&server.uri()),
        /*auth_manager*/ None,
    );
    let pinned = ModelsResponse {
        models: vec![static_entry("only-this-one", 0)],
    };

    let catalog = provider
        .models_manager_without_cache(Some(pinned.clone()))
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(catalog.models, pinned.models);
}

/// The upstream models client already fetches a full catalog for a first-party
/// credential, so discovery must stay out of its way -- one `/models` request
/// per listing, not two.
#[tokio::test]
async fn a_codex_backend_credential_is_not_asked_twice() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param(
            "client_version",
            codex_models_manager::client_version_to_whole(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": []
        })))
        .expect(1)
        .mount(&server)
        .await;
    // Discovery sends no `client_version`, so this mock matches its request and
    // nothing else.
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(query_param_is_missing("client_version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gateway-only-model", "object": "model"}]
        })))
        .expect(0)
        .mount(&server)
        .await;

    let provider = create_model_provider(
        gateway_provider_info(&server.uri()),
        Some(AuthManager::from_auth_for_testing(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        )),
    );

    let catalog = provider
        .models_manager_without_cache(/*config_model_catalog*/ None)
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert!(
        !catalog.models.is_empty(),
        "the upstream catalog still has to reach the picker"
    );
}

#[test]
fn the_first_party_provider_is_left_exactly_as_it_was() {
    let provider = create_model_provider(
        ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        /*auth_manager*/ None,
    );

    let wrapped = with_model_discovery(Arc::clone(&provider));

    assert!(
        Arc::ptr_eq(&provider, &wrapped),
        "the Codex backend already serves an authoritative catalog"
    );
}

#[tokio::test]
async fn discovery_does_not_change_anything_else_about_a_provider() {
    let info = anthropic_gateway_info("http://127.0.0.1:1");
    let undecorated: SharedModelProvider = Arc::new(crate::anthropic::AnthropicModelProvider::new(
        info.clone(),
        /*auth_manager*/ None,
    ));
    let decorated = create_model_provider(info, /*auth_manager*/ None);

    assert!(
        !Arc::ptr_eq(&undecorated, &decorated),
        "this provider is supposed to be discovering"
    );
    assert_eq!(decorated.capabilities(), undecorated.capabilities());
    assert_eq!(
        decorated.approval_review_preferred_model(),
        undecorated.approval_review_preferred_model()
    );
    assert_eq!(
        decorated.memory_extraction_preferred_model(),
        undecorated.memory_extraction_preferred_model()
    );
    assert_eq!(
        decorated.memory_consolidation_preferred_model(),
        undecorated.memory_consolidation_preferred_model()
    );
    assert_eq!(decorated.info().wire_api, undecorated.info().wire_api);
    assert_eq!(decorated.info().base_url, undecorated.info().base_url);
    assert_eq!(decorated.account_state(), undecorated.account_state());
    assert_eq!(
        decorated.supports_attestation(),
        undecorated.supports_attestation()
    );
}

/// A provider that cannot be reached at all must still produce a picker.
#[tokio::test]
async fn an_unreachable_provider_keeps_the_static_catalog() {
    let provider = create_model_provider(
        // Port 1 is never listening.
        gateway_provider_info("http://127.0.0.1:1/v1"),
        /*auth_manager*/ None,
    );
    let manager = provider.models_manager_without_cache(/*config_model_catalog*/ None);
    let static_models = static_catalog_of(&manager).await;

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(catalog.models, static_models);
    assert!(!catalog.models.is_empty());
}

#[test]
fn a_gemini_list_entry_loses_its_models_prefix() {
    // Google's ListModels names every entry `models/<slug>`. Keeping the prefix
    // sent `models/models/<slug>:streamGenerateContent` on the wire AND missed the
    // prefix-keyed facts table, cutting max_output_tokens from 65536 to the
    // unknown-model default of 8192 on every turn.
    let payload: DiscoveryPayload = serde_json::from_str(
        r#"{"models":[{"name":"models/gemini-2.5-pro","displayName":"Gemini 2.5 Pro"}]}"#,
    )
    .expect("parses");
    let models = payload.into_models();
    assert_eq!(models.len(), 1);
    assert_eq!(
        models[0].id, "gemini-2.5-pro",
        "the models/ prefix must be stripped"
    );
    assert_eq!(
        models[0].display_name.as_deref(),
        Some("Gemini 2.5 Pro"),
        "Gemini sends camelCase displayName; without the alias the picker shows the raw id"
    );
}

#[test]
fn a_namespaced_gateway_id_keeps_its_namespace() {
    // The other side of the prefix rule: `anthropic/claude-opus-5` is exactly what
    // LiteLLM accepts on the wire and must survive verbatim.
    let payload: DiscoveryPayload =
        serde_json::from_str(r#"{"data":[{"id":"anthropic/claude-opus-5"}]}"#).expect("parses");
    let models = payload.into_models();
    assert_eq!(models[0].id, "anthropic/claude-opus-5");
}

#[tokio::test]
async fn a_model_the_gateway_stopped_listing_keeps_its_metadata() {
    // LM Studio lists only the models it has LOADED, so a pinned model leaves the
    // list the moment it is unloaded -- and a resumed thread can carry a slug the
    // gateway no longer serves. The model must leave the PICKER (that omission is
    // the curated gateway's whole signal) while its metadata stays resolvable:
    // otherwise get_model_info guesses from the slug and hands a 128k model an
    // OpenAI-shaped 272k window, so auto-compaction sizes against a limit the
    // provider does not have and the turn fails instead of compacting.
    let static_models = vec![
        static_entry("kept-model", 1),
        static_entry("unloaded-model", 2),
    ];
    let manager = manager_over(static_models, FakeDiscovery::serving(&["kept-model"]));

    let catalog = manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;
    assert_eq!(
        slugs(&catalog.models),
        vec!["kept-model"],
        "the gateway's list decides what is offered"
    );

    let info = manager
        .get_model_info("unloaded-model", &ModelsManagerConfig::default())
        .await;
    assert_eq!(
        info.context_window,
        Some(400_000),
        "a model the gateway stopped listing keeps its real limits, not a slug-shaped guess"
    );
    assert_eq!(info.display_name, "Curated unloaded-model");
    assert!(
        !info.used_fallback_model_metadata,
        "resolving it must not be recorded as a metadata fallback"
    );
}

#[tokio::test]
async fn an_unreachable_gateway_is_not_re_probed_on_every_session() {
    // Failure clears `merged`, and OnlineIfUncached only short-circuits on a
    // present value -- so without remembering the failure an unreachable gateway
    // cost the full discovery timeout on EVERY root thread start, forever.
    let discovery = FakeDiscovery::failing();
    let manager = manager_over(
        vec![static_entry("kept-model", 1)],
        Arc::clone(&discovery) as _,
    );

    for _ in 0..3 {
        let catalog = manager
            .raw_model_catalog(RefreshStrategy::OnlineIfUncached, factory())
            .await;
        assert_eq!(slugs(&catalog.models), vec!["kept-model"]);
    }

    assert_eq!(
        discovery.calls(),
        1,
        "a remembered failure must not be re-probed by every session"
    );
}

#[tokio::test]
async fn an_explicit_refresh_retries_after_a_failure() {
    // The negative cache must never make a failure permanent.
    let discovery = FakeDiscovery::failing();
    let manager = manager_over(
        vec![static_entry("kept-model", 1)],
        Arc::clone(&discovery) as _,
    );

    manager
        .raw_model_catalog(RefreshStrategy::OnlineIfUncached, factory())
        .await;
    manager
        .raw_model_catalog(RefreshStrategy::Online, factory())
        .await;

    assert_eq!(
        discovery.calls(),
        2,
        "an explicit Online refresh is the user asking again and must re-probe"
    );
}
