//! Request builder for the Chat Completions API.
//!
//! Restored in ore; upstream deleted this in Feb 2026 (#10157). Converts the
//! Responses-shaped `ResponseItem` transcript into Chat Completions `messages[]`
//! and rewrites tool specs into the `{"type":"function","function":{...}}` form
//! that API expects.
//!
//! Ported from `d2394a2494^`.

use crate::error::ApiError;
use crate::provider::Provider;
use crate::requests::headers::build_session_headers;
use crate::requests::headers::insert_header;
use crate::requests::headers::subagent_header;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::plaintext_agent_message_content;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use serde_json::Value;
use serde_json::json;

/// Assembled request body plus headers for Chat Completions streaming calls.
pub struct ChatRequest {
    pub body: Value,
    pub headers: HeaderMap,
}

/// Spells a call's `(name, namespace)` pair the way the tools encoder
/// advertised it: one flat identifier joined by `__`. Replaying the bare
/// `name` names a tool the model was never offered. The tools-side encoder
/// (codex-tools) must agree on this spelling.
pub(crate) fn flattened_tool_name(name: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(namespace) => {
            let namespace = namespace.trim_end_matches('_');
            let name = name.trim_start_matches('_');
            format!("{namespace}__{name}")
        }
        None => name.to_string(),
    }
}

/// Stands in for a tool result the wire rejects as empty.
const EMPTY_TOOL_RESULT: &str = "(no output)";

/// Providers on this wire cap cache breakpoints at four.
const MAX_CACHE_BREAKPOINTS: usize = 4;

const BYTES_PER_TOKEN: usize = 4;

/// Prompt-cache breakpoints for the Chat Completions wire.
///
/// `cache_control` rides on the tool entry or the message itself, not on a
/// content block. A server that does not define the key may reject the request.
#[derive(Clone, Copy, Debug)]
pub struct ChatCachePolicy {
    /// `ModelInfo::cache_min_prefix_tokens`.
    pub min_prefix_tokens: i64,
}

/// Where a breakpoint sits, in prefix order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheAnchor {
    LastTool,
    Message(usize),
}

impl ChatCachePolicy {
    /// Marks the end of the tool list, the system prompt, the last settled user
    /// turn, and the transcript edge.
    fn mark(self, tools: &mut [Value], messages: &mut [Value]) {
        // The 5/4 margin covers this estimate's undercount on JSON tool schemas.
        let min_bytes = (self.min_prefix_tokens.max(0) as usize) * BYTES_PER_TOKEN * 5 / 4;

        let tools_bytes: usize = tools.iter().map(|tool| tool.to_string().len()).sum();
        let mut running = tools_bytes;
        let prefix_bytes: Vec<usize> = messages
            .iter()
            .map(|message| {
                running += message.to_string().len();
                running
            })
            .collect();

        let role_of = |index: usize| {
            messages
                .get(index)
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
        };
        let system = (0..messages.len()).find(|index| role_of(*index) == Some("system"));
        let last = messages.len().checked_sub(1);
        // A user turn off the tail stays byte-identical while this turn's tool
        // calls churn everything after it.
        let settled_user = (0..messages.len().saturating_sub(1))
            .rev()
            .find(|index| role_of(*index) == Some("user"));

        let mut anchors = Vec::with_capacity(MAX_CACHE_BREAKPOINTS);
        if !tools.is_empty() {
            anchors.push((tools_bytes, CacheAnchor::LastTool));
        }
        for index in [system, settled_user, last].into_iter().flatten() {
            let anchor = CacheAnchor::Message(index);
            if !anchors.iter().any(|(_, seen)| *seen == anchor) {
                anchors.push((prefix_bytes[index], anchor));
            }
        }

        for (bytes, anchor) in anchors.into_iter().take(MAX_CACHE_BREAKPOINTS) {
            // Below the provider's minimum a marker is ignored but still spends
            // one of the four slots.
            if bytes < min_bytes {
                continue;
            }
            let target = match anchor {
                CacheAnchor::LastTool => tools.last_mut(),
                CacheAnchor::Message(index) => messages.get_mut(index),
            };
            if let Some(object) = target.and_then(Value::as_object_mut) {
                object.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
            }
        }
    }
}

