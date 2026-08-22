//! I-TELEM — the telemetry seam holds for every config layer.
//!
//! These assert the *property* (nothing can turn telemetry back on), not the
//! shape of any particular series commit, so they keep meaning across upstream
//! churn at `core/src/config/mod.rs`, `core/src/config/otel.rs`,
//! `config/src/types.rs` and `otel/src/config.rs`.

use std::collections::BTreeMap;
use std::path::Path;

use codex_analytics::AnalyticsEventsClient;
use codex_config::ConfigLayerSource;
use codex_config::LoaderOverrides;
use codex_config::TomlValue;
use codex_config::types::OtelConfig;
use codex_config::types::OtelExporterKind;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_otel::OtelExporter;
use codex_otel::OtelProvider;
use codex_otel::OtelSettings;
use codex_plugin::PluginTelemetryMetadata;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::any;

const ANALYTICS_ON: &str = "[analytics]\nenabled = true\n";
const FEEDBACK_ON: &str = "[feedback]\nenabled = true\n";

async fn load_config(codex_home: &Path, cwd: &Path, loader_overrides: LoaderOverrides) -> Config {
    ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .fallback_cwd(Some(cwd.to_path_buf()))
        .loader_overrides(loader_overrides)
        .build()
        .await
        .expect("config should load")
}

/// What the merged config layers *asked* for, before the fork seam runs. Every
/// test below asserts this is `Some(true)` so that a layer silently failing to
/// load can never make the invariant pass vacuously.
fn requested_flag(config: &Config, table: &str) -> Option<bool> {
    let effective = config.config_layer_stack.effective_config();
    effective
        .get(table)
        .and_then(|section| section.get("enabled"))
        .and_then(TomlValue::as_bool)
}

fn has_managed_layer(config: &Config) -> bool {
    config.config_layer_stack.layers_low_to_high().any(|layer| {
        matches!(
            layer.name,
            ConfigLayerSource::LegacyManagedConfigTomlFromFile { .. }
        )
    })
}

fn has_project_layer(config: &Config) -> bool {
    config
        .config_layer_stack
        .layers_low_to_high()
        .any(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
}

#[tokio::test]
async fn analytics_stays_off_when_the_user_layer_turns_it_on() {
    let codex_home = TempDir::new().expect("tempdir");
    let cwd = TempDir::new().expect("tempdir");
    std::fs::write(codex_home.path().join("config.toml"), ANALYTICS_ON).expect("write user config");

    let config = load_config(
        codex_home.path(),
        cwd.path(),
        LoaderOverrides::without_managed_config_for_tests(),
    )
    .await;

    assert_eq!(requested_flag(&config, "analytics"), Some(true));
    assert_eq!(config.analytics_enabled, Some(false));
}

#[tokio::test]
async fn analytics_stays_off_when_a_managed_layer_turns_it_on() {
    let tmp = TempDir::new().expect("tempdir");
    let codex_home = tmp.path().join("home");
    let managed_config = tmp.path().join("managed_config.toml");
    std::fs::create_dir_all(&codex_home).expect("create codex home");
    std::fs::write(&managed_config, ANALYTICS_ON).expect("write managed config");

    let config = load_config(
        &codex_home,
        tmp.path(),
        LoaderOverrides::with_managed_config_path_for_tests(managed_config),
    )
    .await;

    assert!(has_managed_layer(&config));
    assert_eq!(requested_flag(&config, "analytics"), Some(true));
    assert_eq!(config.analytics_enabled, Some(false));
}

#[tokio::test]
async fn analytics_stays_off_when_a_project_layer_turns_it_on() {
    let tmp = TempDir::new().expect("tempdir");
    // Canonicalize so the project-trust key and the cwd agree on macOS, where
    // the temp dir is reached through a symlink.
    let root = std::fs::canonicalize(tmp.path()).expect("canonicalize tempdir");
    let codex_home = root.join("home");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&codex_home).expect("create codex home");
    std::fs::create_dir_all(workspace.join(".git")).expect("create project root marker");
    std::fs::create_dir_all(workspace.join(".codex")).expect("create project config folder");
    std::fs::write(workspace.join(".codex").join("config.toml"), ANALYTICS_ON)
        .expect("write project config");

    let workspace_key = workspace.display().to_string();
    // A TOML literal string keeps Windows path separators intact.
    let trust = format!("[projects.'{workspace_key}']\ntrust_level = \"trusted\"\n");
    std::fs::write(codex_home.join("config.toml"), trust).expect("write user config");

    let config = load_config(
        &codex_home,
        &workspace,
        LoaderOverrides::without_managed_config_for_tests(),
    )
    .await;

    assert!(has_project_layer(&config));
    assert_eq!(requested_flag(&config, "analytics"), Some(true));
    assert_eq!(config.analytics_enabled, Some(false));
}

