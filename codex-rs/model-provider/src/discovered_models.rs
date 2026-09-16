//! Catalog discovery for providers whose model list is not knowable in advance.
//!
//! The shipped catalog is a good description of the models Ore knows how to
//! drive, and a bad description of the models a given endpoint actually serves.
//! A gateway -- LiteLLM, vLLM, LM Studio, OpenRouter, an internal proxy -- serves
//! a curated set that nobody upstream can enumerate, so a static picker shows
//! models the gateway will reject and hides the ones it exists to offer.
//!
//! So the provider's own `/models` list decides WHICH models exist and the
//! static catalog decides what is KNOWN about them. Both `/models` shapes this
//! fork speaks (Anthropic's `data[].id` + `display_name`, and the
//! OpenAI-compatible `data[].id` + `object`) are read by one lenient parser, and
//! credentials, base URL and provider headers all come from the wrapped
//! provider's own `api_provider`/`api_auth` -- so adding another wire is a line
//! in [`discovery_applies`], not another endpoint implementation.
//!
//! Every failure path -- unreachable host, 404, 500, timeout, HTML error page,
//! JSON that is not a model list, an empty list -- returns the static catalog
//! unchanged. A stale picker is a mild annoyance; an empty picker is a client
//! that cannot start a conversation at all.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use codex_api::ApiError;
use codex_api::Provider;
use codex_api::SharedAuthProvider;
use codex_api::TransportError;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::default_client::create_client_for_route_async;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::cache::ModelsCache;
use codex_models_manager::manager::ModelsManager;
use codex_models_manager::manager::ModelsManagerFuture;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::construct_model_info_from_candidates;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CoreResult;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use http::HeaderName;
use serde::Deserialize;
use tokio::sync::RwLock;
use tokio::sync::TryLockError;
use tokio::time::timeout;
use tracing::info;
use tracing::warn;

use crate::auth::ProviderAuthScope;
use crate::auth::ResolvedProviderAuth;
use crate::provider::ModelProvider;
use crate::provider::ModelProviderFuture;
use crate::provider::ProviderAccountResult;
use crate::provider::ProviderCapabilities;
use crate::provider::ProviderUnauthorizedRecovery;
use crate::provider::SharedModelProvider;

/// Both wires expose the list at `{base_url}/models`.
const DISCOVERY_PATH: &str = "/models";

/// LiteLLM's per-deployment metadata. Read alongside the list: it is the only
/// place a gateway states which reasoning efforts a model accepts, and it is a
/// usable list on its own where `/models` is not served.
const INFO_PATH: &str = "/model/info";

/// Discovery is a startup-blocking side quest, not the request path: a gateway
/// that cannot answer in this long is treated as having no list at all.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Shaped like `codex_ollama`'s connection error: problem, fix, consequence.
///
/// Unlike that one it warns instead of aborting. `--oss` can abort because a
/// missing local server leaves nothing to run; `/models` is optional, and a
/// gateway that does not implement it answers 404 on a healthy setup.
const DISCOVERY_FAILED_MESSAGE: &str = concat!(
    "Could not read the model list from this provider. ",
    "Check that its base_url is reachable and that the credential in its env_key can list models. ",
    "Falling back to the models in the bundled catalog.",
);

const INFO_FAILED_MESSAGE: &str = concat!(
    "Could not read this provider's model metadata, so any reasoning levels or ",
    "context windows it declares will not be applied this session. ",
    "Check that its base_url serves /model/info; a gateway without it is fine.",
);

const LIST_REPLACED_MESSAGE: &str = concat!(
    "This provider does not serve /models; listing its models from /model/info ",
    "instead.",
);

/// Split from [`DISCOVERY_FAILED_MESSAGE`] because the remediation differs: the
/// endpoint answered, so the endpoint is not what to go and look at.
const EMPTY_LIST_MESSAGE: &str = concat!(
    "This provider's model list is empty, so it reported serving no models at all. ",
    "Check that the gateway has models configured and that this credential may see them. ",
    "Falling back to the models in the bundled catalog.",
);

/// One entry from a provider's model list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredModel {
    /// The id to send on the wire. This, not the static slug, is what the
    /// provider accepts.
    pub(crate) id: String,
    /// Anthropic publishes a human label; OpenAI-compatible lists do not.
    pub(crate) display_name: Option<String>,
    /// Advertised input budget, if the list carried one. Consulted only for a
    /// slug the static catalog does not know; a known slug keeps its curated
    /// metadata.
    pub(crate) context_window: Option<i64>,
    /// The efforts the gateway says this model accepts. `None` is "not stated";
    /// `Some(vec![])` is "stated: none". Synthesis never infers a level the
    /// gateway did not list -- vLLM rejects `high` on a model that takes
    /// `xhigh`, so a guessed level is a 400 on the first turn.
    pub(crate) reasoning_levels: Option<Vec<ReasoningEffort>>,
    pub(crate) default_reasoning_level: Option<ReasoningEffort>,
}