pub struct ChatRequestBuilder<'a> {
    model: &'a str,
    instructions: &'a str,
    input: &'a [ResponseItem],
    tools: &'a [Value],
    conversation_id: Option<String>,
    session_source: Option<SessionSource>,
    output_schema: Option<&'a Value>,
    output_schema_strict: bool,
    max_tokens: Option<i64>,
    reasoning_effort: Option<&'a str>,
    cache_policy: Option<ChatCachePolicy>,
}

impl<'a> ChatRequestBuilder<'a> {
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
            conversation_id: None,
            session_source: None,
            output_schema: None,
            output_schema_strict: false,
            max_tokens: None,
            reasoning_effort: None,
            cache_policy: None,
        }
    }

    pub fn conversation_id(mut self, id: Option<String>) -> Self {
        self.conversation_id = id;
        self
    }

    pub fn max_tokens(mut self, max_tokens: Option<i64>) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn reasoning_effort(mut self, effort: Option<&'a str>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    pub fn cache_policy(mut self, policy: Option<ChatCachePolicy>) -> Self {
        self.cache_policy = policy;
        self
    }

    pub fn session_source(mut self, source: Option<SessionSource>) -> Self {
        self.session_source = source;
        self
    }

    pub fn output_schema(mut self, schema: Option<&'a Value>, strict: bool) -> Self {
        self.output_schema = schema;
        self.output_schema_strict = strict;
        self
    }

    pub fn build(self, _provider: &Provider) -> Result<ChatRequest, ApiError> {
        let mut messages = Vec::<Value>::new();
        messages.push(json!({"role": "system", "content": self.instructions}));

        let input = self.input;

        let mut last_assistant_text: Option<String> = None;

        for item in input {
            if !matches!(item, ResponseItem::Message { role, .. } if role == "assistant") {
                last_assistant_text = None;
            }

            match item {
                ResponseItem::Message { role, content, .. } => {
                    let mut text = String::new();
                    let mut items: Vec<Value> = Vec::new();
                    let mut needs_block_list = false;

                    for c in content {
                        match c {
                            ContentItem::InputText { text: t }
                            | ContentItem::OutputText { text: t } => {
                                text.push_str(t);
                                items.push(json!({"type":"text","text": t}));
                            }
                            ContentItem::InputImage { image_url, .. } => {
                                needs_block_list = true;
                                items.push(
                                    json!({"type":"image_url","image_url": {"url": image_url}}),
                                );
                            }
                            ContentItem::InputAudio { audio_url, .. } => {
                                needs_block_list = true;
                                items.push(
                                    json!({"type":"input_audio","input_audio": {"data": audio_url}}),
                                );
                            }
                        }
                    }

                    if role == "assistant" {
                        // An empty assistant turn is not a message: it reaches
                        // the wire as an empty content block, which some
                        // providers reject outright.
                        if text.is_empty() {
                            continue;
                        }
                        // Only an immediate repeat is a duplicate; an identical
                        // answer after another turn is a real one.
                        if let Some(prev) = &last_assistant_text
                            && prev == &text
                        {
                            continue;
                        }
                        last_assistant_text = Some(text.clone());
                    }

                    let content_value = if role == "assistant" {
                        json!(text)
                    } else if needs_block_list {
                        json!(items)
                    } else {
                        json!(text)
                    };

                    messages.push(json!({"role": role, "content": content_value}));
                }
                ResponseItem::FunctionCall {
                    name,
                    namespace,
                    arguments,
                    call_id,
                    ..
                } => {
                    let tool_call = json!({
                        "id": call_id,
                        "type": "function",
                        "function": {
                            // `tools` advertises only the flattened spelling.
                            "name": flattened_tool_name(name, namespace.as_deref()),
                            "arguments": arguments,
                        }
                    });
                    push_tool_call_message(&mut messages, tool_call);
                }
                ResponseItem::LocalShellCall {
                    id,
                    call_id: _,
                    status,
                    action,
                    ..
                } => {
                    let tool_call = json!({
                        "id": id.as_ref().map(ToString::to_string).unwrap_or_default(),
                        "type": "local_shell_call",
                        "status": status,
                        "action": action,
                    });
                    push_tool_call_message(&mut messages, tool_call);
                }
                ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                } => {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": tool_result_content(output),
                    }));
                }
                ResponseItem::CustomToolCall {
                    id: _,
                    call_id,
                    name,
                    namespace,
                    input,
                    status: _,
                    ..
                } => {
                    // Chat Completions has no `custom` type, and the id here must
                    // be the one the tool result pairs with.
                    let tool_call = json!({
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": flattened_tool_name(name, namespace.as_deref()),
                            "arguments": json!({ "input": input }).to_string(),
                        }
                    });
                    push_tool_call_message(&mut messages, tool_call);
                }
                ResponseItem::CustomToolCallOutput {
                    call_id, output, ..
                } => {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": tool_result_content(output),
                    }));
                }
                ResponseItem::AgentMessage { content, .. } => {
                    match plaintext_agent_message_content(content) {
                        Some(text) => messages.push(json!({"role": "assistant", "content": text})),
                        // Encrypted content has no Chat Completions form.
                        None => continue,
                    }
                }
                ResponseItem::Reasoning { .. }
                | ResponseItem::WebSearchCall { .. }
                | ResponseItem::Other
                | ResponseItem::Compaction { .. }
                | ResponseItem::AdditionalTools { .. }
                | ResponseItem::ToolSearchCall { .. }
                | ResponseItem::ImageGenerationCall { .. }
                | ResponseItem::ToolSearchOutput { .. }
                | ResponseItem::CompactionTrigger { .. }
                | ResponseItem::ContextCompaction { .. } => {
                    continue;
                }
            }
        }

        coalesce_adjacent_user_messages(&mut messages);

        let mut tools = self.tools.to_vec();
        if let Some(policy) = self.cache_policy {
            policy.mark(&mut tools, &mut messages);
        }

        let mut payload = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            // Without this the stream carries no usage chunk and every turn
            // reports zero tokens.
            "stream_options": { "include_usage": true },
            "tools": tools,
        });

        if let Some(obj) = payload.as_object_mut() {
            if let Some(max_tokens) = self.max_tokens {
                obj.insert("max_tokens".to_string(), json!(max_tokens));
            }
            if let Some(effort) = self.reasoning_effort {
                obj.insert("reasoning_effort".to_string(), json!(effort));
            }
        }

        if let Some(schema) = self.output_schema
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert(
                "response_format".to_string(),
                json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "output",
                        "schema": schema,
                        "strict": self.output_schema_strict,
                    }
                }),
            );
        }

        let mut headers = build_session_headers(self.conversation_id, /*thread_id*/ None);
        if let Some(subagent) = subagent_header(&self.session_source) {
            insert_header(&mut headers, "x-openai-subagent", &subagent);
        }

        Ok(ChatRequest {
            body: payload,
            headers,
        })
    }
}

