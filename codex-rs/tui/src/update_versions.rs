pub(crate) fn is_newer(latest: &str, current: &str) -> Option<bool> {
    let latest = parse_version(latest)?;
    let current = parse_version(current)?;
    // releases/latest never resolves to a prerelease, and one is not an upgrade
    // to offer even if it did.
    Some(latest.is_release && latest > current)
}

pub(crate) fn extract_version_from_latest_tag(latest_tag_name: &str) -> anyhow::Result<String> {
    latest_tag_name
        .strip_prefix("ore-v")
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse latest tag name '{latest_tag_name}'"))
}

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
    fn an_unparseable_version_reports_nothing() {
        assert_eq!(is_newer("nightly", "1.2.3"), None);
        assert_eq!(is_newer("1.2.3", "not-a-version"), None);
    }
}
