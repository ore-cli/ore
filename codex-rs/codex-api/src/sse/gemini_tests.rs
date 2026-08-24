use super::*;
use assert_matches::assert_matches;
use futures::TryStreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio_util::io::ReaderStream;

/// The REST wire sends bare `data:` frames with no `event:` line.
fn body(frames: &[Value]) -> String {
    frames.iter().fold(String::new(), |mut body, frame| {
        body.push_str(&format!("data: {frame}\n\n"));
        body
    })
}

async fn collect_results(body: &str) -> Vec<Result<ResponseEvent, ApiError>> {
    let reader = ReaderStream::new(std::io::Cursor::new(body.to_string()))
        .map_err(|err| codex_client::TransportError::Network(err.to_string()));
    let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(64);
    tokio::spawn(process_gemini_sse(
        reader,
        tx,
        Duration::from_millis(1000),
        None,
    ));

    let mut out = Vec::new();
    while let Some(event) = rx.recv().await {
        out.push(event);
    }
    out
}

async fn collect_events(body: &str) -> Vec<ResponseEvent> {
    collect_results(body)
        .await
        .into_iter()
        .map(|event| event.expect("stream error"))
        .collect()
}

/// One partial `GenerateContentResponse`.
fn frame(parts: Value) -> Value {
    json!({
        "candidates": [{"content": {"parts": parts, "role": "model"}, "index": 0}],
        "responseId": "resp-1",
    })
}

fn text_frame(text: &str) -> Value {
    frame(json!([{"text": text}]))
}

/// The final frame: `finishReason` and `usageMetadata` arrive together.
fn final_frame(finish_reason: &str, usage: Value) -> Value {
    json!({
        "candidates": [{"content": {"parts": [], "role": "model"}, "finishReason": finish_reason, "index": 0}],
        "usageMetadata": usage,
        "responseId": "resp-1",
    })
}

fn usage() -> Value {
    json!({
        "promptTokenCount": 100,
        "cachedContentTokenCount": 40,
        "candidatesTokenCount": 9,
        "thoughtsTokenCount": 6,
        "totalTokenCount": 115,
    })
}

#[tokio::test]
async fn text_deltas_accumulate_across_frames_and_complete_with_usage() {
    let frames = [
        text_frame("Hi "),
        text_frame("there"),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    assert_matches!(
        &events[..],
        [
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. }),
            ResponseEvent::OutputTextDelta(first),
            ResponseEvent::OutputTextDelta(second),
            ResponseEvent::OutputItemDone(ResponseItem::Message { role, content, .. }),
            ResponseEvent::Completed { response_id, end_turn: Some(true), token_usage: Some(usage) },
        ] if first == "Hi "
            && second == "there"
            && role == "assistant"
            && content == &[ContentItem::OutputText { text: "Hi there".to_string() }]
            && response_id == "resp-1"
            && usage.input_tokens == 100
            && usage.cached_input_tokens == 40
            && usage.output_tokens == 15
            && usage.reasoning_output_tokens == 6
    );
}

/// Usage is a running total repeated on several frames; folding it twice would
/// double every count.
#[tokio::test]
async fn usage_accumulates_to_the_last_frame_that_reported_it() {
    let mut early = text_frame("hello");
    early["usageMetadata"] = json!({"promptTokenCount": 100, "candidatesTokenCount": 2});
    let frames = [
        early,
        final_frame(
            "STOP",
            json!({"promptTokenCount": 100, "candidatesTokenCount": 9, "totalTokenCount": 109}),
        ),
    ];

    let events = collect_events(&body(&frames)).await;

    assert_matches!(
        events.last(),
        Some(ResponseEvent::Completed { token_usage: Some(usage), .. })
            if usage.input_tokens == 100 && usage.output_tokens == 9 && usage.total_tokens == 109
    );
}

