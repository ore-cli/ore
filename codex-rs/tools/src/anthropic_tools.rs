//! Encodes [`ToolSpec`]s into the Anthropic Messages API `tools` payload.
//! Namespaced and freeform tools are renamed, so calls must be mapped back via
//! `bindings`.

use crate::ChatToolBinding;
use crate::ChatToolBindings;
use crate::ChatToolKind;
use crate::ResponsesApiNamespaceTool;
use crate::ResponsesApiTool;
use crate::chat_completions_tools::flatten_tool_name;
use crate::chat_completions_tools::freeform_description;
use crate::chat_completions_tools::freeform_parameters;
use crate::chat_completions_tools::namespaced_description;
use crate::tool_spec::ToolSpec;
use codex_protocol::ToolName;
use serde_json::Value;
use serde_json::json;

/// The Messages API `tools` array plus the bindings to interpret calls.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnthropicTools {
    pub json: Vec<Value>,
    pub bindings: ChatToolBindings,
}

/// Anthropic requires `input_schema.type`; MCP tools need not declare one.
fn input_schema_json(mut parameters: Value) -> Value {
    if let Some(schema) = parameters.as_object_mut() {
        schema.entry("type").or_insert_with(|| json!("object"));
    }
    parameters
}

fn tool_json(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "input_schema": input_schema_json(parameters),
    })
}

fn encode_function(
    tool: &ResponsesApiTool,
    name: String,
    description: String,
    tool_name: ToolName,
    out: &mut AnthropicTools,
) -> Result<(), serde_json::Error> {
    let parameters = serde_json::to_value(&tool.parameters)?;
    let mut entry = tool_json(&name, &description, parameters);
    // `defer_loading` is not forwarded: a deferred tool is reachable only through
    // the tool-search tool, which this encoder drops.
    entry["strict"] = json!(tool.strict);
    out.json.push(entry);
    out.bindings.insert(
        name,
        ChatToolBinding {
            tool_name,
            kind: ChatToolKind::Function,
        },
    );
    Ok(())
}

fn encode_freeform(
    name: String,
    description: String,
    tool_name: ToolName,
    out: &mut AnthropicTools,
) {
    out.json
        .push(tool_json(&name, &description, freeform_parameters()));
    out.bindings.insert(
        name,
        ChatToolBinding {
            tool_name,
            kind: ChatToolKind::Freeform,
        },
    );
}

