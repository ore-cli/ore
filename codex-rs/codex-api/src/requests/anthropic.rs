//! Request builder for the Anthropic Messages API.
//!
//! Converts the Responses-shaped `ResponseItem` transcript into Anthropic
//! `messages[]`. This wire carries an in-sequence `thinking` block and has no
//! `tool` role: tool results ride as blocks inside a user message, and runs of
//! same-role blocks accumulate into one message.

use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::headers::build_session_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_protocol::ResponseItemId;
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
use std::collections::HashSet;
use tracing::warn;

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `max_tokens` is required on the wire; callers override this floor from
/// `ModelInfo::max_output_tokens`.
const DEFAULT_MAX_TOKENS: i64 = 4096;

/// Stands in for audio input, which this wire has no block for.
const AUDIO_PLACEHOLDER: &str = "[audio input omitted: unsupported by the Anthropic Messages API]";

/// Anthropic rejects a `tool_result` whose content is empty.
const EMPTY_TOOL_RESULT: &str = "(no output)";

const ROLE_USER: &str = "user";
const ROLE_ASSISTANT: &str = "assistant";
const ROLE_SYSTEM: &str = "system";

/// Assembled request body plus headers for Anthropic Messages streaming calls.
pub struct AnthropicRequest {
    pub body: Value,
    pub headers: HeaderMap,
}

pub struct AnthropicRequestBuilder<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a [ResponseItem],
    tools: &'a [Value],
    max_tokens: i64,
    effort: Option<&'a str>,
    thinking_enabled: bool,
    supports_inline_system: bool,
    output_schema: Option<&'a Value>,
    conversation_id: Option<String>,
    session_source: Option<SessionSource>,
    cache_policy: Option<AnthropicCachePolicy>,
}

impl<'a> AnthropicRequestBuilder<'a> {
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
            max_tokens: DEFAULT_MAX_TOKENS,
            effort: None,
            thinking_enabled: false,
            supports_inline_system: false,
            output_schema: None,
            conversation_id: None,
            session_source: None,
            cache_policy: None,
        }
    }

    pub fn max_tokens(mut self, max_tokens: i64) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Already clamped to a spelling this wire accepts.
    pub fn effort(mut self, effort: Option<&'a str>) -> Self {
        self.effort = effort;
        self
    }

    pub fn thinking_enabled(mut self, enabled: bool) -> Self {
        self.thinking_enabled = enabled;
        self
    }

    /// `ModelInfo::supports_mid_conversation_system`.
    pub fn supports_inline_system(mut self, supported: bool) -> Self {
        self.supports_inline_system = supported;
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

    /// Breakpoint indices are computed from the assembled body.
    pub fn cache_policy(mut self, policy: Option<AnthropicCachePolicy>) -> Self {
        self.cache_policy = policy;
        self
    }

    pub fn build(self, _provider: &Provider) -> Result<AnthropicRequest, ApiError> {
        let mut dropped = Dropped::default();
        let local_shell_ids = local_shell_tool_use_ids(self.input);
        let mut acc = RunAccumulator::default();
        let mut last_assistant_text: Option<String> = None;

        for (idx, item) in self.input.iter().enumerate() {
            // Only an immediately repeated assistant text is a duplicate.
            if !matches!(item, ResponseItem::Message { role, .. } if role == ROLE_ASSISTANT) {
                last_assistant_text = None;
            }
            match item {
                ResponseItem::Message { role, content, .. } => match role.as_str() {
                    ROLE_ASSISTANT => {
                        let text = joined_text(content);
                        // Anthropic rejects an empty content array.
                        if text.is_empty() {
                            continue;
                        }
                        if last_assistant_text.as_ref() == Some(&text) {
                            continue;
                        }
                        last_assistant_text = Some(text.clone());
                        acc.push(ROLE_ASSISTANT, json!({"type": "text", "text": text}));
                    }
                    "developer" => {
                        let blocks = input_blocks(content, &mut dropped);
                        if self.supports_inline_system {
                            acc.push_all(ROLE_SYSTEM, blocks);
                        } else {
                            acc.push_all(ROLE_USER, system_reminder_blocks(blocks));
                        }
                    }
                    _ => acc.push_all(ROLE_USER, input_blocks(content, &mut dropped)),
                },
                ResponseItem::Reasoning {
                    content,
                    encrypted_content,
                    ..
                } => {
                    // Thinking text is routinely empty (`display` defaults to
                    // "omitted"); only the signature is required.
                    let Some(signature) = encrypted_content.as_deref().filter(|s| !s.is_empty())
                    else {
                        dropped.unsigned_thinking += 1;
                        continue;
                    };
                    let text = content
                        .iter()
                        .flatten()
                        .fold(String::new(), |mut acc, part| {
                            match part {
                                ReasoningItemContent::ReasoningText { text }
                                | ReasoningItemContent::Text { text } => acc.push_str(text),
                            }
                            acc
                        });
                    acc.push(
                        ROLE_ASSISTANT,
                        json!({"type": "thinking", "thinking": text, "signature": signature}),
                    );
                }
                ResponseItem::FunctionCall {
                    name,
                    namespace,
                    arguments,
                    call_id,
                    ..
                } => acc.push(
                    ROLE_ASSISTANT,
                    json!({
                        "type": "tool_use",
                        "id": call_id,
                        // `tools` advertises only the flattened spelling.
                        "name": crate::requests::chat::flattened_tool_name(name, namespace.as_deref()),
                        "input": tool_use_input(arguments, &mut dropped),
                    }),
                ),
                ResponseItem::CustomToolCall {
                    call_id,
                    name,
                    namespace,
                    input,
                    ..
                } => acc.push(
                    ROLE_ASSISTANT,
                    json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": crate::requests::chat::flattened_tool_name(name, namespace.as_deref()),
                        "input": {"input": input},
                    }),
                ),
                ResponseItem::LocalShellCall {
                    id,
                    call_id,
                    status,
                    action,
                    ..
                } => acc.push(
                    ROLE_ASSISTANT,
                    json!({
                        "type": "tool_use",
                        "id": local_shell_call_id(id.as_ref(), call_id.as_deref(), idx),
                        "name": "local_shell",
                        "input": {"status": status, "action": action},
                    }),
                ),
                ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                }
                | ResponseItem::CustomToolCallOutput {
                    call_id, output, ..
                } => {
                    let tool_use_id = local_shell_ids.get(call_id).unwrap_or(call_id);
                    let mut block = json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": tool_result_content(output, &mut dropped),
                    });
                    if output.success == Some(false)
                        && let Some(obj) = block.as_object_mut()
                    {
                        obj.insert("is_error".to_string(), Value::Bool(true));
                    }
                    acc.push(ROLE_USER, block);
                }
                ResponseItem::AgentMessage { content, .. } => {
                    match plaintext_agent_message_content(content) {
                        Some(text) => {
                            acc.push(ROLE_ASSISTANT, json!({"type": "text", "text": text}))
                        }
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

        // Order is load-bearing: legalizing folds a system message into a user
        // turn, which can leave two user runs adjacent for coalescing to merge.
        let mut messages = acc.finish();
        legalize_system_messages(&mut messages);
        coalesce_adjacent_user_messages(&mut messages);
        // Dropping a leading model turn orphans the tool_result that answered
        // it, and pruning that orphan can expose another model turn.
        loop {
            let before = messages.len();
            drop_leading_non_user_messages(&mut messages, &mut dropped);
            repair_tool_pairing(&mut messages);
            if messages.len() == before {
                break;
            }
        }

        let mut tools = self.tools.to_vec();
        let mut system = Vec::new();
        if !self.instructions.is_empty() {
            system.push(json!({"type": "text", "text": self.instructions}));
        }
        enforce_block_order(&mut messages);

        // An empty `messages` array is a 400.
        if messages.is_empty() {
            return Err(ApiError::Stream(
                "anthropic: the transcript produced no messages to send".to_string(),
            ));
        }

        let breakpoints = self
            .cache_policy
            .map(|policy| policy.breakpoints(&tools, &system, &messages))
            .unwrap_or_default();
        attach_cache_control(&breakpoints, &mut tools, &mut system, &mut messages);

        let mut payload = Map::new();
        payload.insert("model".to_string(), json!(self.model));
        payload.insert("max_tokens".to_string(), json!(self.max_tokens));
        if !system.is_empty() {
            payload.insert("system".to_string(), Value::Array(system));
        }
        payload.insert("messages".to_string(), Value::Array(messages));
        payload.insert("tools".to_string(), Value::Array(tools));
        payload.insert("stream".to_string(), Value::Bool(true));
        if self.thinking_enabled {
            // The wire default `display` of "omitted" streams a signature with
            // no text.
            payload.insert(
                "thinking".to_string(),
                json!({"type": "adaptive", "display": "summarized"}),
            );
        }

        let mut output_config = Map::new();
        if let Some(effort) = self.effort {
            output_config.insert("effort".to_string(), json!(effort));
        }
        if let Some(schema) = self.output_schema {
            output_config.insert(
                "format".to_string(),
                json!({"type": "json_schema", "schema": schema}),
            );
        }
        if !output_config.is_empty() {
            payload.insert("output_config".to_string(), Value::Object(output_config));
        }

        let mut headers = build_session_headers(self.conversation_id, /*thread_id*/ None);
        if let Some(subagent) = subagent_header(&self.session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }
        insert_header(&mut headers, "anthropic-version", ANTHROPIC_VERSION);

        Ok(AnthropicRequest {
            body: Value::Object(payload),
            headers,
        })
    }
}

