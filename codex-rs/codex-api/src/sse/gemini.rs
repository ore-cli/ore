//! Streaming parser for the Gemini `streamGenerateContent` API.
//!
//! This wire has no event type and no block boundaries: every frame is a whole
//! `GenerateContentResponse` whose parts are the deltas since the last one, so
//! an item ends only when a part of a different kind arrives. The turn ends
//! with a `finishReason` on the last frame rather than a terminator event,
//! which is why completion is driven from the stream closing rather than from
//! a frame.

use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
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
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

const DEFAULT_REFUSAL: &str = "the model declined to answer this request";

/// How long a finished turn waits for a trailing frame.
///
/// Gemini puts `usageMetadata` on the same frame as `finishReason` and then
/// closes, so this normally expires never. It bounds the case where a gateway
/// splits the two and then holds the connection open, which would otherwise
/// hang the turn for the full idle timeout.
const TRAILING_FRAME_GRACE: Duration = Duration::from_secs(5);

pub(crate) fn spawn_gemini_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) -> ResponseStream {
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);

    // No `RateLimits` event: Gemini publishes no quota headers, and an empty
    // snapshot would read as "limits known, and they are zero".
    let bytes = stream_response.bytes;
    tokio::spawn(process_gemini_sse(bytes, tx_event, idle_timeout, telemetry));

    // Google spells its request id `x-goog-request-id`; the OpenAI-shaped
    // header is read too because a proxy fronting this wire may set it.
    let upstream_request_id = ["x-goog-request-id", "x-request-id"]
        .into_iter()
        .find_map(|name| stream_response.headers.get(name))
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    ResponseStream {
        rx_event,
        upstream_request_id,
    }
}

/// The item deltas are currently streaming into.
///
/// Gemini never marks an item's end, so the arrival of a part of another kind
/// is what closes the open one.
enum Open {
    Text(String),
    Thought {
        text: String,
        signature: Option<String>,
        content_index: i64,
    },
}

/// Everything that accumulates across a turn's frames.
#[derive(Default)]
struct Turn {
    open: Option<Open>,
    next_content_index: i64,
    next_call_index: usize,
}

impl Turn {
    /// Emits the open item and clears it.
    ///
    /// Ordered before anything else that emits: a tool result has to follow the
    /// message that issued the call, so text flushed later would replay between
    /// the two.
    async fn close(&mut self, tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>) {
        let item = match self.open.take() {
            // An empty assistant message replays as a turn the model did not take.
            Some(Open::Text(text)) if text.is_empty() => None,
            Some(Open::Text(text)) => Some(ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText { text }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }),
            Some(Open::Thought {
                text, signature, ..
            }) => Some(ResponseItem::Reasoning {
                id: None,
                summary: Vec::new(),
                content: Some(vec![ReasoningItemContent::ReasoningText { text }]),
                // Replaying a thought on the next turn requires its signature.
                encrypted_content: signature,
                internal_chat_message_metadata_passthrough: None,
            }),
            None => None,
        };

