#[cfg(any(not(debug_assertions), test))]
pub(crate) fn is_newer(latest: &str, current: &str) -> Option<bool> {
    let latest = parse_version(latest)?;
    let current = parse_version(current)?;
    // releases/latest never resolves to a prerelease, and one is not an upgrade
    // to offer even if it did.
    Some(latest.is_release && latest > current)
}

#[cfg(any(not(debug_assertions), test))]
pub(crate) fn extract_version_from_latest_tag(latest_tag_name: &str) -> anyhow::Result<String> {
    latest_tag_name
        .strip_prefix("ore-v")
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse latest tag name '{latest_tag_name}'"))
}

#[cfg(any(not(debug_assertions), test))]
pub(crate) fn is_source_build_version(version: &str) -> bool {
    matches!(
        parse_version(version),
        Some(Version {
            major: 0,
            minor: 0,
            patch: 0,
            ..
        })
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerVersionNoticeKind {
    Older,
    Different,
}

/// Stable clients compare release precedence. Prerelease clients only order versions
/// within the same release line; local builds and other release lines compare identity.
pub(crate) fn server_version_notice_kind(
    client: &str,
    server: &str,
) -> Option<ServerVersionNoticeKind> {
    let client = semver::Version::parse(client).ok()?;
    let server = semver::Version::parse(server).ok()?;
    let client_release = (client.major, client.minor, client.patch);
    let server_release = (server.major, server.minor, server.patch);
    let client_is_local = client_release == (0, 0, 0) || !client.build.is_empty();
    if client_is_local || (!client.pre.is_empty() && client_release != server_release) {
        return (client != server).then_some(ServerVersionNoticeKind::Different);
    }
    (server.build.is_empty() && server_release != (0, 0, 0) && client > server)
        .then_some(ServerVersionNoticeKind::Older)
}

#[cfg(any(not(debug_assertions), test))]
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    /// Last, so the derived ordering puts a prerelease below the release it
    /// precedes: 0.147.0-alpha.4 < 0.147.0.
    is_release: bool,
}

/// Semver precedence, narrowed to the shapes ore's tags produce. Everything
/// after the first `-` is a prerelease marker and is not ordered further:
/// upgrades are only ever offered *to* a release.
#[cfg(any(not(debug_assertions), test))]
fn parse_version(v: &str) -> Option<Version> {
    let v = v.trim();
    let (core, is_release) = match v.split_once('-') {
        Some((core, _)) => (core, false),
        None => (v, true),
    };
    let mut iter = core.split('.');
    Some(Version {
        major: iter.next()?.parse().ok()?,
        minor: iter.next()?.parse().ok()?,
        patch: iter.next()?.parse().ok()?,
        is_release,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn extracts_version_from_latest_tag() {
        assert_eq!(
            extract_version_from_latest_tag("ore-v1.5.0").expect("failed to parse version"),
            "1.5.0"
        );
    }

    #[test]
    fn latest_tag_without_prefix_is_invalid() {
        assert!(extract_version_from_latest_tag("v1.5.0").is_err());
    }

    #[test]
    fn prerelease_version_is_not_considered_newer() {
        assert_eq!(is_newer("0.11.0-beta.1", "0.11.0"), Some(false));
        assert_eq!(is_newer("1.0.0-rc.1", "1.0.0"), Some(false));
    }

    /// A user on a prerelease still has to be told about releases, and the
    /// version they are running no longer parses as three plain numbers.
    #[test]
    fn a_prerelease_install_still_gets_offered_releases() {
        assert_eq!(is_newer("0.146.1", "0.146.0-alpha.4"), Some(true));
        assert_eq!(is_newer("0.147.0", "0.147.0-alpha.4.1"), Some(true));
        assert_eq!(
            is_newer("0.146.1", "0.147.0-alpha.4"),
            Some(false),
            "an alpha of a later version is not behind an earlier release"
        );
    }

    #[test]
    fn plain_semver_comparisons_work() {
        assert_eq!(is_newer("0.11.1", "0.11.0"), Some(true));
        assert_eq!(is_newer("0.11.0", "0.11.1"), Some(false));
        assert_eq!(is_newer("1.0.0", "0.9.9"), Some(true));
        assert_eq!(is_newer("0.9.9", "1.0.0"), Some(false));
    }

    #[test]
    fn source_build_version_is_not_checked() {
        assert!(is_source_build_version("0.0.0"));
        assert!(!is_source_build_version("0.1.0"));
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(is_newer(" 1.2.3 ", "1.2.2"), Some(true));
    }

    #[test]
    fn stable_clients_only_warn_for_older_releases() {
        for (client, server, expected) in [
            ("0.152.1", "0.152.0", Some(ServerVersionNoticeKind::Older)),
            ("0.153.0", "0.152.1", Some(ServerVersionNoticeKind::Older)),
            (
                "0.153.0",
                "0.153.0-alpha.10.1",
                Some(ServerVersionNoticeKind::Older),
            ),
            (
                "0.156.0",
                "0.155.0-alpha.12",
                Some(ServerVersionNoticeKind::Older),
            ),
            ("0.153.0", "0.153.0", None),
            ("0.153.0", "0.154.0", None),
            ("0.153.0", "0.154.0-alpha.1", None),
            ("0.153.0", "0.0.0", None),
            ("0.153.0", "0.0.0-alpha.1", None),
            ("0.153.0", "0.152.0+dev", None),
        ] {
            assert_eq!(server_version_notice_kind(client, server), expected);
        }
    }

    #[test]
    fn prerelease_clients_compare_within_the_same_release_line() {
        for (newer, older) in [
            ("0.155.0-alpha.23", "0.155.0-alpha.22"),
            ("0.155.0-alpha.24", "0.155.0-alpha.23"),
            ("0.153.0-alpha.10", "0.153.0-alpha.9"),
            ("0.153.0-alpha.10.1", "0.153.0-alpha.9.2"),
            ("0.153.0-alpha.10.10", "0.153.0-alpha.10.9"),
            ("0.153.0-alpha.10.1", "0.153.0-alpha.10"),
            ("0.155.0", "0.155.0-alpha.23"),
        ] {
            assert_eq!(
                server_version_notice_kind(newer, older),
                Some(ServerVersionNoticeKind::Older)
            );
            assert_eq!(server_version_notice_kind(older, newer), None);
            assert_eq!(server_version_notice_kind(newer, newer), None);
        }
    }

    #[test]
    fn prerelease_clients_on_other_release_lines_and_local_clients_warn_for_mismatches() {
        for (client, server) in [
            ("0.155.0-alpha.23", "0.156.0"),
            ("0.155.0-alpha.12", "0.154.0"),
            ("0.0.0", "0.153.0"),
            ("0.0.0", "0.153.0-alpha.10"),
            ("0.153.0+dev", "0.153.0"),
            ("0.153.0-alpha.10", "0.0.0"),
        ] {
            assert_eq!(
                server_version_notice_kind(client, server),
                Some(ServerVersionNoticeKind::Different)
            );
            assert_eq!(server_version_notice_kind(client, client), None);
        }
    }

    #[test]
    fn unknown_or_malformed_versions_do_not_produce_notices() {
        for version in [
            "unknown",
            "dev",
            "0.0.0.0",
            "0.153",
            "0.153.0.1",
            " 0.153.0",
            "+0.153.0",
            "0.0153.0",
            "0.153.0-alpha.01",
            "0.153.0-alpha..1",
        ] {
            for release in ["0.153.0", "0.153.0-alpha.10", "0.0.0"] {
                assert_eq!(server_version_notice_kind(version, release), None);
                assert_eq!(server_version_notice_kind(release, version), None);
            }
        }
    }

    #[test]
    fn an_unparseable_version_reports_nothing() {
        assert_eq!(is_newer("nightly", "1.2.3"), None);
        assert_eq!(is_newer("1.2.3", "not-a-version"), None);
    }
}