/// Why a discovery attempt produced no list.
///
/// Carries a message for the log line only: no caller branches on the cause,
/// because every cause has the same consequence -- keep the static catalog.
#[derive(Debug)]
pub(crate) struct DiscoveryError {
    message: String,
    /// The endpoint exists and failed, as opposed to answering that it does
    /// not exist. Worth asking again on the next listing.
    transient: bool,
}

impl DiscoveryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: false,
        }
    }

    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: true,
        }
    }
}

/// What a probe produced. `complete` is false when the list was served but
/// its metadata endpoint failed transiently, so the merge built from it is
/// worth rebuilding on the next listing rather than kept for the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Discovered {
    pub(crate) models: Vec<DiscoveredModel>,
    pub(crate) complete: bool,
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

pub(crate) type DiscoveryFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Source of a provider's live model list.
///
/// A trait so tests can drive the merge and fallback rules without a socket,
/// and so a wire with a different list shape can be added without touching the
/// manager.
pub(crate) trait ModelListDiscovery: fmt::Debug + Send + Sync {
    /// Whether asking this provider is meaningful right now.
    ///
    /// Separate from a failed discovery because "there was nothing to ask" is
    /// not a fault and must not be logged as one.
    fn applies(&self) -> DiscoveryFuture<'_, bool> {
        Box::pin(async { true })
    }

    fn discover(
        &self,
        http_client_factory: HttpClientFactory,
    ) -> DiscoveryFuture<'_, Result<Discovered, DiscoveryError>>;
}

/// Discovers models by asking the provider itself.
///
/// Base URL, query params, configured headers and credentials are taken from
/// the wrapped provider, so this one implementation authenticates correctly
/// against `x-api-key` (Anthropic), `Authorization: Bearer` (OpenAI-compatible
/// gateways) and command-issued tokens without knowing which is in play.
#[derive(Debug)]
pub(crate) struct ProviderModelListDiscovery {
    provider: SharedModelProvider,
    timeout: Duration,
}

