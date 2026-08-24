//! Request builder for the Gemini `generateContent` API.
//!
//! Converts the Responses-shaped `ResponseItem` transcript into Gemini
//! `contents[]`. Three properties of this wire drive everything below: the
//! assistant role is spelled "model", there is no system role inside the
//! transcript, and a tool result is paired to its call by *function name*
//! rather than by a call id, so the id-keyed transcript has to be re-paired on
//! the way out.

use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::chat::flattened_tool_name;
use crate::requests::headers::build_session_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::plaintext_agent_message_content;
use codex_protocol::protocol::SessionSource;
use codex_utils_image::PromptImageMode;
use codex_utils_image::load_data_url_for_prompt;
use http::HeaderMap;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use tracing::warn;

const ROLE_USER: &str = "user";
/// Gemini's spelling of the assistant turn. Sending "assistant" is a 400.
const ROLE_MODEL: &str = "model";
const ROLE_ASSISTANT: &str = "assistant";

/// Stands in for audio input, which this wire has no part for.
const AUDIO_PLACEHOLDER: &str = "[audio input omitted: unsupported by the Gemini API]";

/// Gemini rejects a `functionResponse` whose `response` object is empty.
const EMPTY_TOOL_RESULT: &str = "(no output)";

/// Assembled request body plus headers for a Gemini streaming call.
///
/// `model` rides alongside the body because this wire puts it in the URL path
/// (`models/{model}:streamGenerateContent`), not in the payload.
#[derive(Debug)]
pub struct GeminiRequest {
    pub model: String,
    pub body: Value,
    pub headers: HeaderMap,
}

/// `generationConfig.thinkingConfig`.
#[derive(Debug, Clone, Copy)]
pub struct GeminiThinkingConfig {
    /// `thinkingBudget`; `None` leaves the model's own default in place, which
    /// is what "dynamic thinking" means on this wire.
    pub budget_tokens: Option<i64>,
    /// Without this the model still thinks but streams no thought parts, so the
    /// reasoning stays invisible and unbillable to the user.
    pub include_thoughts: bool,
}

pub struct GeminiRequestBuilder<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a [ResponseItem],
    tools: &'a [Value],
    max_output_tokens: Option<i64>,
    temperature: Option<f64>,
    thinking: Option<GeminiThinkingConfig>,
    output_schema: Option<&'a Value>,
    conversation_id: Option<String>,
    session_source: Option<SessionSource>,
}

impl<'a> GeminiRequestBuilder<'a> {
    pub fn new(
        model: &'a str,
        instructions: &'a str,
        input: &'a [ResponseItem],
        tools: &'a [Value],
    ) -> Self {
        Self {
            model,
            instructions,
            input,
            tools,
            max_output_tokens: None,
            temperature: None,
            thinking: None,
            output_schema: None,
            conversation_id: None,
            session_source: None,
        }
    }

    /// `ModelInfo::max_output_tokens`. Optional here: unlike Anthropic's
    /// `max_tokens`, this wire defaults it when absent.
    pub fn max_output_tokens(mut self, max_output_tokens: Option<i64>) -> Self {
        self.max_output_tokens = max_output_tokens;
        self
    }

    pub fn temperature(mut self, temperature: Option<f64>) -> Self {
        self.temperature = temperature;
        self
    }

    pub fn thinking(mut self, thinking: Option<GeminiThinkingConfig>) -> Self {
        self.thinking = thinking;
        self
    }

    pub fn output_schema(mut self, schema: Option<&'a Value>) -> Self {
        self.output_schema = schema;
        self
    }

    pub fn conversation_id(mut self, id: Option<String>) -> Self {
        self.conversation_id = id;
        self
    }

    pub fn session_source(mut self, source: Option<SessionSource>) -> Self {
        self.session_source = source;
        self
    }

