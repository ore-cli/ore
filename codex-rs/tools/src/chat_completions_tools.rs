//! Encodes [`ToolSpec`]s into the Chat Completions `tools` payload. Namespaced
//! and freeform tools are renamed, so calls must be mapped back via `bindings`.

use crate::ResponsesApiNamespaceTool;
use crate::ResponsesApiTool;
use crate::tool_spec::ToolSpec;
use codex_protocol::ToolName;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

/// Separator between namespace and tool name in a flattened identifier.
/// Providers that reject `.` in tool names still accept `_`.
pub const FLATTENED_TOOL_NAME_DELIMITER: &str = "__";

/// Renders a tool as one flat identifier joined by `__`; every encoder and
/// decoder on the chat and Anthropic wires must agree on this spelling.
pub fn flatten_tool_name(tool_name: &ToolName) -> String {
    match &tool_name.namespace {
        Some(namespace) => {
            // Trimming keeps the delimiter unambiguous when a namespace ends
            // or a name starts with `_`.
            let namespace = namespace.trim_end_matches('_');
            let name = tool_name.name.trim_start_matches('_');
            format!("{namespace}{FLATTENED_TOOL_NAME_DELIMITER}{name}")
        }
        None => tool_name.name.clone(),
    }
}

/// Spells a call's `(name, namespace)` pair the way it was advertised in
/// `tools`. Replaying the bare `name` names a tool the model was never offered.
pub fn flatten_tool_name_from_parts(name: &str, namespace: Option<&str>) -> String {
    flatten_tool_name(&ToolName::new(namespace.map(str::to_string), name))
}

/// Best-effort inverse of [`flatten_tool_name`] for a call that arrives without
/// bindings. A namespace containing `__` cannot be recovered — the bindings
/// map is the authoritative reverse lookup.
pub fn unflatten_tool_name(flattened: &str) -> ToolName {
    match flattened.split_once(FLATTENED_TOOL_NAME_DELIMITER) {
        Some((namespace, name)) if !namespace.is_empty() && !name.is_empty() => {
            ToolName::namespaced(namespace, name)
        }
        _ => ToolName::plain(flattened),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatToolKind {
    /// Arguments are the tool's JSON object, passed through unchanged.
    Function,
    /// Arguments are `{"input": "<body>"}`; the body is the freeform input.
    Freeform,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatToolBinding {
    pub tool_name: ToolName,
    pub kind: ChatToolKind,
}

/// Reverse map from chat function name to the originating tool.
pub type ChatToolBindings = BTreeMap<String, ChatToolBinding>;

/// The Chat Completions `tools` array plus the bindings to interpret calls.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatCompletionsTools {
    pub json: Vec<Value>,
    pub bindings: ChatToolBindings,
}

/// Builds one chat `tools` entry. Only the three schema fields: gateways reject
/// unknown properties, so `strict` and `defer_loading` cannot ride along.
fn function_json(name: &str, description: &str, mut parameters: Value) -> Value {
    // An MCP tool need not declare a root `type`, and a gateway rewriting the
    // schema turns the missing key into an empty string the wire rejects.
    if let Some(schema) = parameters.as_object_mut() {
        schema.entry("type").or_insert_with(|| json!("object"));
    }
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    })
}

fn encode_function(
    tool: &ResponsesApiTool,
    name: String,
    description: String,
    tool_name: ToolName,
    out: &mut ChatCompletionsTools,
) -> Result<(), serde_json::Error> {
    let parameters = serde_json::to_value(&tool.parameters)?;
    out.json
        .push(function_json(&name, &description, parameters));
    out.bindings.insert(
        name,
        ChatToolBinding {
            tool_name,
            kind: ChatToolKind::Function,
        },
    );
    Ok(())
}

/// Chat Completions has no grammar-constrained tool type, so the grammar has to
/// reach the model as prose.
pub(crate) fn freeform_description(description: &str, syntax: &str, definition: &str) -> String {
    format!(
        "{description}\n\nProvide the entire tool body as the `input` string. \
         Format ({syntax}):\n{definition}"
    )
}

pub(crate) fn freeform_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "input": {
                "type": "string",
                "description": "The complete tool body.",
            }
        },
        "required": ["input"],
        "additionalProperties": false,
    })
}

/// Flattening loses the grouping that carried the namespace description, so it
/// is kept in front of each tool. The default `functions` namespace has an
/// empty description, which must not leave a blank prefix.
pub(crate) fn namespaced_description(namespace_description: &str, description: &str) -> String {
    if namespace_description.trim().is_empty() {
        description.to_string()
    } else {
        format!("{namespace_description}\n\n{description}")
    }
}

