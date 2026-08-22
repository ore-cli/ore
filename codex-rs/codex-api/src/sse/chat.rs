//! Streaming parser for the Chat Completions API.
//!
//! The protocol has no positive end-of-turn signal, so `end_turn` stays `None`
//! and callers use their own heuristics; at the output cap it is `Some(false)`.

use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::sse::chat_usage::token_usage_from_chat_usage;
use crate::telemetry::SseTelemetry;
use codex_client::StreamResponse;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

/// Extracts a provider error carried in-band: after a 200 the status cannot
/// change, so mid-stream failures arrive as a frame with no `choices`.
fn provider_error_message(value: &serde_json::Value) -> Option<String> {
    let error = value.get("error")?;
    if error.is_null() {
        return None;
    }
    Some(
        error
            .get("message")
            .and_then(|m| m.as_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| error.to_string()),
    )
}

/// How long a finished turn waits for the trailing usage chunk.
const USAGE_GRACE_PERIOD: Duration = Duration::from_secs(5);

pub(crate) fn spawn_chat_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    _turn_state: Option<Arc<OnceLock<String>>>,
) -> ResponseStream {
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    let bytes = stream_response.bytes;
    tokio::spawn(async move {
        process_chat_sse(bytes, tx_event, idle_timeout, telemetry).await;
    });
    let upstream_request_id = stream_response
        .headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    ResponseStream {
        rx_event,
        upstream_request_id,
    }
}

