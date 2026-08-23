//! Rate limits from the Messages API's response headers.
//!
//! Deliberately separate from `crate::rate_limits`, which is upstream's and
//! reads `x-codex-*` headers from the Codex backend. Anthropic reports a
//! different vocabulary on every Messages response, so this fork parses it
//! here rather than growing a provider branch inside an upstream file. The
//! module is mounted from `sse/anthropic.rs` with `#[path]`, which is also
//! fork-owned, so nothing upstream has to gain a `mod` line.

use chrono::DateTime;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use http::HeaderMap;

/// Anthropic reports absolute counts; `RateLimitWindow` wants a percentage.
struct Window {
    used_percent: f64,
    resets_at: Option<i64>,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// `anthropic-ratelimit-<family>-{limit,remaining,reset}`.
///
/// Returns `None` unless both counts parse and the limit is positive: a zero
/// limit would divide by zero, and a partially-reported family is not a window
/// anyone can act on.
fn window(headers: &HeaderMap, family: &str) -> Option<Window> {
    let limit: f64 = header(headers, &format!("anthropic-ratelimit-{family}-limit"))?
        .parse()
        .ok()?;
    let remaining: f64 = header(headers, &format!("anthropic-ratelimit-{family}-remaining"))?
        .parse()
        .ok()?;
    if limit <= 0.0 {
        return None;
    }

    // Clamped because `remaining` has been observed above `limit` immediately
    // after a window rolls over, which would otherwise render as negative use.
    let used_percent = (((limit - remaining) / limit) * 100.0).clamp(0.0, 100.0);

    // RFC3339, unlike the Codex backend's unix seconds.
    let resets_at = header(headers, &format!("anthropic-ratelimit-{family}-reset"))
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp());

    Some(Window {
        used_percent,
        resets_at,
    })
}

impl From<Window> for RateLimitWindow {
    fn from(value: Window) -> Self {
        Self {
            used_percent: value.used_percent,
            // Anthropic states the reset instant but never the window's
            // duration, and inferring one from the reset would be a guess that
            // renders as fact.
            window_minutes: None,
            resets_at: value.resets_at,
        }
    }
}

/// Builds a snapshot from a Messages response's headers.
///
/// Primary is the token window, because tokens are what actually bind an agent
/// doing large-context turns; requests become secondary. `input-tokens` is
/// preferred over the combined `tokens` family when both are present, since it
/// is the one a long conversation exhausts first. Returns `None` when Anthropic
/// reports no usable window, so the caller sends no event at all rather than an
/// empty one that would read as "limits known, and they are zero".
pub(crate) fn parse_anthropic_rate_limits(headers: &HeaderMap) -> Option<RateLimitSnapshot> {
    let tokens = window(headers, "input-tokens").or_else(|| window(headers, "tokens"));
    let requests = window(headers, "requests");

    let (primary, secondary, limit_name) = match (tokens, requests) {
        (Some(tokens), requests) => (tokens, requests, "input tokens"),
        (None, Some(requests)) => (requests, None, "requests"),
        (None, None) => return None,
    };

    Some(RateLimitSnapshot {
        limit_id: None,
        limit_name: Some(limit_name.to_string()),
        primary: Some(primary.into()),
        secondary: secondary.map(Into::into),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    })
}

#[cfg(test)]
#[path = "anthropic_rate_limits_tests.rs"]
mod tests;
