//! Usage accounting for the Gemini `generateContent` API.
//!
//! Every frame may repeat a whole `usageMetadata`, and its counts are running
//! totals rather than per-frame increments, so a later frame replaces what an
//! earlier one reported instead of adding to it. Absent fields keep whatever
//! the running total already holds, which is what makes a frame that carries
//! only the final counts safe to merge.

use codex_protocol::protocol::TokenUsage;
use serde_json::Value;

/// Clamps at zero: a negative count underflows the subtraction
/// `TokenUsage::non_cached_input` does downstream.
///
/// `Value::as_i64` also reads a float or an out-of-range literal as absent --
/// a gateway reporting `1e400` parses as `+inf` -- rather than saturating into
/// a number the status card would render as fact.
fn field(usage: &Value, key: &str) -> Option<i64> {
    usage
        .get(key)
        .and_then(Value::as_i64)
        .map(|value| value.max(0))
}

/// Merges one frame's `usageMetadata` into the running total.
///
/// Returns `prev` untouched when the frame carries no usage at all.
pub(crate) fn merge_gemini_usage(prev: Option<TokenUsage>, frame: &Value) -> Option<TokenUsage> {
    let Some(usage) = frame.get("usageMetadata") else {
        return prev;
    };

    let (prompt, candidates, cached, thoughts, total) = (
        field(usage, "promptTokenCount"),
        field(usage, "candidatesTokenCount"),
        field(usage, "cachedContentTokenCount"),
        field(usage, "thoughtsTokenCount"),
        field(usage, "totalTokenCount"),
    );
    if prompt.is_none()
        && candidates.is_none()
        && cached.is_none()
        && thoughts.is_none()
        && total.is_none()
    {
        return prev;
    }

    let prev = prev.unwrap_or_default();

    let cached_input_tokens = cached.unwrap_or(prev.cached_input_tokens);
    // Unlike Anthropic's `input_tokens`, `promptTokenCount` already counts the
    // cache hits, so it is stored as it arrives. Folding the cached count in
    // again would double it, and `non_cached_input` subtracts it once.
    let input_tokens = prompt.unwrap_or(prev.input_tokens);

    let reasoning_output_tokens = thoughts.unwrap_or(prev.reasoning_output_tokens);
    // `candidatesTokenCount` excludes the thinking tokens, which are billed as
    // output all the same, so they are folded in here -- and stripped back out
    // of the running total before defaulting, the way `anthropic_usage` strips
    // its folded cache counts.
    let prev_visible = (prev.output_tokens - prev.reasoning_output_tokens).max(0);
    let output_tokens = candidates
        .unwrap_or(prev_visible)
        .saturating_add(reasoning_output_tokens);

    Some(TokenUsage {
        input_tokens,
        cached_input_tokens,
        // This wire reports no cache-write count: an explicit cache is created
        // by a separate API call and billed there, never on the turn that
        // reads it.
        cache_write_input_tokens: 0,
        output_tokens,
        reasoning_output_tokens,
        // `totalTokenCount` already sums prompt, candidates and thoughts.
        total_tokens: total.unwrap_or_else(|| input_tokens.saturating_add(output_tokens)),
        codex_rollout_budget_units: None,
    })
}

#[cfg(test)]
#[path = "gemini_usage_tests.rs"]
mod tests;