/// Processes Server-Sent Events from the legacy Chat Completions streaming API.
///
/// The upstream protocol terminates a streaming response with a final sentinel event
/// (`data: [DONE]`). Historically, some of our test stubs have emitted `data: DONE`
/// (without brackets) instead.
///
/// `eventsource_stream` delivers these sentinels as regular events rather than signaling
/// end-of-stream. If we try to parse them as JSON, we log and skip them, then keep
/// polling for more events.
///
/// On servers that keep the HTTP connection open after emitting the sentinel (notably
/// wiremock on Windows), skipping the sentinel means we never emit `ResponseEvent::Completed`.
/// Higher-level workflows/tests that wait for completion before issuing subsequent model
/// calls will then stall, which shows up as "expected N requests, got 1" verification
/// failures in the mock server.
pub async fn process_chat_sse<S>(
    stream: S,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<std::sync::Arc<dyn SseTelemetry>>,
) where
    S: Stream<Item = Result<bytes::Bytes, codex_client::TransportError>> + Unpin,
{
    let mut stream = stream.eventsource();

    #[derive(Default, Debug)]
    struct ToolCallState {
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    }

    let mut tool_calls: HashMap<usize, ToolCallState> = HashMap::new();
    let mut tool_call_order: Vec<usize> = Vec::new();
    let mut tool_call_order_seen: HashSet<usize> = HashSet::new();
    let mut tool_call_index_by_id: HashMap<String, usize> = HashMap::new();
    let mut next_tool_call_index = 0usize;
    let mut last_tool_call_index: Option<usize> = None;
    let mut assistant_item: Option<ResponseItem> = None;
    let mut reasoning_item: Option<ResponseItem> = None;
    let mut latest_usage: Option<TokenUsage> = None;
    // Set once finish_reason arrives; Completed waits for the usage chunk, which
    // the provider sends after it.
    let mut finished = false;
    let mut end_turn: Option<bool> = None;
    let mut refusal = String::new();

    /// Emits every accumulated tool call in arrival order. Runs on every
    /// turn-ending path: some servers send no `finish_reason` with a tool call.
    async fn flush_tool_calls(
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
        tool_calls: &mut HashMap<usize, ToolCallState>,
        tool_call_order: &mut Vec<usize>,
        tool_call_order_seen: &mut HashSet<usize>,
    ) {
        for index in std::mem::take(tool_call_order) {
            let Some(state) = tool_calls.remove(&index) else {
                continue;
            };
            tool_call_order_seen.remove(&index);
            let ToolCallState {
                id,
                name,
                arguments,
            } = state;
            let Some(name) = name else {
                debug!("Skipping tool call at index {index} because name is missing");
                continue;
            };
            let item = ResponseItem::FunctionCall {
                id: None,
                name,
                namespace: None,
                // The empty string is not valid JSON.
                arguments: if arguments.is_empty() {
                    "{}".to_string()
                } else {
                    arguments
                },
                call_id: id.unwrap_or_else(|| format!("tool-call-{index}")),
                internal_chat_message_metadata_passthrough: None,
                encrypted_function_args: None,
            };
            let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn flush_and_complete(
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
        reasoning_item: &mut Option<ResponseItem>,
        assistant_item: &mut Option<ResponseItem>,
        tool_calls: &mut HashMap<usize, ToolCallState>,
        tool_call_order: &mut Vec<usize>,
        tool_call_order_seen: &mut HashSet<usize>,
        token_usage: Option<TokenUsage>,
        end_turn: Option<bool>,
        refusal: &str,
    ) {
        if let Some(reasoning) = reasoning_item.take() {
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(reasoning)))
                .await;
        }

        if let Some(assistant) = assistant_item.take() {
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemDone(assistant)))
                .await;
        }

        flush_tool_calls(tx_event, tool_calls, tool_call_order, tool_call_order_seen).await;

        // Ordered after the flush so a refusal keeps the text already emitted.
        if !refusal.is_empty() {
            let _ = tx_event
                .send(Err(ApiError::InvalidRequest {
                    message: refusal.to_string(),
                }))
                .await;
            return;
        }

        let _ = tx_event
            .send(Ok(ResponseEvent::Completed {
                response_id: String::new(),
                token_usage,
                end_turn,
            }))
            .await;
    }

    loop {
        if tx_event.is_closed() {
            return;
        }
        let start = Instant::now();
        // After finish the only thing still expected is the usage chunk.
        let wait = if finished {
            USAGE_GRACE_PERIOD.min(idle_timeout)
        } else {
            idle_timeout
        };
        let response = timeout(wait, stream.next()).await;
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
        }
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string()))).await;
                return;
            }
            Ok(None) => {
                flush_and_complete(
                    &tx_event,
                    &mut reasoning_item,
                    &mut assistant_item,
                    &mut tool_calls,
                    &mut tool_call_order,
                    &mut tool_call_order_seen,
                    latest_usage.take(),
                    end_turn,
                    &refusal,
                )
                .await;
                return;
            }
            Err(_) => {
                // A finished turn whose provider never sent the usage chunk or
                // [DONE] still completes; only an unfinished one is an error.
                if finished {
                    flush_and_complete(
                        &tx_event,
                        &mut reasoning_item,
                        &mut assistant_item,
                        &mut tool_calls,
                        &mut tool_call_order,
                        &mut tool_call_order_seen,
                        latest_usage.take(),
                        end_turn,
                        &refusal,
                    )
                    .await;
                    return;
                }
                let _ = tx_event
                    .send(Err(ApiError::Stream("idle timeout waiting for SSE".into())))
                    .await;
                return;
            }
        };

        trace!("SSE event: {}", sse.data);

        let data = sse.data.trim();

        if data.is_empty() {
            continue;
        }

        if data == "[DONE]" || data == "DONE" {
            flush_and_complete(
                &tx_event,
                &mut reasoning_item,
                &mut assistant_item,
                &mut tool_calls,
                &mut tool_call_order,
                &mut tool_call_order_seen,
                latest_usage.take(),
                end_turn,
                &refusal,
            )
            .await;
            return;
        }

        let value: serde_json::Value = match serde_json::from_str(data) {
            Ok(val) => val,
            Err(err) => {
                debug!(
                    "Failed to parse ChatCompletions SSE event: {err}, data: {}",
                    data
                );
                continue;
            }
        };

        // Before the `choices` guard, which would skip the frame and turn a real
        // failure into a silently empty turn.
        if let Some(message) = provider_error_message(&value) {
            let _ = tx_event.send(Err(ApiError::Stream(message))).await;
            return;
        }

        // The usage chunk arrives with `choices: []`, or with no `choices` at
        // all, so it has to be read before the guard below drops it.
        if let Some(usage) = value.get("usage")
            && let Some(parsed) = token_usage_from_chat_usage(usage)
        {
            latest_usage = Some(parsed);
        }

        let Some(choices) = value.get("choices").and_then(|c| c.as_array()) else {
            continue;
        };

        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = reasoning_text(delta) {
                    append_reasoning_text(
                        &tx_event,
                        &mut reasoning_item,
                        &mut assistant_item,
                        text,
                    )
                    .await;
                }

                if let Some(text) = delta.get("refusal").and_then(|r| r.as_str()) {
                    refusal.push_str(text);
                }

                // An empty string is not assistant text: some servers pad
                // tool-call deltas with `"content": ""`, and materializing a
                // message for one leaves an empty assistant turn behind.
                if let Some(content) = delta.get("content") {
                    if content.is_array() {
                        for item in content.as_array().unwrap_or(&vec![]) {
                            if let Some(text) = item.get("text").and_then(|t| t.as_str())
                                && !text.is_empty()
                            {
                                append_assistant_text(
                                    &tx_event,
                                    &mut assistant_item,
                                    &mut reasoning_item,
                                    text.to_string(),
                                )
                                .await;
                            }
                        }
                    } else if let Some(text) = content.as_str()
                        && !text.is_empty()
                    {
                        append_assistant_text(
                            &tx_event,
                            &mut assistant_item,
                            &mut reasoning_item,
                            text.to_string(),
                        )
                        .await;
                    }
                }

                if let Some(tool_call_values) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                    for tool_call in tool_call_values {
                        let mut index = tool_call
                            .get("index")
                            .and_then(serde_json::Value::as_u64)
                            .map(|i| i as usize);

                        let mut call_id_for_lookup = None;
                        if let Some(call_id) = tool_call.get("id").and_then(|i| i.as_str()) {
                            call_id_for_lookup = Some(call_id.to_string());
                            if let Some(existing) = tool_call_index_by_id.get(call_id) {
                                index = Some(*existing);
                            }
                        }

                        if index.is_none() && call_id_for_lookup.is_none() {
                            index = last_tool_call_index;
                        }

                        let index = index.unwrap_or_else(|| {
                            while tool_calls.contains_key(&next_tool_call_index) {
                                next_tool_call_index += 1;
                            }
                            let idx = next_tool_call_index;
                            next_tool_call_index += 1;
                            idx
                        });

                        let call_state = tool_calls.entry(index).or_default();
                        if tool_call_order_seen.insert(index) {
                            tool_call_order.push(index);
                        }

                        if let Some(id) = tool_call.get("id").and_then(|i| i.as_str()) {
                            call_state.id.get_or_insert_with(|| id.to_string());
                            tool_call_index_by_id.entry(id.to_string()).or_insert(index);
                        }

                        if let Some(func) = tool_call.get("function") {
                            if let Some(fname) = func.get("name").and_then(|n| n.as_str())
                                && !fname.is_empty()
                            {
                                call_state.name.get_or_insert_with(|| fname.to_string());
                            }
                            if let Some(arguments) = func.get("arguments").and_then(|a| a.as_str())
                            {
                                call_state.arguments.push_str(arguments);
                            }
                        }

                        last_tool_call_index = Some(index);
                    }
                }
            }

            if let Some(message) = choice.get("message") {
                if let Some(text) = reasoning_text(message) {
                    append_reasoning_text(
                        &tx_event,
                        &mut reasoning_item,
                        &mut assistant_item,
                        text,
                    )
                    .await;
                }

                if let Some(text) = message.get("refusal").and_then(|r| r.as_str()) {
                    refusal.push_str(text);
                }
            }

            let Some(finish_reason) = choice.get("finish_reason").and_then(|r| r.as_str()) else {
                continue;
            };

            finished = true;

            match finish_reason {
                // The output cap, not the context window: an oversized request is
                // rejected up front as a 400 and never reaches this parser.
                "length" => end_turn = Some(false),
                "content_filter" | "refusal" if refusal.is_empty() => {
                    refusal.push_str("the model declined to answer");
                }
                _ => {}
            }

            if let Some(reasoning) = reasoning_item.take() {
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(reasoning)))
                    .await;
            }

            // Here rather than at `[DONE]`: a tool result must follow the
            // message that issued the call, so text flushed later would
            // replay between the two.
            if let Some(assistant) = assistant_item.take() {
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(assistant)))
                    .await;
            }

            flush_tool_calls(
                &tx_event,
                &mut tool_calls,
                &mut tool_call_order,
                &mut tool_call_order_seen,
            )
            .await;
        }
    }
}