/// A `thought` part is the model's scratchpad; routed to visible text it would
/// print as the answer.
#[tokio::test]
async fn thought_parts_stream_as_reasoning_not_as_text() {
    let frames = [
        frame(json!([{"text": "weighing ", "thought": true}])),
        frame(json!([{"text": "options", "thought": true, "thoughtSignature": "sig-1"}])),
        text_frame("The answer."),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    assert_matches!(
        &events[..],
        [
            ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. }),
            ResponseEvent::ReasoningContentDelta { delta: first, content_index: 0 },
            ResponseEvent::ReasoningContentDelta { delta: second, content_index: 0 },
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning { content, encrypted_content, .. }),
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. }),
            ResponseEvent::OutputTextDelta(text),
            ResponseEvent::OutputItemDone(ResponseItem::Message { .. }),
            ResponseEvent::Completed { .. },
        ] if first == "weighing "
            && second == "options"
            && text == "The answer."
            && encrypted_content.as_deref() == Some("sig-1")
            && content == &Some(vec![ReasoningItemContent::ReasoningText {
                text: "weighing options".to_string(),
            }])
    );
}

/// Both parts can ride in one frame, which is how a short turn arrives.
#[tokio::test]
async fn a_single_frame_carrying_thought_and_text_splits_into_two_items() {
    let frames = [
        frame(json!([
            {"text": "thinking", "thought": true},
            {"text": "answer"},
        ])),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    assert_matches!(
        &events[..],
        [
            ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. }),
            ResponseEvent::ReasoningContentDelta { .. },
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning { .. }),
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. }),
            ResponseEvent::OutputTextDelta(_),
            ResponseEvent::OutputItemDone(ResponseItem::Message { .. }),
            ResponseEvent::Completed { .. },
        ]
    );
}

#[tokio::test]
async fn a_function_call_lands_whole_with_a_synthesized_call_id() {
    let frames = [
        frame(json!([{"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}}])),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    assert_matches!(
        &events[..],
        [
            ResponseEvent::ToolCallInputDelta { call_id: Some(delta_id), delta, .. },
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, name, arguments, .. }),
            ResponseEvent::Completed { .. },
        ] if name == "read_file"
            && arguments == r#"{"path":"a.txt"}"#
            && delta == arguments
            && delta_id == call_id
            && call_id == "gemini-call-0"
    );
}

/// A tool result must follow the message that issued the call, so text already
/// streamed has to land before the call does.
#[tokio::test]
async fn text_before_a_function_call_is_closed_first() {
    let frames = [
        text_frame("running that now"),
        frame(json!([{"functionCall": {"name": "run", "args": {}}}])),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    assert_matches!(
        &events[..],
        [
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. }),
            ResponseEvent::OutputTextDelta(_),
            ResponseEvent::OutputItemDone(ResponseItem::Message { .. }),
            ResponseEvent::ToolCallInputDelta { .. },
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { .. }),
            ResponseEvent::Completed { .. },
        ]
    );
}

/// Two calls in one frame is how parallel tool use arrives; both ids must be
/// distinct or the results pair to the wrong call.
#[tokio::test]
async fn parallel_calls_get_distinct_ids() {
    let frames = [
        frame(json!([
            {"functionCall": {"name": "first", "args": {}}},
            {"functionCall": {"name": "second", "args": {}}},
        ])),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    let ids: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, .. }) => {
                Some(call_id.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        vec!["gemini-call-0".to_string(), "gemini-call-1".to_string()]
    );
}

/// A tool taking no arguments carries no `args`, and an empty string fails to
/// parse downstream.
#[tokio::test]
async fn a_call_with_no_args_reports_an_empty_object() {
    let frames = [
        frame(json!([{"functionCall": {"name": "list"}}])),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    assert_matches!(
        &events[..],
        [
            ResponseEvent::ToolCallInputDelta { .. },
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { arguments, .. }),
            ResponseEvent::Completed { .. },
        ] if arguments == "{}"
    );
}

/// Hitting the output cap is not an error; the partial content is the turn.
#[tokio::test]
async fn hitting_the_output_cap_flushes_the_partial_content() {
    let frames = [
        text_frame("half a sentence"),
        final_frame("MAX_TOKENS", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    assert_matches!(
        &events[..],
        [
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. }),
            ResponseEvent::OutputTextDelta(_),
            ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. }),
            ResponseEvent::Completed { end_turn: Some(false), .. },
        ] if content == &[ContentItem::OutputText { text: "half a sentence".to_string() }]
    );
}

/// A safety stop reported as a normal completion leaves the user staring at
/// silence.
#[tokio::test]
async fn a_safety_stop_surfaces_as_a_refusal() {
    let frames = [text_frame("I can"), final_frame("SAFETY", usage())];

    let results = collect_results(&body(&frames)).await;

    assert_matches!(
        &results[..],
        [
            Ok(ResponseEvent::OutputItemAdded(_)),
            Ok(ResponseEvent::OutputTextDelta(_)),
            // The partial content survives the refusal.
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { .. })),
            Err(ApiError::InvalidRequest { message }),
        ] if message.contains("SAFETY")
    );
}