/// Position of one content block in the flat wire order: tools, then system,
/// then every block of every message.
///
/// `message: None` marks a tools or system block; `block` is then its index
/// within that combined prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockCoord {
    pub message: Option<usize>,
    pub block: usize,
}

/// Enumerates the flat block sequence of an assembled body.
pub fn flat_block_layout(body: &Value) -> Vec<BlockCoord> {
    let prefix = array_at(body, "tools").len() + array_at(body, "system").len();
    let mut coords: Vec<BlockCoord> = (0..prefix)
        .map(|block| BlockCoord {
            message: None,
            block,
        })
        .collect();

    for (message, value) in array_at(body, "messages").iter().enumerate() {
        let blocks = value
            .get("content")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        coords.extend((0..blocks).map(|block| BlockCoord {
            message: Some(message),
            block,
        }));
    }

    coords
}

/// Groups consecutive blocks that share a role into one message.
#[derive(Default)]
struct RunAccumulator {
    messages: Vec<Value>,
    role: Option<&'static str>,
    blocks: Vec<Value>,
}

impl RunAccumulator {
    fn push(&mut self, role: &'static str, block: Value) {
        if self.role != Some(role) {
            self.flush();
            self.role = Some(role);
        }
        self.blocks.push(block);
    }

    fn push_all(&mut self, role: &'static str, blocks: Vec<Value>) {
        for block in blocks {
            self.push(role, block);
        }
    }

    fn flush(&mut self) {
        let Some(role) = self.role.take() else {
            return;
        };
        if self.blocks.is_empty() {
            return;
        }
        let blocks = std::mem::take(&mut self.blocks);
        self.messages.push(json!({"role": role, "content": blocks}));
    }

    fn finish(mut self) -> Vec<Value> {
        self.flush();
        self.messages
    }
}

/// Blocks whose params include `cache_control`.
fn block_accepts_cache_control(block: &Value) -> bool {
    matches!(
        block_type(block),
        "text" | "image" | "document" | "tool_use" | "tool_result"
    )
}

/// Restores the block order the API requires, after every pass that can
/// reorder or merge content.
fn enforce_block_order(messages: &mut [Value]) {
    for message in messages {
        if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            blocks.sort_by_key(leading_block_rank);
        }
    }
}

/// Blocks the API requires to lead their message sort first.
fn leading_block_rank(block: &Value) -> u8 {
    match block_type(block) {
        "thinking" | "redacted_thinking" | "tool_result" => 0,
        _ => 1,
    }
}

/// Content this wire cannot carry, counted for one warning per kind per
/// request.
#[derive(Default)]
struct Dropped {
    unsigned_thinking: usize,
    audio: usize,
    unparsable_tool_arguments: usize,
    encrypted_agent_messages: usize,
    leading_non_user_messages: usize,
}

impl Dropped {
    fn warn(&self) {
        if self.unsigned_thinking > 0 {
            warn!(
                "anthropic: dropped {} thinking block(s) with no signature",
                self.unsigned_thinking
            );
        }
        if self.audio > 0 {
            warn!(
                "anthropic: replaced {} audio item(s) with a placeholder; this wire has no audio block",
                self.audio
            );
        }
        if self.unparsable_tool_arguments > 0 {
            warn!(
                "anthropic: sent {} tool call(s) with empty input; their arguments were not a JSON object",
                self.unparsable_tool_arguments
            );
        }
        if self.encrypted_agent_messages > 0 {
            warn!(
                "anthropic: dropped {} encrypted agent message(s)",
                self.encrypted_agent_messages
            );
        }
        if self.leading_non_user_messages > 0 {
            warn!(
                "anthropic: dropped {} leading message(s); this wire requires the transcript to open with a user turn",
                self.leading_non_user_messages
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

fn input_blocks(content: &[ContentItem], dropped: &mut Dropped) -> Vec<Value> {
    let mut blocks = Vec::with_capacity(content.len());
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                // Anthropic rejects an empty-string text block.
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
            }
            ContentItem::InputImage { image_url, .. } => blocks.push(image_block(image_url)),
            ContentItem::InputAudio { .. } => {
                dropped.audio += 1;
                blocks.push(json!({"type": "text", "text": AUDIO_PLACEHOLDER}));
            }
        }
    }
    blocks
}

fn image_block(image_url: &str) -> Value {
    // The loader errors on anything that is not a data url.
    if !image_url.starts_with("data:") {
        return json!({"type": "image", "source": {"type": "url", "url": image_url}});
    }

    match load_data_url_for_prompt(image_url, PromptImageMode::ResizeToFit) {
        Ok(image) => json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": image.mime,
                "data": BASE64_STANDARD.encode(image.bytes.as_ref()),
            }
        }),
        // A data url cannot ride in a `url` source.
        Err(err) => json!({"type": "text", "text": format!("[image omitted: {err}]")}),
    }
}

