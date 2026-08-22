//! Streaming parser for the Anthropic Messages API.
//!
//! Every event carries a content-block index, and the wire reports why the turn
//! ended, so `end_turn` is always known.

use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::sse::anthropic_usage::merge_anthropic_usage;
use crate::telemetry::SseTelemetry;
use codex_client::StreamResponse;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

const DEFAULT_REFUSAL: &str = "the model declined to answer this request";

pub(crate) fn spawn_anthropic_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) -> ResponseStream {
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    let bytes = stream_response.bytes;
    tokio::spawn(async move {
        process_anthropic_sse(bytes, tx_event, idle_timeout, telemetry).await;
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

/// The empty item announced when a block opens, so the consumer has something
/// to attach the block's deltas to.
fn announce_block(state: &BlockState) -> Option<ResponseItem> {
    match state {
        BlockState::Text(_) => Some(ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: Vec::new(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }),
        BlockState::Thinking { .. } => Some(ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        }),
        // A tool call is delivered whole on completion; nothing streams into it.
        BlockState::ToolUse { .. } => None,
    }
}

/// One in-flight content block, accumulating until its `content_block_stop`.
enum BlockState {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        partial_json: String,
    },
}

/// Why the turn ended, plus whatever detail the server attached.
struct Stop {
    reason: String,
    details: Option<Value>,
}

pub async fn process_anthropic_sse<S>(
    stream: S,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) where
    S: Stream<Item = Result<bytes::Bytes, codex_client::TransportError>> + Unpin,
{
    let mut stream = stream.eventsource();

    let mut blocks: HashMap<usize, BlockState> = HashMap::new();
    let mut open: Vec<usize> = Vec::new();
    let mut response_id = String::new();
    let mut usage: Option<TokenUsage> = None;
    let mut stop: Option<Stop> = None;

    loop {
        if tx_event.is_closed() {
            return;
        }
        let start = Instant::now();
        let response = timeout(idle_timeout, stream.next()).await;
        if let Some(telemetry) = telemetry.as_ref() {
            telemetry.on_sse_poll(&response, start.elapsed());
        }

        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(err))) => {
                let _ = tx_event.send(Err(ApiError::Stream(err.to_string()))).await;
                return;
            }
            Ok(None) => {
                finish(
                    &tx_event,
                    &mut blocks,
                    &mut open,
                    &response_id,
                    usage,
                    stop.as_ref(),
                )
                .await;
                return;
            }
            Err(_) => {
                // A recorded stop reason means only the trailing frames went missing.
                if stop.is_some() {
                    finish(
                        &tx_event,
                        &mut blocks,
                        &mut open,
                        &response_id,
                        usage,
                        stop.as_ref(),
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

        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(err) => {
                debug!("failed to parse Anthropic SSE event: {err}, data: {data}");
                continue;
            }
        };

        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "message_start" => {
                if let Some(id) = value.pointer("/message/id").and_then(Value::as_str) {
                    response_id = id.to_string();
                }
                usage = merge_anthropic_usage(usage, &value);
            }
            "content_block_start" => {
                let (Some(index), Some(block)) = (index_of(&value), value.get("content_block"))
                else {
                    continue;
                };
                let Some(state) = open_block(block) else {
                    continue;
                };
                // The consumer rejects a delta with no active item, so the block
                // is announced before its first delta.
                if let Some(item) = announce_block(&state) {
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputItemAdded(item)))
                        .await;
                }
                if blocks.insert(index, state).is_none() {
                    open.push(index);
                }
            }
            "content_block_delta" => {
                let (Some(index), Some(delta)) = (index_of(&value), value.get("delta")) else {
                    continue;
                };
                apply_delta(&tx_event, &mut blocks, &mut open, index, delta).await;
            }
            "content_block_stop" => {
                let Some(index) = index_of(&value) else {
                    continue;
                };
                open.retain(|pending| *pending != index);
                if let Some(state) = blocks.remove(&index)
                    && let Some(item) = close_block(state)
                {
                    let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
                }
            }
            "message_delta" => {
                usage = merge_anthropic_usage(usage, &value);
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    stop = Some(Stop {
                        reason: reason.to_string(),
                        details: value.pointer("/delta/stop_details").cloned(),
                    });
                }
            }
            "message_stop" => {
                finish(
                    &tx_event,
                    &mut blocks,
                    &mut open,
                    &response_id,
                    usage,
                    stop.as_ref(),
                )
                .await;
                return;
            }
            "ping" => {}
            "error" => {
                let _ = tx_event.send(Err(stream_error(&value))).await;
                return;
            }
            other => debug!("ignoring unknown Anthropic SSE event: {other}"),
        }
    }
}

