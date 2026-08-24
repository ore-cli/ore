//! ore: `ORE_MODEL_PROVIDER` / `ORE_MODEL`.
//!
//! A fork-owned file rather than an append to upstream's `config_tests.rs`:
//! that file is 12,000 lines, upstream appends its own tests at the end of it,
//! and it has taken ~300 upstream commits since February. Mounting from
//! `mod.rs` costs two lines there instead of eighty in a collision zone.
//!
//! Serial, and each test restores what it found: these read process
//! environment, which every thread in the test binary shares.

use pretty_assertions::assert_eq;

use crate::config::Config;
use crate::config::ConfigOverrides;
use crate::config::ORE_MODEL_ENV_VAR;
use crate::config::ORE_MODEL_PROVIDER_ENV_VAR;
use crate::config::env_var_nonempty;
use codex_config::config_toml::ConfigToml;
use core_test_support::TempDirExt;
use tempfile::tempdir;

struct EnvGuard {
    name: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(name: &'static str, value: Option<&str>) -> Self {
        let previous = std::env::var(name).ok();
        match value {
            // SAFETY: every test here is #[serial], so no other thread in this
            // binary is reading or writing the environment.
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
        Self { name, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.name, value) },
            None => unsafe { std::env::remove_var(self.name) },
        }
    }
}

// ---------------------------------------------------------------- precedence
//
// These are the tests this change actually needed. The first version of them
// only exercised `env_var_nonempty`, which is why an inverted precedence --
// the environment silently outranking `-c` and every managed config layer --
// shipped with a green suite.

async fn provider_and_model_for(toml: ConfigToml) -> std::io::Result<(String, Option<String>)> {
    let codex_home = tempdir()?;
    let config = Config::load_from_base_config_with_overrides(
        toml,
        ConfigOverrides::default(),
        codex_home.abs(),
    )
    .await?;
    Ok((config.model_provider_id, config.model))
}

#[tokio::test]
#[serial_test::serial]
async fn the_environment_selects_when_no_layer_has_chosen() -> std::io::Result<()> {
    let _p = EnvGuard::set(ORE_MODEL_PROVIDER_ENV_VAR, Some("anthropic"));
    let _m = EnvGuard::set(ORE_MODEL_ENV_VAR, Some("claude-opus-5"));

    let (provider, model) = provider_and_model_for(ConfigToml::default()).await?;

    assert_eq!(
        provider, "anthropic",
        "this is the case the feature exists for"
    );
    assert_eq!(model.as_deref(), Some("claude-opus-5"));
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn a_configured_provider_outranks_the_environment() -> std::io::Result<()> {
    // `cfg` is the whole merged layer stack -- Mdm, System, EnterpriseManaged,
    // User, Project and the `-c` flags all land here. A stale shell export must
    // not silently redirect an install that has been told where to go.
    let _p = EnvGuard::set(ORE_MODEL_PROVIDER_ENV_VAR, Some("anthropic"));
    let _m = EnvGuard::set(ORE_MODEL_ENV_VAR, Some("claude-opus-5"));

    let (provider, model) = provider_and_model_for(ConfigToml {
        model_provider: Some("openai".to_string()),
        model: Some("gpt-5.5".to_string()),
        ..ConfigToml::default()
    })
    .await?;

    assert_eq!(
        provider, "openai",
        "config (including -c and enterprise-managed layers) must beat the environment"
    );
    assert_eq!(model.as_deref(), Some("gpt-5.5"));
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn a_blank_environment_value_does_not_displace_config() -> std::io::Result<()> {
    let _p = EnvGuard::set(ORE_MODEL_PROVIDER_ENV_VAR, Some("   "));

    let (provider, _) = provider_and_model_for(ConfigToml {
        model_provider: Some("openai".to_string()),
        ..ConfigToml::default()
    })
    .await?;

    assert_eq!(provider, "openai");
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn a_blank_environment_value_falls_through_to_the_default() -> std::io::Result<()> {
    let _p = EnvGuard::set(ORE_MODEL_PROVIDER_ENV_VAR, Some(""));

    let (provider, _) = provider_and_model_for(ConfigToml::default()).await?;

    assert_eq!(
        provider, "openai",
        "an exported-but-empty value selects nothing"
    );
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn an_env_provider_does_not_inherit_a_config_model() -> std::io::Result<()> {
    // The regression this pairing exists for. `model` is a commonly-set config
    // key and `model_provider` is rarely set, so the two resolving independently
    // pointed an OpenAI slug at the Anthropic Messages API.
    let _p = EnvGuard::set(ORE_MODEL_PROVIDER_ENV_VAR, Some("anthropic"));
    let _m = EnvGuard::set(ORE_MODEL_ENV_VAR, Some("claude-opus-5"));

    let (provider, model) = provider_and_model_for(ConfigToml {
        model: Some("gpt-5.4".to_string()),
        ..ConfigToml::default()
    })
    .await?;

    assert_eq!(provider, "anthropic");
    assert_eq!(
        model.as_deref(),
        Some("claude-opus-5"),
        "an OpenAI slug must not be carried onto a provider chosen by the environment"
    );
    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn a_config_model_survives_when_config_also_chose_the_provider() -> std::io::Result<()> {
    // The other side of the pairing: if config picked the provider, its model is
    // the right one and the environment must not displace it.
    let _p = EnvGuard::set(ORE_MODEL_PROVIDER_ENV_VAR, Some("anthropic"));
    let _m = EnvGuard::set(ORE_MODEL_ENV_VAR, Some("claude-opus-5"));

    let (provider, model) = provider_and_model_for(ConfigToml {
        model_provider: Some("openai".to_string()),
        model: Some("gpt-5.4".to_string()),
        ..ConfigToml::default()
    })
    .await?;

    assert_eq!(provider, "openai");
    assert_eq!(model.as_deref(), Some("gpt-5.4"));
    Ok(())
}

// ------------------------------------------------------------- the helper
#[test]
#[serial_test::serial]
fn a_set_variable_is_read() {
    let _g = EnvGuard::set(ORE_MODEL_PROVIDER_ENV_VAR, Some("anthropic"));
    assert_eq!(
        env_var_nonempty(ORE_MODEL_PROVIDER_ENV_VAR).as_deref(),
        Some("anthropic")
    );
}

#[test]
#[serial_test::serial]
fn blank_and_whitespace_only_values_fall_through() {
    for blank in ["", "   ", "\t"] {
        let _g = EnvGuard::set(ORE_MODEL_ENV_VAR, Some(blank));
        assert_eq!(env_var_nonempty(ORE_MODEL_ENV_VAR), None);
    }
}

#[test]
#[serial_test::serial]
fn values_are_trimmed() {
    let _g = EnvGuard::set(ORE_MODEL_ENV_VAR, Some("  claude-opus-5\n"));
    assert_eq!(
        env_var_nonempty(ORE_MODEL_ENV_VAR).as_deref(),
        Some("claude-opus-5")
    );
}