    pub fn build(self, _provider: &Provider) -> Result<GeminiRequest, ApiError> {
        let mut dropped = Dropped::default();
        let mut acc = RunAccumulator::default();
        // Gemini pairs a result to its call by name, so the transcript's call
        // ids have to be resolved back to names. Filled as the walk goes rather
        // than in a pre-pass: two turns can reuse a synthesized call id, and a
        // forward pass always resolves to the nearest preceding call.
        let mut call_names: HashMap<String, String> = HashMap::new();
        // A thinking signature with no summary text of its own has no part to
        // ride on; it belongs to the next model part, which is how Gemini
        // itself emits one.
        let mut pending_signature: Option<String> = None;

        for (idx, item) in self.input.iter().enumerate() {
            match item {
                ResponseItem::Message { role, content, .. } => match role.as_str() {
                    ROLE_ASSISTANT => {
                        let text = joined_text(content);
                        // Gemini rejects a part with an empty `text`.
                        if text.is_empty() {
                            continue;
                        }
                        acc.push(
                            ROLE_MODEL,
                            with_signature(json!({"text": text}), &mut pending_signature),
                        );
                    }
                    // This wire has no system role inside `contents`; only the
                    // top-level `systemInstruction` is one, and it is already
                    // spoken for by the base instructions.
                    "developer" => acc.push_all(
                        ROLE_USER,
                        system_reminder_parts(input_parts(content, &mut dropped)),
                    ),
                    _ => acc.push_all(ROLE_USER, input_parts(content, &mut dropped)),
                },
                ResponseItem::Reasoning {
                    content,
                    encrypted_content,
                    ..
                } => {
                    // Replaying a thought without its signature makes the model
                    // reject the turn on the families that check one, and a
                    // summary alone buys nothing, so an unsigned thought is
                    // dropped rather than sent as bare text.
                    let Some(signature) = encrypted_content.as_deref().filter(|s| !s.is_empty())
                    else {
                        dropped.unsigned_thinking += 1;
                        continue;
                    };
                    let text = reasoning_text(content.as_deref());
                    if text.is_empty() {
                        pending_signature = Some(signature.to_string());
                        continue;
                    }
                    acc.push(
                        ROLE_MODEL,
                        json!({"text": text, "thought": true, "thoughtSignature": signature}),
                    );
                }
                ResponseItem::FunctionCall {
                    name,
                    namespace,
                    arguments,
                    call_id,
                    // The signature the SSE layer captured from the part this
                    // call arrived on. Falling into `..` meant it was stored and
                    // never sent, so the call replayed unsigned and Gemini 3
                    // rejected the turn -- the SSE fix was correct and dead.
                    encrypted_function_args,
                    ..
                } => {
                    // `tools` advertises only the flattened spelling.
                    let name = flattened_tool_name(name, namespace.as_deref());
                    call_names.insert(call_id.clone(), name.clone());
                    acc.push(ROLE_MODEL, {
                        let part = json!({"functionCall": {
                            "name": name,
                            "args": tool_call_args(arguments, &mut dropped),
                        }});
                        // The call's own signature outranks a pending one:
                        // `pending_signature` exists for a signature that
                        // arrived with no part of its own to ride.
                        match encrypted_function_args
                            .as_ref()
                            .and_then(|args| args.first())
                            .filter(|sig| !sig.is_empty())
                        {
                            Some(signature) => {
                                pending_signature.take();
                                let mut part = part;
                                if let Some(object) = part.as_object_mut() {
                                    object.insert("thoughtSignature".to_string(), json!(signature));
                                }
                                part
                            }
                            None => with_signature(part, &mut pending_signature),
                        }
                    });
                }
                ResponseItem::CustomToolCall {
                    call_id,
                    name,
                    namespace,
                    input,
                    ..
                } => {
                    let name = flattened_tool_name(name, namespace.as_deref());
                    call_names.insert(call_id.clone(), name.clone());
                    acc.push(
                        ROLE_MODEL,
                        with_signature(
                            json!({"functionCall": {"name": name, "args": {"input": input}}}),
                            &mut pending_signature,
                        ),
                    );
                }
                ResponseItem::LocalShellCall {
                    id,
                    call_id,
                    status,
                    action,
                    ..
                } => {
                    // Either id can appear on the result, so both map onto the
                    // one name the call went out under.
                    for key in [id.as_ref().map(ToString::to_string), call_id.clone()]
                        .into_iter()
                        .flatten()
                    {
                        call_names.insert(key, LOCAL_SHELL_TOOL.to_string());
                    }
                    if id.is_none() && call_id.is_none() {
                        call_names.insert(format!("lsh_{idx}"), LOCAL_SHELL_TOOL.to_string());
                    }
                    acc.push(
                        ROLE_MODEL,
                        with_signature(
                            json!({"functionCall": {
                                "name": LOCAL_SHELL_TOOL,
                                "args": {"status": status, "action": action},
                            }}),
                            &mut pending_signature,
                        ),
                    );
                }
                ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                }
                | ResponseItem::CustomToolCallOutput {
                    call_id, output, ..
                } => {
                    // A result naming a function the model was never offered is
                    // a 400, and there is no id to fall back on here.
                    let Some(name) = call_names.get(call_id) else {
                        dropped.orphaned_tool_results += 1;
                        continue;
                    };
                    acc.push(
                        ROLE_USER,
                        json!({"functionResponse": {
                            "name": name,
                            "response": tool_result_response(output),
                        }}),
                    );
                    // `response` is a JSON object, so an image cannot ride
                    // inside it; it follows as its own part of the same turn.
                    acc.push_all(ROLE_USER, tool_result_media(output, &mut dropped));
                }
                ResponseItem::AgentMessage { content, .. } => {
                    match plaintext_agent_message_content(content) {
                        Some(text) if !text.is_empty() => acc.push(
                            ROLE_MODEL,
                            with_signature(json!({"text": text}), &mut pending_signature),
                        ),
                        Some(_) => {}
                        None => dropped.encrypted_agent_messages += 1,
                    }
                }
                // No representation on this wire.
                ResponseItem::WebSearchCall { .. }
                | ResponseItem::ToolSearchCall { .. }
                | ResponseItem::ToolSearchOutput { .. }
                | ResponseItem::ImageGenerationCall { .. }
                | ResponseItem::Compaction { .. }
                | ResponseItem::ContextCompaction { .. }
                | ResponseItem::CompactionTrigger { .. }
                | ResponseItem::AdditionalTools { .. }
                | ResponseItem::Other => {}
            }
        }