impl ProviderModelListDiscovery {
    pub(crate) fn new(provider: SharedModelProvider) -> Self {
        Self {
            provider,
            timeout: DISCOVERY_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeout(provider: SharedModelProvider, timeout: Duration) -> Self {
        Self { provider, timeout }
    }

    /// A first-party credential means `/models` is the Codex backend's own
    /// catalog, which the upstream models client has already fetched in full
    /// -- with metadata this endpoint cannot see. Asking again would send a
    /// second request per listing and could only lose information.
    ///
    /// Only for a provider with no credential of its own:
    /// `auth_manager_for_provider` passes the signed-in `AuthManager` through to
    /// the rest, whose endpoint it says nothing about.
    async fn applies(&self) -> bool {
        if carries_own_credential(self.provider.info()) {
            return true;
        }
        !self
            .provider
            .auth()
            .await
            .as_ref()
            .is_some_and(CodexAuth::uses_codex_backend)
    }

    async fn fetch(
        &self,
        http_client_factory: HttpClientFactory,
    ) -> Result<Discovered, DiscoveryError> {
        // The whole attempt is bounded, not just the response: building a
        // route-aware client can itself block on proxy resolution, and a
        // command-auth provider shells out for a token.
        timeout(self.timeout, self.fetch_unbounded(http_client_factory))
            .await
            .map_err(|_| DiscoveryError::new("model list request timed out"))?
    }

    async fn fetch_unbounded(
        &self,
        http_client_factory: HttpClientFactory,
    ) -> Result<Discovered, DiscoveryError> {
        let api_provider: Provider =
            self.provider.api_provider().await.map_err(|err| {
                DiscoveryError::new(format!("provider is not requestable: {err}"))
            })?;
        let list_url = api_provider.url_for_path(DISCOVERY_PATH);
        let info_url = api_provider.url_for_path(INFO_PATH);

        let mut headers = api_provider.headers.clone();
        let auth: SharedAuthProvider = self
            .provider
            .api_auth()
            .await
            .map_err(|err| DiscoveryError::new(format!("no usable credential: {err}")))?;
        headers.extend(
            auth.resolve_auth_headers()
                .await
                .map_err(|err| DiscoveryError::new(format!("credential not resolvable: {err}")))?,
        );

        let client = create_client_for_route_async(
            http_client_factory,
            list_url.clone(),
            ClientRouteClass::Api,
        )
        .await
        .map_err(|err| DiscoveryError::new(format!("no http client: {err}")))?;
        // ore: route clients are pooled, and this one can be built before the
        // app-server has learned who the client is, so its baked-in default
        // headers may still say `codex_cli_rs` while an editor is driving. Read
        // the process originator here, after the client await, which is the
        // latest point before the request goes out. Upstream never needs this:
        // every request it sends is thread-scoped and re-states the originator
        // itself, and this probe is the only request the fork adds.
        headers.insert(
            HeaderName::from_static("originator"),
            codex_login::default_client::originator().header_value,
        );

        // Both requests share the one discovery timeout, so they run together
        // rather than in sequence.
        let (list, info) = tokio::join!(
            get_json::<DiscoveryPayload>(&client, &list_url, &headers),
            get_json::<InfoPayload>(&client, &info_url, &headers),
        );
        let info = info.map(InfoPayload::into_models);
        match (list.map(DiscoveryPayload::into_models), info) {
            (Ok(list), Ok(info)) => Ok(Discovered {
                models: enrich(list, info),
                complete: true,
            }),
            (Ok(list), Err(err)) => {
                // A gateway that does not serve the endpoint answers 404 on a
                // healthy setup; the message matters when it usually does
                // serve it, because the levels it declares silently vanish.
                warn!(error = %err, "{INFO_FAILED_MESSAGE}");
                Ok(Discovered {
                    models: list,
                    complete: !err.transient,
                })
            }
            (Err(err), Ok(info)) => {
                warn!(error = %err, "{LIST_REPLACED_MESSAGE}");
                Ok(Discovered {
                    models: info,
                    complete: true,
                })
            }
            (Err(list_err), Err(_)) => Err(list_err),
        }
    }
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &HttpClient,
    url: &str,
    headers: &http::HeaderMap,
) -> Result<T, DiscoveryError> {
    let response = client
        .get(url.to_string())
        .headers(headers.clone())
        .send()
        .await
        .map_err(|err| DiscoveryError::transient(format!("GET {url} failed: {err}")))?;

    let status = response.status();
    if !status.is_success() {
        // A gateway that does not implement an endpoint answers 404 here.
        // That is expected, not exceptional, and not worth asking again.
        let message = format!("GET {url} returned {status}");
        return Err(
            if status.is_server_error() || matches!(status.as_u16(), 408 | 429) {
                DiscoveryError::transient(message)
            } else {
                DiscoveryError::new(message)
            },
        );
    }

    response
        .json::<T>()
        .await
        .map_err(|err| DiscoveryError::new(format!("unreadable model list: {err}")))
}

/// Fills in what `/model/info` states for ids the list already served, and
/// leaves the list's own membership untouched.
fn enrich(list: Vec<DiscoveredModel>, info: Vec<DiscoveredModel>) -> Vec<DiscoveredModel> {
    let mut by_id: HashMap<String, DiscoveredModel> = info
        .into_iter()
        .map(|model| (model.id.clone(), model))
        .collect();
    list.into_iter()
        .map(|mut model| {
            if let Some(extra) = by_id.remove(&model.id) {
                model.context_window = model.context_window.or(extra.context_window);
                model.reasoning_levels = model.reasoning_levels.or(extra.reasoning_levels);
                model.default_reasoning_level = model
                    .default_reasoning_level
                    .or(extra.default_reasoning_level);
            }
            model
        })
        .collect()
}

impl ModelListDiscovery for ProviderModelListDiscovery {
    fn applies(&self) -> DiscoveryFuture<'_, bool> {
        Box::pin(ProviderModelListDiscovery::applies(self))
    }

    fn discover(
        &self,
        http_client_factory: HttpClientFactory,
    ) -> DiscoveryFuture<'_, Result<Discovered, DiscoveryError>> {
        Box::pin(self.fetch(http_client_factory))
    }
}

/// The union of the list shapes this fork has to read.
///
/// Every field is optional and unknown fields are ignored, so a gateway that
/// decorates entries with its own metadata still parses. A body with neither
/// array deserializes to an empty list, which the manager treats exactly like a
/// failure -- the static catalog stands.
#[derive(Debug, Default, Deserialize)]
struct DiscoveryPayload {
    /// Anthropic and OpenAI-compatible lists.
    #[serde(default)]
    data: Vec<DiscoveryEntry>,
    /// Reserved for list shapes that name the array `models`.
    #[serde(default)]
    models: Vec<DiscoveryEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct DiscoveryEntry {
    #[serde(default)]
    id: Option<String>,
    /// Some lists key the id as `name`.
    #[serde(default)]
    name: Option<String>,
    /// Gemini sends camelCase, everything else snake_case. Without the alias the
    /// picker falls back to the raw id for every Gemini model outside the static
    /// catalog.
    #[serde(default, alias = "displayName")]
    display_name: Option<String>,
    /// `"model"` in Anthropic entries.
    #[serde(default, rename = "type")]
    entry_type: Option<String>,
    /// `"model"` in OpenAI-compatible entries.
    #[serde(default)]
    object: Option<String>,
    /// Context-window spellings, read in declaration order.
    ///
    /// Separate fields rather than serde aliases on one: aliases make a list
    /// carrying both spellings a duplicate-field error, losing the entire list
    /// over a redundancy.
    #[serde(default)]
    max_input_tokens: Option<i64>,
    #[serde(default)]
    context_length: Option<i64>,
}

impl DiscoveryPayload {
    fn into_models(self) -> Vec<DiscoveredModel> {
        let entries = if self.data.is_empty() {
            self.models
        } else {
            self.data
        };
        entries
            .into_iter()
            .filter(DiscoveryEntry::is_model)
            .filter_map(DiscoveryEntry::into_model)
            .collect()
    }
}

impl DiscoveryEntry {
    /// An OpenAI-compatible list can carry non-model objects (files, jobs); a
    /// kind we were not told is assumed to be a model, since most gateways omit
    /// the field entirely.
    fn is_model(&self) -> bool {
        match self.entry_type.as_deref().or(self.object.as_deref()) {
            Some(kind) => kind == "model",
            None => true,
        }
    }

