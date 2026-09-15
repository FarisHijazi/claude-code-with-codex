//! `cursor-agent --output-format stream-json` -> Anthropic Messages SSE.
//!
//! Event shapes were read off the installed CLI rather than documentation:
//!
//! ```text
//! {"type":"system","subtype":"init","session_id":"...","model":"Cursor Grok 4.5 High"}
//! {"type":"thinking","subtype":"delta","text":"...","timestamp_ms":...}
//! {"type":"thinking","subtype":"completed","timestamp_ms":...}
//! {"type":"tool_call","subtype":"started","tool_call":{"readToolCall":{"args":{...}}}}
//! {"type":"tool_call","subtype":"completed","tool_call":{"readToolCall":{"args":{...},"result":{...}}}}
//! {"type":"assistant","message":{...},"timestamp_ms":...}   <- delta
//! {"type":"assistant","message":{...}}                      <- settled full text
//! {"type":"result","subtype":"success","is_error":false,"result":"...","usage":{...}}
//! ```
//!
//! # The duplicate-text trap
//!
//! With `--stream-partial-output` the CLI emits per-token `assistant` events
//! and then a **settled event repeating the entire message**. Without that flag
//! only the settled event appears. Emitting both verbatim doubles every reply.
//!
//! `timestamp_ms` looks like it distinguishes the two — it does for a
//! single-step run — but a multi-step agent run was observed emitting the
//! settled message *with* a timestamp, which doubled the text. So the rule is
//! content-based instead, and needs no undocumented field:
//!
//! The translator tracks `segment`, the text of the assistant message being
//! built. An incoming `assistant` event is the settled repeat when its text
//! **equals** the segment, and a superset when it **starts with** it; anything
//! else is a fresh delta and is appended. `segment` resets at every message
//! boundary — a thinking or tool_call event — so it stays scoped to one
//! message across a multi-step run.
//!
//! # Tool calls are not `tool_use`
//!
//! `cursor-agent` runs its own tools inside its own workspace; it cannot be
//! asked to emit a call and wait for an external executor. So tool activity is
//! surfaced as visible text, never as an Anthropic `tool_use` block — Claude
//! Code must not try to execute work the agent has already done.

use serde_json::{Value, json};

use crate::anthropic::sse::encode_sse_event;

fn make_thinking_signature(message_id: &str, index: usize) -> String {
    use base64::Engine;
    let input = format!("ccp:cursor-cli:v1:{message_id}:{index}");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input.as_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    None,
    Thinking(usize),
    Text(usize),
}

#[derive(Debug, Clone, Default)]
pub struct RunUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub struct EventTranslator {
    message_id: String,
    model: String,
    line_buffer: Vec<u8>,
    message_started: bool,
    open: OpenBlock,
    next_index: usize,
    /// Every assistant character forwarded so far. Only used to decide
    /// whether the terminal `result` needs to stand in for a stream that
    /// produced nothing.
    emitted_text: String,
    /// Text of the assistant message currently being built. Reset at each
    /// message boundary, so a settled repeat is recognised per message rather
    /// than against the whole run.
    segment: String,
    usage: RunUsage,
    stop_reason: Option<String>,
    finished: bool,
    session_id: Option<String>,
    resolved_model: Option<String>,
    show_tool_activity: bool,
}

