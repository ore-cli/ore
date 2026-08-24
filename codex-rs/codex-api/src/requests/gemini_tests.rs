use super::*;
use crate::provider::RetryConfig;
use codex_protocol::ResponseItemId;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::LocalShellExecAction;
use codex_protocol::models::LocalShellStatus;
use pretty_assertions::assert_eq;
use std::time::Duration;

/// A 1x1 PNG; small enough that `ResizeToFit` passes the source bytes through.
const PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn provider() -> Provider {
    Provider {
        name: "gemini".to_string(),
        base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
        query_params: None,
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_attempts: 1,
            base_delay: Duration::from_millis(10),
            retry_429: false,
            retry_5xx: true,
            retry_transport: true,
        },
        stream_idle_timeout: Duration::from_secs(1),
    }
}

fn message(role: &str, content: Vec<ContentItem>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn user_message(text: &str) -> ResponseItem {
    message(
        "user",
        vec![ContentItem::InputText {
            text: text.to_string(),
        }],
    )
}

fn assistant_message(text: &str) -> ResponseItem {
    message(
        "assistant",
        vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
    )
}

fn developer_message(text: &str) -> ResponseItem {
    message(
        "developer",
        vec![ContentItem::InputText {
            text: text.to_string(),
        }],
    )
}

fn function_call(call_id: &str, name: &str, arguments: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: name.to_string(),
        namespace: None,
        arguments: arguments.to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
        encrypted_function_args: None,
    }
}

fn function_output(call_id: &str, text: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_text(text.to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn reasoning(text: &str, signature: Option<&str>) -> ResponseItem {
    ResponseItem::Reasoning {
        id: None,
        summary: Vec::new(),
        content: Some(vec![ReasoningItemContent::ReasoningText {
            text: text.to_string(),
        }]),
        encrypted_content: signature.map(str::to_string),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn body_of(input: &[ResponseItem]) -> Value {
    GeminiRequestBuilder::new("gemini-test", "inst", input, &[])
        .build(&provider())
        .expect("request")
        .body
}

#[test]
fn the_transcript_maps_onto_user_and_model_turns() {
    let body = body_of(&[
        user_message("hello"),
        assistant_message("hi"),
        user_message("more"),
    ]);

    assert_eq!(
        body["contents"],
        json!([
            {"role": "user", "parts": [{"text": "hello"}]},
            {"role": "model", "parts": [{"text": "hi"}]},
            {"role": "user", "parts": [{"text": "more"}]},
        ])
    );
    assert_eq!(
        body["systemInstruction"],
        json!({"parts": [{"text": "inst"}]})
    );
}

/// Split into separate turns, adjacent user parts read as turns that never
/// happened between them.
#[test]
fn consecutive_same_role_items_collapse_into_one_turn() {
    let body = body_of(&[user_message("first"), user_message("second")]);

    assert_eq!(
        body["contents"],
        json!([{"role": "user", "parts": [{"text": "first"}, {"text": "second"}]}])
    );
}

/// A tool result is paired to its call by function name, so the call has to be
/// walked before the result can be spelled at all.
#[test]
fn a_tool_result_is_named_after_the_call_it_answers() {
    let body = body_of(&[
        user_message("read it"),
        function_call("call-1", "read_file", r#"{"path":"a.txt"}"#),
        function_output("call-1", "contents"),
    ]);

    assert_eq!(
        body["contents"][1],
        json!({
            "role": "model",
            "parts": [{"functionCall": {"name": "read_file", "args": {"path": "a.txt"}}}],
        })
    );
    assert_eq!(
        body["contents"][2],
        json!({
            "role": "user",
            "parts": [{"functionResponse": {"name": "read_file", "response": {"output": "contents"}}}],
        })
    );
}

/// Two turns can reuse a synthesized call id; the result must name the call
/// that actually preceded it.
#[test]
fn a_reused_call_id_resolves_to_the_nearest_preceding_call() {
    let body = body_of(&[
        user_message("go"),
        function_call("call-0", "first_tool", "{}"),
        function_output("call-0", "one"),
        function_call("call-0", "second_tool", "{}"),
        function_output("call-0", "two"),
    ]);

    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"]["name"],
        json!("first_tool")
    );
    assert_eq!(
        body["contents"][4]["parts"][0]["functionResponse"]["name"],
        json!("second_tool")
    );
}

/// Nothing names the function, and a `functionResponse` for a function the
/// model was never offered is a 400.
#[test]
fn a_tool_result_with_no_preceding_call_is_dropped() {
    let body = body_of(&[user_message("hi"), function_output("call-9", "orphan")]);

    assert_eq!(
        body["contents"],
        json!([{"role": "user", "parts": [{"text": "hi"}]}])
    );
}

#[test]
fn a_failed_tool_result_reports_under_the_error_key() {
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-1".to_string(),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text("boom".to_string()),
            success: Some(false),
        },
        internal_chat_message_metadata_passthrough: None,
    };
    let body = body_of(&[
        user_message("go"),
        function_call("call-1", "run", "{}"),
        output,
    ]);

    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"]["response"],
        json!({"error": "boom"})
    );
}

/// An empty `response` object is rejected.
#[test]
fn an_empty_tool_result_still_carries_text() {
    let body = body_of(&[
        user_message("go"),
        function_call("call-1", "run", "{}"),
        function_output("call-1", ""),
    ]);

    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"]["response"],
        json!({"output": EMPTY_TOOL_RESULT})
    );
}

