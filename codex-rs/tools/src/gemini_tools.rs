//! Encodes [`ToolSpec`]s into the Gemini `tools[].functionDeclarations` payload.
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

/// Gemini's `functionDeclarations` array plus the bindings to interpret calls.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GeminiTools {
    /// The contents of `tools[0].functionDeclarations`. The caller wraps this in
    /// the single-element `tools` array the request body wants; keeping it flat
    /// here matches the other encoders and keeps the bindings alongside it.
    pub function_declarations: Vec<Value>,
    pub bindings: ChatToolBindings,
}

/// JSON Schema keywords Gemini rejects outright.
///
/// `parameters` is NOT validated as JSON Schema: it is coerced into an OpenAPI
/// 3.0 `Schema` object, and an unknown field there is a hard
/// `400 INVALID_ARGUMENT` ("Invalid JSON payload received. Unknown name
/// \"additionalProperties\""), not a warning. Two of these arrive on every run
/// through no fault of the caller:
///
/// * `additionalProperties` — the freeform-tool schema this crate generates sets
///   it to `false`, and OpenAI-style strict schemas set it everywhere.
/// * `$schema` — MCP servers routinely emit a draft declaration at the root of
///   an `inputSchema`, which reaches here verbatim.
/// * `oneOf`, `allOf`, `not`, `if`, `then`, `else` — Google documents these as
///   unsupported ("Don't use if, then, allOf, oneOf, or not"). `JsonSchema` has
///   dedicated fields for `oneOf` and `allOf`, so they serialize by default.
///   `anyOf` is deliberately NOT in this list: Gemini's `Schema` supports it, and
///   an existing test asserts it survives.
/// * `$defs`, `definitions`, `$ref` — a `$ref` Gemini cannot resolve is worse
///   than an absent constraint, and the definition table is dead weight beside
///   it. Dropping the pointer leaves the property unconstrained, which the model
///   tolerates; forwarding it is a 400.
/// * `encrypted` — not in Gemini's `Schema` proto at all. This one is NOT a
///   third-party edge case: multi_agents_spec.rs marks the `message` parameter
///   of send_message, followup_task and send_input with it, so with multi-agent
///   tools enabled EVERY turn would 400. One bad key fails the whole `tools`
///   payload, not the single tool carrying it.
/// * `default`, `optional`, `maximum` — also documented as unsupported.
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
/// sits on a NESTED object property, and one surviving instance fails the whole
/// request. Values are walked as well as objects because a schema can hide under
/// `properties`, `items`, `$defs`, or an `anyOf` array.
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
        Value::Object(map) => {
            for key in UNSUPPORTED_SCHEMA_KEYS {
                map.remove(key);
            }
            for nested in map.values_mut() {
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

/// Gemini requires `parameters.type`; MCP tools need not declare one.
fn parameters_json(mut parameters: Value) -> Value {
    if let Some(schema) = parameters.as_object_mut() {
        schema.entry("type").or_insert_with(|| json!("object"));
    }
    sanitize_for_gemini(&mut parameters);
    parameters
}

fn declaration_json(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "parameters": parameters_json(parameters),
    })
}

fn encode_function(
    tool: &ResponsesApiTool,
    name: String,
    description: String,
    tool_name: ToolName,
    out: &mut GeminiTools,
) -> Result<(), serde_json::Error> {
    let parameters = serde_json::to_value(&tool.parameters)?;
    // `strict` is not forwarded: Gemini has no per-tool strict flag, and an
    // unknown key in a function declaration is a 400 like any other.
    // `defer_loading` is not forwarded either: a deferred tool is reachable only
    // through the tool-search tool, which this encoder drops.
    out.function_declarations
        .push(declaration_json(&name, &description, parameters));
    out.bindings.insert(
        name,
        ChatToolBinding {
            tool_name,
            kind: ChatToolKind::Function,
        },
    );
    Ok(())
}