        dropped.warn();

        let contents = acc.finish();
        // An empty `contents` array is a 400.
        if contents.is_empty() {
            return Err(ApiError::Stream(
                "gemini: the transcript produced no contents to send".to_string(),
            ));
        }

        let declarations: Vec<Value> = self
            .tools
            .iter()
            .filter_map(|tool| function_declaration(tool, &mut dropped))
            .collect();
        dropped.warn_tools();

        let mut payload = Map::new();
        payload.insert("contents".to_string(), Value::Array(contents));
        if !self.instructions.is_empty() {
            payload.insert(
                "systemInstruction".to_string(),
                json!({"parts": [{"text": self.instructions}]}),
            );
        }
        if !declarations.is_empty() {
            payload.insert(
                "tools".to_string(),
                json!([{"functionDeclarations": declarations}]),
            );
            // AUTO is the wire default, but stating it keeps a provider-side
            // default flip from silently disabling tool calls.
            payload.insert(
                "toolConfig".to_string(),
                json!({"functionCallingConfig": {"mode": "AUTO"}}),
            );
        }

        let mut generation_config = Map::new();
        if let Some(temperature) = self.temperature {
            generation_config.insert("temperature".to_string(), json!(temperature));
        }
        if let Some(max_output_tokens) = self.max_output_tokens {
            generation_config.insert("maxOutputTokens".to_string(), json!(max_output_tokens));
        }
        if let Some(thinking) = self.thinking {
            let mut config = Map::new();
            if let Some(budget) = thinking.budget_tokens {
                config.insert("thinkingBudget".to_string(), json!(budget));
            }
            config.insert(
                "includeThoughts".to_string(),
                Value::Bool(thinking.include_thoughts),
            );
            generation_config.insert("thinkingConfig".to_string(), Value::Object(config));
        }
        if let Some(schema) = self.output_schema {
            // The schema alone is ignored: structured output is gated on the
            // response mime type.
            generation_config.insert("responseMimeType".to_string(), json!("application/json"));
            generation_config.insert("responseSchema".to_string(), sanitize_schema(schema));
        }
        if !generation_config.is_empty() {
            payload.insert(
                "generationConfig".to_string(),
                Value::Object(generation_config),
            );
        }

