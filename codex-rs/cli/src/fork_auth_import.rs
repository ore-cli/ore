//! One-shot import of credentials from a legacy `~/.codex` install.
//!
//! Fork-owned: nothing in `codex-login` is patched for this. The import runs at
//! most once per home, so `logout` stays permanent — a load-time fallback to the
//! legacy file would resurrect a deleted session on the next start.
//!
//! Only the API key is imported by default. ChatGPT tokens are shared behind an
//! explicit opt-in because the first refresh performed by either binary rotates
//! the refresh token and leaves the other install holding a stale one.
//!
//! The legacy directory is only ever read.

use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use std::io;
use std::path::Path;

/// Set to a non-empty value other than `0` to also import ChatGPT tokens.
const IMPORT_CHATGPT_AUTH_ENV_VAR: &str = "ORE_IMPORT_CODEX_CHATGPT_AUTH";

/// Written once the import has been considered, so it never runs twice.
const IMPORT_MARKER_FILE: &str = ".auth-import-done";

const AUTH_FILE: &str = "auth.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    /// The import already ran for this home.
    AlreadyConsidered,
    /// This home already has credentials; nothing was read.
    HomeAlreadyHasAuth,
    /// No legacy credentials to import.
    NoLegacyAuth,
    /// Legacy credentials exist but hold no API key, and tokens were not opted in.
    NoImportableCredential,
    ImportedApiKey,
    ImportedAllCredentials,
}

/// Imports legacy credentials if this home has none yet. Best-effort: any
/// failure leaves both homes untouched and lets the caller proceed to login.
pub fn import_legacy_auth_once() {
    let Ok(codex_home) = codex_utils_home_dir::find_codex_home() else {
        return;
    };
    // `None` when CODEX_HOME is set: that home is pinned for upstream-compatible
    // tooling and must not be seeded from anywhere.
    let Some(legacy_home) = codex_utils_home_dir::find_legacy_codex_home() else {
        return;
    };
    let _ = import_legacy_auth(
        codex_home.as_path(),
        legacy_home.as_path(),
        chatgpt_import_opted_in(),
    );
}

fn chatgpt_import_opted_in() -> bool {
    std::env::var(IMPORT_CHATGPT_AUTH_ENV_VAR).is_ok_and(|value| !value.is_empty() && value != "0")
}

fn import_legacy_auth(
    codex_home: &Path,
    legacy_home: &Path,
    import_chatgpt_tokens: bool,
) -> io::Result<ImportOutcome> {
    if codex_home == legacy_home || codex_home.join(IMPORT_MARKER_FILE).exists() {
        return Ok(ImportOutcome::AlreadyConsidered);
    }
    if codex_home.join(AUTH_FILE).exists() {
        write_marker(codex_home)?;
        return Ok(ImportOutcome::HomeAlreadyHasAuth);
    }

    let legacy_auth = match std::fs::read(legacy_home.join(AUTH_FILE)) {
        Ok(legacy_auth) => legacy_auth,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            write_marker(codex_home)?;
            return Ok(ImportOutcome::NoLegacyAuth);
        }
        Err(error) => return Err(error),
    };

    let outcome = if import_chatgpt_tokens {
        // Copied verbatim: re-serializing would have to understand every field,
        // and an unknown one is exactly what must not be dropped.
        write_private(&codex_home.join(AUTH_FILE), &legacy_auth)?;
        ImportOutcome::ImportedAllCredentials
    } else if let Some(api_key) = api_key_from_auth_json(&legacy_auth) {
        codex_login::login_with_api_key(
            codex_home,
            &api_key,
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )?;
        ImportOutcome::ImportedApiKey
    } else {
        ImportOutcome::NoImportableCredential
    };

    write_marker(codex_home)?;
    Ok(outcome)
}

/// Reads only the API key field, so an `auth.json` written by any codex version
/// is readable even when the rest of it no longer deserializes.
fn api_key_from_auth_json(contents: &[u8]) -> Option<String> {
    let auth: serde_json::Value = serde_json::from_slice(contents).ok()?;
    let api_key = auth.get("OPENAI_API_KEY")?.as_str()?;
    if api_key.trim().is_empty() {
        return None;
    }
    Some(api_key.to_string())
}

fn write_marker(codex_home: &Path) -> io::Result<()> {
    std::fs::create_dir_all(codex_home)?;
    std::fs::write(
        codex_home.join(IMPORT_MARKER_FILE),
        "legacy credential import already ran\n",
    )
}

