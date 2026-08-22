//! Maps a Chat Completions `usage` object onto [`TokenUsage`].

use codex_protocol::protocol::TokenUsage;
use serde_json::Value;

/// Clamps at zero; a server-supplied negative underflows later subtractions.
fn field(usage: &Value, key: &str) -> Option<i64> {
    usage
        .get(key)
        .and_then(Value::as_i64)
        .map(|value| value.max(0))
}

fn nested_field(usage: &Value, parent: &str, key: &str) -> Option<i64> {
    usage
        .get(parent)
        .and_then(|details| details.get(key))
        .and_then(Value::as_i64)
        .map(|value| value.max(0))
}

/// Converts the trailing `usage` chunk into [`TokenUsage`]. `None`, not zeros,
/// when neither count is present -- a provider may ignore `stream_options`.
pub(crate) fn token_usage_from_chat_usage(usage: &Value) -> Option<TokenUsage> {
    let input_tokens = field(usage, "prompt_tokens");
    let output_tokens = field(usage, "completion_tokens");
    if input_tokens.is_none() && output_tokens.is_none() {
        return None;
    }

    let input_tokens = input_tokens.unwrap_or(0);
    let output_tokens = output_tokens.unwrap_or(0);

    Some(TokenUsage {
        input_tokens,
        // Cache accounting is spelled three ways depending on the backend.
        cached_input_tokens: nested_field(usage, "prompt_tokens_details", "cached_tokens")
            .or_else(|| field(usage, "cache_read_input_tokens"))
            .or_else(|| field(usage, "prompt_cache_hit_tokens"))
            .unwrap_or(0),
        cache_write_input_tokens: field(usage, "cache_creation_input_tokens")
            .or_else(|| field(usage, "prompt_cache_miss_tokens"))
            .unwrap_or(0),
        output_tokens,
        reasoning_output_tokens: nested_field(
            usage,
            "completion_tokens_details",
            "reasoning_tokens",
        )
        .unwrap_or(0),
        total_tokens: field(usage, "total_tokens")
            .unwrap_or_else(|| input_tokens.saturating_add(output_tokens)),
        codex_rollout_budget_units: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn absurd_counts_stay_bounded_and_non_negative() {
        let usage = token_usage_from_chat_usage(&json!({
            "prompt_tokens": i64::MAX,
            "completion_tokens": i64::MAX,
        }))
        .expect("usage");

        assert!(usage.total_tokens >= 0, "{usage:?}");
    }

    #[test]
    fn negative_counts_are_clamped() {
        let usage = token_usage_from_chat_usage(&json!({
            "prompt_tokens": -10,
            "completion_tokens": -3,
        }))
        .expect("usage");

        assert!(
            usage.input_tokens >= 0 && usage.output_tokens >= 0,
            "{usage:?}"
        );
    }

    #[test]
    fn absent_counts_report_no_usage() {
        assert_eq!(None, token_usage_from_chat_usage(&json!({})));
        assert_eq!(
            None,
            token_usage_from_chat_usage(&json!({"total_tokens": 10}))
        );
    }

    #[test]
    fn totals_default_to_the_sum_when_absent() {
        let usage =
            token_usage_from_chat_usage(&json!({"prompt_tokens": 7, "completion_tokens": 3}))
                .expect("usage");
        assert_eq!(7, usage.input_tokens);
        assert_eq!(3, usage.output_tokens);
        assert_eq!(10, usage.total_tokens);
    }

    #[test]
    fn reads_each_cache_spelling() {
        let nested = token_usage_from_chat_usage(&json!({
            "prompt_tokens": 5,
            "completion_tokens": 1,
            "prompt_tokens_details": {"cached_tokens": 4},
        }))
        .expect("usage");
        assert_eq!(4, nested.cached_input_tokens);

        let anthropic_style = token_usage_from_chat_usage(&json!({
            "prompt_tokens": 5,
            "completion_tokens": 1,
            "cache_read_input_tokens": 2,
            "cache_creation_input_tokens": 6,
        }))
        .expect("usage");
        assert_eq!(2, anthropic_style.cached_input_tokens);
        assert_eq!(6, anthropic_style.cache_write_input_tokens);

        let deepseek_style = token_usage_from_chat_usage(&json!({
            "prompt_tokens": 5,
            "completion_tokens": 1,
            "prompt_cache_hit_tokens": 3,
            "prompt_cache_miss_tokens": 2,
        }))
        .expect("usage");
        assert_eq!(3, deepseek_style.cached_input_tokens);
        assert_eq!(2, deepseek_style.cache_write_input_tokens);
    }

    #[test]
    fn reads_reasoning_tokens() {
        let usage = token_usage_from_chat_usage(&json!({
            "prompt_tokens": 5,
            "completion_tokens": 9,
            "completion_tokens_details": {"reasoning_tokens": 4},
        }))
        .expect("usage");
        assert_eq!(4, usage.reasoning_output_tokens);
    }
}
