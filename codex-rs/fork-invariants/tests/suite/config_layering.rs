//! I-CONFIG — the `~/.codex` base layer under the `~/.ore` home.
//!
//! The fork layers the legacy config file as one extra read-only `User` layer
//! beneath the ore one. These pin what that has to mean: the ore key wins, the
//! merge is key-by-key rather than table-by-table, the writable layer is still
//! the ore file, and nothing ore does writes into the legacy home.
//!
//! Paths are injected through `LoaderOverrides` rather than the environment so
//! the tests stay parallel-safe and never see a developer's real `~/.codex`.

use std::path::Path;
use std::path::PathBuf;

use codex_config::AbsolutePathBuf;
use codex_config::ConfigLayerSource;
use codex_config::LoaderOverrides;
use codex_config::TomlValue;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::edit::ConfigEditsBuilder;
use tempfile::TempDir;

const LEGACY_CONFIG: &str = r#"
model = "from-legacy"
review_model = "from-legacy"

[model_providers.shared]
name = "from-legacy"
base_url = "https://legacy.invalid/v1"
"#;

// Carries the provider table for the same reason LEGACY_CONFIG does: the ore
// fixture below overrides only that provider's base_url, so the name has to
// come from the legacy layer or the merged provider fails validation.
const LEGACY_CONFIG_WITH_PROFILE: &str = r#"
profile = "legacy"
model = "from-legacy"
review_model = "from-legacy"

[model_providers.shared]
name = "from-legacy"
base_url = "https://legacy.invalid/v1"
"#;

const ORE_CONFIG: &str = r#"
model = "from-ore"

[model_providers.shared]
base_url = "https://active.invalid/v1"
"#;

struct Homes {
    _tmp: TempDir,
    legacy_home: PathBuf,
    ore_home: PathBuf,
}

impl Homes {
    /// A populated legacy home and an ore home under one canonicalized temp
    /// root, so path comparisons hold on macOS too.
    fn new(legacy_config: &str) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let root = std::fs::canonicalize(tmp.path()).expect("canonicalize tempdir");
        let legacy_home = root.join(".codex");
        let ore_home = root.join(".ore");
        std::fs::create_dir_all(legacy_home.join("skills")).expect("create legacy home");
        std::fs::create_dir_all(&ore_home).expect("create ore home");
        std::fs::write(legacy_home.join("config.toml"), legacy_config)
            .expect("write legacy config");
        std::fs::write(legacy_home.join("auth.json"), "{}\n").expect("write legacy auth");
        std::fs::write(legacy_home.join("skills").join("demo.md"), "demo\n")
            .expect("write legacy skill");
        std::fs::write(ore_home.join("config.toml"), ORE_CONFIG).expect("write ore config");
        Self {
            _tmp: tmp,
            legacy_home,
            ore_home,
        }
    }

    fn legacy_file(&self) -> PathBuf {
        self.legacy_home.join("config.toml")
    }

    fn ore_file(&self) -> PathBuf {
        self.ore_home.join("config.toml")
    }

    async fn load(&self) -> Config {
        let legacy_user_config_path = AbsolutePathBuf::from_absolute_path(self.legacy_file())
            .expect("absolute legacy config path");
        ConfigBuilder::default()
            .codex_home(self.ore_home.clone())
            .fallback_cwd(Some(self.ore_home.clone()))
            .loader_overrides(LoaderOverrides {
                legacy_user_config_path: Some(legacy_user_config_path),
                // The two homes are siblings, so project discovery would
                // otherwise find the legacy folder walking up from the cwd.
                ignore_project_config: true,
                ..LoaderOverrides::without_managed_config_for_tests()
            })
            .build()
            .await
            .expect("config should load")
    }
}

/// The user-layer files, lowest precedence first.
fn user_layer_files(config: &Config) -> Vec<PathBuf> {
    config
        .config_layer_stack
        .layers_low_to_high()
        .filter_map(|layer| match &layer.name {
            ConfigLayerSource::User { file, .. } => Some(file.to_path_buf()),
            _ => None,
        })
        .collect()
}

fn string_at(value: &TomlValue, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(String::from)
}

/// Every file under `root`, relative path plus contents, sorted.
fn directory_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files);
    files.sort();
    files
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
    for entry in std::fs::read_dir(dir).expect("read directory") {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if entry.file_type().expect("entry file type").is_dir() {
            collect_files(root, &path, files);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("entry under root")
                .to_path_buf();
            files.push((relative, std::fs::read(&path).expect("read file")));
        }
    }
}

#[tokio::test]
async fn the_ore_layer_sits_above_the_legacy_layer_and_wins_key_by_key() {
    let homes = Homes::new(LEGACY_CONFIG);
    let config = homes.load().await;

    // Both layers are live, legacy underneath — otherwise the assertions below
    // would pass vacuously.
    assert_eq!(
        user_layer_files(&config),
        vec![homes.legacy_file(), homes.ore_file()]
    );

    let effective = config.config_layer_stack.effective_config();

    // Scalars: ore wins where both set the key, legacy survives where only it does.
    assert_eq!(
        string_at(&effective, &["model"]),
        Some("from-ore".to_string())
    );
    assert_eq!(
        string_at(&effective, &["review_model"]),
        Some("from-legacy".to_string())
    );

    // Nested tables merge key by key rather than being replaced wholesale.
    assert_eq!(
        string_at(&effective, &["model_providers", "shared", "base_url"]),
        Some("https://active.invalid/v1".to_string())
    );
    assert_eq!(
        string_at(&effective, &["model_providers", "shared", "name"]),
        Some("from-legacy".to_string())
    );
}

#[tokio::test]
async fn config_edits_land_in_the_ore_home_and_never_touch_the_legacy_home() {
    let homes = Homes::new(LEGACY_CONFIG);
    let legacy_before = directory_snapshot(&homes.legacy_home);
    let config = homes.load().await;

    // `for_config` is the production write path: it resolves to the active
    // user layer, which must be the ore file even with a legacy layer present.
    ConfigEditsBuilder::for_config(&config)
        .set_model(Some("written-by-ore"), None)
        .apply()
        .await
        .expect("config edit should apply");

    assert_eq!(directory_snapshot(&homes.legacy_home), legacy_before);
    let written = std::fs::read_to_string(homes.ore_file()).expect("read ore config");
    assert!(
        written.contains("written-by-ore"),
        "the ore config should carry the edit, found: {written}"
    );
}

/// A `~/.codex/config.toml` last written by an older codex can carry a
/// `profile = "..."` selector, which this version rejects outright. ore does
/// not own that file, so the selector is dropped instead of aborting startup.
#[tokio::test]
async fn a_legacy_profile_selector_does_not_abort_startup() {
    let homes = Homes::new(LEGACY_CONFIG_WITH_PROFILE);

    let config = homes.load().await;

    assert_eq!(
        user_layer_files(&config),
        vec![homes.legacy_file(), homes.ore_file()]
    );
    assert!(
        string_at(&config.config_layer_stack.effective_config(), &["profile"]).is_none(),
        "the legacy profile selector must be stripped from the layer"
    );
}