/// `response` is a JSON object, so an image rides as its own part of the same
/// user turn.
#[test]
fn tool_result_images_ride_beside_the_response_part() {
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-1".to_string(),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputText {
                text: "see below".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: PNG_DATA_URL.to_string(),
                detail: None,
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    };
    let body = body_of(&[
        user_message("go"),
        function_call("call-1", "screenshot", "{}"),
        output,
    ]);

    let parts = &body["contents"][2]["parts"];
    assert_eq!(
        parts[0]["functionResponse"]["response"],
        json!({"output": "see below"})
    );
    assert_eq!(parts[1]["inlineData"]["mimeType"], json!("image/png"));
}

#[test]
fn a_local_shell_call_pairs_through_either_id() {
    let call = ResponseItem::LocalShellCall {
        id: Some(ResponseItemId::from_server("lsh-1".to_string())),
        call_id: Some("call-1".to_string()),
        status: LocalShellStatus::Completed,
        action: LocalShellAction::Exec(LocalShellExecAction {
            command: vec!["ls".to_string()],
            timeout_ms: None,
            working_directory: None,
            env: None,
            user: None,
        }),
        internal_chat_message_metadata_passthrough: None,
    };
    let body = body_of(&[user_message("go"), call, function_output("call-1", "a.txt")]);

    assert_eq!(
        body["contents"][1]["parts"][0]["functionCall"]["name"],
        json!(LOCAL_SHELL_TOOL)
    );
    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"]["name"],
        json!(LOCAL_SHELL_TOOL)
    );
}

/// This wire has no system role inside `contents`, so developer text folds into
/// the user turn and is marked so the model can tell it from typed input.
#[test]
fn developer_text_folds_into_the_user_turn_as_a_reminder() {
    let body = body_of(&[user_message("hello"), developer_message("be brief")]);

    assert_eq!(
        body["contents"],
        json!([{
            "role": "user",
            "parts": [
                {"text": "hello"},
                {"text": "<system-reminder>\nbe brief\n</system-reminder>"},
            ],
        }])
    );
}

#[test]
fn a_signed_thought_replays_with_its_signature() {
    let body = body_of(&[user_message("go"), reasoning("weighing", Some("sig-1"))]);

    assert_eq!(
        body["contents"][1]["parts"][0],
        json!({"text": "weighing", "thought": true, "thoughtSignature": "sig-1"})
    );
}

/// A signature with no summary text has no part of its own; Gemini carries it
/// on the next model part, and a text-less part is rejected.
#[test]
fn a_bare_signature_attaches_to_the_next_model_part() {
    let body = body_of(&[
        user_message("go"),
        reasoning("", Some("sig-1")),
        function_call("call-1", "run", "{}"),
        function_output("call-1", "done"),
    ]);

    assert_eq!(
        body["contents"][1]["parts"][0],
        json!({
            "functionCall": {"name": "run", "args": {}},
            "thoughtSignature": "sig-1",
        })
    );
}

