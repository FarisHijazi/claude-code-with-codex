//! Fold an Anthropic Messages SSE stream back into a single Messages JSON
//! response.
//!
//! Every backend that can stream needs this for its non-streaming path, and
//! deriving it from the already-translated SSE rather than writing a second
//! parser means the two reply shapes cannot disagree about block ordering,
//! stop reason, or usage.

use serde_json::{Map, Value, json};

use super::sse::parse_sse_events;

pub fn accumulate_response(
    anthropic_sse: &[u8],
    message_id: &str,
    model: &str,
) -> Result<Value, anyhow::Error> {
    let mut content: Vec<Value> = Vec::new();
    // Partial JSON for tool_use blocks arrives in fragments keyed by index.
    let mut tool_json: Vec<(usize, String)> = Vec::new();
    let mut stop_reason = "end_turn".to_string();
    let mut usage = json!({ "input_tokens": 0, "output_tokens": 0 });

    for event in parse_sse_events(anthropic_sse) {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        match event.event.as_deref() {
            Some("content_block_start") => {
                let index = data["index"].as_u64().unwrap_or_default() as usize;
                let block = data["content_block"].clone();
                if block["type"] == "tool_use" {
                    tool_json.push((index, String::new()));
                }
                while content.len() <= index {
                    content.push(Value::Null);
                }
                content[index] = block;
            }
            Some("content_block_delta") => {
                let index = data["index"].as_u64().unwrap_or_default() as usize;
                let Some(delta) = data.get("delta") else {
                    continue;
                };
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        append_str(&mut content, index, "text", delta["text"].as_str());
                    }
                    Some("thinking_delta") => {
                        append_str(&mut content, index, "thinking", delta["thinking"].as_str());
                    }
                    Some("signature_delta") => {
                        if let Some(block) = content.get_mut(index).and_then(Value::as_object_mut)
                            && let Some(signature) = delta["signature"].as_str()
                        {
                            block.insert("signature".into(), json!(signature));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(fragment) = delta["partial_json"].as_str()
                            && let Some(entry) = tool_json.iter_mut().find(|(key, _)| *key == index)
                        {
                            entry.1.push_str(fragment);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    stop_reason = reason.to_string();
                }
                if let Some(value) = data.get("usage") {
                    usage = value.clone();
                }
            }
            _ => {}
        }
    }

    // Settle each tool_use block's accumulated argument JSON.
    for (index, raw) in tool_json {
        let Some(block) = content.get_mut(index).and_then(Value::as_object_mut) else {
            continue;
        };
        let parsed = serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| {
            // Keep a truncated or malformed argument stream visible rather than
            // silently turning it into an empty call.
            if raw.trim().is_empty() {
                json!({})
            } else {
                json!({ "__raw_arguments": raw })
            }
        });
        block.insert("input".into(), parsed);
    }

    let content: Vec<Value> = content
        .into_iter()
        .filter(|block| !block.is_null())
        .collect();

    let mut message = Map::new();
    message.insert("id".into(), json!(message_id));
    message.insert("type".into(), json!("message"));
    message.insert("role".into(), json!("assistant"));
    message.insert("model".into(), json!(model));
    message.insert("content".into(), Value::Array(content));
    message.insert("stop_reason".into(), json!(stop_reason));
    message.insert("stop_sequence".into(), Value::Null);
    message.insert("usage".into(), usage);
    Ok(Value::Object(message))
}

fn append_str(content: &mut [Value], index: usize, key: &str, value: Option<&str>) {
    let (Some(block), Some(value)) = (content.get_mut(index).and_then(Value::as_object_mut), value)
    else {
        return;
    };
    let existing = block
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    block.insert(key.into(), json!(format!("{existing}{value}")));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an Anthropic SSE stream the way a provider's translator would.
    fn sse(events: &[(&str, Value)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in events {
            out.extend_from_slice(&crate::anthropic::sse::encode_sse_event(
                Some(name),
                &data.to_string(),
            ));
        }
        out
    }

    #[test]
    fn text_deltas_join_into_one_block() {
        let stream = sse(&[
            (
                "content_block_start",
                json!({"index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"text_delta","text":"Hel"}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"text_delta","text":"lo"}}),
            ),
            ("content_block_stop", json!({"index":0})),
            (
                "message_delta",
                json!({"delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":5,"output_tokens":2}}),
            ),
        ]);
        let value = accumulate_response(&stream, "msg_1", "some-model").expect("accumulate");
        assert_eq!(value["content"][0]["text"], "Hello");
        assert_eq!(value["stop_reason"], "end_turn");
        assert_eq!(value["usage"]["input_tokens"], 5);
        assert_eq!(value["id"], "msg_1");
        assert_eq!(value["model"], "some-model");
        assert_eq!(value["role"], "assistant");
    }

    #[test]
    fn tool_input_fragments_are_parsed() {
        let stream = sse(&[
            (
                "content_block_start",
                json!({"index":0,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\""}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"input_json_delta","partial_json":":\"a.txt\"}"}}),
            ),
            ("content_block_stop", json!({"index":0})),
            (
                "message_delta",
                json!({"delta":{"stop_reason":"tool_use"},"usage":{}}),
            ),
        ]);
        let value = accumulate_response(&stream, "msg_2", "m").expect("accumulate");
        assert_eq!(value["content"][0]["input"]["path"], "a.txt");
        assert_eq!(value["stop_reason"], "tool_use");
    }

    /// Truncated arguments must stay visible rather than silently becoming an
    /// empty call the caller would then execute.
    #[test]
    fn malformed_tool_input_is_preserved() {
        let stream = sse(&[
            (
                "content_block_start",
                json!({"index":0,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{oops"}}),
            ),
            ("content_block_stop", json!({"index":0})),
        ]);
        let value = accumulate_response(&stream, "msg_3", "m").expect("accumulate");
        assert_eq!(value["content"][0]["input"]["__raw_arguments"], "{oops");
    }

    #[test]
    fn thinking_block_keeps_text_and_signature() {
        let stream = sse(&[
            (
                "content_block_start",
                json!({"index":0,"content_block":{"type":"thinking","thinking":""}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
            ),
            ("content_block_stop", json!({"index":0})),
        ]);
        let value = accumulate_response(&stream, "msg_4", "m").expect("accumulate");
        assert_eq!(value["content"][0]["thinking"], "hmm");
        assert_eq!(value["content"][0]["signature"], "sig");
    }

    #[test]
    fn empty_stream_is_a_valid_empty_message() {
        let value = accumulate_response(b"", "msg_5", "m").expect("accumulate");
        assert_eq!(value["content"], json!([]));
        assert_eq!(value["stop_reason"], "end_turn");
    }
}