        if let Some(item) = item {
            let _ = tx_event.send(Ok(ResponseEvent::OutputItemDone(item))).await;
        }
    }

    async fn push_text(
        &mut self,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
        text: &str,
    ) {
        if text.is_empty() {
            return;
        }
        if !matches!(self.open, Some(Open::Text(_))) {
            self.close(tx_event).await;
            self.open = Some(Open::Text(String::new()));
            // The consumer rejects a delta with no active item, so the item is
            // announced before its first delta.
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemAdded(ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: Vec::new(),
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                })))
                .await;
        }
        if let Some(Open::Text(buffer)) = &mut self.open {
            buffer.push_str(text);
        }
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputTextDelta(text.to_string())))
            .await;
    }

    async fn push_thought(
        &mut self,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
        text: &str,
    ) {
        if text.is_empty() {
            return;
        }
        if !matches!(self.open, Some(Open::Thought { .. })) {
            self.close(tx_event).await;
            self.open = Some(Open::Thought {
                text: String::new(),
                signature: None,
                content_index: self.next_content_index,
            });
            self.next_content_index += 1;
            let _ = tx_event
                .send(Ok(ResponseEvent::OutputItemAdded(
                    ResponseItem::Reasoning {
                        id: None,
                        summary: Vec::new(),
                        content: None,
                        encrypted_content: None,
                        internal_chat_message_metadata_passthrough: None,
                    },
                )))
                .await;
        }
        let mut index = 0;
        if let Some(Open::Thought {
            text: buffer,
            content_index,
            ..
        }) = &mut self.open
        {
            buffer.push_str(text);
            index = *content_index;
        }
        let _ = tx_event
            .send(Ok(ResponseEvent::ReasoningContentDelta {
                delta: text.to_string(),
                content_index: index,
            }))
            .await;
    }

    /// A signature rides on the part it belongs to rather than on one of its
    /// own, so it is attached to whichever thought is open when it arrives.
    fn set_signature(&mut self, signature: &str) {
        if let Some(Open::Thought {
            signature: held, ..
        }) = &mut self.open
        {
            // Last write wins. Appending glued two consecutive signed thought
            // parts into "SIG_ASIG_B", which is neither signature and is replayed
            // verbatim as `thoughtSignature` on the next turn. A signature
            // describes the part it rides on, so the newest one is the live one.
            *held = Some(signature.to_string());
        }
    }

    /// Emits a complete function call.
    ///
    /// Unlike Anthropic's, this wire delivers a call's arguments whole rather
    /// than as a stream of JSON fragments, and attaches no call id at all: the
    /// id is synthesized so the transcript can pair the result back, and the
    /// request builder resolves it to a function name on the way out.
    async fn push_function_call(
        &mut self,
        tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
        call: &Value,
        signature: Option<&str>,
    ) {
        let Some(name) = call
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        else {
            debug!("ignoring Gemini functionCall with no name: {call}");
            return;
        };

        self.close(tx_event).await;

        let call_id = format!("gemini-call-{}", self.next_call_index);
        self.next_call_index += 1;
        // A tool taking no arguments carries no `args` at all, and an empty
        // string fails to parse downstream.
        let arguments = call
            .get("args")
            .filter(|args| args.is_object())
            .map_or_else(|| "{}".to_string(), ToString::to_string);

        // Consumers that render arguments as they arrive expect at least one
        // delta before the item lands.
        let _ = tx_event
            .send(Ok(ResponseEvent::ToolCallInputDelta {
                item_id: call_id.clone(),
                call_id: Some(call_id.clone()),
                delta: arguments.clone(),
            }))
            .await;
        let _ = tx_event
            .send(Ok(ResponseEvent::OutputItemDone(
                ResponseItem::FunctionCall {
                    id: None,
                    name: name.to_string(),
                    namespace: None,
                    arguments,
                    call_id,
                    internal_chat_message_metadata_passthrough: None,
                    // A signature arriving on a functionCall part belongs to the
                    // CALL. It used to be stamped onto whatever thought happened
                    // to be open -- overwriting that thought's own signature --
                    // or dropped entirely when nothing was open.
                    encrypted_function_args: signature.map(|sig| vec![sig.to_string()]),
                },
            )))
            .await;
    }
}

pub async fn process_gemini_sse<S>(
    stream: S,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) where
    S: Stream<Item = Result<bytes::Bytes, codex_client::TransportError>> + Unpin,
{
    let mut stream = stream.eventsource();

    let mut turn = Turn::default();
    let mut response_id = String::new();
    let mut usage: Option<TokenUsage> = None;
    let mut finish_reason: Option<String> = None;

    loop {
        if tx_event.is_closed() {
            return;
        }
        let start = Instant::now();
        // Once the turn has finished the only thing still expected is a
        // trailing usage frame.
        let wait = if finish_reason.is_some() {
            TRAILING_FRAME_GRACE.min(idle_timeout)
        } else {
            idle_timeout
        };
        let response = timeout(wait, stream.next()).await;
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
                finish(&tx_event, &mut turn, &response_id, usage, finish_reason).await;
                return;
            }
            Err(_) => {
                // A recorded finish reason means only the trailing frame went
                // missing; without one the turn really did stall.
                if finish_reason.is_some() {
                    finish(&tx_event, &mut turn, &response_id, usage, finish_reason).await;
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
                debug!("failed to parse Gemini SSE event: {err}, data: {data}");
                continue;
            }
        };

        // Before anything else reads the frame: after a 200 the status cannot
        // change, so a mid-stream failure arrives as a frame with an `error`
        // and no candidates, which every read below would silently skip.
        if value.get("error").is_some_and(|error| !error.is_null()) {
            turn.close(&tx_event).await;
            let _ = tx_event.send(Err(stream_error(&value))).await;
            return;
        }

        // A prompt rejected by the safety filters produces no candidates at all.
        if let Some(reason) = value
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
        {
            let _ = tx_event
                .send(Err(ApiError::InvalidRequest {
                    message: format!("{DEFAULT_REFUSAL} (blocked: {reason})"),
                }))
                .await;
            return;
        }

        usage = merge_gemini_usage(usage, &value);
        if let Some(id) = value.get("responseId").and_then(Value::as_str) {
            response_id = id.to_string();
        }

        let Some(candidate) = first_candidate(&value) else {
            continue;
        };

        for part in candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
        {
            apply_part(&tx_event, &mut turn, part).await;
        }

        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            finish_reason = Some(reason.to_string());
        }
    }
}

/// Reads the candidate this parser follows.
///
/// Only one is ever tracked: with `candidateCount > 1` the alternatives carry
/// the same `index` semantics but different text, and folding them together
/// would splice two answers into one message.
fn first_candidate(value: &Value) -> Option<&Value> {
    value
        .get("candidates")
        .and_then(Value::as_array)?
        .iter()
        .find(|candidate| {
            candidate
                .get("index")
                .and_then(Value::as_i64)
                .unwrap_or_default()
                == 0
        })
}