/// Providers that model one turn per role need the context block and the prompt
/// after it — separate items, both `user` — as a single message.
fn coalesce_adjacent_user_messages(messages: &mut Vec<Value>) {
    fn role_of(message: &Value) -> &str {
        message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    let mut idx = 1;
    while idx < messages.len() {
        if role_of(&messages[idx - 1]) != "user" || role_of(&messages[idx]) != "user" {
            idx += 1;
            continue;
        }

        let next = messages.remove(idx);
        let merged = merge_user_content(
            messages[idx - 1]
                .get("content")
                .cloned()
                .unwrap_or(Value::Null),
            next.get("content").cloned().unwrap_or(Value::Null),
        );
        if let Some(obj) = messages[idx - 1].as_object_mut() {
            obj.insert("content".to_string(), merged);
        }
    }
}

/// Two text bodies join as text; anything else becomes a block list, since one
/// message cannot hold both a string and image blocks.
fn merge_user_content(first: Value, second: Value) -> Value {
    match (first, second) {
        (Value::String(first), Value::String(second)) => {
            match (first.is_empty(), second.is_empty()) {
                (true, _) => json!(second),
                (_, true) => json!(first),
                _ => json!(format!("{first}\n\n{second}")),
            }
        }
        (first, second) => {
            let mut blocks = content_blocks(first);
            blocks.extend(content_blocks(second));
            Value::Array(blocks)
        }
    }
}

/// Renders a tool result as Chat Completions content. This wire defines neither
/// the Responses-API part names nor an empty result.
fn tool_result_content(output: &FunctionCallOutputPayload) -> Value {
    let Some(items) = output.content_items() else {
        let text = output.text_content().unwrap_or_default();
        return if text.is_empty() {
            json!(EMPTY_TOOL_RESULT)
        } else {
            json!(text)
        };
    };

    let mut mapped: Vec<Value> = items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => {
                (!text.is_empty()).then(|| json!({"type":"text","text": text}))
            }
            FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                Some(json!({"type":"image_url","image_url": {"url": image_url}}))
            }
            FunctionCallOutputContentItem::InputAudio { audio_url, .. } => {
                Some(json!({"type":"input_audio","input_audio": {"data": audio_url}}))
            }
            // Responses-only; the Chat API has no equivalent.
            FunctionCallOutputContentItem::EncryptedContent { .. } => None,
        })
        .collect();
    if mapped.is_empty() {
        mapped.push(json!({"type":"text","text": EMPTY_TOOL_RESULT}));
    }
    json!(mapped)
}