fn system_reminder_blocks(blocks: Vec<Value>) -> Vec<Value> {
    blocks
        .into_iter()
        .map(|block| {
            let wrapped = (block_type(&block) == "text")
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
                .map(|text| {
                    json!({"type": "text", "text": format!("<system-reminder>\n{text}\n</system-reminder>")})
                });
            wrapped.unwrap_or(block)
        })
        .collect()
}

fn tool_use_input(arguments: &str, dropped: &mut Dropped) -> Value {
    if arguments.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(input)) => Value::Object(input),
        // Anthropic rejects a non-object `input`.
        _ => {
            dropped.unparsable_tool_arguments += 1;
            json!({})
        }
    }
}

fn tool_result_content(output: &FunctionCallOutputPayload, dropped: &mut Dropped) -> Vec<Value> {
    let mut blocks = Vec::new();
    if let Some(items) = output.content_items() {
        for item in items {
            match item {
                FunctionCallOutputContentItem::InputText { text } => {
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                    blocks.push(image_block(image_url));
                }
                FunctionCallOutputContentItem::InputAudio { .. } => {
                    dropped.audio += 1;
                    blocks.push(json!({"type": "text", "text": AUDIO_PLACEHOLDER}));
                }
                // Responses-only; opaque here.
                FunctionCallOutputContentItem::EncryptedContent { .. } => {}
            }
        }
    } else if let Some(text) = output.text_content().filter(|text| !text.is_empty()) {
        blocks.push(json!({"type": "text", "text": text}));
    }

    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": EMPTY_TOOL_RESULT}));
    }
    blocks
}

fn local_shell_call_id(id: Option<&ResponseItemId>, call_id: Option<&str>, idx: usize) -> String {
    id.map(ToString::to_string)
        .or_else(|| call_id.map(str::to_string))
        .unwrap_or_else(|| format!("lsh_{idx}"))
}

/// Maps every id a local-shell result could pair against onto the id its
/// `tool_use` block carries.
fn local_shell_tool_use_ids(input: &[ResponseItem]) -> HashMap<String, String> {
    let mut ids = HashMap::new();
    for (idx, item) in input.iter().enumerate() {
        let ResponseItem::LocalShellCall { id, call_id, .. } = item else {
            continue;
        };
        let chosen = local_shell_call_id(id.as_ref(), call_id.as_deref(), idx);
        if let Some(call_id) = call_id {
            ids.insert(call_id.clone(), chosen.clone());
        }
        if let Some(id) = id {
            ids.insert(id.to_string(), chosen.clone());
        }
    }
    ids
}

/// Folds every system message that is illegal where it sits into the adjacent
/// user turn. Legality depends on both neighbours, so it is settled after the
/// runs are built.
fn legalize_system_messages(messages: &mut Vec<Value>) {
    let mut idx = 0;
    while idx < messages.len() {
        if role_of(&messages[idx]) != ROLE_SYSTEM || system_position_is_legal(messages, idx) {
            idx += 1;
            continue;
        }

        let folded = system_reminder_blocks(take_content_blocks(messages.remove(idx)));
        if folded.is_empty() {
            continue;
        }

        if idx > 0 && role_of(&messages[idx - 1]) == ROLE_USER {
            extend_content(&mut messages[idx - 1], folded);
        } else if idx < messages.len() && role_of(&messages[idx]) == ROLE_USER {
            prepend_content(&mut messages[idx], folded);
        } else {
            messages.insert(idx, json!({"role": ROLE_USER, "content": folded}));
            idx += 1;
        }
    }
}

fn system_position_is_legal(messages: &[Value], idx: usize) -> bool {
    if idx == 0 || role_of(&messages[idx - 1]) != ROLE_USER {
        return false;
    }
    idx + 1 == messages.len() || role_of(&messages[idx + 1]) == ROLE_ASSISTANT
}

/// Merges adjacent `user` messages: sent separately, they read as a turn that
/// never happened between them.
fn coalesce_adjacent_user_messages(messages: &mut Vec<Value>) {
    let mut idx = 1;
    while idx < messages.len() {
        if role_of(&messages[idx - 1]) != ROLE_USER || role_of(&messages[idx]) != ROLE_USER {
            idx += 1;
            continue;
        }
        let next = messages.remove(idx);
        extend_content(&mut messages[idx - 1], take_content_blocks(next));
    }
}

/// Where to place `cache_control` breakpoints in an assembled request.
///
/// Anthropic allows at most four, matches them as a strict prefix, and silently
/// ignores one whose prefix is under the model's minimum while still spending
/// the slot.
#[derive(Debug, Clone, Copy)]
pub struct AnthropicCachePolicy {
    /// `ModelInfo::cache_min_prefix_tokens`.
    pub min_prefix_tokens: i64,
}

/// Anthropic caps breakpoints at four.
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Rough bytes-per-token, matching the estimate core uses for context budgeting.
const BYTES_PER_TOKEN: usize = 4;

impl AnthropicCachePolicy {
    /// Returns flat block indices, ascending.
    ///
    /// The four anchors, in prefix order: the end of `tools`, the end of
    /// `system`, the last block of the previous real user turn, and the last
    /// block overall.
    fn breakpoints(self, tools: &[Value], system: &[Value], messages: &[Value]) -> Vec<usize> {
        let mut sizes = Vec::new();
        for block in tools.iter().chain(system.iter()) {
            sizes.push(block.to_string().len());
        }
        let prefix_blocks = sizes.len();

        let mut message_end = Vec::new();
        let mut user_turn_end = Vec::new();
        for message in messages {
            let blocks = message
                .get("content")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice);
            if blocks.is_empty() {
                continue;
            }
            for block in blocks {
                sizes.push(block.to_string().len());
            }
            // A thinking block does not accept cache_control; anchor on the
            // last block that does.
            let cacheable = blocks
                .iter()
                .rposition(block_accepts_cache_control)
                .map(|offset| sizes.len() - blocks.len() + offset);
            let Some(end) = cacheable else {
                continue;
            };
            message_end.push(end);
            // A user message carrying tool_result blocks is the tail of the
            // previous turn.
            let is_real_user_turn = message.get("role").and_then(Value::as_str) == Some(ROLE_USER)
                && !blocks
                    .iter()
                    .any(|block| block_type(block) == "tool_result");
            if is_real_user_turn {
                user_turn_end.push(end);
            }
        }

