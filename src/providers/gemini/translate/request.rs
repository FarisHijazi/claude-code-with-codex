//! Anthropic Messages -> OpenAI chat-completions.
//!
//! gemini-web-api speaks plain OpenAI chat-completions, so this is the whole
//! outbound translation. It is deliberately narrower than the kimi translator:
//! no thinking-budget negotiation and no tool-image modes, because the Gemini
//! web app exposes neither.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::anthropic::schema::MessagesRequest;
use crate::providers::translate_shared::{
    ContentBlock, flatten_system_text, image_source_to_url, normalize_content, parallel_tool_calls,
    wrap_reasoning,
};

/// Anthropic allows an unbounded reply; OpenAI wants a number. Large enough not
/// to truncate a real coding answer.
const DEFAULT_MAX_TOKENS: u32 = 8192;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeminiChatRequest {
    pub model: String,
    pub messages: Vec<GeminiMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GeminiTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    pub stream: bool,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum GeminiMessage {
    /// `system` and plain-text `user`/`assistant` turns.
    Text { role: String, content: String },
    /// A `user` turn carrying images alongside text.
    Parts { role: String, content: Vec<Value> },
    Assistant {
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<GeminiToolCall>>,
    },
    Tool {
        role: String,
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeminiToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: GeminiToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeminiToolCallFunction {
    pub name: String,
    /// JSON-encoded, per the OpenAI wire format.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeminiTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: GeminiFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeminiFunction {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

pub fn translate_request(
    req: &MessagesRequest,
    resolved_model: &str,
) -> Result<GeminiChatRequest, anyhow::Error> {
    let mut messages = Vec::new();

    if let Some(system) = flatten_system_text(req.extra.get("system")) {
        messages.push(GeminiMessage::Text {
            role: "system".into(),
            content: system,
        });
    }

    for message in &req.messages {
        push_message(&mut messages, &message.role, &message.content);
    }

    Ok(GeminiChatRequest {
        model: resolved_model.to_string(),
        messages,
        tools: translate_tools(req.extra.get("tools")),
        tool_choice: translate_tool_choice(req.extra.get("tool_choice")),
        parallel_tool_calls: parallel_tool_calls(req),
        stream: req.stream,
        max_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        temperature: req.extra.get("temperature").and_then(Value::as_f64),
    })
}

fn push_message(out: &mut Vec<GeminiMessage>, role: &str, content: &Value) {
    let blocks = normalize_content(content, json!({}));

    // Tool results are their own `tool` turns in the OpenAI format and must be
    // emitted before the text of the same Anthropic user message, so each
    // result lands adjacent to the assistant turn that requested it.
    for block in &blocks {
        if let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = block
        {
            out.push(GeminiMessage::Tool {
                role: "tool".into(),
                tool_call_id: tool_use_id.clone(),
                content: tool_result_text(content, *is_error),
            });
        }
    }

    let mut text = String::new();
    let mut image_parts: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<GeminiToolCall> = Vec::new();

    for block in &blocks {
        match block {
            ContentBlock::Text { text: value } => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(value);
            }
            // Gemini has no native reasoning channel on input. Prior-turn
            // thinking is rehydrated as tagged text, the same shape the codex
            // and anthropic paths use, so a mid-conversation switch keeps it.
            ContentBlock::Thinking { thinking, .. } => {
                if !thinking.trim().is_empty() {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&wrap_reasoning(thinking));
                }
            }
            ContentBlock::Image { source } => {
                image_parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": image_source_to_url(source) },
                }));
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(GeminiToolCall {
                    id: id.clone(),
                    kind: "function".into(),
                    function: GeminiToolCallFunction {
                        name: name.clone(),
                        arguments: serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                    },
                });
            }
            ContentBlock::ToolResult { .. } => {}
        }
    }

    if role == "assistant" {
        if text.is_empty() && tool_calls.is_empty() {
            return;
        }
        out.push(GeminiMessage::Assistant {
            role: "assistant".into(),
            content: (!text.is_empty()).then_some(text),
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        });
        return;
    }

    if !image_parts.is_empty() {
        let mut parts = Vec::with_capacity(image_parts.len() + 1);
        if !text.is_empty() {
            parts.push(json!({ "type": "text", "text": text }));
        }
        parts.extend(image_parts);
        out.push(GeminiMessage::Parts {
            role: role.to_string(),
            content: parts,
        });
        return;
    }

    if !text.is_empty() {
        out.push(GeminiMessage::Text {
            role: role.to_string(),
            content: text,
        });
    }
}