    fn into_model(self) -> Option<DiscoveredModel> {
        let id = self.id.or(self.name)?;
        // Google's ListModels names every entry `models/<slug>`, and the
        // generateContent path re-adds that prefix. Keeping it would send
        // `models/models/gemini-2.5-pro:streamGenerateContent`, and -- because
        // gemini_model_facts is prefix-keyed -- would also miss the facts table
        // and silently cut max_output_tokens from 65536 to the unknown-model
        // default of 8192 on every turn.
        //
        // Only this one prefix is stripped. A namespaced gateway id such as
        // `anthropic/claude-opus-5` is what LiteLLM actually accepts on the wire
        // and must survive verbatim.
        let id = id
            .trim()
            .strip_prefix("models/")
            .unwrap_or(id.trim())
            .to_string();
        if id.is_empty() {
            return None;
        }
        let display_name = self
            .display_name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        // A non-positive budget is a gateway bug, not a 0-token model. Taking
        // it would drive the derived auto-compaction threshold to zero.
        let context_window = self
            .max_input_tokens
            .or(self.context_length)
            .filter(|tokens| *tokens > 0);
        Some(DiscoveredModel {
            id,
            display_name,
            context_window,
            reasoning_levels: None,
            default_reasoning_level: None,
        })
    }
}

/// LiteLLM's `/model/info`: `data[].model_name` plus a nested `model_info`.
#[derive(Debug, Default, Deserialize)]
struct InfoPayload {
    #[serde(default)]
    data: Vec<InfoEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct InfoEntry {
    #[serde(default)]
    model_name: Option<String>,
    #[serde(default)]
    model_info: InfoFields,
}

#[derive(Debug, Default, Deserialize)]
struct InfoFields {
    #[serde(default)]
    max_input_tokens: Option<i64>,
    #[serde(default)]
    reasoning_effort_levels: Option<Vec<String>>,
    #[serde(default)]
    default_reasoning_effort: Option<String>,
}

impl InfoPayload {
    fn into_models(self) -> Vec<DiscoveredModel> {
        self.data
            .into_iter()
            .filter_map(InfoEntry::into_model)
            .collect()
    }
}

impl InfoEntry {
    fn into_model(self) -> Option<DiscoveredModel> {
        let id = self.model_name?.trim().to_string();
        if id.is_empty() {
            return None;
        }
        let fields = self.model_info;
        let reasoning_levels = fields.reasoning_effort_levels.map(|levels| {
            levels
                .iter()
                .filter_map(|level| level.trim().parse::<ReasoningEffort>().ok())
                .collect::<Vec<_>>()
        });
        // A default the gateway did not also list is a gateway inconsistency;
        // sending it would be a guess.
        let default_reasoning_level = fields
            .default_reasoning_effort
            .and_then(|level| level.trim().parse::<ReasoningEffort>().ok())
            .filter(|level| {
                reasoning_levels
                    .as_ref()
                    .is_some_and(|levels| levels.contains(level))
            });
        Some(DiscoveredModel {
            id,
            display_name: None,
            context_window: fields.max_input_tokens.filter(|tokens| *tokens > 0),
            reasoning_levels,
            default_reasoning_level,
        })
    }
}

fn preset_for(effort: ReasoningEffort) -> ReasoningEffortPreset {
    let description = match &effort {
        ReasoningEffort::None => "No reasoning",
        ReasoningEffort::Minimal => "Minimal reasoning",
        ReasoningEffort::Low => "Fastest, least thorough",
        ReasoningEffort::Medium => "Balances speed and depth",
        ReasoningEffort::High => "Thorough reasoning",
        ReasoningEffort::XHigh => "Best for coding and agentic work",
        ReasoningEffort::Max => "Maximum depth, slowest",
        ReasoningEffort::Ultra => "Highest effort the model offers",
        ReasoningEffort::Persistent => "Persistent reasoning",
        ReasoningEffort::Custom(name) => name.as_str(),
    }
    .to_string();
    ReasoningEffortPreset {
        effort,
        description,
    }
}

/// Merges a discovered model list into the static catalog.
///
/// Existence comes from `discovered`, metadata comes from `static_models`:
/// a discovered id that matches a static entry is served with that entry's
/// metadata untouched (reasoning levels, context window, display name,
/// priority), and one that matches nothing is synthesized so it is still
/// selectable.
///
/// Static entries the provider did not list are dropped -- that omission is the
/// entire signal a curated gateway sends, and the merged list means "what this
/// provider serves".
///
/// Their METADATA is not lost with them: `get_model_info` on the wrapper falls
/// back to the static catalog on a miss. Retaining them here instead, hidden,
/// was tried and reverted -- it leaked into every consumer that reads the
/// catalog as an existence check rather than as a picker list, so `spawn_agent`
/// accepted a model the gateway does not serve and 404'd mid-turn instead of
/// failing fast.
pub(crate) fn merge_catalog(
    static_models: Vec<ModelInfo>,
    discovered: &[DiscoveredModel],
) -> Vec<ModelInfo> {
    // Synthesized entries sort after every known model: an unrecognized id is
    // the least likely thing a user wants preselected.
    let mut next_priority = static_models
        .iter()
        .map(|model| model.priority)
        .max()
        .unwrap_or(0)
        .saturating_add(1);

    let mut seen: HashSet<&str> = HashSet::new();
    let mut merged: Vec<ModelInfo> = Vec::with_capacity(discovered.len());
    for model in discovered {
        if !seen.insert(model.id.as_str()) {
            continue;
        }
        match static_metadata_for(&model.id, &static_models) {
            Some(known) => merged.push(ModelInfo {
                // The gateway's id is what the wire accepts; everything else is
                // the metadata we already trust for this model.
                slug: model.id.clone(),
                ..known.clone()
            }),
            None => {
                merged.push(synthesize_model_info(
                    model,
                    static_models.first(),
                    sibling_metadata_for(&model.id),
                    next_priority,
                ));
                next_priority = next_priority.saturating_add(1);
            }
        }
    }

    // Responses-lite prefixes every request with an `additional_tools` input
    // item, which only the Codex backend parses: vLLM answers
    // `'AdditionalTools' object has no attribute 'get'` with a 500, and a
    // gateway that cools a deployment down on repeated 500s then serves 429 to
    // everything, retries included. Discovery runs only where
    // `discovery_applies` allows, so nothing reaching this merge is first-party.
    for model in &mut merged {
        model.use_responses_lite = false;
        model.tool_mode = None;
    }

    // Stable so that models sharing a priority keep the order the provider
    // listed them in.
    merged.sort_by_key(|model| model.priority);
    merged
}

/// Finds the static entry a discovered id describes.
///
/// The exact slug wins. Failing that, a namespaced id (`anthropic/claude-opus-5`,
/// `openai/gpt-5.3-codex` -- how aggregating gateways name models) matches on
/// its last segment, so a proxied model is not misread as an unknown one.
fn static_metadata_for<'a>(id: &str, static_models: &'a [ModelInfo]) -> Option<&'a ModelInfo> {
    if let Some(exact) = static_models.iter().find(|model| model.slug == id) {
        return Some(exact);
    }
    let suffix = id.rsplit_once('/').map_or(id, |(_, suffix)| suffix);
    if let Some(exact) = static_models.iter().find(|model| model.slug == suffix) {
        return Some(exact);
    }
    // Vendors disagree on the version separator for the same model:
    // Anthropic and ore's catalog write `claude-sonnet-4-6`, OpenRouter writes
    // `claude-sonnet-4.6`; Google's catalog keeps the dot. Compare with both
    // folded to one form so the spelling does not decide whether a model is
    // recognised.
    let folded = suffix.replace('.', "-");
    static_models
        .iter()
        .find(|model| model.slug.replace('.', "-") == folded)
}