fn content_blocks(content: Value) -> Vec<Value> {
    match content {
        Value::Array(blocks) => blocks,
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        Value::Null => Vec::new(),
        other => vec![other],
    }
}

fn push_tool_call_message(messages: &mut Vec<Value>, tool_call: Value) {
    // Chat Completions requires that tool calls are grouped into a single assistant message
    // (with `tool_calls: [...]`) followed by tool role responses.
    //
    // Text emitted alongside those calls rides on the same message rather than
    // a preceding one of its own: two assistant messages in a row is valid here,
    // but providers that enforce alternation will rewrite it.
    let mergeable = messages.last().is_some_and(|last| {
        last.get("role").and_then(Value::as_str) == Some("assistant")
            && last.get("content").is_some_and(|content| {
                content.is_null() || content.as_str().is_some_and(|text| !text.is_empty())
            })
            && last.get("tool_calls").is_none_or(Value::is_array)
    });

    if mergeable
        && let Some(Value::Object(obj)) = messages.last_mut()
        && let Value::Array(tool_calls) = obj
            .entry("tool_calls")
            .or_insert_with(|| Value::Array(Vec::new()))
    {
        tool_calls.push(tool_call);
        return;
    }

    messages.push(json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [tool_call],
    }));
}

#[cfg(test)]
mod custom_tool_call_history_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A replayed apply_patch must pair with its result and use the shape the
    /// encoder advertised, or the model sees an orphaned tool message.
    #[test]
    fn custom_tool_call_replays_as_a_paired_function_call() {
        let mut messages: Vec<Value> = Vec::new();
        let tool_call = json!({
            "id": "call-7",
            "type": "function",
            "function": {
                "name": "apply_patch",
                "arguments": json!({ "input": "*** Begin Patch" }).to_string(),
            }
        });
        push_tool_call_message(&mut messages, tool_call);
        messages.push(json!({
            "role": "tool",
            "tool_call_id": "call-7",
            "content": "done",
        }));

        let call = &messages[0]["tool_calls"][0];
        assert_eq!("function", call["type"], "chat has no `custom` tool type");
        assert_eq!("apply_patch", call["function"]["name"]);
        assert_eq!(
            call["id"], messages[1]["tool_call_id"],
            "the result must pair with the call"
        );
        let args: Value =
            serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!("*** Begin Patch", args["input"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::RetryConfig;
    use codex_protocol::models::AgentMessageInputContent;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ReasoningItemContent;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    fn provider() -> Provider {
        Provider {
            name: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
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

    #[test]
    fn attaches_conversation_and_subagent_headers() {
        let prompt_input = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hi".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }];
        let req = ChatRequestBuilder::new("gpt-test", "inst", &prompt_input, &[])
            .conversation_id(Some("conv-1".into()))
            .session_source(Some(SessionSource::SubAgent(SubAgentSource::Review)))
            .build(&provider())
            .expect("request");

        // The old chat-specific `build_conversation_headers` emitted `session_id`;
        // this now goes through the shared `build_session_headers`, which uses the
        // hyphenated `session-id` that the rest of the client sends.
        assert_eq!(
            req.headers.get("session-id"),
            Some(&HeaderValue::from_static("conv-1"))
        );
        assert_eq!(
            req.headers.get("x-openai-subagent"),
            Some(&HeaderValue::from_static("review"))
        );
    }

    #[test]
    fn groups_consecutive_tool_calls_into_a_single_assistant_message() {
        let prompt_input = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "read these".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "read_file".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
                call_id: "call-a".to_string(),
                namespace: None,
                internal_chat_message_metadata_passthrough: None,
                encrypted_function_args: None,
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "read_file".to_string(),
                arguments: r#"{"path":"b.txt"}"#.to_string(),
                call_id: "call-b".to_string(),
                namespace: None,
                internal_chat_message_metadata_passthrough: None,
                encrypted_function_args: None,
            },
            ResponseItem::FunctionCall {
                id: None,
                name: "read_file".to_string(),
                arguments: r#"{"path":"c.txt"}"#.to_string(),
                call_id: "call-c".to_string(),
                namespace: None,
                internal_chat_message_metadata_passthrough: None,
                encrypted_function_args: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-a".to_string(),
                output: FunctionCallOutputPayload::from_text("A".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-b".to_string(),
                output: FunctionCallOutputPayload::from_text("B".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-c".to_string(),
                output: FunctionCallOutputPayload::from_text("C".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        ];

        let req = ChatRequestBuilder::new("gpt-test", "inst", &prompt_input, &[])
            .build(&provider())
            .expect("request");

        let messages = req
            .body
            .get("messages")
            .and_then(|v| v.as_array())
            .expect("messages array");
        // system + user + assistant(tool_calls=[...]) + 3 tool outputs
        assert_eq!(messages.len(), 6);

        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");

        let tool_calls_msg = &messages[2];
        assert_eq!(tool_calls_msg["role"], "assistant");
        assert_eq!(tool_calls_msg["content"], serde_json::Value::Null);
        let tool_calls = tool_calls_msg["tool_calls"]
            .as_array()
            .expect("tool_calls array");
        assert_eq!(tool_calls.len(), 3);
        assert_eq!(tool_calls[0]["id"], "call-a");
        assert_eq!(tool_calls[1]["id"], "call-b");
        assert_eq!(tool_calls[2]["id"], "call-c");

        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-a");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["tool_call_id"], "call-b");
        assert_eq!(messages[5]["role"], "tool");
        assert_eq!(messages[5]["tool_call_id"], "call-c");
    }

    fn user_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn assistant_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn messages_of(input: &[ResponseItem]) -> Vec<Value> {
        ChatRequestBuilder::new("gpt-test", "inst", input, &[])
            .build(&provider())
            .expect("request")
            .body
            .get("messages")
            .and_then(|v| v.as_array())
            .expect("messages array")
            .clone()
    }

    /// Two assistant messages in a row make a provider enforcing alternation
    /// insert a turn of its own.
    #[test]
    fn assistant_text_merges_into_the_message_carrying_its_tool_calls() {
        let messages = messages_of(&[
            user_message("find it"),
            assistant_message("Let me look."),
            ResponseItem::FunctionCall {
                id: None,
                name: "read_file".to_string(),
                arguments: r#"{"path":"a.txt"}"#.to_string(),
                call_id: "call-a".to_string(),
                namespace: None,
                internal_chat_message_metadata_passthrough: None,
                encrypted_function_args: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-a".to_string(),
                output: FunctionCallOutputPayload::from_text("A".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        ]);

        // system + user + assistant(text and tool_calls together) + tool
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "Let me look.");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call-a");
        assert_eq!(messages[3]["role"], "tool");

        let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
        assert!(
            !roles
                .windows(2)
                .any(|pair| pair == ["assistant", "assistant"]),
            "consecutive assistant messages: {roles:?}"
        );
    }

    /// An empty content block is rejected outright by some providers.
    #[test]
    fn empty_assistant_text_is_dropped() {
        let messages = messages_of(&[user_message("hi"), assistant_message("")]);

        assert_eq!(messages.len(), 2, "{messages:?}");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
    }

    /// Every turn opens with a context item and the prompt after it: two `user`
    /// items. Sent separately, a provider enforcing alternation puts its own
    /// turn between them.
    #[test]
    fn adjacent_user_messages_are_sent_as_one() {
        let messages = messages_of(&[
            user_message("# AGENTS.md instructions"),
            user_message("rename the helper"),
            assistant_message("On it."),
            user_message("thanks"),
        ]);

        // system + merged user + assistant + user
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert_eq!(
            messages[1]["content"],
            "# AGENTS.md instructions\n\nrename the helper"
        );

        let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
        assert!(
            !roles.windows(2).any(|pair| pair == ["user", "user"]),
            "consecutive user messages: {roles:?}"
        );
    }

    /// An image forces the block-list shape, which text alone does not use.
    #[test]
    fn adjacent_user_messages_merge_into_blocks_when_one_carries_an_image() {
        let with_image = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,AAAA".to_string(),
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let messages = messages_of(&[user_message("look at this"), with_image]);

        assert_eq!(messages.len(), 2, "{messages:?}");
        let blocks = messages[1]["content"].as_array().expect("block list");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "look at this");
        assert_eq!(blocks[1]["type"], "image_url");
    }

    /// The bare name names a tool that is not in `tools`.
    #[test]
    fn namespaced_tool_call_replays_under_its_advertised_name() {
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
        ]);

        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["name"],
            "mcp__gmail__list_messages",
        );
    }

    #[test]
    fn namespaced_custom_tool_call_replays_under_its_advertised_name() {
        let messages = messages_of(&[
            user_message("apply it"),
            ResponseItem::CustomToolCall {
                id: None,
                status: None,
                call_id: "call-2".to_string(),
                name: "apply_patch".to_string(),
                namespace: Some("mcp__editor".to_string()),
                input: "*** Begin Patch".to_string(),
                internal_chat_message_metadata_passthrough: None,
            },
        ]);

        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["name"],
            "mcp__editor__apply_patch",
        );
    }

    #[test]
    fn audio_content_reaches_the_wire() {
        let with_audio = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "transcribe this".to_string(),
                },
                ContentItem::InputAudio {
                    audio_url: "data:audio/wav;base64,AAAA".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let messages = messages_of(&[with_audio]);

        let blocks = messages[1]["content"]
            .as_array()
            .expect("audio forces the block list form");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "input_audio");
    }

    #[test]
    fn output_schema_reaches_the_wire() {
        let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}});
        let body = ChatRequestBuilder::new("gpt-test", "inst", &[user_message("hi")], &[])
            .output_schema(Some(&schema), /*strict*/ true)
            .build(&provider())
            .expect("request")
            .body;

        let format = &body["response_format"];
        assert_eq!(format["type"], "json_schema");
        assert_eq!(format["json_schema"]["schema"], schema);
        assert_eq!(format["json_schema"]["strict"], true);
    }

    #[test]
    fn output_schema_is_absent_when_unset() {
        let body = ChatRequestBuilder::new("gpt-test", "inst", &[user_message("hi")], &[])
            .build(&provider())
            .expect("request")
            .body;

        assert!(body.get("response_format").is_none(), "{body}");
    }

    #[test]
    fn output_cap_and_effort_reach_the_wire_only_when_set() {
        let body = ChatRequestBuilder::new("gpt-test", "inst", &[user_message("hi")], &[])
            .max_tokens(Some(64_000))
            .reasoning_effort(Some("high"))
            .build(&provider())
            .expect("request")
            .body;

        assert_eq!(json!(64_000), body["max_tokens"]);
        assert_eq!(json!("high"), body["reasoning_effort"]);

        let body = ChatRequestBuilder::new("gpt-test", "inst", &[user_message("hi")], &[])
            .build(&provider())
            .expect("request")
            .body;

        assert!(body.get("max_tokens").is_none(), "{body}");
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    /// A server that does not define `cache_control` may reject the request.
    #[test]
    fn no_cache_markers_without_a_policy() {
        let body = ChatRequestBuilder::new("gpt-test", "inst", &[user_message("hi")], &[])
            .build(&provider())
            .expect("request")
            .body;

        assert!(!body.to_string().contains("cache_control"), "{body}");
    }

    #[test]
    fn cache_markers_land_on_the_stable_prefix_and_the_edge() {
        let tools = vec![json!({"type": "function", "function": {"name": "read_file"}})];
        let input = vec![
            user_message("first"),
            assistant_message("answer"),
            user_message("second"),
        ];

        let body = ChatRequestBuilder::new("gpt-test", "inst", &input, &tools)
            .cache_policy(Some(ChatCachePolicy {
                min_prefix_tokens: 0,
            }))
            .build(&provider())
            .expect("request")
            .body;

        let marked: Vec<&Value> = body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|message| message.get("cache_control").is_some())
            .collect();

        assert!(
            body["tools"][0].get("cache_control").is_some(),
            "the tool list is the largest stable block: {body}"
        );
        assert_eq!(
            vec!["system", "user", "user"],
            marked
                .iter()
                .map(|message| message["role"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            "expected the system prompt, the settled user turn, and the edge: {body}"
        );
        let total = marked.len()
            + body["tools"].as_array().map_or(0, |tools| {
                tools
                    .iter()
                    .filter(|tool| tool.get("cache_control").is_some())
                    .count()
            });
        assert!(
            total <= MAX_CACHE_BREAKPOINTS,
            "over the provider's breakpoint cap: {body}"
        );
    }

    /// Below the minimum a marker is ignored but still spends one of the four
    /// slots.
    #[test]
    fn a_prefix_under_the_minimum_is_not_marked() {
        let body = ChatRequestBuilder::new("gpt-test", "inst", &[user_message("hi")], &[])
            .cache_policy(Some(ChatCachePolicy {
                min_prefix_tokens: 4_096,
            }))
            .build(&provider())
            .expect("request")
            .body;

        assert!(!body.to_string().contains("cache_control"), "{body}");
    }

    #[test]
    fn a_repeated_assistant_text_separated_by_a_user_turn_survives() {
        let messages = messages_of(&[
            user_message("say ok"),
            assistant_message("ok"),
            user_message("again"),
            assistant_message("ok"),
        ]);

        let roles: Vec<&str> = messages
            .iter()
            .map(|message| message["role"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            vec!["system", "user", "assistant", "user", "assistant"],
            roles,
            "{messages:?}"
        );
    }

    #[test]
    fn consecutive_identical_assistant_text_is_still_collapsed() {
        let messages = messages_of(&[
            user_message("say ok"),
            assistant_message("ok"),
            assistant_message("ok"),
        ]);

        assert_eq!(
            1,
            messages
                .iter()
                .filter(|message| message["role"] == "assistant")
                .count(),
            "{messages:?}"
        );
    }

    #[test]
    fn agent_messages_survive_as_assistant_text() {
        let messages = messages_of(&[
            user_message("delegate"),
            ResponseItem::AgentMessage {
                id: None,
                author: "worker".to_string(),
                recipient: "lead".to_string(),
                content: vec![AgentMessageInputContent::InputText {
                    text: "from the other agent".to_string(),
                }],
                internal_chat_message_metadata_passthrough: None,
            },
        ]);

        assert_eq!(
            Some("from the other agent"),
            messages.last().and_then(|m| m["content"].as_str()),
            "{messages:?}"
        );
    }

    /// The payload's own part names are Responses-API types this wire does not
    /// define, and a strict endpoint rejects them.
    #[test]
    fn a_custom_tool_result_uses_chat_part_types() {
        let messages = messages_of(&[
            user_message("run it"),
            ResponseItem::CustomToolCall {
                id: None,
                call_id: "call-a".to_string(),
                name: "execute".to_string(),
                namespace: None,
                input: "print(1)".to_string(),
                status: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::CustomToolCallOutput {
                id: None,
                call_id: "call-a".to_string(),
                name: None,
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::ContentItems(vec![
                        FunctionCallOutputContentItem::InputText {
                            text: "exit 0".to_string(),
                        },
                        FunctionCallOutputContentItem::InputText {
                            text: "1".to_string(),
                        },
                    ]),
                    success: Some(true),
                },
                internal_chat_message_metadata_passthrough: None,
            },
        ]);

        let content = messages.last().expect("tool message")["content"]
            .as_array()
            .expect("content blocks")
            .clone();
        assert_eq!(
            vec!["text", "text"],
            content
                .iter()
                .map(|block| block["type"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            "{content:?}"
        );
    }

    /// A gateway validating the stock OpenAI schema rejects any property it does
    /// not know, failing the whole request.
    #[test]
    fn messages_carry_only_standard_properties() {
        const STANDARD: [&str; 5] = ["role", "content", "tool_calls", "tool_call_id", "name"];

        let body = ChatRequestBuilder::new(
            "gpt-test",
            "inst",
            &[
                user_message("run it"),
                ResponseItem::Reasoning {
                    id: None,
                    summary: Vec::new(),
                    content: Some(vec![ReasoningItemContent::ReasoningText {
                        text: "thinking about it".to_string(),
                    }]),
                    encrypted_content: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCall {
                    id: None,
                    name: "noop".to_string(),
                    arguments: "{}".to_string(),
                    call_id: "call-a".to_string(),
                    namespace: None,
                    internal_chat_message_metadata_passthrough: None,
                    encrypted_function_args: None,
                },
                assistant_message("done"),
            ],
            &[],
        )
        .build(&provider())
        .expect("request")
        .body;

        for message in body["messages"].as_array().expect("messages") {
            for key in message.as_object().expect("object").keys() {
                assert!(
                    STANDARD.contains(&key.as_str()),
                    "non-standard property {key:?} on {message}"
                );
            }
        }
    }

    /// The wire rejects a tool result that arrives empty.
    #[test]
    fn an_empty_tool_result_is_given_a_body() {
        let messages = messages_of(&[
            user_message("run it"),
            ResponseItem::FunctionCall {
                id: None,
                name: "noop".to_string(),
                arguments: "{}".to_string(),
                call_id: "call-a".to_string(),
                namespace: None,
                internal_chat_message_metadata_passthrough: None,
                encrypted_function_args: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-a".to_string(),
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text(String::new()),
                    success: Some(true),
                },
                internal_chat_message_metadata_passthrough: None,
            },
        ]);

        assert_eq!(
            Some(EMPTY_TOOL_RESULT),
            messages.last().and_then(|m| m["content"].as_str()),
            "{messages:?}"
        );
    }
}