/// Encodes `tools` for the Anthropic Messages API, with the bindings to resolve
/// calls. Hosted tools have no Messages API spelling and are dropped.
pub fn create_tools_json_for_anthropic_api(
    tools: &[ToolSpec],
) -> Result<AnthropicTools, serde_json::Error> {
    let mut out = AnthropicTools::default();

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
                            encode_freeform(name, description, tool_name, &mut out);
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
                encode_freeform(
                    tool.name.clone(),
                    description,
                    ToolName::plain(tool.name.clone()),
                    &mut out,
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

    fn names(tools: &AnthropicTools) -> Vec<String> {
        tools
            .json
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    fn keys(tool: &Value) -> Vec<String> {
        let mut keys: Vec<_> = tool.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn tools_are_flat_and_carry_an_object_input_schema() {
        let tools = create_tools_json_for_anthropic_api(&[ToolSpec::Function(function("shell"))])
            .expect("encode");

        let entry = &tools.json[0];
        assert_eq!(
            vec!["description", "input_schema", "name", "strict"],
            keys(entry),
            "the Messages API takes the schema flat, not under a function wrapper"
        );
        assert_eq!("shell", entry["name"]);
        assert_eq!(
            json!("object"),
            entry["input_schema"]["type"],
            "a schema without a type is rejected as an input_schema"
        );
        assert_eq!(
            Some(&ChatToolBinding {
                tool_name: ToolName::plain("shell"),
                kind: ChatToolKind::Function,
            }),
            tools.bindings.get("shell"),
        );
    }

    /// `defer_loading` must not ride along: this encoder drops the tool-search tool
    /// that is the only way to reach a deferred tool.
    #[test]
    fn strict_rides_along_and_defer_loading_does_not() {
        let mut tool = function("shell");
        tool.strict = true;
        tool.defer_loading = Some(true);

        let tools =
            create_tools_json_for_anthropic_api(&[ToolSpec::Function(tool)]).expect("encode");

        let entry = &tools.json[0];
        assert_eq!(
            vec!["description", "input_schema", "name", "strict"],
            keys(entry),
        );
        assert_eq!(json!(true), entry["strict"]);
    }

    /// The freeform grammar reaches the model as prose on both wires.
    #[test]
    fn freeform_tools_match_the_chat_encoder() {
        let freeform = ToolSpec::Freeform(FreeformTool {
            name: "apply_patch".to_string(),
            description: "Apply a patch.".to_string(),
            defer_loading: None,
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: PATCH".to_string(),
            },
        });

        let anthropic = create_tools_json_for_anthropic_api(std::slice::from_ref(&freeform))
            .expect("encode anthropic");
        let chat =
            crate::create_tools_json_for_chat_completions_api(std::slice::from_ref(&freeform))
                .expect("encode chat");

        assert_eq!(
            anthropic.json[0]["description"],
            chat.json[0]["function"]["description"],
        );
        assert_eq!(
            anthropic.json[0]["input_schema"],
            chat.json[0]["function"]["parameters"],
        );
    }

    #[test]
    fn namespaced_tools_are_flattened_and_bound_back() {
        let tools =
            create_tools_json_for_anthropic_api(&[ToolSpec::Namespace(ResponsesApiNamespace {
                name: "docs".to_string(),
                description: "Docs namespace.".to_string(),
                tools: vec![ResponsesApiNamespaceTool::Function(function("search"))],
            })])
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
            tools.json[0]["description"]
                .as_str()
                .unwrap()
                .starts_with("Docs namespace."),
            "the namespace description is lost by flattening unless carried per tool"
        );
    }

    #[test]
    fn namespaced_custom_tools_are_flattened_freeform_tools() {
        let tools =
            create_tools_json_for_anthropic_api(&[ToolSpec::Namespace(ResponsesApiNamespace {
                name: "docs".to_string(),
                description: "Docs namespace.".to_string(),
                tools: vec![ResponsesApiNamespaceTool::Custom(FreeformTool {
                    name: "apply_patch".to_string(),
                    description: "Edit files.".to_string(),
                    defer_loading: None,
                    format: FreeformToolFormat {
                        r#type: "grammar".to_string(),
                        syntax: "lark".to_string(),
                        definition: "start: patch".to_string(),
                    },
                })],
            })])
            .expect("encode");

        assert_eq!(vec!["docs__apply_patch".to_string()], names(&tools));
        assert_eq!(
            Some(&ChatToolBinding {
                tool_name: ToolName::namespaced("docs", "apply_patch"),
                kind: ChatToolKind::Freeform,
            }),
            tools.bindings.get("docs__apply_patch"),
        );
        assert_eq!(json!(["input"]), tools.json[0]["input_schema"]["required"]);
    }

    #[test]
    fn freeform_tools_become_a_single_input_string() {
        let tools = create_tools_json_for_anthropic_api(&[ToolSpec::Freeform(FreeformTool {
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
        let schema = &tools.json[0]["input_schema"];
        assert_eq!(json!(["input"]), schema["required"]);
        assert_eq!("string", schema["properties"]["input"]["type"]);

        let description = tools.json[0]["description"].as_str().unwrap();
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
        let tools = create_tools_json_for_anthropic_api(&[
            ToolSpec::WebSearch {
                external_web_access: None,
                indexed_web_access: None,
                filters: None,
                user_location: None,
                search_context_size: None,
                search_content_types: None,
            },
            ToolSpec::ToolSearch {
                execution: "local".to_string(),
                description: "Search tools.".to_string(),
                parameters: JsonSchema::default(),
            },
            ToolSpec::Function(function("shell")),
        ])
        .expect("encode");

        assert_eq!(vec!["shell".to_string()], names(&tools));
        assert_eq!(1, tools.bindings.len());
    }

    /// A rewritten root type hands the model a schema its tool cannot be called
    /// against.
    #[test]
    fn an_existing_root_type_is_not_rewritten() {
        let mut tool = function("shell");
        tool.parameters = crate::parse_tool_input_schema(&json!({
            "type": "array",
            "items": {"type": "string"},
        }))
        .expect("schema");

        let tools =
            create_tools_json_for_anthropic_api(&[ToolSpec::Function(tool)]).expect("encode");

        assert_eq!(json!("array"), tools.json[0]["input_schema"]["type"]);
    }
}
