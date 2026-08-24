use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn frame(usage: Value) -> Value {
    json!({"candidates": [{"content": {"parts": [], "role": "model"}}], "usageMetadata": usage})
}

#[test]
fn a_frame_with_no_usage_leaves_the_running_total_alone() {
    assert_eq!(None, merge_gemini_usage(None, &json!({"candidates": []})));

    let running = TokenUsage {
        input_tokens: 5,
        ..TokenUsage::default()
    };
    assert_eq!(
        Some(running.clone()),
        merge_gemini_usage(Some(running), &json!({"candidates": []}))
    );
}

/// An empty `usageMetadata` is not "zero tokens were used".
#[test]
fn an_empty_usage_object_leaves_the_running_total_alone() {
    let running = TokenUsage {
        input_tokens: 5,
        ..TokenUsage::default()
    };
    assert_eq!(
        Some(running.clone()),
        merge_gemini_usage(Some(running), &frame(json!({})))
    );
}

/// `promptTokenCount` counts the cache hits, so it is stored as it arrives;
/// thinking tokens are billed as output but reported apart from
/// `candidatesTokenCount`.
#[test]
fn cached_input_is_already_counted_and_thoughts_fold_into_output() {
    let usage = merge_gemini_usage(
        None,
        &frame(json!({
            "promptTokenCount": 500,
            "cachedContentTokenCount": 400,
            "candidatesTokenCount": 20,
            "thoughtsTokenCount": 30,
            "totalTokenCount": 550,
        })),
    )
    .expect("usage");

    assert_eq!(
        usage,
        TokenUsage {
            input_tokens: 500,
            cached_input_tokens: 400,
            cache_write_input_tokens: 0,
            output_tokens: 50,
            reasoning_output_tokens: 30,
            total_tokens: 550,
            codex_rollout_budget_units: None,
        }
    );
    // Fresh input billed is the prompt minus what the cache served.
    assert_eq!(100, usage.non_cached_input());
}

/// The counts are running totals, so repeating them must not accumulate.
#[test]
fn repeated_running_totals_replace_rather_than_add() {
    let first = merge_gemini_usage(
        None,
        &frame(json!({
            "promptTokenCount": 100,
            "cachedContentTokenCount": 40,
            "candidatesTokenCount": 5,
            "thoughtsTokenCount": 2,
        })),
    );

    let merged = merge_gemini_usage(
        first,
        &frame(json!({
            "promptTokenCount": 100,
            "cachedContentTokenCount": 40,
            "candidatesTokenCount": 9,
            "thoughtsTokenCount": 6,
            "totalTokenCount": 115,
        })),
    )
    .expect("usage");

    assert_eq!(
        merged,
        TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 40,
            cache_write_input_tokens: 0,
            output_tokens: 15,
            reasoning_output_tokens: 6,
            total_tokens: 115,
            codex_rollout_budget_units: None,
        }
    );
}

/// Early frames often carry only the prompt count; the trailing frame adds the
/// output counts and must not lose the input ones.
#[test]
fn a_trailing_output_count_merges_into_the_opening_frame() {
    let start = merge_gemini_usage(
        None,
        &frame(json!({"promptTokenCount": 42, "cachedContentTokenCount": 7})),
    );

    let merged =
        merge_gemini_usage(start, &frame(json!({"candidatesTokenCount": 77}))).expect("usage");

    assert_eq!(42, merged.input_tokens);
    assert_eq!(7, merged.cached_input_tokens);
    assert_eq!(77, merged.output_tokens);
    assert_eq!(119, merged.total_tokens);
}

/// The thinking tokens are folded into `output_tokens`, so a later frame that
/// restates only the visible count must not re-add the old thoughts.
#[test]
fn folded_thinking_tokens_do_not_accumulate() {
    let start = merge_gemini_usage(
        None,
        &frame(json!({"candidatesTokenCount": 10, "thoughtsTokenCount": 5})),
    );
    assert_eq!(Some(15), start.as_ref().map(|usage| usage.output_tokens));

    let merged =
        merge_gemini_usage(start, &frame(json!({"candidatesTokenCount": 12}))).expect("usage");

    assert_eq!(17, merged.output_tokens);
    assert_eq!(5, merged.reasoning_output_tokens);
}

/// Token counts are server-controlled; overflow panics in a checked build and
/// wraps negative in release.
#[test]
fn absurd_token_counts_stay_bounded_and_non_negative() {
    let usage = merge_gemini_usage(
        None,
        &frame(json!({
            "promptTokenCount": i64::MAX,
            "candidatesTokenCount": i64::MAX,
            "cachedContentTokenCount": i64::MAX,
            "thoughtsTokenCount": i64::MAX,
        })),
    )
    .expect("usage");

    for value in [
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.output_tokens,
        usage.reasoning_output_tokens,
        usage.total_tokens,
    ] {
        assert!(value >= 0, "{usage:?}");
    }
}

#[test]
fn negative_token_counts_are_clamped() {
    let usage = merge_gemini_usage(
        None,
        &frame(json!({"promptTokenCount": -5, "candidatesTokenCount": -1})),
    )
    .expect("usage");

    assert!(
        usage.input_tokens >= 0 && usage.output_tokens >= 0,
        "{usage:?}"
    );
}

/// A count that is not an integer -- a gateway reporting a float, or a value
/// past `i64` -- must not land in the total as a number the status card would
/// render as fact.
#[test]
fn non_finite_and_fractional_counts_read_as_absent() {
    let running = TokenUsage {
        input_tokens: 11,
        output_tokens: 3,
        ..TokenUsage::default()
    };

    // Parsed from text rather than built with `json!`, which would normalize
    // these away before the parser ever sees them.
    let frame: Value = serde_json::from_str(
        r#"{"usageMetadata": {"promptTokenCount": 1e40, "candidatesTokenCount": 2.5, "totalTokenCount": 14}}"#,
    )
    .expect("frame");

    let usage = merge_gemini_usage(Some(running), &frame).expect("usage");

    assert_eq!(11, usage.input_tokens);
    assert_eq!(3, usage.output_tokens);
    assert_eq!(14, usage.total_tokens);
}
