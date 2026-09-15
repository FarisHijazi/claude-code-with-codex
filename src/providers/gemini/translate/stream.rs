//! OpenAI chat-completions SSE -> Anthropic Messages SSE, incrementally.
//!
//! Translation is streaming rather than buffered because the backend drives a
//! browser session: a reply can take many seconds, and buffering would hold
//! every token until the end.
//!
//! `reasoning_content` is handled even though gemini-web-api does not currently
//! emit it — the base URL is configurable, so the same translator serves any
//! OpenAI-compatible server, and several do emit it.

use serde_json::{Value, json};

use crate::anthropic::sse::encode_sse_event;

/// Anthropic rejects a `thinking` block replayed without a signature. The
/// upstream has none to give, so one is derived from the message id and block
/// index — stable across a replay of the same turn.
fn make_thinking_signature(message_id: &str, index: usize) -> String {
    use base64::Engine;
    let input = format!("ccp:gemini:v1:{message_id}:{index}");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input.as_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    None,
    Thinking(usize),
    Text(usize),
}

#[derive(Debug, Clone)]
struct ToolBlock {
    anthropic_index: usize,
    started: bool,
    id: String,
    name: String,
}

#[derive(Debug, Clone, Default)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}

/// Rough characters-per-token, matching the estimator the other backends use.
const CHARS_PER_TOKEN: u64 = 4;

/// Incremental OpenAI-SSE -> Anthropic-SSE translator.
pub struct StreamTranslator {
    message_id: String,
    model: String,
    buffer: Vec<u8>,
    message_started: bool,
    open: OpenBlock,
    next_index: usize,
    /// Keyed by the upstream `tool_calls[].index`, which is how OpenAI
    /// correlates argument fragments with the call they belong to.
    tools: Vec<(u64, ToolBlock)>,
    usage: Usage,
    /// Estimate used when the upstream reports no usage at all.
    ///
    /// gemini-web-api's stream carries no usage chunk and has no
    /// `stream_options.include_usage`, so without this every gemini turn would
    /// report 0/0 and Claude Code would believe the context was empty.
    fallback_input_tokens: u64,
    output_chars: u64,
    stop_reason: Option<String>,
    finished: bool,
}