/// Ore's own catalogs for the other wires. A Claude or Gemini id reached
/// through an OpenAI-compatible gateway is still that model, and its curated
/// reasoning levels are the same ones the native wire would serve; measured
/// through LiteLLM on both the direct and the OpenRouter route, every level
/// the Anthropic catalog lists is accepted on the chat and responses wires.
fn sibling_metadata_for(id: &str) -> Option<ModelInfo> {
    let siblings = crate::anthropic::static_model_catalog()
        .models
        .into_iter()
        .chain(crate::gemini::static_model_catalog().models);
    let siblings: Vec<ModelInfo> = siblings.collect();
    static_metadata_for(id, &siblings).cloned()
}

/// Builds a selectable entry for a model the static catalog has never heard of.
///
/// The provider's own default model is the template, so an unknown model
/// inherits provider-appropriate instructions, truncation policy and context
/// window rather than the generic OpenAI-shaped guess. Three fields are
/// deliberately not inherited:
///
/// * reasoning levels are cleared, because sending a reasoning parameter to a
///   model that does not take one is a 400 on the first turn;
/// * the description is cleared, because inheriting one would describe a
///   different model to the user;
/// * visibility is forced to `List`, because the fallback metadata is
///   `ModelVisibility::None` -- which would discover a model and then hide it.
fn synthesize_model_info(
    discovered: &DiscoveredModel,
    template: Option<&ModelInfo>,
    sibling: Option<ModelInfo>,
    priority: i32,
) -> ModelInfo {
    let base = match template {
        Some(template) => ModelInfo {
            // Marks the metadata as guessed for anything downstream that cares.
            used_fallback_model_metadata: true,
            ..template.clone()
        },
        None => model_info_from_slug(&discovered.id),
    };
    let (context_window, max_context_window, auto_compact_token_limit) =
        match discovered.context_window {
            // `auto_compact_token_limit` must be dropped, not carried over: it
            // is an absolute token count derived from the TEMPLATE's window, so
            // pairing it with a different window compacts at the wrong point.
            // `None` makes core re-derive it at 90% of the real budget.
            Some(window) => (Some(window), Some(window), None),
            None => (
                base.context_window,
                base.max_context_window,
                base.auto_compact_token_limit,
            ),
        };
    // What the gateway states for this deployment outranks what ore knows
    // about the model in general; a sibling catalog fills in only when the
    // gateway said nothing.
    let (supported_reasoning_levels, default_reasoning_level) =
        match (&discovered.reasoning_levels, &sibling) {
            (Some(levels), _) => (
                levels.iter().cloned().map(preset_for).collect(),
                discovered.default_reasoning_level.clone(),
            ),
            (None, Some(sibling)) => (
                sibling.supported_reasoning_levels.clone(),
                sibling.default_reasoning_level.clone(),
            ),
            (None, None) => (Vec::new(), None),
        };
    ModelInfo {
        slug: discovered.id.clone(),
        display_name: discovered
            .display_name
            .clone()
            .unwrap_or_else(|| discovered.id.clone()),
        description: None,
        default_reasoning_level,
        supported_reasoning_levels,
        visibility: ModelVisibility::List,
        supported_in_api: true,
        priority,
        upgrade: None,
        context_window,
        max_context_window,
        auto_compact_token_limit,
        ..base
    }
}

