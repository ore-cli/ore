//! Usage accounting for the Anthropic Messages API.
//!
//! Usage arrives split across two frames: `message_start` carries the input and
//! cache counts, `message_delta` the final output count. Both are folded into
//! one running [`TokenUsage`].

use codex_protocol::protocol::TokenUsage;
use serde_json::Value;

fn field(usage: &Value, key: &str) -> Option<i64> {
    usage
        .get(key)
        .and_then(Value::as_i64)
        .map(|value| value.max(0))
}

/// Merges the `usage` object of one SSE frame into the running total, reading it
/// from either `message_start` (nested under `message`) or `message_delta`.
///
/// Returns `prev` untouched when the frame carries no usage at all.
pub(crate) fn merge_anthropic_usage(prev: Option<TokenUsage>, frame: &Value) -> Option<TokenUsage> {
    let Some(usage) = frame
        .pointer("/message/usage")
        .or_else(|| frame.get("usage"))
    else {
        return prev;
    };

    let (uncached, read, write, output) = (
        field(usage, "input_tokens"),
        field(usage, "cache_read_input_tokens"),
        field(usage, "cache_creation_input_tokens"),
        field(usage, "output_tokens"),
    );
    if uncached.is_none() && read.is_none() && write.is_none() && output.is_none() {
        return prev;
    }

    let prev = prev.unwrap_or_default();
    // The stored `input_tokens` has the cache counts folded in; strip them to
    // recover the raw wire field.
    let prev_uncached =
        (prev.input_tokens - prev.cached_input_tokens - prev.cache_write_input_tokens).max(0);

    let uncached_input = uncached.unwrap_or(prev_uncached);
    let cache_read = read.unwrap_or(prev.cached_input_tokens);
    let cache_write = write.unwrap_or(prev.cache_write_input_tokens);
    let output_tokens = output.unwrap_or(prev.output_tokens);

    // Anthropic excludes cache hits from `input_tokens`, and
    // `TokenUsage::non_cached_input` subtracts them again.
    let input_tokens = uncached_input
        .saturating_add(cache_read)
        .saturating_add(cache_write);

    Some(TokenUsage {
        input_tokens,
        cached_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        output_tokens,
        // Thinking tokens are billed as output and never broken out separately.
        reasoning_output_tokens: 0,
        // Never sent on this wire.
        total_tokens: input_tokens.saturating_add(output_tokens),
        codex_rollout_budget_units: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn message_start(usage: Value) -> Value {
        json!({"type": "message_start", "message": {"id": "msg_1", "usage": usage}})
    }

    #[test]
    fn a_frame_with_no_usage_leaves_the_running_total_alone() {
        assert_eq!(None, merge_anthropic_usage(None, &json!({"type": "ping"})));

        let running = TokenUsage {
            input_tokens: 5,
            ..TokenUsage::default()
        };
        assert_eq!(
            Some(running.clone()),
            merge_anthropic_usage(Some(running), &json!({"type": "ping"}))
        );
    }

    /// Anthropic reports `input_tokens` with cache hits excluded and sends no total.
    #[test]
    fn cache_counts_fold_into_input_and_total() {
        let usage = merge_anthropic_usage(
            None,
            &message_start(json!({
                "input_tokens": 10,
                "cache_read_input_tokens": 400,
                "cache_creation_input_tokens": 90,
                "output_tokens": 1,
            })),
        )
        .expect("usage");

        assert_eq!(
            usage,
            TokenUsage {
                input_tokens: 500,
                cached_input_tokens: 400,
                cache_write_input_tokens: 90,
                output_tokens: 1,
                reasoning_output_tokens: 0,
                total_tokens: 501,
                codex_rollout_budget_units: None,
            }
        );
        // Fresh input billed is the uncached input plus the cache write.
        assert_eq!(100, usage.non_cached_input());
    }

    /// `message_delta` carries only the final output count; the input totals must
    /// survive it.
    #[test]
    fn the_trailing_output_count_merges_into_the_opening_frame() {
        let start = merge_anthropic_usage(
            None,
            &message_start(json!({
                "input_tokens": 10,
                "cache_read_input_tokens": 4,
                "cache_creation_input_tokens": 2,
                "output_tokens": 1,
            })),
        );

        let merged = merge_anthropic_usage(
        start,
        &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 77}}),
    )
    .expect("usage");

        assert_eq!(
            merged,
            TokenUsage {
                input_tokens: 16,
                cached_input_tokens: 4,
                cache_write_input_tokens: 2,
                output_tokens: 77,
                reasoning_output_tokens: 0,
                total_tokens: 93,
                codex_rollout_budget_units: None,
            }
        );
    }

    /// Newer versions repeat the input counts on `message_delta`; folding them twice
    /// doubles the cache totals.
    #[test]
    fn repeated_input_counts_do_not_accumulate() {
        let start = merge_anthropic_usage(
            None,
            &message_start(json!({
                "input_tokens": 10,
                "cache_read_input_tokens": 4,
                "output_tokens": 1,
            })),
        );

        let merged = merge_anthropic_usage(
            start,
            &json!({
                "type": "message_delta",
                "usage": {"input_tokens": 10, "cache_read_input_tokens": 4, "output_tokens": 9},
            }),
        )
        .expect("usage");

        assert_eq!(14, merged.input_tokens);
        assert_eq!(4, merged.cached_input_tokens);
        assert_eq!(23, merged.total_tokens);
    }

    /// Token counts are server-controlled; overflow panics in a checked build and
    /// wraps negative in release.
    #[test]
    fn absurd_token_counts_stay_bounded_and_non_negative() {
        let frame = json!({
            "usage": {
                "input_tokens": i64::MAX,
                "cache_read_input_tokens": i64::MAX,
                "cache_creation_input_tokens": i64::MAX,
                "output_tokens": i64::MAX,
            }
        });

        let usage = merge_anthropic_usage(None, &frame).expect("usage");

        for value in [
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.cache_write_input_tokens,
            usage.output_tokens,
            usage.total_tokens,
        ] {
            assert!(value >= 0, "{usage:?}");
        }
    }

    #[test]
    fn negative_token_counts_are_clamped() {
        let frame = json!({"usage": {"input_tokens": -5, "output_tokens": -1}});

        let usage = merge_anthropic_usage(None, &frame).expect("usage");

        assert!(
            usage.input_tokens >= 0 && usage.output_tokens >= 0,
            "{usage:?}"
        );
    }
}