impl StreamTranslator {
    pub fn new(message_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            model: model.into(),
            buffer: Vec::new(),
            message_started: false,
            open: OpenBlock::None,
            next_index: 0,
            tools: Vec::new(),
            usage: Usage::default(),
            fallback_input_tokens: 0,
            output_chars: 0,
            stop_reason: None,
            finished: false,
        }
    }

    /// Supply the prompt-size estimate to report if the upstream sends none.
    pub fn with_fallback_input_tokens(mut self, tokens: u64) -> Self {
        self.fallback_input_tokens = tokens;
        self
    }

    /// Feed raw upstream bytes; returns whatever Anthropic SSE is now complete.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=position).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            self.push_line(&line, &mut out);
        }
        out
    }

    fn push_line(&mut self, line: &str, out: &mut Vec<u8>) {
        let Some(payload) = line.strip_prefix("data:") else {
            return;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            return;
        }
        if payload == "[DONE]" {
            self.finish(out);
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        self.push_chunk(&value, out);
    }

    fn push_chunk(&mut self, chunk: &Value, out: &mut Vec<u8>) {
        // Usage can arrive on its own trailing chunk with no choices.
        if let Some(usage) = chunk.get("usage").and_then(Value::as_object) {
            if let Some(value) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                self.usage.input_tokens = value;
            }
            if let Some(value) = usage.get("completion_tokens").and_then(Value::as_u64) {
                self.usage.output_tokens = value;
            }
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return;
        };

        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                self.emit_thinking_delta(text, out);
            }
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                self.emit_text_delta(text, out);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.emit_tool_delta(call, out);
                }
            }
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = Some(map_stop_reason(reason).to_string());
        }
    }

    fn emit(&self, out: &mut Vec<u8>, event: &str, data: &Value) {
        out.extend_from_slice(&encode_sse_event(Some(event), &data.to_string()));
    }

    fn ensure_message_started(&mut self, out: &mut Vec<u8>) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        let data = json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 }
            }
        });
        self.emit(out, "message_start", &data);
    }

    /// Close whichever text/thinking block is open. Tool blocks are closed
    /// separately, at finish, because their argument fragments interleave.
    fn close_open_block(&mut self, out: &mut Vec<u8>) {
        match self.open {
            OpenBlock::None => {}
            OpenBlock::Thinking(index) => {
                let signature = make_thinking_signature(&self.message_id, index);
                let data = json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": { "type": "signature_delta", "signature": signature }
                });
                self.emit(out, "content_block_delta", &data);
                let data = json!({ "type": "content_block_stop", "index": index });
                self.emit(out, "content_block_stop", &data);
            }
            OpenBlock::Text(index) => {
                let data = json!({ "type": "content_block_stop", "index": index });
                self.emit(out, "content_block_stop", &data);
            }
        }
        self.open = OpenBlock::None;
    }

    fn emit_thinking_delta(&mut self, text: &str, out: &mut Vec<u8>) {
        self.ensure_message_started(out);
        let index = match self.open {
            OpenBlock::Thinking(index) => index,
            _ => {
                self.close_open_block(out);
                let index = self.next_index;
                self.next_index += 1;
                let data = json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": { "type": "thinking", "thinking": "" }
                });
                self.emit(out, "content_block_start", &data);
                self.open = OpenBlock::Thinking(index);
                index
            }
        };
        let data = json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "thinking_delta", "thinking": text }
        });
        self.emit(out, "content_block_delta", &data);
    }

    fn emit_text_delta(&mut self, text: &str, out: &mut Vec<u8>) {
        self.ensure_message_started(out);
        let index = match self.open {
            OpenBlock::Text(index) => index,
            _ => {
                self.close_open_block(out);
                let index = self.next_index;
                self.next_index += 1;
                let data = json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": { "type": "text", "text": "" }
                });
                self.emit(out, "content_block_start", &data);
                self.open = OpenBlock::Text(index);
                index
            }
        };
        self.output_chars = self
            .output_chars
            .saturating_add(text.chars().count() as u64);
        let data = json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "text_delta", "text": text }
        });
        self.emit(out, "content_block_delta", &data);
    }

    fn emit_tool_delta(&mut self, call: &Value, out: &mut Vec<u8>) {
        self.ensure_message_started(out);
        let upstream_index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
        let name = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let arguments = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if !self.tools.iter().any(|(key, _)| *key == upstream_index) {
            self.close_open_block(out);
            let anthropic_index = self.next_index;
            self.next_index += 1;
            self.tools.push((
                upstream_index,
                ToolBlock {
                    anthropic_index,
                    started: false,
                    id: String::new(),
                    name: String::new(),
                },
            ));
        }

        let position = self
            .tools
            .iter()
            .position(|(key, _)| *key == upstream_index)
            .expect("tool block was just inserted");

        // id and name may land on the first fragment or be split across it.
        if !id.is_empty() {
            self.tools[position].1.id = id.to_string();
        }
        if !name.is_empty() {
            self.tools[position].1.name.push_str(name);
        }

        // Hold content_block_start until a name exists: Anthropic requires the
        // tool name up front and it cannot be amended afterwards.
        if !self.tools[position].1.started && !self.tools[position].1.name.is_empty() {
            let block = self.tools[position].1.clone();
            let tool_id = if block.id.is_empty() {
                format!("toolu_{}", uuid::Uuid::new_v4().simple())
            } else {
                block.id.clone()
            };
            self.tools[position].1.id = tool_id.clone();
            self.tools[position].1.started = true;
            let data = json!({
                "type": "content_block_start",
                "index": block.anthropic_index,
                "content_block": { "type": "tool_use", "id": tool_id, "name": block.name, "input": {} }
            });
            self.emit(out, "content_block_start", &data);
        }

        if !arguments.is_empty() && self.tools[position].1.started {
            let index = self.tools[position].1.anthropic_index;
            let data = json!({
                "type": "content_block_delta",
                "index": index,
                "delta": { "type": "input_json_delta", "partial_json": arguments }
            });
            self.emit(out, "content_block_delta", &data);
        }
    }

    /// Emit the terminal events. Idempotent, so a `[DONE]` followed by end of
    /// stream does not double-close.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ensure_message_started(out);
        self.close_open_block(out);

        let tools: Vec<ToolBlock> = self
            .tools
            .iter()
            .filter(|(_, block)| block.started)
            .map(|(_, block)| block.clone())
            .collect();
        let had_tools = !tools.is_empty();
        for block in tools {
            let data = json!({ "type": "content_block_stop", "index": block.anthropic_index });
            self.emit(out, "content_block_stop", &data);
        }

        // A reply that called tools but reported "stop" still has to say
        // `tool_use`, or Claude Code will not run them.
        let stop_reason = match self.stop_reason.as_deref() {
            Some("tool_use") => "tool_use",
            _ if had_tools => "tool_use",
            Some(reason) => reason,
            None => "end_turn",
        }
        .to_string();

        // Only estimate what the upstream did not report, so a server that
        // does send usage keeps its exact numbers.
        let input_tokens = if self.usage.input_tokens == 0 {
            self.fallback_input_tokens
        } else {
            self.usage.input_tokens
        };
        let output_tokens = if self.usage.output_tokens == 0 && self.output_chars > 0 {
            (self.output_chars / CHARS_PER_TOKEN).max(1)
        } else {
            self.usage.output_tokens
        };

        let data = json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason, "stop_sequence": null },
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens
            }
        });
        self.emit(out, "message_delta", &data);
        self.emit(out, "message_stop", &json!({ "type": "message_stop" }));
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