#[tokio::test]
async fn a_blocked_prompt_surfaces_before_any_candidate_arrives() {
    let frames = [json!({"promptFeedback": {"blockReason": "SAFETY"}})];

    let results = collect_results(&body(&frames)).await;

    assert_matches!(
        &results[..],
        [Err(ApiError::InvalidRequest { message })] if message.contains("SAFETY")
    );
}

/// Nothing usable came back, and only a retry can produce a call the backend
/// parses.
#[tokio::test]
async fn a_malformed_function_call_asks_for_a_retry() {
    let frames = [final_frame("MALFORMED_FUNCTION_CALL", usage())];

    let results = collect_results(&body(&frames)).await;

    assert_matches!(&results[..], [Err(ApiError::Retryable { .. })]);
}

/// After the 200 the status cannot change, so a quota failure arrives in-band
/// and has to get the backoff its status code would have.
#[tokio::test]
async fn an_in_band_quota_error_maps_to_a_backoff() {
    let frames = [
        text_frame("partial"),
        json!({"error": {"code": 429, "message": "Quota exceeded", "status": "RESOURCE_EXHAUSTED"}}),
    ];

    let results = collect_results(&body(&frames)).await;

    assert_matches!(
        &results[..],
        [
            Ok(ResponseEvent::OutputItemAdded(_)),
            Ok(ResponseEvent::OutputTextDelta(_)),
            Ok(ResponseEvent::OutputItemDone(_)),
            Err(ApiError::Retryable { message, .. }),
        ] if message == "Quota exceeded"
    );
}

#[tokio::test]
async fn an_in_band_argument_error_is_not_retried() {
    let frames =
        [json!({"error": {"code": 400, "message": "bad request", "status": "INVALID_ARGUMENT"}})];

    let results = collect_results(&body(&frames)).await;

    assert_matches!(
        &results[..],
        [Err(ApiError::Stream(message))] if message == "bad request"
    );
}

/// A truncated frame must not take the turn down with it.
#[tokio::test]
async fn a_malformed_frame_is_skipped() {
    let mut stream = body(&[text_frame("kept")]);
    // A truncated object and a line that is not JSON at all.
    stream.push_str("data: {\"candidates\": [{\"content\":\n\n");
    stream.push_str("data: not json at all\n\n");
    stream.push_str(&body(&[final_frame("STOP", usage())]));

    let events = collect_events(&stream).await;

    assert_matches!(
        &events[..],
        [
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. }),
            ResponseEvent::OutputTextDelta(text),
            ResponseEvent::OutputItemDone(ResponseItem::Message { .. }),
            ResponseEvent::Completed { end_turn: Some(true), .. },
        ] if text == "kept"
    );
}

/// A missing `Completed` reads as a dropped connection and retries the turn.
#[tokio::test]
async fn a_stream_that_ends_without_a_finish_reason_still_completes() {
    let frames = [text_frame("cut off")];

    let events = collect_events(&body(&frames)).await;

    assert_matches!(
        &events[..],
        [
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. }),
            ResponseEvent::OutputTextDelta(_),
            ResponseEvent::OutputItemDone(ResponseItem::Message { .. }),
            ResponseEvent::Completed { end_turn: None, .. },
        ]
    );
}

/// Alternative candidates carry a different answer to the same prompt; folding
/// them together would splice two answers into one message.
#[tokio::test]
async fn only_the_first_candidate_is_followed() {
    let frames = [
        json!({
            "candidates": [
                {"content": {"parts": [{"text": "chosen"}], "role": "model"}, "index": 0},
                {"content": {"parts": [{"text": "alternative"}], "role": "model"}, "index": 1},
            ],
        }),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    let text: String = events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputTextDelta(delta) => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "chosen");
}