fn encode_freeform(name: String, description: String, tool_name: ToolName, out: &mut GeminiTools) {
    out.function_declarations
        .push(declaration_json(&name, &description, freeform_parameters()));
    out.bindings.insert(
        name,
        ChatToolBinding {
            tool_name,
            kind: ChatToolKind::Freeform,
        },
    );
}

/// Encodes `tools` as Gemini function declarations, with the bindings to resolve
/// calls. Hosted tools have no `functionDeclarations` spelling and are dropped;
/// Gemini's own `googleSearch` is a sibling of `functionDeclarations` rather than
/// an entry in it, and mixing the two is rejected on most models.
pub fn create_tools_json_for_gemini_api(
    tools: &[ToolSpec],
) -> Result<GeminiTools, serde_json::Error> {
    let mut out = GeminiTools::default();

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

    fn names(tools: &GeminiTools) -> Vec<String> {
        tools
            .function_declarations
            .iter()
            .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    fn keys(tool: &Value) -> Vec<String> {
        let mut keys: Vec<_> = tool.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    /// Walks the whole encoded payload looking for a key, so a stripped keyword
    /// cannot survive by hiding one level deeper than the assertion looks.
    fn contains_key(value: &Value, needle: &str) -> bool {
        match value {
            Value::Object(map) => {
                map.contains_key(needle) || map.values().any(|nested| contains_key(nested, needle))
            }
            Value::Array(items) => items.iter().any(|item| contains_key(item, needle)),
            _ => false,
        }
    }

    #[test]
    fn declarations_are_flat_and_carry_an_object_parameter_schema() {
        let tools = create_tools_json_for_gemini_api(&[ToolSpec::Function(function("shell"))])
            .expect("encode");

        let entry = &tools.function_declarations[0];
        assert_eq!(
            vec!["description", "name", "parameters"],
            keys(entry),
            "a functionDeclaration takes exactly these three fields; anything else is a 400"
        );
        assert_eq!("shell", entry["name"]);
        assert_eq!(
            json!("object"),
            entry["parameters"]["type"],
            "a schema without a type is rejected as an OpenAPI Schema"
        );
        assert_eq!(
            Some(&ChatToolBinding {
                tool_name: ToolName::plain("shell"),
                kind: ChatToolKind::Function,
            }),
            tools.bindings.get("shell"),
        );
    }

    /// Gemini has no per-tool `strict` flag, and `defer_loading` must not ride
    /// along either: this encoder drops the tool-search tool that is the only
    /// way to reach a deferred tool.
    #[test]
    fn neither_strict_nor_defer_loading_is_forwarded() {
        let mut tool = function("shell");
        tool.strict = true;
        tool.defer_loading = Some(true);

        let tools = create_tools_json_for_gemini_api(&[ToolSpec::Function(tool)]).expect("encode");

        assert_eq!(
            vec!["description", "name", "parameters"],
            keys(&tools.function_declarations[0]),
        );
    }

    /// `additionalProperties` and `$schema` are a hard `400 INVALID_ARGUMENT`,
    /// not a warning: Gemini coerces `parameters` into an OpenAPI Schema and
    /// rejects unknown fields.
    #[test]
    fn keywords_gemini_rejects_are_stripped_at_every_depth() {
        let mut tool = function("edit");
        tool.parameters = crate::parse_tool_input_schema(&json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string"},
                "options": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"backup": {"type": "boolean"}},
                },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"text": {"type": "string"}},
                    },
                },
            },
        }))
        .expect("schema");

        let tools = create_tools_json_for_gemini_api(&[ToolSpec::Function(tool)]).expect("encode");

        let entry = &tools.function_declarations[0];
        assert!(
            !contains_key(entry, "additionalProperties"),
            "one surviving additionalProperties fails the whole request: {entry}"
        );
        // The rest of the schema is untouched — stripping must not flatten it.
        assert_eq!(
            json!("string"),
            entry["parameters"]["properties"]["path"]["type"]
        );
        assert_eq!(
            json!("boolean"),
            entry["parameters"]["properties"]["options"]["properties"]["backup"]["type"],
        );
        assert_eq!(
            json!("string"),
            entry["parameters"]["properties"]["edits"]["items"]["properties"]["text"]["type"],
        );
    }

    /// MCP servers routinely emit a `$schema` declaration at the root of an
    /// `inputSchema`, and it reaches this encoder verbatim.
    #[test]
    fn a_schema_declaration_from_an_mcp_tool_is_stripped() {
        let tools = create_tools_json_for_gemini_api(&[ToolSpec::Function(ResponsesApiTool {
            parameters: crate::parse_tool_input_schema(&json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"query": {"type": "string"}},
            }))
            .expect("schema"),
            ..function("search")
        })])
        .expect("encode");

        let entry = &tools.function_declarations[0];
        assert!(!contains_key(entry, "$schema"), "got {entry}");
        assert_eq!(json!("object"), entry["parameters"]["type"]);
    }

    /// The freeform schema this crate generates sets `additionalProperties:
    /// false`, so every freeform tool would 400 without the strip.
    #[test]
    fn freeform_tools_become_a_single_input_string_with_no_rejected_keywords() {
        let tools = create_tools_json_for_gemini_api(&[ToolSpec::Freeform(FreeformTool {
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
        let entry = &tools.function_declarations[0];
        assert!(
            !contains_key(entry, "additionalProperties"),
            "the generated freeform schema sets it, so it must be stripped here"
        );
        assert_eq!(json!(["input"]), entry["parameters"]["required"]);
        assert_eq!("string", entry["parameters"]["properties"]["input"]["type"]);

        let description = entry["description"].as_str().unwrap();
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

    /// The freeform grammar reaches the model as prose on every wire.
    #[test]
    fn freeform_descriptions_match_the_chat_encoder() {
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

        let gemini = create_tools_json_for_gemini_api(std::slice::from_ref(&freeform))
            .expect("encode gemini");
        let chat =
            crate::create_tools_json_for_chat_completions_api(std::slice::from_ref(&freeform))
                .expect("encode chat");

        assert_eq!(
            gemini.function_declarations[0]["description"],
            chat.json[0]["function"]["description"],
        );
    }

    #[test]
    fn namespaced_tools_are_flattened_and_bound_back() {
        let tools =
            create_tools_json_for_gemini_api(&[ToolSpec::Namespace(ResponsesApiNamespace {
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
            tools.function_declarations[0]["description"]
                .as_str()
                .unwrap()
                .starts_with("Docs namespace."),
            "the namespace description is lost by flattening unless carried per tool"
        );
    }

    #[test]
    fn namespaced_custom_tools_are_flattened_freeform_tools() {
        let tools =
            create_tools_json_for_gemini_api(&[ToolSpec::Namespace(ResponsesApiNamespace {
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
        assert_eq!(
            json!(["input"]),
            tools.function_declarations[0]["parameters"]["required"]
        );
    }

    #[test]
    fn hosted_tools_are_omitted() {
        let tools = create_tools_json_for_gemini_api(&[
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

        let tools = create_tools_json_for_gemini_api(&[ToolSpec::Function(tool)]).expect("encode");

        assert_eq!(
            json!("array"),
            tools.function_declarations[0]["parameters"]["type"]
        );
    }

    #[test]
    fn an_empty_tool_list_produces_no_declarations() {
        let tools = create_tools_json_for_gemini_api(&[]).expect("encode");

        assert!(
            tools.function_declarations.is_empty(),
            "the caller must be able to omit `tools` entirely rather than send an empty array"
        );
        assert!(tools.bindings.is_empty());
    }
}

#[cfg(test)]
mod unsupported_key_tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    /// Every key Gemini's `Schema` coercion rejects, at every depth.
    ///
    /// `encrypted` is the one that made this urgent: multi_agents_spec.rs marks
    /// three first-party tool parameters with it, so with multi-agent tools
    /// enabled every turn would 400 — and one bad key fails the whole `tools`
    /// payload, not just the tool carrying it.
    #[test]
    fn every_documented_unsupported_key_is_stripped_at_depth() {
        let mut schema = json!({
            "type": "object",
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {"Target": {"type": "string"}},
            "additionalProperties": false,
            "properties": {
                "target": {"$ref": "#/$defs/Target"},
                "mode": {"oneOf": [{"type": "string"}, {"type": "number"}]},
                "merged": {"allOf": [{"type": "object"}]},
                "secret": {"type": "string", "encrypted": true},
                "nested": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "deep": {"type": "string", "default": "x", "optional": true}
                    }
                },
                "list": {
                    "type": "array",
                    "items": {"type": "object", "additionalProperties": false}
                }
            }
        });
        sanitize_for_gemini(&mut schema);

        let rendered = schema.to_string();
        for key in UNSUPPORTED_SCHEMA_KEYS {
            assert!(
                !rendered.contains(&format!("\"{key}\"")),
                "{key} survived at some depth: {rendered}"
            );
        }
        // The schema must still be usable, not gutted.
        assert_eq!(schema["type"], json!("object"));
        assert_eq!(
            schema["properties"]["nested"]["properties"]["deep"]["type"],
            json!("string")
        );
        assert_eq!(
            schema["properties"]["list"]["items"]["type"],
            json!("object")
        );
    }
}

#[cfg(test)]
mod schema_contract_tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    /// Names the keys literally.
    ///
    /// The sibling test iterates `UNSUPPORTED_SCHEMA_KEYS` and asserts each is
    /// stripped, which is tautologically true of the stripper and says nothing
    /// about the LIST: a review shrank the list from 14 keys back to 2 and every
    /// test still passed. `encrypted` in particular is marked on three
    /// first-party multi-agent tool parameters, and one unknown key fails the
    /// whole `tools` payload.
    #[test]
    fn the_keys_gemini_rejects_are_actually_in_the_list() {
        for key in [
            "additionalProperties",
            "$schema",
            "oneOf",
            "allOf",
            "not",
            "if",
            "then",
            "else",
            "encrypted",
            "default",
            "optional",
        ] {
            assert!(
                UNSUPPORTED_SCHEMA_KEYS.contains(&key),
                "{key} is documented as unsupported and must be stripped"
            );
        }
        assert!(
            !UNSUPPORTED_SCHEMA_KEYS.contains(&"anyOf"),
            "anyOf IS supported by Gemini; stripping it would degrade a valid schema"
        );
    }

    /// A `$ref` is resolved, not deleted.
    #[test]
    fn a_local_ref_is_inlined_rather_than_leaving_an_untyped_property() {
        let mut schema = json!({
            "type": "object",
            "$defs": {"Target": {"type": "string", "description": "where"}},
            "properties": {"target": {"$ref": "#/$defs/Target"}},
            "required": ["target"]
        });
        sanitize_for_gemini(&mut schema);

        assert_eq!(
            schema["properties"]["target"]["type"],
            json!("string"),
            "deleting the $ref left a REQUIRED property with no type at all"
        );
        assert!(
            schema.get("$defs").is_none(),
            "the definition table itself is not a Gemini schema key"
        );
        assert!(schema["properties"]["target"].get("$ref").is_none());
    }

    #[test]
    fn a_dangling_ref_is_still_removed() {
        // Unresolvable pointers stay the stripper's problem: a $ref Gemini cannot
        // follow is worse than an absent constraint.
        let mut schema = json!({
            "type": "object",
            "properties": {"target": {"$ref": "https://example.com/remote.json"}}
        });
        sanitize_for_gemini(&mut schema);
        assert!(schema["properties"]["target"].get("$ref").is_none());
    }

    #[test]
    fn a_self_referential_schema_terminates() {
        let mut schema = json!({
            "type": "object",
            "$defs": {"Node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/Node"}}}},
            "properties": {"root": {"$ref": "#/$defs/Node"}}
        });
        sanitize_for_gemini(&mut schema);
        assert_eq!(schema["properties"]["root"]["type"], json!("object"));
    }
}