fn map_stop_reason(reason: &str) -> &'static str {
    match reason {
        "tool_calls" | "function_call" => "tool_use",
        "length" | "max_tokens" => "max_tokens",
        _ => "end_turn",
    }
}

/// Translate a complete upstream body in one call.
///
/// Test helper only: both real paths drive [`StreamTranslator`] directly so
/// they can supply the usage fallback.
#[cfg(test)]
pub fn translate_stream_bytes(input: &[u8], message_id: &str, model: &str) -> Vec<u8> {
    let mut translator = StreamTranslator::new(message_id, model);
    let mut out = translator.push_bytes(input);
    translator.finish(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(sse: &[u8]) -> Vec<(String, Value)> {
        crate::anthropic::sse::parse_sse_events(sse)
            .into_iter()
            .map(|event| {
                (
                    event.event.unwrap_or_default(),
                    serde_json::from_str(&event.data).unwrap_or(Value::Null),
                )
            })
            .collect()
    }

    fn names(sse: &[u8]) -> Vec<String> {
        events(sse).into_iter().map(|(name, _)| name).collect()
    }

    #[test]
    fn text_stream_produces_a_well_formed_anthropic_message() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"He\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let sse = translate_stream_bytes(upstream.as_bytes(), "msg_1", "gemini-3-pro");
        assert_eq!(
            names(&sse),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let events = events(&sse);
        let text: String = events
            .iter()
            .filter(|(name, _)| name == "content_block_delta")
            .filter_map(|(_, data)| data.pointer("/delta/text").and_then(Value::as_str))
            .collect();
        assert_eq!(text, "Hello");
        let (_, delta) = events
            .iter()
            .find(|(name, _)| name == "message_delta")
            .expect("message_delta");
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(delta["usage"]["input_tokens"], 11);
        assert_eq!(delta["usage"]["output_tokens"], 2);
    }

    #[test]
    fn tool_calls_become_tool_use_blocks_with_json_deltas() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let sse = translate_stream_bytes(upstream.as_bytes(), "msg_2", "gemini-3-pro");
        let events = events(&sse);

        let (_, start) = events
            .iter()
            .find(|(name, _)| name == "content_block_start")
            .expect("content_block_start");
        assert_eq!(start["content_block"]["type"], "tool_use");
        assert_eq!(start["content_block"]["id"], "call_1");
        assert_eq!(start["content_block"]["name"], "read");

        let arguments: String = events
            .iter()
            .filter_map(|(_, data)| data.pointer("/delta/partial_json").and_then(Value::as_str))
            .collect();
        assert_eq!(arguments, r#"{"path":"a.txt"}"#);
        assert!(serde_json::from_str::<Value>(&arguments).is_ok());

        let (_, delta) = events
            .iter()
            .find(|(name, _)| name == "message_delta")
            .expect("message_delta");
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    }

    /// Some servers report `stop` even when they emitted tool calls; Claude
    /// Code would then never execute them.
    #[test]
    fn tool_calls_force_tool_use_even_when_upstream_says_stop() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let sse = translate_stream_bytes(upstream.as_bytes(), "msg_3", "gemini-3-pro");
        let (_, delta) = events(&sse)
            .into_iter()
            .find(|(name, _)| name == "message_delta")
            .expect("message_delta");
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn parallel_tool_calls_get_distinct_block_indices() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let sse = translate_stream_bytes(upstream.as_bytes(), "msg_4", "gemini-3-pro");
        let starts: Vec<(u64, String)> = events(&sse)
            .into_iter()
            .filter(|(name, _)| name == "content_block_start")
            .map(|(_, data)| {
                (
                    data["index"].as_u64().unwrap_or_default(),
                    data["content_block"]["name"].as_str().unwrap_or("").into(),
                )
            })
            .collect();
        assert_eq!(
            starts,
            vec![(0, "read".to_string()), (1, "write".to_string())]
        );
        let stops: Vec<u64> = events(&sse)
            .into_iter()
            .filter(|(name, _)| name == "content_block_stop")
            .filter_map(|(_, data)| data["index"].as_u64())
            .collect();
        assert_eq!(stops, vec![0, 1]);
    }

    #[test]
    fn reasoning_becomes_a_signed_thinking_block() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"pondering\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let sse = translate_stream_bytes(upstream.as_bytes(), "msg_5", "gemini-3-flash-thinking");
        let events = events(&sse);
        let (_, start) = events
            .iter()
            .find(|(name, _)| name == "content_block_start")
            .expect("content_block_start");
        assert_eq!(start["content_block"]["type"], "thinking");
        assert!(
            events
                .iter()
                .any(|(_, data)| data.pointer("/delta/type") == Some(&json!("signature_delta"))),
            "thinking block must be signed"
        );
        // thinking closes at index 0, text opens at index 1.
        let text_start = events
            .iter()
            .filter(|(name, _)| name == "content_block_start")
            .nth(1)
            .expect("text block");
        assert_eq!(text_start.1["index"], 1);
        assert_eq!(text_start.1["content_block"]["type"], "text");
    }

    #[test]
    fn chunks_split_mid_line_are_reassembled() {
        let mut translator = StreamTranslator::new("msg_6", "gemini-3-pro");
        let mut out = translator.push_bytes(b"data: {\"choices\":[{\"delta\":{\"cont");
        out.extend(translator.push_bytes(b"ent\":\"split\"}}]}\n\n"));
        out.extend(translator.push_bytes(b"data: [DONE]\n\n"));
        let text: String = events(&out)
            .into_iter()
            .filter_map(|(_, data)| {
                data.pointer("/delta/text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(text, "split");
        assert!(translator.is_finished());
    }

    #[test]
    fn finish_is_idempotent() {
        let mut translator = StreamTranslator::new("msg_7", "gemini-3-pro");
        let mut out = translator.push_bytes(b"data: [DONE]\n\n");
        let before = out.len();
        translator.finish(&mut out);
        assert_eq!(out.len(), before, "second finish must emit nothing");
    }

    #[test]
    fn empty_stream_still_produces_a_valid_message() {
        let sse = translate_stream_bytes(b"", "msg_8", "gemini-3-pro");
        assert_eq!(
            names(&sse),
            vec!["message_start", "message_delta", "message_stop"]
        );
    }

    /// gemini-web-api sends no usage chunk; 0/0 would make Claude Code think
    /// the context was empty.
    #[test]
    fn missing_usage_falls_back_to_an_estimate() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"12345678\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut translator =
            StreamTranslator::new("msg_u", "gemini-3-pro").with_fallback_input_tokens(123);
        let mut out = translator.push_bytes(upstream.as_bytes());
        translator.finish(&mut out);
        let (_, delta) = events(&out)
            .into_iter()
            .find(|(name, _)| name == "message_delta")
            .expect("message_delta");
        assert_eq!(delta["usage"]["input_tokens"], 123);
        // 8 characters at ~4 chars/token.
        assert_eq!(delta["usage"]["output_tokens"], 2);
    }

    /// A server that does report usage must keep its exact numbers.
    #[test]
    fn reported_usage_is_never_overwritten_by_the_estimate() {
        let upstream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello there\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        );
        let mut translator =
            StreamTranslator::new("msg_u2", "gemini-3-pro").with_fallback_input_tokens(999);
        let mut out = translator.push_bytes(upstream.as_bytes());
        translator.finish(&mut out);
        let (_, delta) = events(&out)
            .into_iter()
            .find(|(name, _)| name == "message_delta")
            .expect("message_delta");
        assert_eq!(delta["usage"]["input_tokens"], 11);
        assert_eq!(delta["usage"]["output_tokens"], 3);
    }

    #[test]
    fn length_finish_maps_to_max_tokens() {
        let upstream = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
        let sse = translate_stream_bytes(upstream.as_bytes(), "msg_9", "gemini-3-pro");
        let (_, delta) = events(&sse)
            .into_iter()
            .find(|(name, _)| name == "message_delta")
            .expect("message_delta");
        assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn malformed_json_lines_are_skipped() {
        let upstream = concat!(
            "data: not json at all\n\n",
            ": a comment line\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let sse = translate_stream_bytes(upstream.as_bytes(), "msg_10", "gemini-3-pro");
        let text: String = events(&sse)
            .into_iter()
            .filter_map(|(_, data)| {
                data.pointer("/delta/text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(text, "ok");
    }
}