/// Flushes pending blocks and terminates the stream with exactly one
/// `Completed` or refusal. Callers treat a missing `Completed` as a dropped
/// connection, so every exit path goes through here.
async fn finish(
    tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    blocks: &mut HashMap<usize, BlockState>,
    open: &mut Vec<usize>,
    response_id: &str,
    usage: Option<TokenUsage>,
    stop: Option<&Stop>,
) {
    // Ordered before the stop reason so partial content survives the output cap.
    for index in std::mem::take(open) {
        if let Some(state) = blocks.remove(&index)
            && let Some(item) = close_block(state)
        {
            let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
        }
    }

    if stop.is_some_and(|stop| stop.reason == "model_context_window_exceeded") {
        let _ = tx_event.send(Err(ApiError::ContextWindowExceeded)).await;
        return;
    }

    if let Some(stop) = stop.filter(|stop| stop.reason == "refusal") {
        let _ = tx_event
            .send(Err(ApiError::InvalidRequest {
                message: refusal_message(stop.details.as_ref()),
            }))
            .await;
        return;
    }

    let _ = tx_event
        .send(Ok(ResponseEvent::Completed {
            response_id: response_id.to_string(),
            token_usage: usage,
            end_turn: stop.and_then(|stop| match stop.reason.as_str() {
                "end_turn" | "stop_sequence" | "tool_use" => Some(true),
                "max_tokens" | "pause_turn" => Some(false),
                _ => None,
            }),
        }))
        .await;
}

async fn apply_delta(
    tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    blocks: &mut HashMap<usize, BlockState>,
    open: &mut Vec<usize>,
    index: usize,
    delta: &Value,
) {
    let field = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or_default();

    match delta
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "text_delta" => {
            let text = field("text");
            match blocks.get_mut(&index) {
                Some(BlockState::Text(buffer)) => buffer.push_str(text),
                // A delta whose block never opened would stream to the user but
                // never reach the recorded turn.
                None => {
                    blocks.insert(index, BlockState::Text(text.to_string()));
                    open.push(index);
                }
                Some(_) => {}
            }
            if !text.is_empty() {
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputTextDelta(text.to_string())))
                    .await;
            }
        }
        "thinking_delta" => {
            let text = field("thinking");
            if let Some(BlockState::Thinking { text: buffer, .. }) = blocks.get_mut(&index) {
                buffer.push_str(text);
            }
            if !text.is_empty() {
                let _ = tx_event
                    .send(Ok(ResponseEvent::ReasoningContentDelta {
                        delta: text.to_string(),
                        content_index: index as i64,
                    }))
                    .await;
            }
        }
        "signature_delta" => {
            if let Some(BlockState::Thinking { signature, .. }) = blocks.get_mut(&index) {
                signature
                    .get_or_insert_default()
                    .push_str(field("signature"));
            }
        }
        "input_json_delta" => {
            let partial = field("partial_json");
            let mut call_id = None;
            if let Some(BlockState::ToolUse {
                id, partial_json, ..
            }) = blocks.get_mut(&index)
            {
                partial_json.push_str(partial);
                call_id = Some(id.clone());
            }
            if let Some(call_id) = call_id {
                let _ = tx_event
                    .send(Ok(ResponseEvent::ToolCallInputDelta {
                        item_id: call_id.clone(),
                        call_id: Some(call_id),
                        delta: partial.to_string(),
                    }))
                    .await;
            }
        }
        other => debug!("ignoring unknown Anthropic content block delta: {other}"),
    }
}