/// Keep-alive frames and parts this wire has no transcript item for must not
/// materialize an empty turn.
#[tokio::test]
async fn empty_and_unmodelled_parts_are_ignored() {
    let frames = [
        frame(json!([])),
        frame(json!([{"text": ""}])),
        frame(json!([{"inlineData": {"mimeType": "image/png", "data": "AA=="}}])),
        text_frame("hi"),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    // added, delta, done, completed
    assert_eq!(events.len(), 4, "{events:?}");
    assert_matches!(events.last(), Some(ResponseEvent::Completed { .. }));
}

/// A delta arriving with no active item panics the turn consumer in a checked
/// build, so every delta follows the `OutputItemAdded` that opens its item.
#[tokio::test]
async fn every_streaming_delta_follows_an_item_announcement() {
    let frames = [
        frame(json!([{"text": "thinking", "thought": true}])),
        text_frame("hello"),
        frame(json!([{"text": "more thinking", "thought": true}])),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    let mut announced = 0usize;
    for event in &events {
        match event {
            ResponseEvent::OutputItemAdded(_) => announced += 1,
            ResponseEvent::OutputTextDelta(_) | ResponseEvent::ReasoningContentDelta { .. } => {
                assert!(announced > 0, "delta with no announced item: {events:?}");
            }
            _ => {}
        }
    }
    assert_eq!(announced, 3, "{events:?}");
}

/// A second reasoning item is a new content index; reusing the first one merges
/// two separate thoughts downstream.
#[tokio::test]
async fn a_reopened_reasoning_item_gets_a_fresh_content_index() {
    let frames = [
        frame(json!([{"text": "first", "thought": true}])),
        text_frame("interlude"),
        frame(json!([{"text": "second", "thought": true}])),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    let indices: Vec<i64> = events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::ReasoningContentDelta { content_index, .. } => Some(*content_index),
            _ => None,
        })
        .collect();
    assert_eq!(indices, vec![0, 1]);
}

#[tokio::test]
async fn a_single_part_thought_keeps_its_own_signature() {
    // The ordinary Gemini shape: ONE part carrying text and its signature. The
    // first version read the signature before push_thought opened the item, so it
    // landed on whatever was open before -- usually nothing -- and was lost. The
    // request builder drops a Reasoning with no encrypted_content, so the model's
    // whole thought chain then vanished from the next turn, which is the state
    // Gemini 3 rejects when a functionCall follows.
    //
    // thought_parts_stream_as_reasoning_not_as_text cannot catch this: its
    // signature rides a SECOND part, after the item is already open.
    let frames = [
        frame(json!([{"text": "planning", "thought": true, "thoughtSignature": "THOUGHT_SIG"}])),
        text_frame("The answer."),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    let signature = events.iter().find_map(|event| match event {
        ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
            encrypted_content, ..
        }) => Some(encrypted_content.clone()),
        _ => None,
    });
    assert_eq!(
        signature,
        Some(Some("THOUGHT_SIG".to_string())),
        "a single-part thought must keep its own signature, or the turn is unreplayable"
    );
}

#[tokio::test]
async fn a_function_calls_signature_goes_to_the_call_not_the_thought() {
    // It used to be stamped onto whichever thought was open, overwriting that
    // thought's own signature; with nothing open it was dropped entirely.
    let frames = [
        frame(json!([{"text": "planning", "thought": true, "thoughtSignature": "THOUGHT_SIG"}])),
        frame(
            json!([{"functionCall": {"name": "ls", "args": {}}, "thoughtSignature": "CALL_SIG"}]),
        ),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    let thought_sig = events.iter().find_map(|event| match event {
        ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
            encrypted_content, ..
        }) => Some(encrypted_content.clone()),
        _ => None,
    });
    let call_sig = events.iter().find_map(|event| match event {
        ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
            encrypted_function_args,
            ..
        }) => Some(encrypted_function_args.clone()),
        _ => None,
    });

    assert_eq!(
        thought_sig,
        Some(Some("THOUGHT_SIG".to_string())),
        "the thought keeps its own signature"
    );
    assert_eq!(
        call_sig,
        Some(Some(vec!["CALL_SIG".to_string()])),
        "a call's signature belongs to the call"
    );
}

#[tokio::test]
async fn a_stringified_thought_flag_is_still_reasoning() {
    // A gateway that JSON-encodes the flag would otherwise print the model's
    // scratchpad as its visible answer.
    let frames = [
        frame(json!([{"text": "scratch", "thought": "true"}])),
        final_frame("STOP", usage()),
    ];

    let events = collect_events(&body(&frames)).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning { .. })
        )),
        "a stringified thought flag must still route to reasoning"
    );
}