/// Reads reasoning text out of a `delta` or `message` object.
///
/// `reasoning`, `reasoning_content`, and `thinking_blocks` are alternate
/// spellings of the same text, so exactly one is taken.
fn reasoning_text(container: &serde_json::Value) -> Option<String> {
    let structured = container
        .get("thinking_blocks")
        .and_then(|v| v.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("thinking").and_then(|v| v.as_str()))
                .collect::<String>()
        })
        .filter(|text| !text.is_empty());

    structured
        .or_else(|| {
            container
                .get("reasoning_content")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            let reasoning = container.get("reasoning")?;
            reasoning
                .as_str()
                .or_else(|| reasoning.get("text").and_then(|v| v.as_str()))
                .or_else(|| reasoning.get("content").and_then(|v| v.as_str()))
                .map(str::to_string)
        })
        .filter(|text| !text.is_empty())
}

/// Closes whichever item is open before a different one is announced.
///
/// Callers track a single active item; two open at once orphans the first.
async fn close_open_item(
    tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    item: &mut Option<ResponseItem>,
) {
    if let Some(item) = item.take() {
        let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
    }
}

async fn append_assistant_text(
    tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    assistant_item: &mut Option<ResponseItem>,
    reasoning_item: &mut Option<ResponseItem>,
    text: String,
) {
    if assistant_item.is_none() {
        close_open_item(tx_event, reasoning_item).await;
        let item = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        *assistant_item = Some(item.clone());
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(item)))
            .await;
    }

    if let Some(ResponseItem::Message { content, .. }) = assistant_item {
        content.push(ContentItem::OutputText { text: text.clone() });
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputTextDelta(text.clone())))
            .await;
    }
}