fn open_block(block: &Value) -> Option<BlockState> {
    let text = |key: &str| {
        block
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    match block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "text" => Some(BlockState::Text(text("text"))),
        "thinking" => Some(BlockState::Thinking {
            text: text("thinking"),
            signature: block
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        "tool_use" => Some(BlockState::ToolUse {
            id: text("id"),
            name: text("name"),
            partial_json: String::new(),
        }),
        other => {
            debug!("ignoring unknown Anthropic content block: {other}");
            None
        }
    }
}

fn close_block(state: BlockState) -> Option<ResponseItem> {
    match state {
        // An empty assistant message replays as a turn the model did not take.
        BlockState::Text(text) if text.is_empty() => None,
        BlockState::Text(text) => Some(ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText { text }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }),
        BlockState::Thinking { text, signature } => Some(ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: Some(vec![ReasoningItemContent::ReasoningText { text }]),
            // Replaying a thinking block on the next turn requires its signature.
            encrypted_content: signature,
            internal_chat_message_metadata_passthrough: None,
        }),
        BlockState::ToolUse {
            id,
            name,
            partial_json,
        } => Some(ResponseItem::FunctionCall {
            id: None,
            name,
            namespace: None,
            // A tool taking no arguments streams no `input_json_delta` at all.
            arguments: if partial_json.trim().is_empty() {
                "{}".to_string()
            } else {
                partial_json
            },
            call_id: id,
            internal_chat_message_metadata_passthrough: None,
            encrypted_function_args: None,
        }),
    }
}

fn index_of(value: &Value) -> Option<usize> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .map(|index| index as usize)
}