/// Serves a provider's discovered catalog, falling back to `inner`.
///
/// `inner` owns the static catalog (bundled, provider-static, or cached) and
/// this type owns only the merge: whenever discovery does not produce a list,
/// what `inner` returned is passed through untouched.
#[derive(Debug)]
pub(crate) struct DiscoveringModelsManager {
    inner: SharedModelsManager,
    discovery: Arc<dyn ModelListDiscovery>,
    /// The last successful merge. Also the answer for `Offline`, which must not
    /// open a socket.
    merged: RwLock<Option<Vec<ModelInfo>>>,
    /// Whether the last probe failed. A failure is a cached answer too: without
    /// it, an unreachable gateway costs the discovery timeout on every session
    /// start, because failure clears `merged` and `OnlineIfUncached` only
    /// short-circuits on a present value.
    probe_failed: RwLock<bool>,
    /// Whether `merged` was built from a complete probe. An incomplete one is
    /// still served -- `Offline` must not open a socket -- but does not
    /// short-circuit `OnlineIfUncached`, so the next listing asks again.
    merged_complete: RwLock<bool>,
    auth_manager: Option<Arc<AuthManager>>,
}

impl DiscoveringModelsManager {
    pub(crate) fn new(
        inner: SharedModelsManager,
        discovery: Arc<dyn ModelListDiscovery>,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        Self {
            inner,
            discovery,
            merged: RwLock::new(None),
            probe_failed: RwLock::new(false),
            merged_complete: RwLock::new(true),
            auth_manager,
        }
    }

