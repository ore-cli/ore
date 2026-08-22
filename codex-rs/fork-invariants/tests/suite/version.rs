//! I-VER — scheme C: the ore version is `1.{upstream minor}.{ore patch}`.
//!
//! Note this deliberately does NOT compare `CARGO_PKG_VERSION` to
//! `fork/VERSION`: `[workspace.package] version` is written by
//! `fork/assemble.sh`, never by a series commit, so the two agree on generated
//! `main` and differ on `delta`. That equality is checked by
//! `fork/verify/version_check.py`, which knows which branch it is looking at.

use std::path::Path;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("codex-rs/fork-invariants sits two levels below the repo root")
        .to_path_buf()
}

/// Splits `MAJOR.MINOR.PATCH` into its three numeric components, rejecting
/// anything with a missing or extra component.
fn semver_core(version: &str) -> Option<(u64, u64, u64)> {
    let mut components = version.split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next()?.parse().ok()?;
    let patch = components.next()?.parse().ok()?;
    if components.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Reads a top-level `key = "value"` out of `fork/UPSTREAM` without pulling in
/// a TOML parser, so the check survives that file gaining comments or fields.
fn scalar(document: &str, key: &str) -> Option<String> {
    for line in document.lines() {
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            continue;
        };
        if raw_key.trim() != key {
            continue;
        }
        let value = raw_value.trim();
        let value = value.strip_prefix('"')?;
        let value = value.strip_suffix('"')?;
        return Some(value.to_owned());
    }
    None
}

#[test]
fn fork_version_is_scheme_c_and_tracks_the_upstream_minor() {
    let root = repo_root();

    let raw_version =
        std::fs::read_to_string(root.join("fork").join("VERSION")).expect("read fork/VERSION");
    let version = raw_version.trim();
    let (core, prerelease) = match version.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (version, None),
    };
    let (major, minor, _patch) = semver_core(core)
        .unwrap_or_else(|| panic!("fork/VERSION must be MAJOR.MINOR.PATCH, found {version:?}"));
    assert_eq!(major, 1, "scheme C pins the ore major to 1: {version:?}");
    if let Some(prerelease) = prerelease {
        assert!(
            prerelease.starts_with("alpha") || prerelease.starts_with("beta"),
            "only alpha/beta prereleases are allowed: {version:?}"
        );
    }

    let upstream =
        std::fs::read_to_string(root.join("fork").join("UPSTREAM")).expect("read fork/UPSTREAM");
    let tag = scalar(&upstream, "tag").expect("fork/UPSTREAM must define tag");
    let upstream_core = tag
        .strip_prefix("rust-v")
        .unwrap_or_else(|| panic!("upstream tag must be rust-vX.Y.Z, found {tag:?}"));
    let (_, upstream_minor, _) = semver_core(upstream_core)
        .unwrap_or_else(|| panic!("upstream tag must be rust-vX.Y.Z, found {tag:?}"));

    assert_eq!(
        minor, upstream_minor,
        "scheme C derives the ore minor from the upstream base ({version} vs {tag}); \
         fork/assemble.sh sets it on a new base tag"
    );
}