#[tokio::test]
async fn feedback_stays_off_when_the_user_layer_turns_it_on() {
    let codex_home = TempDir::new().expect("tempdir");
    let cwd = TempDir::new().expect("tempdir");
    std::fs::write(codex_home.path().join("config.toml"), FEEDBACK_ON).expect("write user config");

    let config = load_config(
        codex_home.path(),
        cwd.path(),
        LoaderOverrides::without_managed_config_for_tests(),
    )
    .await;

    assert_eq!(requested_flag(&config, "feedback"), Some(true));
    assert!(!config.feedback_enabled);
}

#[test]
fn otel_config_defaults_disable_every_exporter() {
    let defaults = OtelConfig::default();

    assert_eq!(defaults.exporter, OtelExporterKind::None);
    assert_eq!(defaults.trace_exporter, OtelExporterKind::None);
    assert_eq!(defaults.metrics_exporter, OtelExporterKind::None);
}

#[tokio::test]
async fn otel_exporters_resolve_to_none_when_the_config_is_silent() {
    let codex_home = TempDir::new().expect("tempdir");
    let cwd = TempDir::new().expect("tempdir");

    let config = load_config(
        codex_home.path(),
        cwd.path(),
        LoaderOverrides::without_managed_config_for_tests(),
    )
    .await;

    assert_eq!(config.otel.exporter, OtelExporterKind::None);
    assert_eq!(config.otel.trace_exporter, OtelExporterKind::None);
    assert_eq!(config.otel.metrics_exporter, OtelExporterKind::None);
}

/// The Statsig route is stripped, so a settings object that still asks for it
/// installs no provider at all. Unlike upstream's `cfg!(debug_assertions)`
/// suppression this must hold in release too, which is why the release gate
/// re-runs this crate with `--cargo-profile release`.
#[test]
fn a_statsig_metrics_exporter_installs_no_otel_provider() {
    let codex_home = TempDir::new().expect("tempdir");
    let settings = OtelSettings {
        environment: "fork-invariants".to_string(),
        service_name: "fork-invariants".to_string(),
        service_version: "0.0.0".to_string(),
        codex_home: codex_home.path().to_path_buf(),
        exporter: OtelExporter::None,
        trace_exporter: OtelExporter::None,
        metrics_exporter: OtelExporter::Statsig,
        runtime_metrics: false,
        span_attributes: BTreeMap::new(),
        tracestate: BTreeMap::new(),
    };

    let provider = OtelProvider::try_new(&settings).expect("otel settings should resolve");

    assert!(provider.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_analytics_client_never_posts_under_the_resolved_flag() {
    let codex_home = TempDir::new().expect("tempdir");
    let cwd = TempDir::new().expect("tempdir");
    std::fs::write(codex_home.path().join("config.toml"), ANALYTICS_ON).expect("write user config");

    let config = load_config(
        codex_home.path(),
        cwd.path(),
        LoaderOverrides::without_managed_config_for_tests(),
    )
    .await;

    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = AnalyticsEventsClient::new(
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
        server.uri(),
        config.analytics_enabled,
    );

    assert!(!client.is_enabled());
    // The plugin path is the one that also fired for API-key auth upstream.
    client.track_plugin_installed(PluginTelemetryMetadata {
        plugin_id: None,
        remote_plugin_id: None,
        capability_summary: None,
    });
    client.flush().await;

    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}