        // The API key rides in `x-goog-api-key`, which the auth provider
        // attaches; a key spelled here would bypass every auth mode.
        let mut headers = build_session_headers(self.conversation_id, /*thread_id*/ None);
        if let Some(subagent) = subagent_header(&self.session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }

        Ok(GeminiRequest {
            model: self.model.to_string(),
            body: Value::Object(payload),
            headers,
        })
    }
}

/// The name a local shell call goes out under, since this wire carries no
/// dedicated shell tool.
const LOCAL_SHELL_TOOL: &str = "local_shell";

/// Groups consecutive parts that share a role into one `contents` entry.
///
/// Sending them separately reads as turns that never happened, and a
/// `functionResponse` in a turn of its own loses its attachment to the call.
#[derive(Default)]
struct RunAccumulator {
    contents: Vec<Value>,
    role: Option<&'static str>,
    parts: Vec<Value>,
}

impl RunAccumulator {
    fn push(&mut self, role: &'static str, part: Value) {
        if self.role != Some(role) {
            self.flush();
            self.role = Some(role);
        }
        self.parts.push(part);
    }

    fn push_all(&mut self, role: &'static str, parts: Vec<Value>) {
        for part in parts {
            self.push(role, part);
        }
    }

    fn flush(&mut self) {
        let Some(role) = self.role.take() else {
            return;
        };
        // Gemini rejects a `contents` entry with an empty `parts` array.
        if self.parts.is_empty() {
            return;
        }
        let parts = std::mem::take(&mut self.parts);
        self.contents.push(json!({"role": role, "parts": parts}));
    }

    fn finish(mut self) -> Vec<Value> {
        self.flush();
        self.contents
    }
}

/// Attaches a held thinking signature to the next model part.
fn with_signature(mut part: Value, pending: &mut Option<String>) -> Value {
    if let Some(signature) = pending.take()
        && let Some(object) = part.as_object_mut()
    {
        object.insert("thoughtSignature".to_string(), json!(signature));
    }
    part
}

/// Content this wire cannot carry, counted for one warning per kind per
/// request.
#[derive(Default)]
struct Dropped {
    unsigned_thinking: usize,
    audio: usize,
    unparsable_tool_arguments: usize,
    encrypted_agent_messages: usize,
    orphaned_tool_results: usize,
    unusable_tools: usize,
}

impl Dropped {
    fn warn(&self) {
        if self.unsigned_thinking > 0 {
            warn!(
                "gemini: dropped {} thinking block(s) with no signature",
                self.unsigned_thinking
            );
        }
        if self.audio > 0 {
            warn!(
                "gemini: replaced {} audio item(s) with a placeholder; this wire has no audio part",
                self.audio
            );
        }
        if self.unparsable_tool_arguments > 0 {
            warn!(
                "gemini: sent {} tool call(s) with empty args; their arguments were not a JSON object",
                self.unparsable_tool_arguments
            );
        }
        if self.encrypted_agent_messages > 0 {
            warn!(
                "gemini: dropped {} encrypted agent message(s)",
                self.encrypted_agent_messages
            );
        }
        if self.orphaned_tool_results > 0 {
            warn!(
                "gemini: dropped {} tool result(s) with no preceding call; this wire pairs them by function name",
                self.orphaned_tool_results
            );
        }
    }

