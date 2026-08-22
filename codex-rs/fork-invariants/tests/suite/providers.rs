//! I-PROVIDER — Anthropic is usable out of the box, and never outranks the user.
//!
//! `merge_configured_model_providers` resolves collisions with `or_insert`, so a
//! built-in silently wins over a user's own table of the same id. Registering
//! Anthropic unconditionally would therefore have made
//! `[model_providers.anthropic]` a no-op -- a custom base_url (a proxy, a
//! gateway, a test double) would be accepted and then ignored. These pin both
//! halves: it works with no config, and it disappears the moment the user
//! defines the id themselves.

use std::path::Path;

use codex_config::LoaderOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use tempfile::TempDir;

async fn load(home: &Path, cfg: &str) -> std::io::Result<Config> {
    std::fs::write(home.join("config.toml"), cfg).expect("write config");
    ConfigBuilder::default()
        .codex_home(home.to_path_buf())
        .fallback_cwd(Some(home.to_path_buf()))
        .loader_overrides(LoaderOverrides {
            ignore_project_config: true,
            ..LoaderOverrides::without_managed_config_for_tests()
        })
        .build()
        .await
}

#[tokio::test]
async fn anthropic_resolves_with_no_provider_table() {
    let home = TempDir::new().expect("tempdir");
    let config = load(home.path(), "model_provider = \"anthropic\"\n")
        .await
        .expect("selecting anthropic must not require a provider table");

    assert_eq!(config.model_provider_id, "anthropic");
    assert!(
        config
            .model_provider
            .base_url
            .as_deref()
            .is_some_and(|url| url.contains("api.anthropic.com")),
        "the built-in must point at Anthropic's API, got {:?}",
        config.model_provider.base_url
    );
}

#[tokio::test]
async fn a_user_provider_table_overrides_the_built_in() {
    let home = TempDir::new().expect("tempdir");
    let config = load(
        home.path(),
        r#"
model_provider = "anthropic"

[model_providers.anthropic]
name = "anthropic via a proxy"
base_url = "https://proxy.invalid/v1"
wire_api = "anthropic"
"#,
    )
    .await
    .expect("a user-defined anthropic provider must load");

    assert_eq!(
        config.model_provider.base_url.as_deref(),
        Some("https://proxy.invalid/v1"),
        "the built-in must not outrank the user's own table"
    );
}