impl EventTranslator {
    pub fn new(message_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            message_id: message_id.into(),
            model: model.into(),
            line_buffer: Vec::new(),
            message_started: false,
            open: OpenBlock::None,
            next_index: 0,
            emitted_text: String::new(),
            segment: String::new(),
            usage: RunUsage::default(),
            stop_reason: None,
            finished: false,
            session_id: None,
            resolved_model: None,
            show_tool_activity: true,
        }
    }

    /// Hide the `> tool …` progress lines, for callers that want only the
    /// final answer.
    pub fn without_tool_activity(mut self) -> Self {
        self.show_tool_activity = false;
        self
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn resolved_model(&self) -> Option<&str> {
        self.resolved_model.as_deref()
    }

    pub fn usage(&self) -> &RunUsage {
        &self.usage
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.line_buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(position) = self.line_buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.line_buffer.drain(..=position).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            if !line.is_empty() {
                self.push_line(&line, &mut out);
            }
        }
        out
    }

    fn push_line(&mut self, line: &str, out: &mut Vec<u8>) {
        // The CLI can interleave non-JSON notices on stdout; ignore them
        // rather than failing the whole turn.
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "system" => self.on_system(&event),
            "thinking" => self.on_thinking(&event, out),
            "assistant" => self.on_assistant(&event, out),
            "tool_call" => self.on_tool_call(&event, out),
            "result" => self.on_result(&event, out),
            _ => {}
        }
    }

    fn on_system(&mut self, event: &Value) {
        if let Some(session) = event.get("session_id").and_then(Value::as_str) {
            self.session_id = Some(session.to_string());
        }
        if let Some(model) = event.get("model").and_then(Value::as_str) {
            self.resolved_model = Some(model.to_string());
        }
    }

    fn on_thinking(&mut self, event: &Value, out: &mut Vec<u8>) {
        match event.get("subtype").and_then(Value::as_str) {
            Some("delta") => {
                let Some(text) = event.get("text").and_then(Value::as_str) else {
                    return;
                };
                if text.is_empty() {
                    return;
                }
                self.end_segment();
                self.emit_thinking_delta(text, out);
            }
            Some("completed") => self.close_open_block(out),
            _ => {}
        }
    }

    fn on_assistant(&mut self, event: &Value, out: &mut Vec<u8>) {
        let Some(text) = assistant_text(event) else {
            return;
        };
        let addition = self.assistant_contribution(&text);
        if addition.is_empty() {
            // The settled repeat of a message already streamed.
            self.segment = text;
            return;
        }
        self.segment.push_str(&addition);
        self.emitted_text.push_str(&addition);
        self.emit_text_delta(&addition, out);
    }

    /// Decide what an `assistant` event contributes, given the message built
    /// so far.
    ///
    /// Equal to the segment -> the settled repeat, contributes nothing.
    /// Extends the segment -> a settled superset, contributes the tail.
    /// Anything else -> a fresh delta, contributes all of itself.
    fn assistant_contribution(&self, text: &str) -> String {
        if self.segment.is_empty() {
            return text.to_string();
        }
        if text == self.segment {
            return String::new();
        }
        if let Some(rest) = text.strip_prefix(self.segment.as_str()) {
            return rest.to_string();
        }
        text.to_string()
    }

    /// A thinking or tool_call event ends the assistant message in progress, so
    /// the next `assistant` text is compared against a fresh segment.
    fn end_segment(&mut self) {
        self.segment.clear();
    }

    fn on_tool_call(&mut self, event: &Value, out: &mut Vec<u8>) {
        if !self.show_tool_activity {
            return;
        }
        // Only announce the start, so a call is reported once.
        if event.get("subtype").and_then(Value::as_str) != Some("started") {
            return;
        }
        let Some(summary) = tool_call_summary(event) else {
            return;
        };
        self.end_segment();
        // Tool activity is context about work already done, so it belongs in
        // the thinking channel rather than the answer.
        let line = format!("{summary}\n");
        self.emit_thinking_delta(&line, out);
    }

    fn on_result(&mut self, event: &Value, out: &mut Vec<u8>) {
        if let Some(usage) = event.get("usage") {
            self.usage.input_tokens = usage
                .get("inputTokens")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            self.usage.output_tokens = usage
                .get("outputTokens")
                .and_then(Value::as_u64)
                .unwrap_or_default();
        }

        let is_error = event
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // `result` repeats the final answer, which the assistant events have
        // already delivered. It is used only as a fallback, when the stream
        // produced no assistant text at all — otherwise it would append a
        // second copy of the reply.
        if self.emitted_text.is_empty()
            && let Some(result) = event.get("result").and_then(Value::as_str)
            && !result.is_empty()
        {
            self.emitted_text.push_str(result);
            self.emit_text_delta(result, out);
        }

        if is_error {
            let detail = event
                .get("result")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| "cursor-agent reported a failure".to_string());
            if self.emitted_text.is_empty() {
                self.emit_text_delta(&format!("cursor-agent error: {detail}"), out);
            }
        }
        self.stop_reason = Some("end_turn".to_string());
        self.finish(out);
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
        let data = json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "text_delta", "text": text }
        });
        self.emit(out, "content_block_delta", &data);
    }

    /// Emit an error as assistant text and terminate. Used when the process
    /// dies or times out, so the client always gets a closed message.
    pub fn fail(&mut self, message: &str, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        self.ensure_message_started(out);
        self.emit_text_delta(message, out);
        self.stop_reason = Some("end_turn".to_string());
        self.finish(out);
    }

    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.ensure_message_started(out);
        self.close_open_block(out);
        let data = json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": self.stop_reason.clone().unwrap_or_else(|| "end_turn".into()),
                "stop_sequence": null
            },
            "usage": {
                "input_tokens": self.usage.input_tokens,
                "output_tokens": self.usage.output_tokens
            }
        });
        self.emit(out, "message_delta", &data);
        self.emit(out, "message_stop", &json!({ "type": "message_stop" }));
    }
}