    /// Separate from [`Self::warn`] because tools are read after the transcript
    /// walk has already reported its own drops.
    fn warn_tools(&self) {
        if self.unusable_tools > 0 {
            warn!(
                "gemini: dropped {} tool definition(s) with no readable name",
                self.unusable_tools
            );
        }
    }
}

fn joined_text(content: &[ContentItem]) -> String {
    content.iter().fold(String::new(), |mut acc, item| {
        if let ContentItem::InputText { text } | ContentItem::OutputText { text } = item {
            acc.push_str(text);
        }
        acc
    })
}

fn reasoning_text(content: Option<&[ReasoningItemContent]>) -> String {
    content
        .unwrap_or_default()
        .iter()
        .fold(String::new(), |mut acc, part| {
            match part {
                ReasoningItemContent::ReasoningText { text }
                | ReasoningItemContent::Text { text } => acc.push_str(text),
            }
            acc
        })
}

fn input_parts(content: &[ContentItem], dropped: &mut Dropped) -> Vec<Value> {
    let mut parts = Vec::with_capacity(content.len());
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                // Gemini rejects a part with an empty `text`.
                if !text.is_empty() {
                    parts.push(json!({"text": text}));
                }
            }
            ContentItem::InputImage { image_url, .. } => parts.push(image_part(image_url)),
            ContentItem::InputAudio { .. } => {
                dropped.audio += 1;
                parts.push(json!({"text": AUDIO_PLACEHOLDER}));
            }
        }
    }
    parts
}

/// `inlineData` is the only image part that takes bytes. The `fileData`
/// alternative addresses the Files API, not arbitrary URLs, so a remote image
/// degrades to text rather than a 400 on a URI the backend cannot fetch.
fn image_part(image_url: &str) -> Value {
    if !image_url.starts_with("data:") {
        return json!({"text": format!("[image omitted: {image_url} is not inline data]")});
    }

    match load_data_url_for_prompt(image_url, PromptImageMode::ResizeToFit) {
        Ok(image) => json!({
            "inlineData": {
                "mimeType": image.mime,
                "data": BASE64_STANDARD.encode(image.bytes.as_ref()),
            }
        }),
        Err(err) => json!({"text": format!("[image omitted: {err}]")}),
    }
}

/// Marks folded-in developer text so the model can tell it from something the
/// user typed, matching what the sibling wires do with the same items.
fn system_reminder_parts(parts: Vec<Value>) -> Vec<Value> {
    parts
        .into_iter()
        .map(|part| {
            let wrapped = part.get("text").and_then(Value::as_str).map(
                |text| json!({"text": format!("<system-reminder>\n{text}\n</system-reminder>")}),
            );
            wrapped.unwrap_or(part)
        })
        .collect()
}

/// Gemini takes `args` as a JSON object, not the Responses string.
fn tool_call_args(arguments: &str, dropped: &mut Dropped) -> Value {
    if arguments.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(args)) => Value::Object(args),
        _ => {
            dropped.unparsable_tool_arguments += 1;
            json!({})
        }
    }
}

/// `functionResponse.response` must be a JSON object, so the tool's text is
/// wrapped in one. A failed call reports under `error` because that is the key
/// the model is trained to read as a failure.
fn tool_result_response(output: &FunctionCallOutputPayload) -> Value {
    let mut text = String::new();
    if let Some(items) = output.content_items() {
        for item in items {
            match item {
                FunctionCallOutputContentItem::InputText { text: item } => text.push_str(item),
                // Carried as their own parts, or unrepresentable here.
                FunctionCallOutputContentItem::InputImage { .. }
                | FunctionCallOutputContentItem::InputAudio { .. }
                | FunctionCallOutputContentItem::EncryptedContent { .. } => {}
            }
        }
    } else if let Some(item) = output.text_content() {
        text.push_str(item);
    }

    if text.is_empty() {
        text.push_str(EMPTY_TOOL_RESULT);
    }

    if output.success == Some(false) {
        json!({"error": text})
    } else {
        json!({"output": text})
    }
}