async fn apply_part(
    tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    turn: &mut Turn,
    part: &Value,
) {
    // Captured, not applied yet. Applying it here was the bug: the ordinary
    // Gemini shape carries a part's text AND its signature together, and
    // `set_signature` can only write into an ALREADY-OPEN item -- which
    // `push_thought` opens one statement later. So every single-part signature
    // landed on whatever was open before, usually nothing, and was lost. The
    // request builder then drops a `Reasoning` with no `encrypted_content`, so
    // the model's whole thought chain vanished from the next turn -- the state
    // Gemini 3 rejects when a functionCall follows.
    let signature = part.get("thoughtSignature").and_then(Value::as_str);

    if let Some(call) = part.get("functionCall") {
        turn.push_function_call(tx_event, call, signature).await;
        return;
    }

    if let Some(text) = part.get("text").and_then(Value::as_str) {
        // `thought` marks reasoning; routing it to visible text would print the
        // model's scratchpad as its answer.
        // A gateway that stringifies the flag ("thought": "true") would
        // otherwise print the model's scratchpad as its answer.
        let is_thought = match part.get("thought") {
            Some(Value::Bool(flag)) => *flag,
            Some(Value::String(flag)) => flag.eq_ignore_ascii_case("true"),
            _ => false,
        };
        if is_thought {
            turn.push_thought(tx_event, text).await;
        } else {
            turn.push_text(tx_event, text).await;
        }
        // AFTER the push, so the item the signature belongs to is open.
        if let Some(signature) = signature {
            turn.set_signature(signature);
        }
        return;
    }

    // A signature can arrive on a part with no content of its own; it still
    // belongs to whatever is currently open.
    if let Some(signature) = signature {
        turn.set_signature(signature);
        return;
    }

    // `inlineData` (a generated image) and `functionResponse` (an echo of the
    // request) have no transcript item on the way back.
    debug!("ignoring unmodelled Gemini part: {part}");
}

/// Flushes the open item and terminates the stream with exactly one `Completed`
/// or error. Callers treat a missing `Completed` as a dropped connection, so
/// every exit path goes through here.
async fn finish(
    tx_event: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
    turn: &mut Turn,
    response_id: &str,
    usage: Option<TokenUsage>,
    finish_reason: Option<String>,
) {
    // Ordered before the finish reason so partial content survives a stop that
    // is reported as an error.
    turn.close(tx_event).await;

    let reason = finish_reason.as_deref().unwrap_or_default();

    // A safety stop is a refusal, not a turn: reporting it as a normal
    // completion leaves the user staring at silence.
    if is_refusal(reason) {
        let _ = tx_event
            .send(Err(ApiError::InvalidRequest {
                message: format!("{DEFAULT_REFUSAL} (finish reason: {reason})"),
            }))
            .await;
        return;
    }

    // The model emitted a call the backend could not parse; nothing usable came
    // back, and a retry is the only way forward.
    if reason == "MALFORMED_FUNCTION_CALL" {
        let _ = tx_event
            .send(Err(ApiError::Retryable {
                message: "gemini returned a malformed function call".to_string(),
                delay: None,
            }))
            .await;
        return;
    }

    let _ = tx_event
        .send(Ok(ResponseEvent::Completed {
            response_id: response_id.to_string(),
            token_usage: usage,
            end_turn: match reason {
                "STOP" => Some(true),
                // The output cap, not the context window: an oversized request
                // is rejected up front as a 400 and never reaches this parser.
                "MAX_TOKENS" => Some(false),
                _ => None,
            },
        }))
        .await;
}

/// Finish reasons that mean the content was withheld rather than produced.
fn is_refusal(reason: &str) -> bool {
    matches!(
        reason,
        "SAFETY"
            | "RECITATION"
            | "BLOCKLIST"
            | "PROHIBITED_CONTENT"
            | "SPII"
            | "IMAGE_SAFETY"
            | "UNEXPECTED_TOOL_CALL"
    )
}

fn stream_error(value: &Value) -> ApiError {
    let error = value.get("error");
    let status = error
        .and_then(|error| error.get("status"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| error.map_or_else(|| value.to_string(), ToString::to_string));

    // In-band quota and overload failures get the same backoff their pre-stream
    // status codes would: once the 200 is out, the status can no longer say so.
    if status == "RESOURCE_EXHAUSTED" || status == "UNAVAILABLE" || code == 429 || code == 503 {
        return ApiError::Retryable {
            message,
            delay: None,
        };
    }

    ApiError::Stream(message)
}

#[cfg(test)]
#[path = "gemini_tests.rs"]
mod tests;

// Mounted here rather than from `sse/mod.rs`: that file is upstream's, and this
// module is entirely the fork's.
#[path = "gemini_usage.rs"]
mod gemini_usage;
use gemini_usage::merge_gemini_usage;