/// `stop_details` is `{type, category, explanation}`.
fn refusal_message(details: Option<&Value>) -> String {
    let field = |key: &str| {
        details
            .and_then(|details| details.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };

    let explanation = ["explanation", "message", "refusal", "reason"]
        .iter()
        .find_map(|key| field(key))
        .unwrap_or(DEFAULT_REFUSAL);

    match field("category") {
        Some(category) => format!("{explanation} (category: {category})"),
        None => explanation.to_string(),
    }
}

fn stream_error(value: &Value) -> ApiError {
    let error = value.get("error");
    let kind = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| error.map_or_else(|| value.to_string(), ToString::to_string));

    // `ApiError::ServerOverloaded` maps to a non-retryable error, so an in-band
    // overload uses `Retryable` to match the pre-stream 529 path.
    if kind == "overloaded_error" {
        return ApiError::Retryable {
            message,
            delay: None,
        };
    }

    ApiError::Stream(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use futures::TryStreamExt;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio_util::io::ReaderStream;

    fn body(events: &[Value]) -> String {
        events.iter().fold(String::new(), |mut body, event| {
            body.push_str(&format!("event: {}\ndata: {event}\n\n", event["type"]));
            body
        })
    }

    async fn collect_results(body: &str) -> Vec<Result<ResponseEvent, ApiError>> {
        let reader = ReaderStream::new(std::io::Cursor::new(body.to_string()))
            .map_err(|err| codex_client::TransportError::Network(err.to_string()));
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(64);
        tokio::spawn(process_anthropic_sse(
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

    fn message_start() -> Value {
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_1",
                "model": "claude-test",
                "usage": {
                    "input_tokens": 10,
                    "cache_read_input_tokens": 4,
                    "cache_creation_input_tokens": 2,
                    "output_tokens": 1,
                }
            }
        })
    }

    fn message_delta(stop_reason: &str, output_tokens: i64) -> Value {
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": output_tokens},
        })
    }

    fn text_block(index: usize, chunks: &[&str]) -> Vec<Value> {
        let mut events = vec![json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "text", "text": ""},
        })];
        events.extend(chunks.iter().map(|chunk| {
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": chunk},
            })
        }));
        events.push(json!({"type": "content_block_stop", "index": index}));
        events
    }

    #[tokio::test]
    async fn streams_text_and_completes_with_usage() {
        let mut events = vec![message_start()];
        events.extend(text_block(0, &["Hi ", "there"]));
        events.push(message_delta("end_turn", 7));
        events.push(json!({"type": "message_stop"}));

        let events = collect_events(&body(&events)).await;

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
                && response_id == "msg_1"
                && usage.output_tokens == 7
                && usage.input_tokens == 16
                && usage.cached_input_tokens == 4
        );
    }

    /// The signature arrives on its own delta after the text.
    #[tokio::test]
    async fn thinking_deltas_accumulate_text_and_signature() {
        let events = [
            message_start(),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""},
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "because"},
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": "sig-"},
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": "1"},
            }),
            json!({"type": "content_block_stop", "index": 0}),
            message_delta("end_turn", 1),
            json!({"type": "message_stop"}),
        ];

        let events = collect_events(&body(&events)).await;

        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. }),
                ResponseEvent::ReasoningContentDelta { delta, content_index: 0 },
                ResponseEvent::OutputItemDone(ResponseItem::Reasoning { content, encrypted_content, .. }),
                ResponseEvent::Completed { .. },
            ] if delta == "because"
                && encrypted_content.as_deref() == Some("sig-1")
                && content == &Some(vec![ReasoningItemContent::ReasoningText { text: "because".to_string() }])
        );
    }

    /// With `display` omitted only a signature streams; the block still reaches the
    /// transcript.
    #[tokio::test]
    async fn a_signature_only_thinking_block_still_lands() {
        let events = [
            message_start(),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "thinking", "thinking": ""},
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "signature_delta", "signature": "sig-1"},
            }),
            json!({"type": "content_block_stop", "index": 0}),
            message_delta("end_turn", 1),
            json!({"type": "message_stop"}),
        ];

        let events = collect_events(&body(&events)).await;

        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. }),
                ResponseEvent::OutputItemDone(ResponseItem::Reasoning { encrypted_content, .. }),
                ResponseEvent::Completed { .. },
            ] if encrypted_content.as_deref() == Some("sig-1")
        );
    }

    #[tokio::test]
    async fn tool_use_input_accumulates_across_deltas() {
        let events = [
            message_start(),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file"},
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"},
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "\"a.txt\"}"},
            }),
            json!({"type": "content_block_stop", "index": 0}),
            message_delta("tool_use", 3),
            json!({"type": "message_stop"}),
        ];

        let events = collect_events(&body(&events)).await;

        assert_matches!(
            &events[..],
            [
                ResponseEvent::ToolCallInputDelta { call_id: Some(first), .. },
                ResponseEvent::ToolCallInputDelta { call_id: Some(second), .. },
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, name, arguments, .. }),
                ResponseEvent::Completed { end_turn: Some(true), .. },
            ] if first == "toolu_1"
                && second == "toolu_1"
                && call_id == "toolu_1"
                && name == "read_file"
                && arguments == r#"{"path":"a.txt"}"#
        );
    }

    /// A tool with no arguments streams no `input_json_delta`, and an empty
    /// `arguments` string fails to parse downstream.
    #[tokio::test]
    async fn a_tool_use_with_no_input_deltas_reports_an_empty_object() {
        let events = [
            message_start(),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "list"},
            }),
            json!({"type": "content_block_stop", "index": 0}),
            message_delta("tool_use", 1),
            json!({"type": "message_stop"}),
        ];

        let events = collect_events(&body(&events)).await;

        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { arguments, .. }),
                ResponseEvent::Completed { .. },
            ] if arguments == "{}"
        );
    }

    /// A `max_tokens` stop is not an error; the partial content is the turn's output.
    #[tokio::test]
    async fn hitting_the_output_cap_flushes_the_partial_content() {
        let mut events = vec![message_start()];
        events.push(json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        }));
        events.push(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "half a sentence"},
        }));
        events.push(message_delta("max_tokens", 99));
        events.push(json!({"type": "message_stop"}));

        let events = collect_events(&body(&events)).await;

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

    #[tokio::test]
    async fn a_paused_turn_completes_without_ending_the_turn() {
        let mut events = vec![message_start()];
        events.extend(text_block(0, &["thinking about it"]));
        events.push(message_delta("pause_turn", 4));
        events.push(json!({"type": "message_stop"}));

        let events = collect_events(&body(&events)).await;

        assert_matches!(
            events.last(),
            Some(ResponseEvent::Completed {
                end_turn: Some(false),
                ..
            })
        );
    }

    /// A refusal can arrive with no content at all.
    #[tokio::test]
    async fn a_refusal_with_no_content_surfaces_as_a_refusal_error() {
        let events = [
            message_start(),
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": "refusal",
                    "stop_details": {
                        "type": "refusal",
                        "category": "cyber",
                        "explanation": "I can't help with that.",
                    },
                },
                "usage": {"output_tokens": 0},
            }),
            json!({"type": "message_stop"}),
        ];

        let results = collect_results(&body(&events)).await;

        assert_matches!(
            &results[..],
            [Err(ApiError::InvalidRequest { message })]
                if message == "I can't help with that. (category: cyber)"
        );
    }

    /// The refusal category is optional.
    #[tokio::test]
    async fn a_refusal_without_a_category_carries_just_the_explanation() {
        let events = [
            message_start(),
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": "refusal",
                    "stop_details": {"type": "refusal", "explanation": "Not something I can do."},
                },
            }),
            json!({"type": "message_stop"}),
        ];

        let results = collect_results(&body(&events)).await;

        assert_matches!(
            &results[..],
            [Err(ApiError::InvalidRequest { message })] if message == "Not something I can do."
        );
    }

    #[tokio::test]
    async fn a_refusal_without_details_still_carries_a_message() {
        let events = [
            message_start(),
            json!({"type": "message_delta", "delta": {"stop_reason": "refusal"}}),
            json!({"type": "message_stop"}),
        ];

        let results = collect_results(&body(&events)).await;

        assert_matches!(
            &results[..],
            [Err(ApiError::InvalidRequest { message })] if message == DEFAULT_REFUSAL
        );
    }

    #[tokio::test]
    async fn an_in_band_error_frame_surfaces() {
        let events = [
            message_start(),
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad prompt"}}),
        ];

        let results = collect_results(&body(&events)).await;

        assert_matches!(
            &results[..],
            [Err(ApiError::Stream(message))] if message == "bad prompt"
        );
    }

    /// The in-band `overloaded_error` maps to a retryable error, as the HTTP 529 does.
    #[tokio::test]
    async fn an_overloaded_error_frame_maps_to_a_backoff() {
        let events = [
            message_start(),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ];

        let results = collect_results(&body(&events)).await;

        assert_matches!(
            &results[..],
            [Err(ApiError::Retryable { message, .. })] if message == "Overloaded"
        );
    }

    /// A missing `Completed` reads as a dropped connection and retries the turn.
    #[tokio::test]
    async fn a_stream_that_ends_early_still_completes() {
        let mut events = vec![message_start()];
        events.push(json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        }));
        events.push(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "cut off"},
        }));

        let events = collect_events(&body(&events)).await;

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

    #[tokio::test]
    async fn pings_and_unknown_frames_are_ignored() {
        let mut events = vec![message_start(), json!({"type": "ping"})];
        events.extend(text_block(0, &["hi"]));
        events.push(json!({"type": "message_bogus"}));
        events.push(message_delta("end_turn", 1));
        events.push(json!({"type": "message_stop"}));

        let events = collect_events(&body(&events)).await;

        // added, delta, done, completed
        assert_eq!(events.len(), 4, "{events:?}");
        assert_matches!(events.last(), Some(ResponseEvent::Completed { .. }));
    }

    #[tokio::test]
    async fn interleaved_blocks_are_tracked_by_index() {
        let mut events = vec![message_start()];
        events.push(json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        }));
        events.push(json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file"},
        }));
        events.push(json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "{}"},
        }));
        events.push(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "reading"},
        }));
        events.push(json!({"type": "content_block_stop", "index": 0}));
        events.push(json!({"type": "content_block_stop", "index": 1}));
        events.push(message_delta("tool_use", 5));
        events.push(json!({"type": "message_stop"}));

        let events = collect_events(&body(&events)).await;

        assert_matches!(
            &events[..],
            [
                ResponseEvent::OutputItemAdded(ResponseItem::Message { .. }),
                ResponseEvent::ToolCallInputDelta { .. },
                ResponseEvent::OutputTextDelta(text),
                ResponseEvent::OutputItemDone(ResponseItem::Message { .. }),
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { arguments, .. }),
                ResponseEvent::Completed { .. },
            ] if text == "reading" && arguments == "{}"
        );
    }

    /// A delta for a block that never opened, from an unmodelled block type or a
    /// frame out of order, still reaches the transcript.
    #[tokio::test]
    async fn a_delta_without_an_open_block_is_still_recorded() {
        let events = [
            message_start(),
            json!({
                "type": "content_block_delta",
                "index": 7,
                "delta": {"type": "text_delta", "text": "orphaned"},
            }),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ];

        let events = collect_events(&body(&events)).await;

        assert!(
            events.iter().any(|event| matches!(
                event,
                ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. })
                    if content.iter().any(|c| matches!(
                        c,
                        ContentItem::OutputText { text } if text == "orphaned"
                    ))
            )),
            "{events:?}"
        );
    }

    /// A delta arriving with no active item panics the turn consumer in a checked
    /// build, so every delta follows the `OutputItemAdded` that opens its item.
    #[tokio::test]
    async fn every_streaming_delta_follows_an_item_announcement() {
        let mut events = vec![message_start()];
        events.extend(text_block(0, &["hello"]));
        events.push(json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {"type": "thinking", "thinking": ""},
        }));
        events.push(json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": {"type": "thinking_delta", "thinking": "weighing"},
        }));
        events.push(json!({"type": "content_block_stop", "index": 1}));
        events.push(message_delta("end_turn", 1));
        events.push(json!({"type": "message_stop"}));

        let events = collect_events(&body(&events)).await;

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
        assert_eq!(announced, 2, "{events:?}");
    }
}