async fn append_reasoning_text(
    tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    reasoning_item: &mut Option<ResponseItem>,
    assistant_item: &mut Option<ResponseItem>,
    text: String,
) {
    if reasoning_item.is_none() {
        close_open_item(tx_event, assistant_item).await;
        let item = ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: Some(vec![]),
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        };
        *reasoning_item = Some(item.clone());
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemAdded(item)))
            .await;
    }

    if let Some(ResponseItem::Reasoning {
        content: Some(content),
        ..
    }) = reasoning_item
    {
        // Deltas are fragments of one block.
        let content_index = content.len().saturating_sub(1) as i64;
        match content.last_mut() {
            Some(ReasoningItemContent::ReasoningText { text: existing }) => {
                existing.push_str(&text);
            }
            _ => content.push(ReasoningItemContent::ReasoningText { text: text.clone() }),
        }

        let _ = tx_event
            .send(Ok(ResponseEvent::ReasoningContentDelta {
                delta: text.clone(),
                content_index,
            }))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use codex_protocol::models::ResponseItem;
    use futures::TryStreamExt;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::io::ReaderStream;

    fn build_body(events: &[serde_json::Value]) -> String {
        let mut body = String::new();
        for e in events {
            body.push_str(&format!("event: message\ndata: {e}\n\n"));
        }
        body
    }

    /// Regression test: the stream should complete when we see a `[DONE]` sentinel.
    ///
    /// This is important for tests/mocks that don't immediately close the underlying
    /// connection after emitting the sentinel.
    #[tokio::test]
    async fn completes_on_done_sentinel_without_json() {
        let events = collect_events("event: message\ndata: [DONE]\n\n").await;
        assert_matches!(&events[..], [ResponseEvent::Completed { .. }]);
    }

    /// Gateways send the usage chunk after finish_reason.
    #[tokio::test]
    async fn usage_after_finish_reason_reaches_completed() {
        let body = concat!(
            r#"data: {"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"finish_reason":"stop","index":0,"delta":{}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{},"index":0}],"usage":{"completion_tokens":25,"#,
            r#""completion_tokens_details":{"reasoning_tokens":0},"prompt_tokens":9,"#,
            r#""prompt_tokens_details":{"cached_tokens":0},"total_tokens":34}}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );

        let events = collect_events(body).await;
        let usage = events
            .iter()
            .find_map(|ev| match ev {
                ResponseEvent::Completed { token_usage, .. } => token_usage.clone(),
                _ => None,
            })
            .expect("Completed should carry usage sent after finish_reason");

        assert_eq!(9, usage.input_tokens);
        assert_eq!(25, usage.output_tokens);
        assert_eq!(34, usage.total_tokens);
    }

    #[tokio::test]
    async fn finish_without_usage_still_completes() {
        let body = concat!(
            r#"data: {"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"finish_reason":"stop","index":0,"delta":{}}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );

        let events = collect_events(body).await;
        assert!(
            events.iter().any(|ev| matches!(
                ev,
                ResponseEvent::Completed {
                    token_usage: None,
                    ..
                }
            )),
            "expected a Completed with no usage, got {events:?}"
        );
    }

    #[tokio::test]
    async fn in_band_provider_error_surfaces() {
        let body = concat!(
            r#"data: {"error":{"message":"rate limit exceeded","code":"429"}}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let err = collect_results(body)
            .await
            .into_iter()
            .find_map(Result::err)
            .expect("the error frame should surface as a stream error");
        assert!(
            format!("{err}").contains("rate limit exceeded"),
            "expected the provider message, got {err}"
        );
    }

    /// `"error": null` rides along on normal frames from some providers.
    #[tokio::test]
    async fn null_error_field_is_not_an_error() {
        let body = concat!(
            r#"data: {"error":null,"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        assert!(
            collect_results(body).await.iter().all(Result::is_ok),
            "a null error must not abort the stream"
        );
    }

    async fn collect_results(body: &str) -> Vec<Result<ResponseEvent, ApiError>> {
        let reader = ReaderStream::new(std::io::Cursor::new(body.to_string()))
            .map_err(|err| codex_client::TransportError::Network(err.to_string()));
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(16);
        tokio::spawn(process_chat_sse(
            reader,
            tx,
            Duration::from_millis(1000),
            None,
        ));
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
    }

    async fn collect_events(body: &str) -> Vec<ResponseEvent> {
        let reader = ReaderStream::new(std::io::Cursor::new(body.to_string()))
            .map_err(|err| codex_client::TransportError::Network(err.to_string()));
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(16);
        tokio::spawn(process_chat_sse(
            reader,
            tx,
            Duration::from_millis(1000),
            None,
        ));

        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev.expect("stream error"));
        }
        out
    }

    /// For streams that end in an error rather than `Completed`.
    async fn collect_events_and_error(body: &str) -> (Vec<ResponseEvent>, Option<ApiError>) {
        let reader = ReaderStream::new(std::io::Cursor::new(body.to_string()))
            .map_err(|err| codex_client::TransportError::Network(err.to_string()));
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(16);
        tokio::spawn(process_chat_sse(
            reader,
            tx,
            Duration::from_millis(1000),
            None,
        ));

        let mut out = Vec::new();
        let mut error = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                Ok(event) => out.push(event),
                Err(err) => error = Some(err),
            }
        }
        (out, error)
    }

    #[tokio::test]
    async fn concatenates_tool_call_arguments_across_deltas() {
        let delta_name = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_a",
                        "index": 0,
                        "function": { "name": "do_a" }
                    }]
                }
            }]
        });

        let delta_args_1 = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "{ \"foo\":" }
                    }]
                }
            }]
        });

        let delta_args_2 = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "1}" }
                    }]
                }
            }]
        });

        let finish = json!({
            "choices": [{
                "finish_reason": "tool_calls"
            }]
        });

        let body = build_body(&[delta_name, delta_args_1, delta_args_2, finish]);
        let events = collect_events(&body).await;
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, name, arguments, .. }),
                ResponseEvent::Completed { .. }
            ] if call_id == "call_a" && name == "do_a" && arguments == "{ \"foo\":1}"
        );
    }

    #[tokio::test]
    async fn emits_multiple_tool_calls() {
        let delta_a = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_a",
                        "function": { "name": "do_a", "arguments": "{\"foo\":1}" }
                    }]
                }
            }]
        });

        let delta_b = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_b",
                        "function": { "name": "do_b", "arguments": "{\"bar\":2}" }
                    }]
                }
            }]
        });

        let finish = json!({
            "choices": [{
                "finish_reason": "tool_calls"
            }]
        });

        let body = build_body(&[delta_a, delta_b, finish]);
        let events = collect_events(&body).await;
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id: call_a, name: name_a, arguments: args_a, .. }),
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id: call_b, name: name_b, arguments: args_b, .. }),
                ResponseEvent::Completed { .. }
            ] if call_a == "call_a" && name_a == "do_a" && args_a == "{\"foo\":1}" && call_b == "call_b" && name_b == "do_b" && args_b == "{\"bar\":2}"
        );
    }

    #[tokio::test]
    async fn emits_tool_calls_for_multiple_choices() {
        let payload = json!({
            "choices": [
                {
                    "delta": {
                        "tool_calls": [{
                            "id": "call_a",
                            "index": 0,
                            "function": { "name": "do_a", "arguments": "{}" }
                        }]
                    },
                    "finish_reason": "tool_calls"
                },
                {
                    "delta": {
                        "tool_calls": [{
                            "id": "call_b",
                            "index": 0,
                            "function": { "name": "do_b", "arguments": "{}" }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }
            ]
        });

        let body = build_body(&[payload]);
        let events = collect_events(&body).await;
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id: call_a, name: name_a, arguments: args_a, .. }),
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id: call_b, name: name_b, arguments: args_b, .. }),
                ResponseEvent::Completed { .. }
            ] if call_a == "call_a" && name_a == "do_a" && args_a == "{}" && call_b == "call_b" && name_b == "do_b" && args_b == "{}"
        );
    }

    #[tokio::test]
    async fn merges_tool_calls_by_index_when_id_missing_on_subsequent_deltas() {
        let delta_with_id = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_a",
                        "function": { "name": "do_a", "arguments": "{ \"foo\":" }
                    }]
                }
            }]
        });

        let delta_without_id = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "1}" }
                    }]
                }
            }]
        });

        let finish = json!({
            "choices": [{
                "finish_reason": "tool_calls"
            }]
        });

        let body = build_body(&[delta_with_id, delta_without_id, finish]);
        let events = collect_events(&body).await;
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, name, arguments, .. }),
                ResponseEvent::Completed { .. }
            ] if call_id == "call_a" && name == "do_a" && arguments == "{ \"foo\":1}"
        );
    }

    #[tokio::test]
    async fn preserves_tool_call_name_when_empty_deltas_arrive() {
        let delta_with_name = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_a",
                        "function": { "name": "do_a" }
                    }]
                }
            }]
        });

        let delta_with_empty_name = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_a",
                        "function": { "name": "", "arguments": "{}" }
                    }]
                }
            }]
        });

        let finish = json!({
            "choices": [{
                "finish_reason": "tool_calls"
            }]
        });

        let body = build_body(&[delta_with_name, delta_with_empty_name, finish]);
        let events = collect_events(&body).await;
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { name, arguments, .. }),
                ResponseEvent::Completed { .. }
            ] if name == "do_a" && arguments == "{}"
        );
    }

    #[tokio::test]
    async fn emits_tool_calls_even_when_content_and_reasoning_present() {
        let delta_content_and_tools = json!({
            "choices": [{
                "delta": {
                    "content": [{"text": "hi"}],
                    "reasoning": "because",
                    "tool_calls": [{
                        "id": "call_a",
                        "function": { "name": "do_a", "arguments": "{}" }
                    }]
                }
            }]
        });

        let finish = json!({
            "choices": [{
                "finish_reason": "tool_calls"
            }]
        });

        let body = build_body(&[delta_content_and_tools, finish]);
        let events = collect_events(&body).await;

        // The message lands before the call it accompanied, not after: the
        // transcript replays in this order, and Chat Completions requires a
        // tool result to follow the message that issued the call.
        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. }),
                ResponseEvent::ReasoningContentDelta { .. },
                ResponseEvent::OutputItemDone(ResponseItem::Reasoning { .. }),
                ResponseEvent::OutputItemAdded(ResponseItem::Message { .. }),
                ResponseEvent::OutputTextDelta(delta),
                ResponseEvent::OutputItemDone(ResponseItem::Message { .. }),
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, name, .. }),
                ResponseEvent::Completed { .. }
            ] if delta == "hi" && call_id == "call_a" && name == "do_a"
        );
    }

    /// A streamed item takes its id from the one active item.
    #[tokio::test]
    async fn only_one_item_is_open_at_a_time() {
        let body = build_body(&[
            json!({"choices": [{"delta": {"reasoning": "because"}}]}),
            json!({"choices": [{"delta": {"content": "hi"}}]}),
            json!({"choices": [{"finish_reason": "stop"}]}),
        ]);
        let events = collect_events(&body).await;

        let mut open = 0i32;
        for event in &events {
            match event {
                ResponseEvent::OutputItemAdded(_) => open += 1,
                ResponseEvent::OutputItemDone(_) => open -= 1,
                _ => {}
            }
            assert!(open <= 1, "two items open at once: {events:?}");
        }
        assert_eq!(0, open, "an item was left open: {events:?}");
    }

    /// A gateway that pads tool-call deltas with `"content": ""` must not
    /// produce an assistant message; an empty one reaches the model as a turn
    /// it did not take.
    #[tokio::test]
    async fn empty_content_delta_does_not_open_an_assistant_message() {
        let delta_empty_content_and_tools = json!({
            "choices": [{
                "delta": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_a",
                        "function": { "name": "do_a", "arguments": "{}" }
                    }]
                }
            }]
        });

        let finish = json!({
            "choices": [{
                "finish_reason": "tool_calls"
            }]
        });

        let body = build_body(&[delta_empty_content_and_tools, finish]);
        let events = collect_events(&body).await;

        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, name, .. }),
                ResponseEvent::Completed { .. }
            ] if call_id == "call_a" && name == "do_a"
        );
    }

    fn tool_call_delta() -> serde_json::Value {
        json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_a",
                        "function": { "name": "do_a", "arguments": "{}" }
                    }]
                }
            }]
        })
    }

    fn tool_call_names(events: &[ResponseEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { name, .. }) => {
                    Some(name.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Servers disagree on which finish_reason accompanies a tool call, and
    /// some send none at all.
    #[tokio::test]
    async fn tool_calls_survive_every_stream_termination() {
        for terminator in [
            Some(json!({"choices": [{"finish_reason": "stop"}]})),
            Some(json!({"choices": [{"finish_reason": "eos"}]})),
            None,
        ] {
            let mut events = vec![tool_call_delta()];
            events.extend(terminator.clone());
            let body = build_body(&events);

            let events = collect_events(&body).await;

            assert_eq!(
                vec!["do_a".to_string()],
                tool_call_names(&events),
                "terminator {terminator:?} dropped the tool call: {events:?}"
            );
        }
    }

    #[tokio::test]
    async fn output_cap_keeps_the_partial_turn_and_asks_for_another() {
        let body = build_body(&[
            json!({"choices": [{"delta": {"content": "half a sentence"}}]}),
            json!({"choices": [{"finish_reason": "length"}]}),
        ]);

        let events = collect_events(&body).await;

        assert!(
            events.iter().any(|event| matches!(
                event,
                ResponseEvent::OutputItemDone(ResponseItem::Message { .. })
            )),
            "the partial answer was discarded: {events:?}"
        );
        assert_matches!(
            events.last(),
            Some(ResponseEvent::Completed {
                end_turn: Some(false),
                ..
            }),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_refusal_is_reported_as_a_refusal() {
        let body = build_body(&[
            json!({"choices": [{"delta": {"refusal": "I can't help with that."}}]}),
            json!({"choices": [{"finish_reason": "content_filter"}]}),
        ]);

        let (events, error) = collect_events_and_error(&body).await;

        assert_matches!(
            error,
            Some(ApiError::InvalidRequest { message }) if message == "I can't help with that.",
            "{events:?}"
        );
    }

    /// Some proxies emit `reasoning_content`; gateways fronting Anthropic emit
    /// `thinking_blocks`.
    #[tokio::test]
    async fn every_reasoning_spelling_is_read() {
        for delta in [
            json!({"reasoning": "why"}),
            json!({"reasoning": {"text": "why"}}),
            json!({"reasoning_content": "why"}),
            json!({"thinking_blocks": [{"type": "thinking", "thinking": "why"}]}),
        ] {
            let body = build_body(&[
                json!({"choices": [{"delta": delta}]}),
                json!({"choices": [{"finish_reason": "stop"}]}),
            ]);

            let events = collect_events(&body).await;

            assert!(
                events.iter().any(|event| matches!(
                    event,
                    ResponseEvent::ReasoningContentDelta { delta, .. } if delta == "why"
                )),
                "{delta:?} produced no reasoning: {events:?}"
            );
        }
    }

    #[tokio::test]
    async fn reasoning_deltas_join_into_one_block() {
        let body = build_body(&[
            json!({"choices": [{"delta": {"reasoning": "be"}}]}),
            json!({"choices": [{"delta": {"reasoning": "cause"}}]}),
            json!({"choices": [{"finish_reason": "stop"}]}),
        ]);

        let events = collect_events(&body).await;

        let done = events.iter().find_map(|event| match event {
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
                content: Some(content),
                ..
            }) => Some(content.clone()),
            _ => None,
        });
        assert_eq!(
            Some(vec![ReasoningItemContent::ReasoningText {
                text: "because".to_string()
            }]),
            done,
            "{events:?}"
        );
    }
}