        let mut anchors = Vec::new();
        if prefix_blocks > 0 {
            if !tools.is_empty() {
                anchors.push(tools.len() - 1);
            }
            anchors.push(prefix_blocks - 1);
        }
        // Second-most-recent, so it stays put while the current turn grows.
        if user_turn_end.len() >= 2 {
            anchors.push(user_turn_end[user_turn_end.len() - 2]);
        }
        if let Some(last) = message_end.last() {
            anchors.push(*last);
        }

        anchors.sort_unstable();
        anchors.dedup();

        let mut running = 0usize;
        let mut cumulative = Vec::with_capacity(sizes.len());
        for size in &sizes {
            running += size;
            cumulative.push(running);
        }

        // Below the model's minimum a breakpoint is silently ignored but still
        // spends a slot. The margin covers the undercount on JSON tool schemas.
        let required_bytes = self
            .min_prefix_tokens
            .max(0)
            .saturating_mul(BYTES_PER_TOKEN as i64)
            .saturating_mul(5)
            / 4;
        let mut placed: Vec<usize> = anchors
            .into_iter()
            .filter(|index| cumulative.get(*index).copied().unwrap_or(0) as i64 >= required_bytes)
            .collect();

        // A later breakpoint's prefix covers the earlier ones.
        if placed.len() > MAX_CACHE_BREAKPOINTS {
            placed.drain(..placed.len() - MAX_CACHE_BREAKPOINTS);
        }
        placed
    }
}

/// The Messages API requires the first message to be `user`; a resumed thread
/// can begin with an assistant turn.
fn drop_leading_non_user_messages(messages: &mut Vec<Value>, dropped: &mut Dropped) {
    let first_user = messages
        .iter()
        .position(|message| role_of(message) == ROLE_USER);
    match first_user {
        Some(0) => {}
        Some(index) => {
            dropped.leading_non_user_messages += index;
            messages.drain(..index);
        }
        None => {
            dropped.leading_non_user_messages += messages.len();
            messages.clear();
        }
    }
}

fn attach_cache_control(
    breakpoints: &[usize],
    tools: &mut [Value],
    system: &mut [Value],
    messages: &mut [Value],
) {
    if breakpoints.is_empty() {
        return;
    }

    // A later breakpoint's prefix covers the earlier ones.
    let mut ordered: Vec<usize> = breakpoints.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    if ordered.len() > MAX_CACHE_BREAKPOINTS {
        ordered.drain(..ordered.len() - MAX_CACHE_BREAKPOINTS);
    }
    let wanted: HashSet<usize> = ordered.into_iter().collect();
    let mut flat = 0usize;
    let mut mark = |block: &mut Value| {
        let index = flat;
        flat += 1;
        if wanted.contains(&index)
            && let Some(obj) = block.as_object_mut()
        {
            obj.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
        }
    };

    for block in tools.iter_mut().chain(system.iter_mut()) {
        mark(block);
    }
    for message in messages {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in blocks {
            mark(block);
        }
    }
}

/// Drops `tool_use` blocks nothing answers and `tool_result` blocks answering
/// nothing; either one is a 400.
fn repair_tool_pairing(messages: &mut Vec<Value>) {
    let ids = |message: &Value, block: &str, key: &str| -> HashSet<String> {
        array_at(message, "content")
            .iter()
            .filter(|value| block_type(value) == block)
            .filter_map(|value| value.get(key).and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    };

    let calls: Vec<HashSet<String>> = messages
        .iter()
        .map(|message| ids(message, "tool_use", "id"))
        .collect();
    let results: Vec<HashSet<String>> = messages
        .iter()
        .map(|message| ids(message, "tool_result", "tool_use_id"))
        .collect();

    let mut dropped = Vec::new();
    for (idx, message) in messages.iter_mut().enumerate() {
        let answered = results.get(idx + 1);
        let called = idx.checked_sub(1).and_then(|prev| calls.get(prev));
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        content.retain(|block| match block_type(block) {
            "tool_use" => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                let keep = answered.is_some_and(|answered| answered.contains(id));
                if !keep {
                    dropped.push(format!("unanswered tool_use {id}"));
                }
                keep
            }
            "tool_result" => {
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let keep = called.is_some_and(|called| called.contains(id));
                if !keep {
                    dropped.push(format!("orphaned tool_result {id}"));
                }
                keep
            }
            _ => true,
        });
    }

    // The API rejects an empty content array.
    messages.retain(|message| !array_at(message, "content").is_empty());

    if !dropped.is_empty() {
        dropped.sort();
        warn!(
            "anthropic: dropped unpaired tool blocks: {}",
            dropped.join("; ")
        );
    }
}

fn array_at<'v>(value: &'v Value, key: &str) -> &'v [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