#[test]
fn an_unsigned_thought_is_dropped() {
    let body = body_of(&[user_message("go"), reasoning("weighing", None)]);

    assert_eq!(
        body["contents"],
        json!([{"role": "user", "parts": [{"text": "go"}]}])
    );
}

/// `args` is an object on this wire; the Responses transcript stores a string.
#[test]
fn unparsable_call_arguments_become_an_empty_object() {
    let body = body_of(&[
        user_message("go"),
        function_call("call-1", "run", "not json"),
        function_output("call-1", "done"),
    ]);

    assert_eq!(
        body["contents"][1]["parts"][0]["functionCall"]["args"],
        json!({})
    );
}

#[test]
fn an_empty_transcript_is_rejected_before_it_reaches_the_wire() {
    let error = GeminiRequestBuilder::new("gemini-test", "inst", &[], &[])
        .build(&provider())
        .expect_err("empty transcript");

    assert!(
        matches!(error, ApiError::Stream(message) if message.contains("no contents")),
        "unexpected error"
    );
}

/// The tools encoder is shared across wires and spells the same tool three
/// ways; all three have to land as one `functionDeclarations` entry.
#[test]
fn tool_definitions_are_read_in_every_sibling_wire_spelling() {
    let schema = json!({"type": "object", "properties": {"path": {"type": "string"}}});
    let tools = vec![
        json!({"type": "function", "function": {"name": "chat_shaped", "description": "a", "parameters": schema}}),
        json!({"type": "function", "name": "responses_shaped", "description": "b", "parameters": schema}),
        json!({"name": "anthropic_shaped", "description": "c", "input_schema": schema}),
    ];

    let body = GeminiRequestBuilder::new("gemini-test", "inst", &[user_message("go")], &tools)
        .build(&provider())
        .expect("request")
        .body;

    assert_eq!(
        body["tools"],
        json!([{"functionDeclarations": [
            {"name": "chat_shaped", "description": "a", "parameters": schema},
            {"name": "responses_shaped", "description": "b", "parameters": schema},
            {"name": "anthropic_shaped", "description": "c", "parameters": schema},
        ]}])
    );
    assert_eq!(
        body["toolConfig"],
        json!({"functionCallingConfig": {"mode": "AUTO"}})
    );
}

#[test]
fn a_tool_with_no_name_is_dropped_rather_than_sent() {
    let tools = vec![json!({"type": "function", "function": {"description": "no name"}})];

    let body = GeminiRequestBuilder::new("gemini-test", "inst", &[user_message("go")], &tools)
        .build(&provider())
        .expect("request")
        .body;

    assert_eq!(body.get("tools"), None);
    assert_eq!(body.get("toolConfig"), None);
}

/// `parameters` is coerced into an OpenAPI schema, where an unknown field is a
/// hard 400 rather than a warning, and both of these arrive through no fault of
/// the caller.
#[test]
fn schema_keys_gemini_rejects_are_stripped_at_every_depth() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": {"type": "string"},
            "nested": {"type": "object", "additionalProperties": false},
            "tags": {"type": "array", "items": {"type": "object", "additionalProperties": false}},
            "either": {"anyOf": [{"type": "string"}, {"type": "object", "additionalProperties": false}]},
        },
        "required": ["path"],
    });

    assert_eq!(
        sanitize_schema(&schema),
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "nested": {"type": "object"},
                "tags": {"type": "array", "items": {"type": "object"}},
                "either": {"anyOf": [{"type": "string"}, {"type": "object"}]},
            },
            "required": ["path"],
        })
    );
}

/// Constraints Gemini does understand must survive; stripping them silently
/// widens what the model may return.
#[test]
fn schema_constraints_gemini_understands_are_preserved() {
    let schema = json!({
        "type": "object",
        "properties": {"depth": {"type": "integer", "minimum": 1, "description": "how deep"}},
        "propertyOrdering": ["depth"],
    });

    assert_eq!(sanitize_schema(&schema), schema);
}