/// Image and audio items of a tool result, which cannot ride inside the
/// `response` object.
fn tool_result_media(output: &FunctionCallOutputPayload, dropped: &mut Dropped) -> Vec<Value> {
    let mut parts = Vec::new();
    for item in output.content_items().unwrap_or_default() {
        match item {
            FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                parts.push(image_part(image_url));
            }
            FunctionCallOutputContentItem::InputAudio { .. } => {
                dropped.audio += 1;
                parts.push(json!({"text": AUDIO_PLACEHOLDER}));
            }
            FunctionCallOutputContentItem::InputText { .. }
            | FunctionCallOutputContentItem::EncryptedContent { .. } => {}
        }
    }
    parts
}

/// Reads one tool definition in whichever shape the tools encoder produced.
///
/// The encoder is shared across wires: Chat nests the tool under `function`,
/// Responses spells it flat with `parameters`, and Anthropic spells the schema
/// `input_schema`. Accepting all three keeps this file working whichever one
/// the Gemini glue ends up calling; a definition with no name is dropped,
/// because a declaration the model cannot call is a 400.
fn function_declaration(tool: &Value, dropped: &mut Dropped) -> Option<Value> {
    let body = tool.get("function").unwrap_or(tool);
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty());
    let Some(name) = name else {
        dropped.unusable_tools += 1;
        return None;
    };

    let mut declaration = Map::new();
    declaration.insert("name".to_string(), json!(name));
    if let Some(description) = body.get("description").and_then(Value::as_str) {
        declaration.insert("description".to_string(), json!(description));
    }
    if let Some(schema) = body
        .get("parameters")
        .or_else(|| body.get("input_schema"))
        .filter(|schema| schema.is_object())
    {
        declaration.insert("parameters".to_string(), sanitize_schema(schema));
    }
    Some(Value::Object(declaration))
}

/// JSON Schema keywords Gemini rejects outright.
///
/// `parameters` and `responseSchema` are not validated as JSON Schema: they are
/// coerced into an OpenAPI `Schema`, where an unknown field is a hard
/// `400 INVALID_ARGUMENT` rather than a warning. `additionalProperties` rides on
/// every strict schema and `$schema` on most MCP-authored ones, and Gemini
/// neither enforces nor needs either, so dropping them loses nothing the model
/// would have honoured. A stripped key costs at worst a retry; a forwarded one
/// costs the turn.
///
/// Tool schemas are already cleaned by the Gemini tools encoder; this pass
/// backstops the schemas that never pass through it, above all the
/// user-supplied `output_schema`.
// * `additionalProperties` — the freeform-tool schema this crate generates sets
///   it to `false`, and OpenAI-style strict schemas set it everywhere.
// * `$schema` — MCP servers routinely emit a draft declaration at the root of
///   an `inputSchema`, which reaches here verbatim.
// * `oneOf`, `allOf`, `not`, `if`, `then`, `else` — Google documents these as
///   unsupported ("Don't use if, then, allOf, oneOf, or not"). `JsonSchema` has
///   dedicated fields for `oneOf` and `allOf`, so they serialize by default.
// * `$defs`, `definitions`, `$ref` — a `$ref` Gemini cannot resolve is worse
///   than an absent constraint, and the definition table is dead weight beside
///   it. Dropping the pointer leaves the property unconstrained, which the model
///   tolerates; forwarding it is a 400.
// * `encrypted` — not in Gemini's `Schema` proto at all. This one is NOT a
///   third-party edge case: multi_agents_spec.rs marks the `message` parameter
///   of send_message, followup_task and send_input with it, so with multi-agent
///   tools enabled EVERY turn would 400. One bad key fails the whole `tools`
///   payload, not the single tool carrying it.
// * `default`, `optional`, `maximum` — also documented as unsupported.
///
/// Structurally meaningless to Gemini, so dropping them loses nothing the model
/// would have honoured.
const UNSUPPORTED_SCHEMA_KEYS: [&str; 14] = [
    "additionalProperties",
    "$schema",
    "$defs",
    "definitions",
    "$ref",
    "oneOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
    "encrypted",
    "default",
    "optional",
];