/// Pull text out of an `assistant` event's content blocks.
fn assistant_text(event: &Value) -> Option<String> {
    let content = event.pointer("/message/content")?.as_array()?;
    let text: String = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    (!text.is_empty()).then_some(text)
}

/// One readable line describing a tool the agent ran, e.g. `> read sample.txt`.
///
/// The payload is a single-key object naming the tool (`readToolCall`,
/// `shellToolCall`, ...), so the key is the tool name.
fn tool_call_summary(event: &Value) -> Option<String> {
    let call = event.get("tool_call")?.as_object()?;
    let (raw_name, body) = call.iter().find(|(key, _)| key.ends_with("ToolCall"))?;
    let name = raw_name.trim_end_matches("ToolCall");
    let detail = body
        .get("args")
        .and_then(|args| {
            args.get("path")
                .or_else(|| args.get("command"))
                .or_else(|| args.get("query"))
                .or_else(|| args.get("pattern"))
                .and_then(Value::as_str)
        })
        .map(shorten)
        .unwrap_or_default();
    Some(if detail.is_empty() {
        format!("> {name}")
    } else {
        format!("> {name} {detail}")
    })
}

fn shorten(value: &str) -> String {
    const LIMIT: usize = 120;
    let single_line = value.replace('\n', " ");
    if single_line.chars().count() <= LIMIT {
        return single_line;
    }
    let truncated: String = single_line.chars().take(LIMIT).collect();
    format!("{truncated}…")
}

