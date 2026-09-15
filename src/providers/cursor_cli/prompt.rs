//! Flatten an Anthropic conversation into the single prompt `cursor-agent`
//! takes as its argument.
//!
//! Each turn re-sends the whole conversation rather than using
//! `cursor-agent --resume`. Claude Code owns the history — it compacts and
//! edits it — so replaying it verbatim is the only way to guarantee the agent
//! sees what Claude Code believes it sees. A resumed CLI session would hold a
//! second, diverging copy.
//!
//! Claude Code's `tools` are deliberately **not** rendered. `cursor-agent`
//! brings its own tools; advertising Claude Code's would invite it to describe
//! calls that nothing will execute.

use serde_json::Value;

use crate::anthropic::schema::MessagesRequest;
use crate::providers::translate_shared::{ContentBlock, flatten_system_text, normalize_content};

pub fn render_prompt(req: &MessagesRequest) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(system) = flatten_system_text(req.extra.get("system")) {
        sections.push(format!("<system>\n{system}\n</system>"));
    }

    for message in &req.messages {
        if let Some(rendered) = render_message(&message.role, &message.content) {
            let role = &message.role;
            sections.push(format!("<{role}>\n{rendered}\n</{role}>"));
        }
    }

    sections.join("\n\n")
}

fn render_message(role: &str, content: &Value) -> Option<String> {
    let blocks = normalize_content(content, serde_json::json!({}));
    let mut parts: Vec<String> = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text } if !text.trim().is_empty() => parts.push(text),
            ContentBlock::Text { .. } => {}
            // Prior reasoning is context, not instruction; tag it so the agent
            // can tell the difference.
            ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                parts.push(crate::providers::translate_shared::wrap_reasoning(
                    &thinking,
                ));
            }
            ContentBlock::Thinking { .. } => {}
            // A tool call from an earlier turn is history: it says what was
            // already done, so the agent does not repeat it.
            ContentBlock::ToolUse { name, input, .. } => {
                parts.push(format!("[called tool {name} with {input}]"));
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                let body = tool_result_text(&content);
                let label = if is_error.unwrap_or(false) {
                    "tool error"
                } else {
                    "tool result"
                };
                parts.push(format!("[{label}]\n{body}"));
            }
            // cursor-agent takes a text prompt; an image cannot be passed
            // through, so say so rather than dropping it silently.
            ContentBlock::Image { .. } => {
                parts.push("[image omitted: cursor-agent takes text only]".to_string())
            }
        }
    }

    let joined = parts.join("\n\n");
    let _ = role;
    (!joined.trim().is_empty()).then_some(joined)
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => {
            let texts: Vec<String> = blocks
                .iter()
                .filter_map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::schema::Message;
    use serde_json::json;

    fn request(messages: Vec<Message>, extra: serde_json::Map<String, Value>) -> MessagesRequest {
        MessagesRequest {
            model: Some("cursor-cli".into()),
            max_tokens: Some(1024),
            messages,
            stream: false,
            bypass_provider_model_override: false,
            extra,
        }
    }

    #[test]
    fn system_and_turns_are_tagged() {
        let mut extra = serde_json::Map::new();
        extra.insert("system".into(), json!("be terse"));
        let messages = vec![
            Message {
                role: "user".into(),
                content: json!("first"),
            },
            Message {
                role: "assistant".into(),
                content: json!("second"),
            },
            Message {
                role: "user".into(),
                content: json!("third"),
            },
        ];
        let prompt = render_prompt(&request(messages, extra));
        assert!(prompt.contains("<system>\nbe terse\n</system>"), "{prompt}");
        assert!(prompt.contains("<user>\nfirst\n</user>"), "{prompt}");
        assert!(
            prompt.contains("<assistant>\nsecond\n</assistant>"),
            "{prompt}"
        );
        assert!(prompt.contains("<user>\nthird\n</user>"), "{prompt}");
        // Ordering must be preserved or the conversation reads wrong.
        let first = prompt.find("first").expect("first");
        let second = prompt.find("second").expect("second");
        let third = prompt.find("third").expect("third");
        assert!(first < second && second < third, "{prompt}");
    }

    /// The agent has its own tools; Claude Code's must not be advertised.
    #[test]
    fn claude_code_tools_are_not_rendered() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "tools".into(),
            json!([{ "name": "Bash", "description": "run a shell command", "input_schema": {} }]),
        );
        let prompt = render_prompt(&request(
            vec![Message {
                role: "user".into(),
                content: json!("hi"),
            }],
            extra,
        ));
        assert!(!prompt.contains("<tools>"), "{prompt}");
        assert!(!prompt.contains("Bash"), "{prompt}");
    }

    #[test]
    fn prior_tool_calls_and_results_become_history() {
        let messages = vec![
            Message {
                role: "assistant".into(),
                content: json!([
                    { "type": "tool_use", "id": "t1", "name": "read", "input": { "path": "a.txt" } }
                ]),
            },
            Message {
                role: "user".into(),
                content: json!([
                    { "type": "tool_result", "tool_use_id": "t1", "content": "hello world" }
                ]),
            },
        ];
        let prompt = render_prompt(&request(messages, serde_json::Map::new()));
        assert!(prompt.contains("[called tool read with"), "{prompt}");
        assert!(prompt.contains("[tool result]\nhello world"), "{prompt}");
    }

    #[test]
    fn tool_errors_are_labelled() {
        let messages = vec![Message {
            role: "user".into(),
            content: json!([{
                "type": "tool_result",
                "tool_use_id": "t1",
                "is_error": true,
                "content": [{ "type": "text", "text": "no such file" }]
            }]),
        }];
        let prompt = render_prompt(&request(messages, serde_json::Map::new()));
        assert!(prompt.contains("[tool error]\nno such file"), "{prompt}");
    }

    #[test]
    fn images_are_reported_rather_than_silently_dropped() {
        let messages = vec![Message {
            role: "user".into(),
            content: json!([
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "QUJD" } }
            ]),
        }];
        let prompt = render_prompt(&request(messages, serde_json::Map::new()));
        assert!(prompt.contains("[image omitted"), "{prompt}");
    }

    #[test]
    fn empty_turns_are_skipped() {
        let messages = vec![
            Message {
                role: "assistant".into(),
                content: json!([]),
            },
            Message {
                role: "user".into(),
                content: json!("only this"),
            },
        ];
        let prompt = render_prompt(&request(messages, serde_json::Map::new()));
        assert!(!prompt.contains("<assistant>"), "{prompt}");
        assert!(prompt.contains("only this"), "{prompt}");
    }

    #[test]
    fn prior_thinking_is_tagged() {
        let messages = vec![Message {
            role: "assistant".into(),
            content: json!([
                { "type": "thinking", "thinking": "considering" },
                { "type": "text", "text": "answer" }
            ]),
        }];
        let prompt = render_prompt(&request(messages, serde_json::Map::new()));
        assert!(prompt.contains("<previous_reasoning>"), "{prompt}");
        assert!(prompt.contains("considering"), "{prompt}");
    }
}
