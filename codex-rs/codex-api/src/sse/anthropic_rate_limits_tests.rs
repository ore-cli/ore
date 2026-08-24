use super::*;
use pretty_assertions::assert_eq;

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            name.parse::<http::HeaderName>().expect("header name"),
            value.parse().expect("header value"),
        );
    }
    map
}

#[test]
fn tokens_are_primary_and_requests_secondary() {
    let snapshot = parse_anthropic_rate_limits(&headers(&[
        ("anthropic-ratelimit-input-tokens-limit", "100000"),
        ("anthropic-ratelimit-input-tokens-remaining", "25000"),
        (
            "anthropic-ratelimit-input-tokens-reset",
            "2026-08-23T09:00:00Z",
        ),
        ("anthropic-ratelimit-requests-limit", "1000"),
        ("anthropic-ratelimit-requests-remaining", "900"),
    ]))
    .expect("a snapshot");

    let primary = snapshot.primary.expect("primary");
    assert_eq!(primary.used_percent, 75.0);
    assert_eq!(
        primary.resets_at,
        Some(
            DateTime::parse_from_rfc3339("2026-08-23T09:00:00Z")
                .expect("fixture parses")
                .timestamp()
        )
    );
    assert_eq!(
        primary.window_minutes, None,
        "Anthropic reports the reset instant, never the window's duration"
    );
    assert_eq!(snapshot.secondary.expect("secondary").used_percent, 10.0);
    assert_eq!(snapshot.limit_name.as_deref(), Some("input tokens"));
}

#[test]
fn the_combined_tokens_family_is_a_fallback_for_input_tokens() {
    let snapshot = parse_anthropic_rate_limits(&headers(&[
        ("anthropic-ratelimit-tokens-limit", "80"),
        ("anthropic-ratelimit-tokens-remaining", "20"),
    ]))
    .expect("a snapshot");

    assert_eq!(snapshot.primary.expect("primary").used_percent, 75.0);
}

#[test]
fn requests_alone_still_produce_a_snapshot() {
    let snapshot = parse_anthropic_rate_limits(&headers(&[
        ("anthropic-ratelimit-requests-limit", "50"),
        ("anthropic-ratelimit-requests-remaining", "40"),
    ]))
    .expect("a snapshot");

    assert_eq!(snapshot.primary.expect("primary").used_percent, 20.0);
    assert_eq!(snapshot.secondary, None);
    assert_eq!(snapshot.limit_name.as_deref(), Some("requests"));
}

#[test]
fn no_rate_limit_headers_is_no_snapshot() {
    assert_eq!(parse_anthropic_rate_limits(&headers(&[])), None);
    assert_eq!(
        parse_anthropic_rate_limits(&headers(&[("anthropic-version", "2023-06-01")])),
        None,
        "an unrelated header must not manufacture an all-zero snapshot"
    );
}

#[test]
fn a_half_reported_family_is_not_a_window() {
    assert_eq!(
        parse_anthropic_rate_limits(&headers(&[(
            "anthropic-ratelimit-input-tokens-limit",
            "100"
        )])),
        None,
        "a limit without a remaining count cannot be turned into a percentage"
    );
}

#[test]
fn a_zero_limit_does_not_divide_by_zero() {
    assert_eq!(
        parse_anthropic_rate_limits(&headers(&[
            ("anthropic-ratelimit-input-tokens-limit", "0"),
            ("anthropic-ratelimit-input-tokens-remaining", "0"),
        ])),
        None
    );
}

#[test]
fn remaining_above_limit_clamps_rather_than_going_negative() {
    let snapshot = parse_anthropic_rate_limits(&headers(&[
        ("anthropic-ratelimit-input-tokens-limit", "100"),
        ("anthropic-ratelimit-input-tokens-remaining", "150"),
    ]))
    .expect("a snapshot");

    assert_eq!(snapshot.primary.expect("primary").used_percent, 0.0);
}

#[test]
fn an_unparseable_reset_keeps_the_window() {
    let snapshot = parse_anthropic_rate_limits(&headers(&[
        ("anthropic-ratelimit-input-tokens-limit", "100"),
        ("anthropic-ratelimit-input-tokens-remaining", "50"),
        ("anthropic-ratelimit-input-tokens-reset", "not-a-timestamp"),
    ]))
    .expect("a snapshot");

    let primary = snapshot.primary.expect("primary");
    assert_eq!(primary.used_percent, 50.0);
    assert_eq!(
        primary.resets_at, None,
        "a bad reset must cost the timestamp, not the whole window"
    );
}

#[test]
fn non_finite_values_produce_no_window() {
    // Each of these parses successfully as f64 and would otherwise reach the
    // status card as `used_percent: NaN`, which renders as "0% used".
    for (limit, remaining) in [
        ("NaN", "10"),
        ("inf", "10"),
        ("infinity", "10"),
        ("1e400", "10"),
        ("-inf", "10"),
        ("100", "NaN"),
        ("100", "inf"),
        ("100", "1e400"),
    ] {
        let snapshot = parse_anthropic_rate_limits(&headers(&[
            ("anthropic-ratelimit-input-tokens-limit", limit),
            ("anthropic-ratelimit-input-tokens-remaining", remaining),
        ]));
        assert_eq!(
            snapshot, None,
            "limit={limit:?} remaining={remaining:?} must yield no window, not a NaN percentage"
        );
    }
}

#[test]
fn a_finite_window_never_yields_a_non_finite_percentage() {
    for (limit, remaining) in [("100", "25"), ("1", "0"), ("1e308", "1"), ("100", "-50")] {
        let snapshot = parse_anthropic_rate_limits(&headers(&[
            ("anthropic-ratelimit-input-tokens-limit", limit),
            ("anthropic-ratelimit-input-tokens-remaining", remaining),
        ]))
        .expect("a snapshot");
        let used = snapshot.primary.expect("primary").used_percent;
        assert!(
            used.is_finite() && (0.0..=100.0).contains(&used),
            "limit={limit:?} remaining={remaining:?} produced {used}"
        );
    }
}