fn block_type(block: &Value) -> &str {
    block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn role_of(message: &Value) -> &str {
    message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn take_content_blocks(message: Value) -> Vec<Value> {
    let Value::Object(mut message) = message else {
        return Vec::new();
    };
    match message.remove("content") {
        Some(Value::Array(blocks)) => blocks,
        _ => Vec::new(),
    }
}

fn extend_content(message: &mut Value, blocks: Vec<Value>) {
    if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
        content.extend(blocks);
    }
}

fn prepend_content(message: &mut Value, mut blocks: Vec<Value>) {
    if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
        blocks.append(content);
        *content = blocks;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::RetryConfig;
    use codex_protocol::models::AgentMessageInputContent;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::LocalShellAction;
    use codex_protocol::models::LocalShellExecAction;
    use codex_protocol::models::LocalShellStatus;
    use codex_protocol::protocol::SubAgentSource;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    /// A 1x1 PNG; small enough that `ResizeToFit` passes the source bytes through.
    const PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    fn provider() -> Provider {
        Provider {
            name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com/v1".to_string(),
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

    fn local_shell_call(id: Option<&str>, call_id: Option<&str>) -> ResponseItem {
        ResponseItem::LocalShellCall {
            id: id.map(|id| ResponseItemId::from_server(id.to_string())),
            call_id: call_id.map(str::to_string),
            status: LocalShellStatus::Completed,
            action: LocalShellAction::Exec(LocalShellExecAction {
                command: vec!["ls".to_string()],
                timeout_ms: None,
                working_directory: None,
                env: None,
                user: None,
            }),
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
        AnthropicRequestBuilder::new("claude-test", "inst", input, &[])
            .build(&provider())
            .expect("request")
            .body
    }

    fn messages_of(input: &[ResponseItem]) -> Vec<Value> {
        body_of(input)["messages"]
            .as_array()
            .expect("messages array")
            .clone()
    }

    fn inline_system_messages_of(input: &[ResponseItem]) -> Vec<Value> {
        AnthropicRequestBuilder::new("claude-test", "inst", input, &[])
            .supports_inline_system(true)
            .build(&provider())
            .expect("request")
            .body["messages"]
            .as_array()
            .expect("messages array")
            .clone()
    }

    #[test]
    fn attaches_the_api_version_and_session_headers() {
        let request =
            AnthropicRequestBuilder::new("claude-test", "inst", &[user_message("hi")], &[])
                .conversation_id(Some("conv-1".into()))
                .session_source(Some(SessionSource::SubAgent(SubAgentSource::Review)))
                .build(&provider())
                .expect("request");

        assert_eq!(
            request.headers.get("anthropic-version"),
            Some(&HeaderValue::from_static("2023-06-01"))
        );
        assert_eq!(
            request.headers.get("session-id"),
            Some(&HeaderValue::from_static("conv-1"))
        );
        assert_eq!(
            request.headers.get("x-openai-subagent"),
            Some(&HeaderValue::from_static("review"))
        );
    }

    /// A `cache_control` breakpoint attaches to a block, so a bare-string user turn
    /// has nowhere to carry one.
    #[test]
    fn user_turns_are_always_block_arrays() {
        let messages = messages_of(&[user_message("hi")]);

        assert_eq!(
            messages,
            vec![json!({"role": "user", "content": [{"type": "text", "text": "hi"}]})]
        );
    }

    #[test]
    fn system_is_an_array_even_for_a_single_block() {
        let body = body_of(&[user_message("hi")]);

        assert_eq!(body["system"], json!([{"type": "text", "text": "inst"}]));
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        assert_eq!(body["stream"], json!(true));
        assert!(body.get("stream_options").is_none(), "{body}");
    }

    #[test]
    fn a_tool_call_is_answered_by_a_tool_result_in_the_next_message() {
        let messages = messages_of(&[
            user_message("read it"),
            function_call("call-a", "read_file", r#"{"path":"a.txt"}"#),
            function_output("call-a", "contents"),
        ]);

        assert_eq!(
            messages,
            vec![
                json!({"role": "user", "content": [{"type": "text", "text": "read it"}]}),
                json!({"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "call-a",
                    "name": "read_file",
                    "input": {"path": "a.txt"},
                }]}),
                json!({"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call-a",
                    "content": [{"type": "text", "text": "contents"}],
                }]}),
            ]
        );
    }

    #[test]
    fn parallel_tool_calls_share_one_assistant_message_and_one_result_message() {
        let messages = messages_of(&[
            user_message("read both"),
            function_call("call-a", "read_file", "{}"),
            function_call("call-b", "read_file", "{}"),
            function_output("call-a", "A"),
            function_output("call-b", "B"),
        ]);

        assert_eq!(messages.len(), 3, "{messages:?}");
        assert_eq!(messages[1]["content"].as_array().map(Vec::len), Some(2));
        assert_eq!(messages[2]["content"].as_array().map(Vec::len), Some(2));
    }

    /// `display` defaults to "omitted", so a signed block with no text is common;
    /// dropping it breaks the next turn.
    #[test]
    fn thinking_round_trips_when_its_text_is_empty() {
        let messages = messages_of(&[
            user_message("go"),
            reasoning("", Some("sig-1")),
            assistant_message("done"),
        ]);

        assert_eq!(
            messages[1],
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": "sig-1"},
                {"type": "text", "text": "done"},
            ]})
        );
    }

    #[test]
    fn thinking_without_a_signature_is_dropped() {
        let messages = messages_of(&[
            user_message("go"),
            reasoning("unsigned", None),
            assistant_message("done"),
        ]);

        assert_eq!(
            messages[1],
            json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]})
        );
    }

    #[test]
    fn thinking_leads_its_message_whatever_the_transcript_order() {
        let messages = messages_of(&[
            user_message("go"),
            assistant_message("looking"),
            reasoning("because", Some("sig-1")),
            function_call("call-a", "read_file", "{}"),
            function_output("call-a", "A"),
        ]);

        let types: Vec<&str> = messages[1]["content"]
            .as_array()
            .expect("blocks")
            .iter()
            .filter_map(|block| block["type"].as_str())
            .collect();
        assert_eq!(types, vec!["thinking", "text", "tool_use"]);
    }

    #[test]
    fn a_legally_placed_developer_message_stays_a_system_message() {
        let messages = inline_system_messages_of(&[user_message("hi"), developer_message("rules")]);

        assert_eq!(
            messages,
            vec![
                json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
                json!({"role": "system", "content": [{"type": "text", "text": "rules"}]}),
            ]
        );
    }

    /// A system message cannot open the conversation.
    #[test]
    fn a_leading_developer_message_folds_into_the_user_turn() {
        let messages = inline_system_messages_of(&[developer_message("rules"), user_message("hi")]);

        assert_eq!(
            messages,
            vec![json!({"role": "user", "content": [
                {"type": "text", "text": "<system-reminder>\nrules\n</system-reminder>"},
                {"type": "text", "text": "hi"},
            ]})]
        );
    }

    /// Coalescing runs after the fold; otherwise two adjacent user messages remain.
    #[test]
    fn a_developer_message_between_user_turns_folds_and_then_coalesces() {
        let messages = inline_system_messages_of(&[
            user_message("first"),
            developer_message("rules"),
            user_message("second"),
        ]);

        assert_eq!(
            messages,
            vec![json!({"role": "user", "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "<system-reminder>\nrules\n</system-reminder>"},
                {"type": "text", "text": "second"},
            ]})]
        );
    }

    #[test]
    fn a_developer_message_folds_inline_when_the_model_has_no_system_turn() {
        let messages = messages_of(&[user_message("hi"), developer_message("rules")]);

        assert_eq!(
            messages,
            vec![json!({"role": "user", "content": [
                {"type": "text", "text": "hi"},
                {"type": "text", "text": "<system-reminder>\nrules\n</system-reminder>"},
            ]})]
        );
    }

    /// Image encoding must be byte-stable, or every turn replaying it misses the
    /// prompt cache.
    #[test]
    fn the_same_image_encodes_identically_on_every_turn() {
        let input = [message(
            "user",
            vec![ContentItem::InputImage {
                image_url: PNG_DATA_URL.to_string(),
                detail: None,
            }],
        )];

        let first = messages_of(&input);
        let second = messages_of(&input);

        assert_eq!(first, second);
        assert_eq!(first[0]["content"][0]["source"]["type"], "base64");
        assert_eq!(first[0]["content"][0]["source"]["media_type"], "image/png");
    }

    #[test]
    fn a_remote_image_url_rides_as_a_url_source() {
        let messages = messages_of(&[message(
            "user",
            vec![ContentItem::InputImage {
                image_url: "https://example.com/cat.png".to_string(),
                detail: None,
            }],
        )]);

        assert_eq!(
            messages[0]["content"][0],
            json!({"type": "image", "source": {"type": "url", "url": "https://example.com/cat.png"}})
        );
    }

    #[test]
    fn audio_becomes_a_placeholder_block() {
        let messages = messages_of(&[message(
            "user",
            vec![
                ContentItem::InputText {
                    text: "transcribe this".to_string(),
                },
                ContentItem::InputAudio {
                    audio_url: "data:audio/wav;base64,AAAA".to_string(),
                },
            ],
        )]);

        assert_eq!(
            messages[0]["content"][1],
            json!({"type": "text", "text": AUDIO_PLACEHOLDER})
        );
    }

    /// Anthropic rejects a non-object `input`, and dropping the block would orphan
    /// its `tool_result`.
    #[test]
    fn unparsable_tool_arguments_become_an_empty_object() {
        let messages = messages_of(&[
            user_message("go"),
            function_call("call-a", "read_file", "not json"),
            function_output("call-a", "A"),
        ]);

        assert_eq!(messages[1]["content"][0]["input"], json!({}));
    }

    #[test]
    fn an_empty_tool_result_gets_a_placeholder_body() {
        let messages = messages_of(&[
            user_message("go"),
            function_call("call-a", "read_file", "{}"),
            function_output("call-a", ""),
        ]);

        assert_eq!(
            messages[2]["content"][0]["content"],
            json!([{"type": "text", "text": EMPTY_TOOL_RESULT}])
        );
    }

    #[test]
    fn a_failed_tool_result_is_flagged() {
        let failed = ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-a".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("boom".to_string()),
                success: Some(false),
            },
            internal_chat_message_metadata_passthrough: None,
        };
        let messages = messages_of(&[
            user_message("go"),
            function_call("call-a", "read_file", "{}"),
            failed,
        ]);

        assert_eq!(messages[2]["content"][0]["is_error"], json!(true));
    }

    /// Namespaced tools are advertised flattened, so a replayed call carries the
    /// flattened name.
    #[test]
    fn namespaced_tool_calls_replay_under_their_advertised_name() {
        let messages = messages_of(&[
            user_message("check my mail"),
            ResponseItem::FunctionCall {
                id: None,
                name: "list_messages".to_string(),
                namespace: Some("mcp__gmail".to_string()),
                arguments: "{}".to_string(),
                call_id: "call-1".to_string(),
                internal_chat_message_metadata_passthrough: None,
                encrypted_function_args: None,
            },
            function_output("call-1", "none"),
            ResponseItem::CustomToolCall {
                id: None,
                status: None,
                call_id: "call-2".to_string(),
                name: "apply_patch".to_string(),
                namespace: Some("mcp__editor".to_string()),
                input: "*** Begin Patch".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: "call-2".to_string(),
                name: None,
                output: FunctionCallOutputPayload::from_text("applied".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        ]);

        assert_eq!(
            messages[1]["content"][0]["name"],
            "mcp__gmail__list_messages"
        );
        assert_eq!(
            messages[3]["content"][0]["name"],
            "mcp__editor__apply_patch"
        );
        assert_eq!(
            messages[3]["content"][0]["input"],
            json!({"input": "*** Begin Patch"})
        );
    }

    /// The result pairs on `call_id` while the `tool_use` block carries the item
    /// `id`; unreconciled ids are rejected.
    #[test]
    fn a_local_shell_result_pairs_with_the_id_its_call_chose() {
        let messages = messages_of(&[
            user_message("run it"),
            local_shell_call(Some("lsh_abc"), Some("call-x")),
            function_output("call-x", "a.txt"),
        ]);

        assert_eq!(messages[1]["content"][0]["name"], "local_shell");
        assert_eq!(messages[1]["content"][0]["id"], "lsh_abc");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "lsh_abc");
    }

    #[test]
    fn a_local_shell_call_with_no_ids_synthesizes_one_from_its_position() {
        let messages = messages_of(&[
            user_message("run it"),
            local_shell_call(None, None),
            function_output("lsh_1", "a.txt"),
        ]);

        assert_eq!(messages[1]["content"][0]["id"], "lsh_1");
    }

    #[test]
    fn agent_messages_survive_as_assistant_text() {
        let messages = messages_of(&[
            user_message("ask the reviewer"),
            ResponseItem::AgentMessage {
                id: None,
                author: "reviewer".to_string(),
                recipient: "main".to_string(),
                content: vec![AgentMessageInputContent::InputText {
                    text: "looks good".to_string(),
                }],
                internal_chat_message_metadata_passthrough: None,
            },
        ]);

        assert_eq!(
            messages[1],
            json!({"role": "assistant", "content": [{"type": "text", "text": "looks good"}]})
        );
    }

    #[test]
    fn empty_and_repeated_assistant_text_is_dropped() {
        let messages = messages_of(&[
            user_message("hi"),
            assistant_message(""),
            assistant_message("same"),
            assistant_message("same"),
        ]);

        assert_eq!(
            messages,
            vec![
                json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "same"}]}),
            ]
        );
    }

    #[test]
    fn items_with_no_wire_representation_are_skipped() {
        let messages = messages_of(&[
            user_message("hi"),
            ResponseItem::WebSearchCall {
                id: None,
                status: None,
                action: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::CompactionTrigger {},
            ResponseItem::Other,
        ]);

        assert_eq!(messages.len(), 1, "{messages:?}");
    }

    #[test]
    fn the_flat_layout_walks_tools_then_system_then_messages() {
        let tools = vec![json!({"name": "read_file"})];
        let body = AnthropicRequestBuilder::new(
            "claude-test",
            "inst",
            &[user_message("hi"), assistant_message("hello")],
            &tools,
        )
        .build(&provider())
        .expect("request")
        .body;

        assert_eq!(
            flat_block_layout(&body),
            vec![
                BlockCoord {
                    message: None,
                    block: 0
                },
                BlockCoord {
                    message: None,
                    block: 1
                },
                BlockCoord {
                    message: Some(0),
                    block: 0
                },
                BlockCoord {
                    message: Some(1),
                    block: 0
                },
            ]
        );
    }

    #[test]
    fn thinking_and_output_config_are_absent_unless_asked_for() {
        let body = body_of(&[user_message("hi")]);

        assert!(body.get("thinking").is_none(), "{body}");
        assert!(body.get("output_config").is_none(), "{body}");
    }

    #[test]
    fn thinking_and_output_config_ride_along_when_requested() {
        let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
        let body = AnthropicRequestBuilder::new("claude-test", "inst", &[user_message("hi")], &[])
            .thinking_enabled(true)
            .effort(Some("high"))
            .output_schema(Some(&schema))
            .max_tokens(64_000)
            .build(&provider())
            .expect("request")
            .body;

        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(
            body["output_config"],
            json!({"effort": "high", "format": {"type": "json_schema", "schema": schema}})
        );
        assert_eq!(body["max_tokens"], json!(64_000));
    }

    /// A turn aborted between a call and its output leaves an unpaired block, which
    /// Anthropic answers with a 400.
    #[test]
    fn an_unanswered_tool_call_is_dropped() {
        let messages = messages_of(&[
            user_message("go"),
            function_call("call-a", "read_file", "{}"),
        ]);

        assert!(
            !serde_json::to_string(&messages)
                .expect("serialize")
                .contains("tool_use"),
            "{messages:#?}"
        );
    }

    #[test]
    fn an_orphaned_tool_result_is_dropped() {
        let messages = messages_of(&[user_message("go"), function_output("call-b", "A")]);

        assert!(
            !serde_json::to_string(&messages)
                .expect("serialize")
                .contains("tool_result"),
            "{messages:#?}"
        );
    }

    /// The API rejects an empty content array.
    #[test]
    fn repairing_never_leaves_an_empty_message() {
        let messages = messages_of(&[user_message("go"), function_output("call-b", "A")]);

        for message in &messages {
            assert!(
                !message["content"]
                    .as_array()
                    .expect("content array")
                    .is_empty(),
                "{message:#?}"
            );
        }
    }

    /// Anthropic rejects a user message whose `tool_result` blocks do not come first.
    #[test]
    fn tool_results_lead_their_user_message() {
        let call = function_call("call-a", "read_file", "{}");
        let output = function_output("call-a", "contents");

        let shapes: Vec<(&str, Vec<ResponseItem>)> = vec![
            (
                "developer message between a call and its output",
                vec![
                    user_message("go"),
                    call.clone(),
                    developer_message("remember the budget"),
                    output.clone(),
                ],
            ),
            (
                "user interjection between a call and its output",
                vec![
                    user_message("go"),
                    call.clone(),
                    user_message("actually, hurry"),
                    output.clone(),
                ],
            ),
            (
                "plain call then output",
                vec![user_message("go"), call, output],
            ),
        ];

        for (label, input) in shapes {
            for messages in [messages_of(&input), inline_system_messages_of(&input)] {
                for message in &messages {
                    let blocks = message["content"].as_array().expect("content array");
                    let last_tool_result = blocks
                        .iter()
                        .rposition(|block| block["type"] == "tool_result");
                    let Some(last_tool_result) = last_tool_result else {
                        continue;
                    };
                    let leading = blocks[..=last_tool_result]
                        .iter()
                        .all(|block| block["type"] == "tool_result");
                    assert!(leading, "{label}: tool_result not first in {message:#?}");
                }
            }
        }
    }

    /// A repeat separated by a user turn is a real turn; dropping it would fuse the
    /// user turns around it.
    #[test]
    fn a_repeated_assistant_text_separated_by_a_user_turn_survives() {
        let messages = messages_of(&[
            user_message("one"),
            assistant_message("Done."),
            user_message("two"),
            assistant_message("Done."),
        ]);

        let assistant_turns = messages
            .iter()
            .filter(|message| message["role"] == "assistant")
            .count();
        let user_turns = messages
            .iter()
            .filter(|message| message["role"] == "user")
            .count();

        assert_eq!(assistant_turns, 2, "{messages:#?}");
        assert_eq!(user_turns, 2, "{messages:#?}");
    }

    fn cached_body(input: &[ResponseItem], tools: &[Value], min_prefix_tokens: i64) -> Value {
        AnthropicRequestBuilder::new("claude-test", "inst", input, tools)
            .cache_policy(Some(AnthropicCachePolicy { min_prefix_tokens }))
            .build(&provider())
            .expect("request")
            .body
    }

    fn cache_marked(body: &Value) -> Vec<BlockCoord> {
        let mut marked = Vec::new();
        let mut coords = flat_block_layout(body).into_iter();
        let blocks = body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(body["system"].as_array().into_iter().flatten())
            .chain(
                body["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|message| message["content"].as_array().into_iter().flatten()),
            );
        for block in blocks {
            let coord = coords.next().expect("layout covers every block");
            if block.get("cache_control").is_some() {
                marked.push(coord);
            }
        }
        marked
    }

    fn big_tool(name: &str) -> Value {
        json!({
            "name": name,
            "description": "x".repeat(4000),
            "input_schema": {"type": "object", "properties": {}},
        })
    }

    /// A breakpoint under the model minimum is ignored but still spends a slot.
    #[test]
    fn a_prefix_under_the_model_minimum_places_no_breakpoint() {
        let body = cached_body(&[user_message("hi")], &[], /*min_prefix_tokens*/ 4096);

        assert!(cache_marked(&body).is_empty(), "{body:#?}");
    }

    #[test]
    fn breakpoints_never_exceed_the_api_limit_of_four() {
        let mut input = vec![user_message("start")];
        for turn in 0..12 {
            input.push(assistant_message(&format!("reply {turn}")));
            input.push(user_message(&format!("turn {turn}")));
        }
        let tools = vec![big_tool("a"), big_tool("b")];

        let body = cached_body(&input, &tools, /*min_prefix_tokens*/ 0);

        assert!(cache_marked(&body).len() <= 4, "{:#?}", cache_marked(&body));
    }

    /// The stable anchors are the end of `tools` and the end of `system`.
    #[test]
    fn the_tools_and_system_prefix_is_cached() {
        let tools = vec![big_tool("a")];
        let body = cached_body(&[user_message("hi")], &tools, /*min_prefix_tokens*/ 0);

        let marked = cache_marked(&body);
        assert!(
            marked.iter().any(|coord| coord.message.is_none()),
            "{marked:#?}"
        );
    }

    /// A user message carrying tool results must not anchor the rolling breakpoint.
    #[test]
    fn a_tool_result_message_is_not_treated_as_a_user_turn() {
        let input = vec![
            user_message("go"),
            function_call("call-a", "read_file", "{}"),
            function_output("call-a", "contents"),
            assistant_message("done"),
            user_message("again"),
        ];

        let body = cached_body(&input, &[], /*min_prefix_tokens*/ 0);
        let messages = body["messages"].as_array().expect("messages");

        for coord in cache_marked(&body) {
            let Some(index) = coord.message else { continue };
            let message = &messages[index];
            let carries_tool_result = message["content"]
                .as_array()
                .expect("content")
                .iter()
                .any(|block| block["type"] == "tool_result");
            let is_last = index + 1 == messages.len();
            assert!(
                !carries_tool_result || is_last,
                "tool-result message anchored a breakpoint: {message:#?}"
            );
        }
    }

    /// Coalescing and system folding run after layout and can move text ahead of a
    /// `tool_result`, which is a 400.
    #[test]
    fn tool_results_still_lead_after_folding_and_coalescing() {
        let input = vec![
            user_message("go"),
            function_call("call-a", "read_file", "{}"),
            user_message("actually, hurry"),
            developer_message("remember the budget"),
            function_output("call-a", "contents"),
        ];

        for (label, messages) in [
            ("folded", messages_of(&input)),
            ("inline", inline_system_messages_of(&input)),
        ] {
            for message in &messages {
                let blocks = message["content"].as_array().expect("content array");
                let Some(last) = blocks.iter().rposition(|b| b["type"] == "tool_result") else {
                    continue;
                };
                assert!(
                    blocks[..=last].iter().all(|b| b["type"] == "tool_result"),
                    "{label}: {messages:#?}"
                );
            }
        }
    }

    /// Anthropic rejects a request whose `messages` array is empty.
    #[test]
    fn a_transcript_that_reduces_to_nothing_is_an_error() {
        let result = AnthropicRequestBuilder::new(
            "claude-test",
            "inst",
            &[function_output("orphan", "hi")],
            &[],
        )
        .build(&provider());

        assert!(result.is_err(), "expected an error, got a sendable body");
    }

    /// The Messages API requires the first message to be `user`.
    #[test]
    fn the_transcript_always_opens_with_a_user_turn() {
        let messages = messages_of(&[assistant_message("hello"), user_message("hi")]);

        assert_eq!(messages[0]["role"], "user", "{messages:#?}");
    }

    /// An unanswered `tool_result` in the first message is a 400.
    #[test]
    fn dropping_a_leading_model_turn_takes_its_orphaned_tool_result_with_it() {
        let messages = messages_of(&[
            reasoning("thinking", Some("sig")),
            function_call("call-a", "shell", "{}"),
            function_output("call-a", "output"),
            user_message("carry on"),
        ]);

        assert_eq!(messages[0]["role"], "user", "{messages:#?}");
        assert!(
            !messages[0]["content"]
                .as_array()
                .expect("content blocks")
                .iter()
                .any(|block| block["type"] == "tool_result"),
            "the first message answers a tool call that is no longer there: {messages:#?}"
        );
    }

    /// Whatever survives pruning still has to open on a user turn.
    #[test]
    fn the_transcript_opens_with_a_user_turn_however_much_is_trimmed() {
        let messages = messages_of(&[
            function_call("call-a", "shell", "{}"),
            function_output("call-a", "output"),
            assistant_message("and here is why"),
            function_call("call-b", "shell", "{}"),
            function_output("call-b", "more output"),
            user_message("carry on"),
        ]);

        assert_eq!(messages[0]["role"], "user", "{messages:#?}");
        assert!(
            messages
                .iter()
                .all(|message| !array_at(message, "content").is_empty()),
            "an emptied message survived: {messages:#?}"
        );
    }

    /// `cache_control` is not permitted on a thinking block, so the rolling anchor
    /// skips it.
    #[test]
    fn cache_control_never_lands_on_a_thinking_block() {
        let input = vec![
            user_message("go"),
            reasoning("weighing it", Some("sig-1")),
            assistant_message("done"),
            user_message("again"),
            reasoning("more", Some("sig-2")),
        ];

        let body = cached_body(&input, &[], /*min_prefix_tokens*/ 0);

        for message in body["messages"].as_array().expect("messages") {
            for block in message["content"].as_array().expect("content") {
                if block["type"] == "thinking" {
                    assert!(
                        block.get("cache_control").is_none(),
                        "cache_control on a thinking block: {body:#?}"
                    );
                }
            }
        }
    }

    /// Returns the JSON of every block up to and including `through`, in wire order.
    fn block_prefix(body: &Value, through: usize) -> Vec<String> {
        let mut blocks: Vec<String> = Vec::new();
        for block in body["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(body["system"].as_array().into_iter().flatten())
            .chain(
                body["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|message| message["content"].as_array().into_iter().flatten()),
            )
        {
            blocks.push(block.to_string());
            if blocks.len() > through {
                break;
            }
        }
        blocks
    }

    /// Anthropic matches a cached prefix byte for byte: a second turn reads the entry
    /// the first wrote only if every block up to the breakpoint is unchanged.
    #[test]
    fn a_second_turn_reuses_the_first_turns_cached_prefix_byte_for_byte() {
        let tools = vec![big_tool("read_file"), big_tool("write_file")];

        let turn_one = vec![user_message("first question")];
        let turn_two = vec![
            user_message("first question"),
            assistant_message("first answer"),
            user_message("second question"),
        ];

        let first = cached_body(&turn_one, &tools, /*min_prefix_tokens*/ 0);
        let second = cached_body(&turn_two, &tools, /*min_prefix_tokens*/ 0);

        let first_marks = cache_marked(&first);
        assert!(
            !first_marks.is_empty(),
            "turn one placed nothing: {first:#?}"
        );
        assert!(
            !cache_marked(&second).is_empty(),
            "turn two placed nothing: {second:#?}"
        );

        let last_prefix_mark = first_marks
            .iter()
            .filter(|coord| coord.message.is_none())
            .map(|coord| coord.block)
            .max()
            .expect("a tools/system breakpoint");

        assert_eq!(
            block_prefix(&first, last_prefix_mark),
            block_prefix(&second, last_prefix_mark),
            "the cached prefix changed between turns, so turn two cannot read turn one's entry"
        );
    }

    /// Tools render first, so any change to them invalidates every breakpoint after.
    #[test]
    fn a_changed_tool_set_moves_the_cached_prefix() {
        let before = cached_body(
            &[user_message("hi")],
            &[big_tool("read_file")],
            /*min_prefix_tokens*/ 0,
        );
        let after = cached_body(
            &[user_message("hi")],
            &[big_tool("read_file"), big_tool("write_file")],
            /*min_prefix_tokens*/ 0,
        );

        assert_ne!(
            block_prefix(&before, 0),
            block_prefix(&after, 0),
            "a changed tool set must be visible in the prefix"
        );
    }
}