/// Translate a complete cursor-agent stream in one call. Used by tests.
pub fn translate_all(input: &[u8], message_id: &str, model: &str) -> Vec<u8> {
    let mut translator = EventTranslator::new(message_id, model);
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

    fn text_of(sse: &[u8]) -> String {
        events(sse)
            .into_iter()
            .filter_map(|(_, data)| {
                data.pointer("/delta/text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    fn thinking_of(sse: &[u8]) -> String {
        events(sse)
            .into_iter()
            .filter_map(|(_, data)| {
                data.pointer("/delta/thinking")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    /// Captured verbatim from the installed CLI with
    /// `--stream-partial-output`: per-token events, then a settled repeat.
    const PARTIAL_STREAM: &str = concat!(
        r#"{"type":"system","subtype":"init","session_id":"sess-1","model":"Cursor Grok 4.5 High"}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
        "\n",
        r#"{"type":"thinking","subtype":"delta","text":"The user wants","timestamp_ms":1}"#,
        "\n",
        r#"{"type":"thinking","subtype":"completed","timestamp_ms":2}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"HELLO"}]},"timestamp_ms":3}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"_"}]},"timestamp_ms":4}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"PROBE"}]},"timestamp_ms":5}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"HELLO_PROBE"}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"HELLO_PROBE","usage":{"inputTokens":12323,"outputTokens":34}}"#,
        "\n",
    );

    /// The single most important behaviour in this file.
    #[test]
    fn settled_message_does_not_duplicate_streamed_text() {
        let sse = translate_all(PARTIAL_STREAM.as_bytes(), "msg_1", "cursor-cli");
        assert_eq!(text_of(&sse), "HELLO_PROBE");
    }

    /// Regression, reproduced from a real multi-step agent run: the settled
    /// message arrived **with** a `timestamp_ms`, so a timestamp-based
    /// discriminator classified it as a delta and emitted the whole sentence a
    /// second time ("I'll read note.txt.I'll read note.txt.").
    #[test]
    fn settled_message_carrying_a_timestamp_does_not_double() {
        let stream = concat!(
            r#"{"type":"thinking","subtype":"delta","text":"Reading note.txt.","timestamp_ms":1}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll read "}]},"timestamp_ms":2}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"note.txt."}]},"timestamp_ms":3}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll read note.txt."}]},"timestamp_ms":4}"#,
            "\n",
            r#"{"type":"tool_call","subtype":"started","tool_call":{"readToolCall":{"args":{"path":"note.txt"}}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"**42**"}]},"timestamp_ms":5}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"**42**"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"**42**","usage":{"inputTokens":9,"outputTokens":3}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_multi", "cursor-cli");
        assert_eq!(text_of(&sse), "I'll read note.txt.**42**");
    }

    /// A second message that happens to repeat the first must still appear
    /// twice — the segment resets at the tool_call boundary.
    #[test]
    fn identical_text_in_two_separate_messages_is_kept() {
        let stream = concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"checking"}]},"timestamp_ms":1}"#,
            "\n",
            r#"{"type":"tool_call","subtype":"started","tool_call":{"readToolCall":{"args":{"path":"a"}}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"checking"}]},"timestamp_ms":2}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"checking","usage":{}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_two", "cursor-cli");
        assert_eq!(text_of(&sse), "checkingchecking");
    }

    /// `result` repeats the final answer; appending it would duplicate the
    /// reply a second time.
    #[test]
    fn result_does_not_append_when_text_already_streamed() {
        let stream = concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"the answer"}]},"timestamp_ms":1}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"the answer","usage":{}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_res", "cursor-cli");
        assert_eq!(text_of(&sse), "the answer");
    }

    /// ...but it must still stand in when nothing else produced text.
    #[test]
    fn result_is_the_fallback_when_no_assistant_text_arrived() {
        let stream = concat!(
            r#"{"type":"thinking","subtype":"delta","text":"hmm","timestamp_ms":1}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"only here","usage":{}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_fb", "cursor-cli");
        assert_eq!(text_of(&sse), "only here");
    }

    /// Regression: an earlier implementation subtracted deltas by substring
    /// search, so a delta that already appeared anywhere in the buffer was
    /// dropped. Token streams are full of repeats — a lone space, `the`, a
    /// newline — and each one silently corrupted the reply.
    #[test]
    fn repeated_short_deltas_are_not_swallowed() {
        let stream = concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"the cat"}]},"timestamp_ms":1}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":" "}]},"timestamp_ms":2}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"the"}]},"timestamp_ms":3}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":" hat"}]},"timestamp_ms":4}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"the cat the hat"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"the cat the hat","usage":{}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_dup", "cursor-cli");
        assert_eq!(text_of(&sse), "the cat the hat");
    }

    /// Thinking deltas are pure increments and must never be de-duplicated.
    #[test]
    fn repeated_thinking_deltas_are_not_swallowed() {
        let stream = concat!(
            r#"{"type":"thinking","subtype":"delta","text":"step one. ","timestamp_ms":1}"#,
            "\n",
            r#"{"type":"thinking","subtype":"delta","text":"step one. ","timestamp_ms":2}"#,
            "\n",
            r#"{"type":"thinking","subtype":"completed","timestamp_ms":3}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_dup2", "cursor-cli");
        assert_eq!(thinking_of(&sse), "step one. step one. ");
    }

    /// Assistant text that neither equals nor extends the current segment is
    /// treated as new content and kept.
    ///
    /// It is indistinguishable, on the wire, from the agent simply starting to
    /// say something else — which is what a multi-step run does. Dropping it
    /// would silently lose model output, so the tie breaks toward keeping it;
    /// the real settled repeat is caught by the equality case above.
    #[test]
    fn text_that_does_not_extend_the_segment_is_kept_as_new_content() {
        let stream = concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"first part. "}]},"timestamp_ms":1}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"second part."}]},"timestamp_ms":2}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"first part. second part."}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"first part. second part.","usage":{}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_div", "cursor-cli");
        // The settled event equals the segment, so it adds nothing.
        assert_eq!(text_of(&sse), "first part. second part.");
    }

    /// Without `--stream-partial-output` only the settled event arrives, so
    /// the same rule must still yield the full text exactly once.
    #[test]
    fn settled_only_stream_emits_the_text_once() {
        let stream = concat!(
            r#"{"type":"system","subtype":"init","session_id":"s","model":"Cursor Grok 4.5 High"}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ALPHA BETA GAMMA"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ALPHA BETA GAMMA","usage":{"inputTokens":33,"outputTokens":36}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_2", "cursor-cli");
        assert_eq!(text_of(&sse), "ALPHA BETA GAMMA");
    }

    #[test]
    fn usage_and_session_and_model_are_captured() {
        let mut translator = EventTranslator::new("msg_3", "cursor-cli");
        let mut out = translator.push_bytes(PARTIAL_STREAM.as_bytes());
        translator.finish(&mut out);
        assert_eq!(translator.session_id(), Some("sess-1"));
        assert_eq!(translator.resolved_model(), Some("Cursor Grok 4.5 High"));
        assert_eq!(translator.usage().input_tokens, 12323);
        assert_eq!(translator.usage().output_tokens, 34);

        let (_, delta) = events(&out)
            .into_iter()
            .find(|(name, _)| name == "message_delta")
            .expect("message_delta");
        assert_eq!(delta["usage"]["input_tokens"], 12323);
        assert_eq!(delta["usage"]["output_tokens"], 34);
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn thinking_becomes_a_signed_thinking_block() {
        let sse = translate_all(PARTIAL_STREAM.as_bytes(), "msg_4", "cursor-cli");
        let events = events(&sse);
        let (_, start) = events
            .iter()
            .find(|(name, _)| name == "content_block_start")
            .expect("content_block_start");
        assert_eq!(start["content_block"]["type"], "thinking");
        assert!(
            events
                .iter()
                .any(|(_, data)| data.pointer("/delta/type") == Some(&json!("signature_delta")))
        );
        assert!(thinking_of(&sse).contains("The user wants"));
    }

    /// Tool activity must be visible, but never as a `tool_use` block: the
    /// agent already ran it.
    #[test]
    fn tool_calls_are_progress_text_not_tool_use_blocks() {
        let stream = concat!(
            r#"{"type":"tool_call","subtype":"started","tool_call":{"readToolCall":{"args":{"path":"/tmp/sample.txt"}}}}"#,
            "\n",
            r#"{"type":"tool_call","subtype":"completed","tool_call":{"readToolCall":{"args":{"path":"/tmp/sample.txt"},"result":{"success":{"content":"hi"}}}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","usage":{"inputTokens":1,"outputTokens":1}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_5", "cursor-cli");
        let blocks: Vec<String> = events(&sse)
            .into_iter()
            .filter(|(name, _)| name == "content_block_start")
            .map(|(_, data)| data["content_block"]["type"].as_str().unwrap_or("").into())
            .collect();
        assert!(!blocks.iter().any(|kind| kind == "tool_use"), "{blocks:?}");
        let thinking = thinking_of(&sse);
        assert!(thinking.contains("> read /tmp/sample.txt"), "{thinking}");
        // Reported once, on `started` only.
        assert_eq!(thinking.matches("> read").count(), 1, "{thinking}");
        assert_eq!(text_of(&sse), "done");
    }

    #[test]
    fn shell_tool_calls_summarize_their_command() {
        let stream = concat!(
            r#"{"type":"tool_call","subtype":"started","tool_call":{"shellToolCall":{"args":{"command":"ls -la"}}}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_6", "cursor-cli");
        assert!(thinking_of(&sse).contains("> shell ls -la"));
    }

    #[test]
    fn error_result_surfaces_as_text() {
        let stream = concat!(
            r#"{"type":"result","subtype":"error","is_error":true,"result":"hit your usage limit","usage":{}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_7", "cursor-cli");
        assert!(text_of(&sse).contains("hit your usage limit"));
    }

    #[test]
    fn stream_split_across_chunk_boundaries_is_reassembled() {
        let mut translator = EventTranslator::new("msg_8", "cursor-cli");
        let bytes = PARTIAL_STREAM.as_bytes();
        let mut out = Vec::new();
        // Feed three bytes at a time to force splits mid-line.
        for chunk in bytes.chunks(3) {
            out.extend(translator.push_bytes(chunk));
        }
        translator.finish(&mut out);
        assert_eq!(text_of(&out), "HELLO_PROBE");
    }

    #[test]
    fn non_json_lines_are_ignored() {
        let stream = concat!(
            "Workspace trust prompt or other prose\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{}}"#,
            "\n",
        );
        let sse = translate_all(stream.as_bytes(), "msg_9", "cursor-cli");
        assert_eq!(text_of(&sse), "ok");
    }

    #[test]
    fn fail_produces_a_closed_message() {
        let mut translator = EventTranslator::new("msg_10", "cursor-cli");
        let mut out = Vec::new();
        translator.fail("cursor-agent timed out after 900s", &mut out);
        let names: Vec<String> = events(&out).into_iter().map(|(name, _)| name).collect();
        assert_eq!(names.first().map(String::as_str), Some("message_start"));
        assert_eq!(names.last().map(String::as_str), Some("message_stop"));
        assert!(text_of(&out).contains("timed out"));
        assert!(translator.is_finished());
    }

    #[test]
    fn finish_after_result_is_idempotent() {
        let mut translator = EventTranslator::new("msg_11", "cursor-cli");
        let mut out = translator.push_bytes(PARTIAL_STREAM.as_bytes());
        let before = out.len();
        translator.finish(&mut out);
        assert_eq!(out.len(), before, "result already finished the message");
    }

    #[test]
    fn tool_activity_can_be_suppressed() {
        let stream = concat!(
            r#"{"type":"tool_call","subtype":"started","tool_call":{"readToolCall":{"args":{"path":"a.txt"}}}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","usage":{}}"#,
            "\n",
        );
        let mut translator = EventTranslator::new("msg_12", "cursor-cli").without_tool_activity();
        let mut out = translator.push_bytes(stream.as_bytes());
        translator.finish(&mut out);
        assert!(thinking_of(&out).is_empty());
        assert_eq!(text_of(&out), "ok");
    }
}