#[test]
fn generation_config_carries_the_sampling_and_thinking_knobs() {
    let body = GeminiRequestBuilder::new("gemini-test", "inst", &[user_message("go")], &[])
        .max_output_tokens(Some(4096))
        .temperature(Some(0.25))
        .thinking(Some(GeminiThinkingConfig {
            budget_tokens: Some(2048),
            include_thoughts: true,
        }))
        .build(&provider())
        .expect("request")
        .body;

    assert_eq!(
        body["generationConfig"],
        json!({
            "temperature": 0.25,
            "maxOutputTokens": 4096,
            "thinkingConfig": {"thinkingBudget": 2048, "includeThoughts": true},
        })
    );
}

/// The schema alone is ignored; structured output is gated on the mime type.
#[test]
fn an_output_schema_sets_the_json_response_mime_type() {
    let schema = json!({"type": "object", "additionalProperties": false});
    let body = GeminiRequestBuilder::new("gemini-test", "inst", &[user_message("go")], &[])
        .output_schema(Some(&schema))
        .build(&provider())
        .expect("request")
        .body;

    assert_eq!(
        body["generationConfig"],
        json!({"responseMimeType": "application/json", "responseSchema": {"type": "object"}})
    );
}

/// Absent knobs must leave the field out entirely: an empty
/// `generationConfig` pins defaults the model would otherwise choose.
#[test]
fn an_unconfigured_request_sends_no_generation_config() {
    let body = body_of(&[user_message("go")]);

    assert_eq!(body.get("generationConfig"), None);
}

#[test]
fn the_model_rides_beside_the_body_because_the_url_needs_it() {
    let request = GeminiRequestBuilder::new("gemini-test", "inst", &[user_message("go")], &[])
        .conversation_id(Some("thread-1".to_string()))
        .build(&provider())
        .expect("request");

    assert_eq!(request.model, "gemini-test");
    assert_eq!(request.body.get("model"), None);
    assert_eq!(
        request
            .headers
            .get("session-id")
            .and_then(|id| id.to_str().ok()),
        Some("thread-1")
    );
    // The API key belongs to the auth provider.
    assert!(request.headers.get("x-goog-api-key").is_none());
}

#[test]
fn an_inline_image_rides_as_inline_data() {
    let body = body_of(&[message(
        "user",
        vec![ContentItem::InputImage {
            image_url: PNG_DATA_URL.to_string(),
            detail: None,
        }],
    )]);

    assert_eq!(
        body["contents"][0]["parts"][0]["inlineData"]["mimeType"],
        json!("image/png")
    );
}

/// `fileData` addresses the Files API, not arbitrary URLs, so a remote image
/// would 400 on a URI the backend cannot fetch.
#[test]
fn a_remote_image_degrades_to_text() {
    let body = body_of(&[message(
        "user",
        vec![ContentItem::InputImage {
            image_url: "https://example.com/cat.png".to_string(),
            detail: None,
        }],
    )]);

    let text = body["contents"][0]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(text.starts_with("[image omitted"), "{text}");
}

#[test]
fn a_signed_function_call_is_replayed_with_its_signature() {
    // The SSE layer captures a call's thoughtSignature, but the request builder
    // destructured FunctionCall with `..` and dropped it -- so the round-2 fix was
    // correct at one layer and dead on the wire, and Gemini 3 rejects an unsigned
    // call that follows a thought.
    let mut call = function_call("call-1", "run", "{}");
    if let ResponseItem::FunctionCall {
        encrypted_function_args,
        ..
    } = &mut call
    {
        *encrypted_function_args = Some(vec!["CALL_SIG".to_string()]);
    }

    let body = body_of(&[
        user_message("go"),
        reasoning("weighing", Some("THOUGHT_SIG")),
        call,
    ]);

    let rendered = body.to_string();
    assert!(
        rendered.contains("CALL_SIG"),
        "the call's own signature must reach the wire: {rendered}"
    );
    assert!(
        rendered.contains("THOUGHT_SIG"),
        "and the thought keeps its own: {rendered}"
    );
}
