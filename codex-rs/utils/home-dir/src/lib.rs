use codex_utils_absolute_path::AbsolutePathBuf;
use dirs::home_dir;
use std::path::PathBuf;

/// Returns the path to the ore configuration directory, which can be
/// specified by the `ORE_HOME` environment variable, or by `CODEX_HOME` which
/// is honored as a full alias. If neither is set, defaults to `~/.ore`.
///
/// - If either variable is set, the value must exist and be a directory. The
///   value will be canonicalized and this function will Err otherwise.
/// - If neither variable is set, this function does not verify that the
///   directory exists.
pub fn find_codex_home() -> std::io::Result<AbsolutePathBuf> {
    let (env_var, home_env) = match env_var_nonempty("ORE_HOME") {
        Some(val) => ("ORE_HOME", Some(val)),
        None => ("CODEX_HOME", env_var_nonempty("CODEX_HOME")),
    };
    find_codex_home_from_env(env_var, home_env.as_deref())
}

/// Returns the legacy `~/.codex` directory, which is layered underneath the
/// home above as a read-only base.
///
/// Returns `None` when `CODEX_HOME` is set: that variable pins an explicit home
/// for upstream-compatible tooling, and layering anything underneath such a
/// home would change what that tooling sees.
pub fn find_legacy_codex_home() -> Option<AbsolutePathBuf> {
    find_legacy_codex_home_from_env(env_var_nonempty("CODEX_HOME").as_deref(), home_dir())
}

fn env_var_nonempty(env_var: &str) -> Option<String> {
    std::env::var(env_var).ok().filter(|val| !val.is_empty())
}

fn find_legacy_codex_home_from_env(
    codex_home_env: Option<&str>,
    home: Option<PathBuf>,
) -> Option<AbsolutePathBuf> {
    if codex_home_env.is_some() {
        return None;
    }
    let legacy_home = home?.join(".codex");
    if !legacy_home.is_dir() {
        return None;
    }
    AbsolutePathBuf::from_absolute_path(legacy_home).ok()
}

fn find_codex_home_from_env(
    env_var: &str,
    codex_home_env: Option<&str>,
) -> std::io::Result<AbsolutePathBuf> {
    // Honor the `ORE_HOME`/`CODEX_HOME` environment variable when it is set to
    // allow users (and tests) to override the default location.
    match codex_home_env {
        Some(val) => {
            let path = PathBuf::from(val);
            let metadata = std::fs::metadata(&path).map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{env_var} points to {val:?}, but that path does not exist"),
                ),
                _ => std::io::Error::new(
                    err.kind(),
                    format!("failed to read {env_var} {val:?}: {err}"),
                ),
            })?;

            if !metadata.is_dir() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{env_var} points to {val:?}, but that path is not a directory"),
                ))
            } else {
                let canonical = path.canonicalize().map_err(|err| {
                    std::io::Error::new(
                        err.kind(),
                        format!("failed to canonicalize {env_var} {val:?}: {err}"),
                    )
                })?;
                AbsolutePathBuf::from_absolute_path(canonical)
            }
        }
        None => {
            let mut p = home_dir().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Could not find home directory",
                )
            })?;
            p.push(".ore");
            AbsolutePathBuf::from_absolute_path(p)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::find_codex_home_from_env;
    use super::find_legacy_codex_home_from_env;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use dirs::home_dir;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::io::ErrorKind;
    use tempfile::TempDir;

    #[test]
    fn find_codex_home_env_missing_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing-codex-home");
        let missing_str = missing
            .to_str()
            .expect("missing codex home path should be valid utf-8");

        let err =
            find_codex_home_from_env("ORE_HOME", Some(missing_str)).expect_err("missing ORE_HOME");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(
            err.to_string().contains("ORE_HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_file_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let file_path = temp_home.path().join("codex-home.txt");
        fs::write(&file_path, "not a directory").expect("write temp file");
        let file_str = file_path
            .to_str()
            .expect("file codex home path should be valid utf-8");

        let err = find_codex_home_from_env("ORE_HOME", Some(file_str)).expect_err("file ORE_HOME");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_valid_directory_canonicalizes() {
        let temp_home = TempDir::new().expect("temp home");
        let temp_str = temp_home
            .path()
            .to_str()
            .expect("temp codex home path should be valid utf-8");

        let resolved =
            find_codex_home_from_env("ORE_HOME", Some(temp_str)).expect("valid ORE_HOME");
        let expected = temp_home
            .path()
            .canonicalize()
            .expect("canonicalize temp home");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn find_codex_home_without_env_uses_default_home_dir() {
        let resolved = find_codex_home_from_env("ORE_HOME", /*codex_home_env*/ None)
            .expect("default ORE_HOME");
        let mut expected = home_dir().expect("home dir");
        expected.push(".ore");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn find_codex_home_env_errors_name_the_variable_that_was_set() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing-codex-home");
        let missing_str = missing
            .to_str()
            .expect("missing codex home path should be valid utf-8");

        let err = find_codex_home_from_env("CODEX_HOME", Some(missing_str))
            .expect_err("missing CODEX_HOME");
        assert!(
            err.to_string().contains("CODEX_HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_legacy_codex_home_is_none_when_codex_home_is_set() {
        let temp_home = TempDir::new().expect("temp home");
        fs::create_dir(temp_home.path().join(".codex")).expect("create legacy home");

        assert_eq!(
            find_legacy_codex_home_from_env(
                Some("/pinned-home"),
                Some(temp_home.path().to_path_buf())
            ),
            None
        );
    }

    #[test]
    fn find_legacy_codex_home_without_env_uses_home_dir() {
        let temp_home = TempDir::new().expect("temp home");
        let legacy_home = temp_home.path().join(".codex");
        fs::create_dir(&legacy_home).expect("create legacy home");
        let expected = AbsolutePathBuf::from_absolute_path(&legacy_home).expect("absolute home");

        assert_eq!(
            find_legacy_codex_home_from_env(None, Some(temp_home.path().to_path_buf())),
            Some(expected)
        );
    }

    #[test]
    fn find_legacy_codex_home_is_none_when_directory_is_absent() {
        let temp_home = TempDir::new().expect("temp home");

        assert_eq!(
            find_legacy_codex_home_from_env(None, Some(temp_home.path().to_path_buf())),
            None
        );
    }
}