/// Strips [`UNSUPPORTED_SCHEMA_KEYS`] everywhere they can appear.
///
/// Recursion is required, not tidiness: `additionalProperties: false` most often
/// sits on a nested property, and one surviving instance fails the whole
/// request. Arrays are walked too, because a schema also hides under `anyOf`.
fn sanitize_schema(schema: &Value) -> Value {
    let mut schema = schema.clone();
    sanitize_for_gemini(&mut schema);
    schema
}

/// Inlines `$ref` pointers against the schema's own `$defs`/`definitions` table
/// before anything is stripped.
///
/// Deleting a `$ref` outright leaves the property as `{}` -- and if it was in
/// `required`, the tool advertises a mandatory argument with no type at all; a
/// top-level `$ref` erases the parameter list entirely and the model calls the
/// tool with `{}`. That trades a loud 400 for a silently wrong schema, which is
/// the worse failure. `$ref` is also the normal output of pydantic- and
/// zod-authored MCP `inputSchema`, so this is the common shape, not an edge.
///
/// Only local `#/$defs/...` and `#/definitions/...` pointers resolve; anything
/// remote or unresolvable is left for the stripper, because a dangling pointer
/// really is worse than an absent constraint. Recursion is depth-bounded: a
/// self-referential schema would otherwise inline forever.
fn inline_local_refs(value: &mut Value, defs: &Value, depth: usize) {
    const MAX_DEPTH: usize = 8;
    if depth > MAX_DEPTH {
        return;
    }
    match value {
        Value::Object(map) => {
            if let Some(Value::String(pointer)) = map.get("$ref") {
                let resolved = pointer
                    .strip_prefix("#/$defs/")
                    .or_else(|| pointer.strip_prefix("#/definitions/"))
                    .and_then(|name| defs.get(name))
                    .cloned();
                if let Some(mut resolved) = resolved {
                    inline_local_refs(&mut resolved, defs, depth + 1);
                    // Sibling keys of a $ref win: JSON Schema 2020-12 allows them
                    // and they are the caller's own annotations.
                    if let Some(target) = resolved.as_object() {
                        for (key, nested) in target {
                            map.entry(key.clone()).or_insert_with(|| nested.clone());
                        }
                    }
                    map.remove("$ref");
                }
            }
            for nested in map.values_mut() {
                inline_local_refs(nested, defs, depth + 1);
            }
        }
        Value::Array(items) => {
            for item in items {
                inline_local_refs(item, defs, depth + 1);
            }
        }
        _ => {}
    }
}

/// Resolves local `$ref`s, then removes the keys Gemini rejects.
fn sanitize_for_gemini(value: &mut Value) {
    let defs = value
        .get("$defs")
        .or_else(|| value.get("definitions"))
        .cloned()
        .unwrap_or(Value::Null);
    if !defs.is_null() {
        inline_local_refs(value, &defs, 0);
    }
    strip_unsupported_schema_keys(value);
}

fn strip_unsupported_schema_keys(value: &mut Value) {
    match value {
        Value::Object(schema) => {
            for key in UNSUPPORTED_SCHEMA_KEYS {
                schema.remove(key);
            }
            for nested in schema.values_mut() {
                strip_unsupported_schema_keys(nested);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_unsupported_schema_keys(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "gemini_tests.rs"]
mod tests;