/// Mirrors how the login crate writes `auth.json`: owner-only on unix.
fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.truncate(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::AUTH_FILE;
    use super::IMPORT_MARKER_FILE;
    use super::ImportOutcome;
    use super::import_legacy_auth;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use tempfile::TempDir;

    const LEGACY_API_KEY_AUTH: &str = r#"{
  "OPENAI_API_KEY": "sk-legacy-key",
  "tokens": null,
  "last_refresh": null
}"#;

    const LEGACY_CHATGPT_AUTH: &str = r#"{
  "auth_mode": "chatgpt",
  "OPENAI_API_KEY": null,
  "tokens": {"refresh_token": "rt-legacy"}
}"#;

    fn homes() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = TempDir::new().expect("temp root");
        let codex_home = root.path().join(".ore");
        let legacy_home = root.path().join(".codex");
        std::fs::create_dir_all(&codex_home).expect("create home");
        std::fs::create_dir_all(&legacy_home).expect("create legacy home");
        (root, codex_home, legacy_home)
    }

    fn write_legacy_auth(legacy_home: &Path, contents: &str) {
        std::fs::write(legacy_home.join(AUTH_FILE), contents).expect("write legacy auth");
    }

    #[test]
    fn imports_api_key_when_home_has_no_auth() {
        let (_root, codex_home, legacy_home) = homes();
        write_legacy_auth(&legacy_home, LEGACY_API_KEY_AUTH);

        let outcome = import_legacy_auth(&codex_home, &legacy_home, /*chatgpt*/ false)
            .expect("import succeeds");

        assert_eq!(outcome, ImportOutcome::ImportedApiKey);
        let imported =
            std::fs::read_to_string(codex_home.join(AUTH_FILE)).expect("imported auth.json");
        assert!(
            imported.contains("sk-legacy-key"),
            "unexpected auth.json: {imported}"
        );
        assert!(codex_home.join(IMPORT_MARKER_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn verbatim_import_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (_root, codex_home, legacy_home) = homes();
        write_legacy_auth(&legacy_home, LEGACY_CHATGPT_AUTH);

        import_legacy_auth(&codex_home, &legacy_home, /*chatgpt*/ true).expect("import succeeds");

        let mode = std::fs::metadata(codex_home.join(AUTH_FILE))
            .expect("imported auth.json")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn skips_chatgpt_tokens_without_opt_in() {
        let (_root, codex_home, legacy_home) = homes();
        write_legacy_auth(&legacy_home, LEGACY_CHATGPT_AUTH);

        let outcome = import_legacy_auth(&codex_home, &legacy_home, /*chatgpt*/ false)
            .expect("import succeeds");

        assert_eq!(outcome, ImportOutcome::NoImportableCredential);
        assert!(!codex_home.join(AUTH_FILE).exists());
        assert!(codex_home.join(IMPORT_MARKER_FILE).exists());
    }

    #[test]
    fn imports_chatgpt_tokens_verbatim_when_opted_in() {
        let (_root, codex_home, legacy_home) = homes();
        write_legacy_auth(&legacy_home, LEGACY_CHATGPT_AUTH);

        let outcome = import_legacy_auth(&codex_home, &legacy_home, /*chatgpt*/ true)
            .expect("import succeeds");

        assert_eq!(outcome, ImportOutcome::ImportedAllCredentials);
        assert_eq!(
            std::fs::read_to_string(codex_home.join(AUTH_FILE)).expect("imported auth.json"),
            LEGACY_CHATGPT_AUTH
        );
    }

    #[test]
    fn runs_at_most_once_so_logout_sticks() {
        let (_root, codex_home, legacy_home) = homes();
        write_legacy_auth(&legacy_home, LEGACY_API_KEY_AUTH);

        import_legacy_auth(&codex_home, &legacy_home, /*chatgpt*/ false).expect("first import");
        std::fs::remove_file(codex_home.join(AUTH_FILE)).expect("logout");

        let outcome = import_legacy_auth(&codex_home, &legacy_home, /*chatgpt*/ false)
            .expect("second import");

        assert_eq!(outcome, ImportOutcome::AlreadyConsidered);
        assert!(!codex_home.join(AUTH_FILE).exists());
    }

    #[test]
    fn never_overwrites_existing_auth() {
        let (_root, codex_home, legacy_home) = homes();
        write_legacy_auth(&legacy_home, LEGACY_API_KEY_AUTH);
        std::fs::write(codex_home.join(AUTH_FILE), "{}").expect("existing auth");

        let outcome = import_legacy_auth(&codex_home, &legacy_home, /*chatgpt*/ false)
            .expect("import succeeds");

        assert_eq!(outcome, ImportOutcome::HomeAlreadyHasAuth);
        assert_eq!(
            std::fs::read_to_string(codex_home.join(AUTH_FILE)).expect("auth.json"),
            "{}"
        );
    }

    #[test]
    fn marks_done_when_there_is_nothing_to_import() {
        let (_root, codex_home, legacy_home) = homes();

        let outcome = import_legacy_auth(&codex_home, &legacy_home, /*chatgpt*/ false)
            .expect("import succeeds");

        assert_eq!(outcome, ImportOutcome::NoLegacyAuth);
        assert!(codex_home.join(IMPORT_MARKER_FILE).exists());
    }

    #[test]
    fn unparsable_legacy_auth_is_not_fatal() {
        let (_root, codex_home, legacy_home) = homes();
        write_legacy_auth(&legacy_home, "{not json");

        let outcome = import_legacy_auth(&codex_home, &legacy_home, /*chatgpt*/ false)
            .expect("import succeeds");

        assert_eq!(outcome, ImportOutcome::NoImportableCredential);
        assert!(!codex_home.join(AUTH_FILE).exists());
    }
}