    async fn raw_model_catalog(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsResponse {
        let static_models = self
            .inner
            .raw_model_catalog(refresh_strategy, http_client_factory.clone())
            .await
            .models;

        match refresh_strategy {
            // Offline is a promise to the caller that nothing will be sent, and
            // the picker is shown on that path while the machine is on a plane.
            RefreshStrategy::Offline => {
                return ModelsResponse {
                    models: self.merged_or(static_models).await,
                };
            }
            RefreshStrategy::OnlineIfUncached => {
                let merged = self.merged.read().await.clone();
                if let Some(merged) = merged
                    && *self.merged_complete.read().await
                {
                    return ModelsResponse { models: merged };
                }
                // A FAILED probe is a cached answer too. Without this, an
                // unreachable gateway cost the full discovery timeout on every
                // root thread start -- five seconds, every session, forever --
                // because failure cleared `merged` and this arm only short-
                // circuits on `Some`. `Online` still forces a real probe, so
                // the user is never stuck with a failure they cannot retry.
                if *self.probe_failed.read().await {
                    return ModelsResponse {
                        models: static_models,
                    };
                }
            }
            RefreshStrategy::Online => {
                // An explicit refresh is the user asking again; a past failure
                // must not suppress it.
                *self.probe_failed.write().await = false;
            }
        }

        if !self.discovery.applies().await {
            return ModelsResponse {
                models: static_models,
            };
        }

        match self.discovery.discover(http_client_factory).await {
            Ok(discovered) if !discovered.models.is_empty() => {
                info!(
                    discovered = discovered.models.len(),
                    "provider model list discovered"
                );
                let merged = merge_catalog(static_models, &discovered.models);
                self.remember_merged(merged.clone(), discovered.complete)
                    .await;
                ModelsResponse { models: merged }
            }
            Ok(_) => {
                warn!("{EMPTY_LIST_MESSAGE}");
                self.forget_merged(static_models).await
            }
            Err(err) => {
                // Remediation in the message, transport detail in a field --
                // the split `codex_ollama`'s probe uses.
                warn!(error = %err, "{DISCOVERY_FAILED_MESSAGE}");
                self.forget_merged(static_models).await
            }
        }
    }

    /// Drops any earlier merge so that every accessor agrees on one snapshot.
    ///
    /// Leaving the stale merge in place would let the picker and
    /// `get_model_info` disagree about which models exist.
    /// Records a successful probe, clearing any remembered failure.
    ///
    /// Without this the flag was cleared only by an explicit `Online` refresh, so
    /// a single transient blip pinned every new session to the static catalog
    /// until the next forced refresh -- 180s in the app-server, where a worker
    /// polls on that interval. A success is the strongest possible evidence the
    /// gateway is back.
    async fn remember_merged(&self, merged: Vec<ModelInfo>, complete: bool) {
        *self.probe_failed.write().await = false;
        *self.merged_complete.write().await = complete;
        *self.merged.write().await = Some(merged);
    }

    async fn forget_merged(&self, static_models: Vec<ModelInfo>) -> ModelsResponse {
        *self.merged.write().await = None;
        *self.probe_failed.write().await = true;
        ModelsResponse {
            models: static_models,
        }
    }

    async fn merged_or(&self, static_models: Vec<ModelInfo>) -> Vec<ModelInfo> {
        let merged = self.merged.read().await.clone();
        match merged {
            Some(merged) => merged,
            None => static_models,
        }
    }
}

impl ModelsManager for DiscoveringModelsManager {
    /// Resolves metadata from the STATIC catalog when the merged one has dropped
    /// the model.
    ///
    /// Discovery narrows what a provider offers, which is right for the picker
    /// and wrong for a lookup: a model can be pinned in config, or carried by a
    /// resumed thread, after the gateway stopped listing it -- LM Studio lists
    /// only the models it has LOADED, so unloading one is enough. The trait
    /// default searches the merged list, missed, and fell through to
    /// `model_info_from_slug`, which guesses from the name: an OpenAI-shaped 272k
    /// context window on a 128k model, so auto-compaction sized against a limit
    /// the provider does not have and the turn failed instead of compacting.
    ///
    /// Deliberately NOT solved by keeping the entry in the merged catalog. That
    /// was tried: hidden entries leak into every consumer that reads the catalog
    /// as an existence check rather than a picker list, and `spawn_agent` then
    /// accepted a model the gateway does not serve and 404'd mid-turn instead of
    /// failing fast. Narrowing belongs in the list; metadata belongs here.
    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, ModelInfo> {
        Box::pin(async move {
            let merged = self.get_remote_models().await;
            if merged.iter().any(|known| known.slug == model) {
                return construct_model_info_from_candidates(model, &merged, config);
            }
            // Fall back to what the provider shipped, which still knows this
            // model's real limits, before anyone guesses from the slug.
            let static_models = self.inner.get_remote_models().await;
            if static_models.iter().any(|known| known.slug == model) {
                return construct_model_info_from_candidates(model, &static_models, config);
            }
            construct_model_info_from_candidates(model, &merged, config)
        })
    }