/// Anthropic tool results are text, a block array, or arbitrary JSON; OpenAI
/// wants one string.
fn tool_result_text(content: &Value, is_error: Option<bool>) -> String {
    let body = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => {
            let texts: Vec<String> = blocks
                .iter()
                .filter_map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| match block.get("type").and_then(Value::as_str) {
                            Some("image") => Some("[image omitted]".to_string()),
                            _ => None,
                        })
                })
                .collect();
            if texts.is_empty() {
                content.to_string()
            } else {
                texts.join("\n")
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if is_error.unwrap_or(false) {
        format!("[tool error] {body}")
    } else {
        body
    }
}

fn translate_tools(tools: Option<&Value>) -> Option<Vec<GeminiTool>> {
    let tools = tools?.as_array()?;
    let mapped: Vec<GeminiTool> = tools
        .iter()
        .filter_map(|tool| {
            // Anthropic server-side tools (web_search, computer, ...) have no
            // OpenAI function equivalent; dropping them is better than sending
            // a shape the backend will reject.
            let name = tool.get("name").and_then(Value::as_str)?;
            Some(GeminiTool {
                kind: "function".into(),
                function: GeminiFunction {
                    name: name.to_string(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    parameters: tool.get("input_schema").cloned(),
                },
            })
        })
        .collect();
    (!mapped.is_empty()).then_some(mapped)
}

fn translate_tool_choice(choice: Option<&Value>) -> Option<Value> {
    let kind = choice?.get("type")?.as_str()?;
    match kind {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => {
            let name = choice?.get("name")?.as_str()?;
            Some(json!({ "type": "function", "function": { "name": name } }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::schema::Message;

    fn request(messages: Vec<Message>, extra: serde_json::Map<String, Value>) -> MessagesRequest {
        MessagesRequest {
            model: Some("gemini-3-pro".into()),
            max_tokens: Some(1024),
            messages,
            stream: false,
            bypass_provider_model_override: false,
            extra,
        }
    }

    fn user(content: Value) -> Message {
        Message {
            role: "user".into(),
            content,
        }
    }

    #[test]
    fn system_becomes_the_first_message() {
        let mut extra = serde_json::Map::new();
        extra.insert("system".into(), json!("be terse"));
        let out = translate_request(&request(vec![user(json!("hi"))], extra), "gemini-3-pro")
            .expect("translate");
        assert_eq!(
            out.messages[0],
            GeminiMessage::Text {
                role: "system".into(),
                content: "be terse".into()
            }
        );
        assert_eq!(out.model, "gemini-3-pro");
        assert_eq!(out.max_tokens, 1024);
    }

    #[test]
    fn missing_max_tokens_gets_a_default() {
        let mut req = request(vec![user(json!("hi"))], serde_json::Map::new());
        req.max_tokens = None;
        let out = translate_request(&req, "gemini-3-pro").expect("translate");
        assert_eq!(out.max_tokens, DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn tool_use_becomes_an_assistant_tool_call() {
        let messages = vec![Message {
            role: "assistant".into(),
            content: json!([
                { "type": "text", "text": "looking" },
                { "type": "tool_use", "id": "toolu_1", "name": "read", "input": { "path": "a.txt" } }
            ]),
        }];
        let out = translate_request(&request(messages, serde_json::Map::new()), "gemini-3-pro")
            .expect("translate");
        let GeminiMessage::Assistant {
            content,
            tool_calls,
            ..
        } = &out.messages[0]
        else {
            panic!("expected assistant message, got {:?}", out.messages[0]);
        };
        assert_eq!(content.as_deref(), Some("looking"));
        let calls = tool_calls.as_ref().expect("tool calls");
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.txt"}"#);
    }

    /// A tool result must precede the user's own text so it sits next to the
    /// assistant turn that asked for it.
    #[test]
    fn tool_result_precedes_user_text_in_the_same_turn() {
        let messages = vec![user(json!([
            { "type": "tool_result", "tool_use_id": "toolu_1", "content": "file contents" },
            { "type": "text", "text": "now explain" }
        ]))];
        let out = translate_request(&request(messages, serde_json::Map::new()), "gemini-3-pro")
            .expect("translate");
        assert_eq!(
            out.messages[0],
            GeminiMessage::Tool {
                role: "tool".into(),
                tool_call_id: "toolu_1".into(),
                content: "file contents".into()
            }
        );
        assert_eq!(
            out.messages[1],
            GeminiMessage::Text {
                role: "user".into(),
                content: "now explain".into()
            }
        );
    }

    #[test]
    fn tool_result_blocks_and_errors_flatten_to_text() {
        let messages = vec![user(json!([{
            "type": "tool_result",
            "tool_use_id": "toolu_9",
            "is_error": true,
            "content": [{ "type": "text", "text": "boom" }]
        }]))];
        let out = translate_request(&request(messages, serde_json::Map::new()), "gemini-3-pro")
            .expect("translate");
        assert_eq!(
            out.messages[0],
            GeminiMessage::Tool {
                role: "tool".into(),
                tool_call_id: "toolu_9".into(),
                content: "[tool error] boom".into()
            }
        );
    }

    #[test]
    fn images_become_openai_parts() {
        let messages = vec![user(json!([
            { "type": "text", "text": "what is this" },
            { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "QUJD" } }
        ]))];
        let out = translate_request(&request(messages, serde_json::Map::new()), "gemini-3-pro")
            .expect("translate");
        let GeminiMessage::Parts { content, .. } = &out.messages[0] else {
            panic!("expected parts message, got {:?}", out.messages[0]);
        };
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn prior_thinking_is_rehydrated_as_tagged_text() {
        let messages = vec![Message {
            role: "assistant".into(),
            content: json!([
                { "type": "thinking", "thinking": "step one" },
                { "type": "text", "text": "answer" }
            ]),
        }];
        let out = translate_request(&request(messages, serde_json::Map::new()), "gemini-3-pro")
            .expect("translate");
        let GeminiMessage::Assistant { content, .. } = &out.messages[0] else {
            panic!("expected assistant message");
        };
        let content = content.as_deref().expect("content");
        assert!(content.contains("<previous_reasoning>"), "{content}");
        assert!(content.contains("step one"), "{content}");
        assert!(content.contains("answer"), "{content}");
    }

    #[test]
    fn tools_and_tool_choice_map_to_openai() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "tools".into(),
            json!([{ "name": "read", "description": "read a file", "input_schema": { "type": "object" } }]),
        );
        extra.insert("tool_choice".into(), json!({ "type": "any" }));
        let out = translate_request(&request(vec![user(json!("hi"))], extra), "gemini-3-pro")
            .expect("translate");
        let tools = out.tools.expect("tools");
        assert_eq!(tools[0].function.name, "read");
        assert_eq!(tools[0].kind, "function");
        assert_eq!(out.tool_choice, Some(json!("required")));
    }

    #[test]
    fn named_tool_choice_maps_to_a_function_selector() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "tool_choice".into(),
            json!({ "type": "tool", "name": "read" }),
        );
        let out = translate_request(&request(vec![user(json!("hi"))], extra), "gemini-3-pro")
            .expect("translate");
        assert_eq!(
            out.tool_choice,
            Some(json!({ "type": "function", "function": { "name": "read" } }))
        );
    }

    #[test]
    fn empty_assistant_turns_are_dropped() {
        let messages = vec![Message {
            role: "assistant".into(),
            content: json!([]),
        }];
        let out = translate_request(&request(messages, serde_json::Map::new()), "gemini-3-pro")
            .expect("translate");
        assert!(out.messages.is_empty(), "{:?}", out.messages);
    }
}