/// Encodes `tools` for Chat Completions, with the bindings to resolve calls.
/// Hosted tools (`web_search`, `tool_search`) have no equivalent and are dropped.
pub fn create_tools_json_for_chat_completions_api(
    tools: &[ToolSpec],
) -> Result<ChatCompletionsTools, serde_json::Error> {
    let mut out = ChatCompletionsTools::default();

    for tool in tools {
        match tool {
            ToolSpec::Function(tool) => {
                encode_function(
                    tool,
                    tool.name.clone(),
                    tool.description.clone(),
                    ToolName::plain(tool.name.clone()),
                    &mut out,
                )?;
            }
            ToolSpec::Namespace(namespace) => {
                for nested in &namespace.tools {
                    match nested {
                        ResponsesApiNamespaceTool::Function(nested) => {
                            let tool_name =
                                ToolName::namespaced(namespace.name.clone(), nested.name.clone());
                            let name = flatten_tool_name(&tool_name);
                            let description =
                                namespaced_description(&namespace.description, &nested.description);
                            encode_function(nested, name, description, tool_name, &mut out)?;
                        }
                        ResponsesApiNamespaceTool::Custom(nested) => {
                            let tool_name =
                                ToolName::namespaced(namespace.name.clone(), nested.name.clone());
                            let name = flatten_tool_name(&tool_name);
                            let description = namespaced_description(
                                &namespace.description,
                                &freeform_description(
                                    &nested.description,
                                    &nested.format.syntax,
                                    &nested.format.definition,
                                ),
                            );
                            out.json.push(function_json(
                                &name,
                                &description,
                                freeform_parameters(),
                            ));
                            out.bindings.insert(
                                name,
                                ChatToolBinding {
                                    tool_name,
                                    kind: ChatToolKind::Freeform,
                                },
                            );
                        }
                    }
                }
            }
            ToolSpec::Freeform(tool) => {
                let description = freeform_description(
                    &tool.description,
                    &tool.format.syntax,
                    &tool.format.definition,
                );
                out.json.push(function_json(
                    &tool.name,
                    &description,
                    freeform_parameters(),
                ));
                out.bindings.insert(
                    tool.name.clone(),
                    ChatToolBinding {
                        tool_name: ToolName::plain(tool.name.clone()),
                        kind: ChatToolKind::Freeform,
                    },
                );
            }
            ToolSpec::ToolSearch { .. } | ToolSpec::WebSearch { .. } => {}
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FreeformTool;
    use crate::FreeformToolFormat;
    use crate::JsonSchema;
    use crate::ResponsesApiNamespace;
    use pretty_assertions::assert_eq;

    fn function(name: &str) -> ResponsesApiTool {
        ResponsesApiTool {
            name: name.to_string(),
            description: format!("{name} description"),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::default(),
            output_schema: None,
        }
    }

    fn freeform(name: &str) -> FreeformTool {
        FreeformTool {
            name: name.to_string(),
            description: "Edit files.".to_string(),
            defer_loading: None,
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: patch".to_string(),
            },
        }
    }

    fn names(tools: &ChatCompletionsTools) -> Vec<String> {
        tools
            .json
            .iter()
            .map(|t| {
                t["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn flattened_names_round_trip_through_the_helpers() {
        let tool_name = ToolName::namespaced("docs", "search");

        let flattened = flatten_tool_name(&tool_name);

        assert_eq!("docs__search", flattened);
        assert_eq!(
            flattened,
            flatten_tool_name_from_parts("search", Some("docs"))
        );
        assert_eq!(tool_name, unflatten_tool_name(&flattened));
        assert_eq!(ToolName::plain("shell"), unflatten_tool_name("shell"));
    }

    /// Trimming keeps the delimiter unambiguous; the parts spelling and the
    /// `ToolName` spelling must stay byte-identical, because the request
    /// encoders replay historical calls through the parts form.
    #[test]
    fn flattening_trims_underscores_at_the_seam() {
        assert_eq!(
            "ns__tool",
            flatten_tool_name(&ToolName::namespaced("ns_", "_tool"))
        );
        assert_eq!(
            "ns__tool",
            flatten_tool_name_from_parts("_tool", Some("ns_"))
        );
    }

    #[test]
    fn emits_only_the_fields_chat_completions_defines() {
        let tools =
            create_tools_json_for_chat_completions_api(&[ToolSpec::Function(function("shell"))])
                .expect("encode");

        let entry = &tools.json[0];
        assert_eq!(
            vec!["function", "type"],
            {
                let mut keys: Vec<_> = entry.as_object().unwrap().keys().cloned().collect();
                keys.sort();
                keys
            },
            "no top-level name beside function"
        );
        let mut fields: Vec<_> = entry["function"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        fields.sort();
        assert_eq!(vec!["description", "name", "parameters"], fields);
    }

    #[test]
    fn namespaced_tools_are_flattened_and_bound_back() {
        let tools = create_tools_json_for_chat_completions_api(&[ToolSpec::Namespace(
            ResponsesApiNamespace {
                name: "docs".to_string(),
                description: "Docs namespace.".to_string(),
                tools: vec![ResponsesApiNamespaceTool::Function(function("search"))],
            },
        )])
        .expect("encode");

        assert_eq!(vec!["docs__search".to_string()], names(&tools));
        assert_eq!(
            Some(&ChatToolBinding {
                tool_name: ToolName::namespaced("docs", "search"),
                kind: ChatToolKind::Function,
            }),
            tools.bindings.get("docs__search"),
            "a flattened call must resolve back to its namespace"
        );
        assert!(
            tools.json[0]["function"]["description"]
                .as_str()
                .unwrap()
                .starts_with("Docs namespace."),
            "the namespace description is lost by flattening unless carried per tool"
        );
    }

    #[test]
    fn namespaced_custom_tools_are_flattened_freeform_tools() {
        let tools = create_tools_json_for_chat_completions_api(&[ToolSpec::Namespace(
            ResponsesApiNamespace {
                name: "docs".to_string(),
                description: "Docs namespace.".to_string(),
                tools: vec![ResponsesApiNamespaceTool::Custom(freeform("apply_patch"))],
            },
        )])
        .expect("encode");

        assert_eq!(vec!["docs__apply_patch".to_string()], names(&tools));
        assert_eq!(
            Some(&ChatToolBinding {
                tool_name: ToolName::namespaced("docs", "apply_patch"),
                kind: ChatToolKind::Freeform,
            }),
            tools.bindings.get("docs__apply_patch"),
        );
        let params = &tools.json[0]["function"]["parameters"];
        assert_eq!(json!(["input"]), params["required"]);
    }

    /// The default `functions` namespace has an empty description at 0.149; a
    /// blank prefix would waste the top of every tool description.
    #[test]
    fn an_empty_namespace_description_leaves_no_blank_prefix() {
        let tools = create_tools_json_for_chat_completions_api(&[ToolSpec::Namespace(
            ResponsesApiNamespace {
                name: "functions".to_string(),
                description: String::new(),
                tools: vec![ResponsesApiNamespaceTool::Function(function("shell"))],
            },
        )])
        .expect("encode");

        assert_eq!(
            "shell description",
            tools.json[0]["function"]["description"].as_str().unwrap()
        );
    }

    #[test]
    fn freeform_tools_become_a_single_input_string() {
        let tools =
            create_tools_json_for_chat_completions_api(&[ToolSpec::Freeform(FreeformTool {
                name: "apply_patch".to_string(),
                description: "Edit files.".to_string(),
                defer_loading: None,
                format: FreeformToolFormat {
                    r#type: "grammar".to_string(),
                    syntax: "lark".to_string(),
                    definition: "start: patch".to_string(),
                },
            })])
            .expect("encode");

        assert_eq!(vec!["apply_patch".to_string()], names(&tools));
        let params = &tools.json[0]["function"]["parameters"];
        assert_eq!(json!(["input"]), params["required"]);
        assert_eq!("string", params["properties"]["input"]["type"]);

        let description = tools.json[0]["function"]["description"].as_str().unwrap();
        assert!(
            description.contains("lark") && description.contains("start: patch"),
            "the grammar must reach the model as prose: {description}"
        );
        assert_eq!(
            Some(&ChatToolBinding {
                tool_name: ToolName::plain("apply_patch"),
                kind: ChatToolKind::Freeform,
            }),
            tools.bindings.get("apply_patch"),
            "freeform calls need unwrapping from {{\"input\": ...}}"
        );
    }

    #[test]
    fn hosted_tools_are_omitted() {
        let tools = create_tools_json_for_chat_completions_api(&[
            ToolSpec::WebSearch {
                external_web_access: None,
                indexed_web_access: None,
                filters: None,
                user_location: None,
                search_context_size: None,
                search_content_types: None,
            },
            ToolSpec::Function(function("shell")),
        ])
        .expect("encode");

        assert_eq!(vec!["shell".to_string()], names(&tools));
    }

    #[test]
    fn a_schema_without_a_root_type_gets_one() {
        let mut tool = function("shell");
        tool.parameters = crate::parse_tool_input_schema(&json!({
            "properties": {"cmd": {"type": "string"}},
        }))
        .expect("schema");

        let tools = create_tools_json_for_chat_completions_api(&[ToolSpec::Function(tool)])
            .expect("encode");

        assert_eq!(
            json!("object"),
            tools.json[0]["function"]["parameters"]["type"]
        );
    }

    #[test]
    fn an_existing_root_type_is_not_rewritten() {
        let mut tool = function("shell");
        tool.parameters = crate::parse_tool_input_schema(&json!({
            "type": "array",
            "items": {"type": "string"},
        }))
        .expect("schema");

        let tools = create_tools_json_for_chat_completions_api(&[ToolSpec::Function(tool)])
            .expect("encode");

        assert_eq!(
            json!("array"),
            tools.json[0]["function"]["parameters"]["type"]
        );
    }
}