    fn raw_model_catalog(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ModelsResponse> {
        Box::pin(DiscoveringModelsManager::raw_model_catalog(
            self,
            refresh_strategy,
            http_client_factory,
        ))
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        Box::pin(async move {
            // Bound in its own statement: the read guard must not be alive
            // while the inner manager is awaited.
            let merged = self.merged.read().await.clone();
            match merged {
                Some(merged) => merged,
                None => self.inner.get_remote_models().await,
            }
        })
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        match self.merged.try_read()?.clone() {
            Some(merged) => Ok(merged),
            None => self.inner.try_get_remote_models(),
        }
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.auth_manager.as_deref()
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        self.inner.list_collaboration_modes()
    }

    fn refresh_if_new_etag(
        &self,
        etag: String,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ()> {
        Box::pin(async move {
            // The static catalog underneath is about to change, so the merge
            // built on the old one is void; the next online listing rebuilds it.
            *self.merged.write().await = None;
            self.inner
                .refresh_if_new_etag(etag, http_client_factory)
                .await;
        })
    }
}

/// Wraps `provider` so its catalog is discovered, when discovery makes sense.
///
/// Returns the provider unchanged otherwise, so the first-party path keeps
/// exactly the behavior it has today.
pub(crate) fn with_model_discovery(provider: SharedModelProvider) -> SharedModelProvider {
    if !discovery_applies(provider.info()) {
        return provider;
    }
    Arc::new(DiscoveringModelProvider { inner: provider })
}

/// Which providers get discovery.
///
/// Not the first-party OpenAI path: that catalog is already served by the Ore
/// backend, whose `/models` answer is authoritative and whose account-scoped
/// list would be replaced here by the raw platform inventory (embeddings,
/// speech, image models) that Ore cannot drive.
///
/// Adding a wire is a variant in this `matches!`; the list shape and auth are
/// already handled generically.
fn carries_own_credential(info: &ModelProviderInfo) -> bool {
    info.env_key.is_some() || info.experimental_bearer_token.is_some() || info.has_command_auth()
}

fn discovery_applies(info: &ModelProviderInfo) -> bool {
    if info.requires_openai_auth {
        return false;
    }
    // Bedrock speaks an OpenAI-shaped wire but serves no model list at
    // `{base_url}/models`; asking would be a guaranteed 404 on every startup.
    if info.is_amazon_bedrock() {
        return false;
    }
    // Gemini serves `GET {base_url}/models` too, returning {"models":[{"name":
    // "models/gemini-2.5-pro", ...}]}. Included so a Vertex or gateway deployment
    // that exposes a curated subset is reflected in the picker, which is the whole
    // point of discovery.
    matches!(
        info.wire_api,
        WireApi::Chat | WireApi::Responses | WireApi::Anthropic | WireApi::Gemini
    )
}

/// A provider that answers exactly like the one it wraps, except that its model
/// managers discover.
///
/// Every method is forwarded explicitly. A provider override that this
/// decorator forgot to forward would silently become the trait default -- for
/// the Anthropic provider that would re-enable web search and image generation
/// its wire cannot serve -- so new trait methods must be added here too.
#[derive(Debug)]
struct DiscoveringModelProvider {
    inner: SharedModelProvider,
}

impl DiscoveringModelProvider {
    fn wrap(
        &self,
        inner_manager: SharedModelsManager,
        config_model_catalog_is_authoritative: bool,
    ) -> SharedModelsManager {
        // A catalog pinned in config is the user's explicit answer to "which
        // models exist"; discovery must not argue with it.
        if config_model_catalog_is_authoritative {
            return inner_manager;
        }
        Arc::new(DiscoveringModelsManager::new(
            inner_manager,
            Arc::new(ProviderModelListDiscovery::new(Arc::clone(&self.inner))),
            self.inner.auth_manager(),
        ))
    }
}

impl ModelProvider for DiscoveringModelProvider {
    fn info(&self) -> &ModelProviderInfo {
        self.inner.info()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    fn approval_review_preferred_model(&self) -> &'static str {
        self.inner.approval_review_preferred_model()
    }

    fn memory_extraction_preferred_model(&self) -> &'static str {
        self.inner.memory_extraction_preferred_model()
    }

    fn memory_consolidation_preferred_model(&self) -> &'static str {
        self.inner.memory_consolidation_preferred_model()
    }

    fn supports_attestation(&self) -> bool {
        self.inner.supports_attestation()
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.inner.auth_manager()
    }

    fn is_recoverable_auth_error(&self, error: &TransportError) -> bool {
        self.inner.is_recoverable_auth_error(error)
    }

    fn recover_from_unauthorized(
        &self,
    ) -> ModelProviderFuture<'_, CoreResult<ProviderUnauthorizedRecovery>> {
        self.inner.recover_from_unauthorized()
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        self.inner.auth()
    }

    fn account_state(&self) -> ProviderAccountResult {
        self.inner.account_state()
    }

    fn map_api_error(&self, error: ApiError) -> CodexErr {
        self.inner.map_api_error(error)
    }

    fn api_provider(&self) -> ModelProviderFuture<'_, CoreResult<Provider>> {
        self.inner.api_provider()
    }

    fn runtime_base_url(&self) -> ModelProviderFuture<'_, CoreResult<Option<String>>> {
        self.inner.runtime_base_url()
    }

    fn api_auth(&self) -> ModelProviderFuture<'_, CoreResult<SharedAuthProvider>> {
        self.inner.api_auth()
    }

    fn api_auth_for_scope(
        &self,
        scope: ProviderAuthScope,
    ) -> ModelProviderFuture<'_, CoreResult<ResolvedProviderAuth>> {
        self.inner.api_auth_for_scope(scope)
    }

    fn models_manager(
        &self,
        codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        let is_authoritative = config_model_catalog.is_some();
        let inner_manager = self.inner.models_manager(codex_home, config_model_catalog);
        self.wrap(inner_manager, is_authoritative)
    }

    fn models_manager_without_cache(
        &self,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        let is_authoritative = config_model_catalog.is_some();
        let inner_manager = self
            .inner
            .models_manager_without_cache(config_model_catalog);
        self.wrap(inner_manager, is_authoritative)
    }

    fn models_manager_with_cache(
        &self,
        config_model_catalog: Option<ModelsResponse>,
        cache: Arc<dyn ModelsCache>,
    ) -> SharedModelsManager {
        let is_authoritative = config_model_catalog.is_some();
        let inner_manager = self
            .inner
            .models_manager_with_cache(config_model_catalog, cache);
        self.wrap(inner_manager, is_authoritative)
    }
}

#[cfg(test)]
#[path = "discovered_models_tests.rs"]
mod tests;
