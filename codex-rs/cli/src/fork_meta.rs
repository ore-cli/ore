//! Fork identity for `--version`.
//!
//! clap renders `--version` as `{display_name} {long_version}`, so the string built here
//! starts at the version rather than at the program name. Two invariants ride on its shape:
//! the second whitespace token of line 1 must stay byte-equal to `CARGO_PKG_VERSION`
//! (app-server-daemon compares it against the running backend's user-agent version to decide
//! whether to restart), and line 2 must end in `)` so the installers' trailing-token version
//! regexes only ever match line 1.

use std::sync::LazyLock;

/// Written by `fork/assemble.sh`; a tracked placeholder on the `delta` branch.
const UPSTREAM: &str = include_str!("../../../fork/UPSTREAM");

const SHORT_COMMIT_LEN: usize = 10;

static LONG_VERSION: LazyLock<String> = LazyLock::new(|| {
    let version = env!("CARGO_PKG_VERSION");
    let tag = upstream_field("tag").unwrap_or("unknown");
    let commit = upstream_field("commit").unwrap_or("unknown");
    let short_commit = commit.get(..SHORT_COMMIT_LEN).unwrap_or(commit);
    format!("{version}\ncodex-base: {tag} ({short_commit})")
});

/// Value for clap's `long_version`, which clap prefixes with the command's display name.
pub(crate) fn long_version() -> &'static str {
    LONG_VERSION.as_str()
}

/// Reads a `key = "value"` line out of the generated `fork/UPSTREAM` file.
fn upstream_field(key: &str) -> Option<&'static str> {
    UPSTREAM.lines().find_map(|line| {
        line.trim_start()
            .strip_prefix(key)?
            .trim_start()
            .strip_prefix('=')?
            .trim()
            .strip_prefix('"')?
            .strip_suffix('"')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use pretty_assertions::assert_eq;

    #[test]
    fn long_version_is_program_name_then_cargo_version_then_base_line() {
        let rendered = crate::MultitoolCli::command().render_long_version();
        assert!(
            rendered.ends_with(&format!("{}\n", long_version())),
            "--version must render fork_meta::long_version(): {rendered:?}"
        );

        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2, "--version must be two lines: {rendered:?}");

        let tokens: Vec<&str> = lines[0].split_whitespace().collect();
        assert_eq!(
            tokens.len(),
            2,
            "line 1 must be `<name> <version>`: {:?}",
            lines[0]
        );
        assert_eq!(tokens[1], env!("CARGO_PKG_VERSION"));

        assert!(
            lines[1].starts_with("codex-base: ") && lines[1].ends_with(')'),
            "line 2 must name the base tag and end in `)`: {:?}",
            lines[1]
        );
    }
}
