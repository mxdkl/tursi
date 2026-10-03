//! OpenAI-compatible chat/completions adapter: SSE streaming, tool calls.
//! Providers with automatic prefix caching (DeepSeek-style) reward the §8.4
//! stable-prefix discipline with no extra work here.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{ApiError, ChatRequest, Message, StreamEvent, ToolCall, Turn, Usage};

pub struct Client {
    http: reqwest::Client,
    key: String,
    base_url: String,
}

/// Accumulates one tool call from its streamed fragments.
#[derive(Default)]
struct ToolCallFrag {
    id: String,
    name: String,
    arguments: String,
}

impl Client {
    pub fn new(key: String, base_url: String) -> Client {
        Client { http: reqwest::Client::new(), key, base_url }
    }

    /// POST /chat/completions (stream: true), parse SSE deltas, forward text
    /// to `events`, accumulate the settled `Turn` with usage.
    pub async fn chat(&self, req: ChatRequest, events: mpsc::Sender<StreamEvent>) -> Result<Turn> {
        let body = build_body(&req);
        let mut resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .context("chat request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ApiError { status, body: crate::output::redact(&text) }.into());
        }

        let mut stream = SseTurn::default();
        while !stream.done {
            let Some(chunk) = resp.chunk().await? else { break };
            for event in stream.feed(&chunk) {
                let _ = events.send(event).await;
            }
        }
        Ok(stream.finish())
    }
}

/// One streamed response, assembled from raw SSE bytes.
#[derive(Default)]
struct SseTurn {
    /// Raw bytes, decoded a whole line at a time: a chunk boundary can split a
    /// multi-byte character, and decoding per chunk would mangle it.
    buffer: Vec<u8>,
    text: String,
    frags: Vec<ToolCallFrag>,
    usage: Usage,
    finish_reason: String,
    /// `[DONE]` seen.
    done: bool,
}

impl SseTurn {
    /// Consume one network chunk; returns the UI events it produced.
    fn feed(&mut self, chunk: &[u8]) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        self.buffer.extend_from_slice(chunk);
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buffer.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw);
            let Some(payload) = line.trim().strip_prefix("data:").map(str::trim) else { continue };
            if payload == "[DONE]" {
                self.done = true;
                break;
            }
            let event: Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(_) => continue, // tolerate keep-alives / partial noise
            };
            if let Some(u) = event.get("usage").filter(|u| !u.is_null()) {
                self.usage = parse_usage(u);
            }
            if let Some(reason) = event.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
                self.finish_reason = reason.to_string();
            }
            let Some(delta) = event.pointer("/choices/0/delta") else { continue };
            if let Some(t) = delta.get("content").and_then(Value::as_str) {
                if !t.is_empty() {
                    self.text.push_str(t);
                    out.push(StreamEvent::TextDelta(t.to_string()));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    while self.frags.len() <= index {
                        self.frags.push(ToolCallFrag::default());
                    }
                    let frag = &mut self.frags[index];
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        frag.id.push_str(id);
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                        if frag.name.is_empty() {
                            out.push(StreamEvent::ToolCallStarted { name: name.to_string() });
                        }
                        frag.name.push_str(name);
                    }
                    if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                        frag.arguments.push_str(args);
                    }
                }
            }
        }
        out
    }

    /// The settled turn. A call whose arguments don't parse is marked
    /// malformed — with the token-limit explanation when the response was cut
    /// off (finish_reason "length"), which is the usual cause.
    fn finish(self) -> Turn {
        let cut_off = self.finish_reason == "length";
        let tool_calls = self
            .frags
            .into_iter()
            .filter(|f| !f.name.is_empty())
            .map(|f| {
                let (arguments, malformed) = match parse_arguments(&f.name, &f.arguments) {
                    Ok(v) => (v, None),
                    Err(_) if cut_off => (json!({}), Some(TRUNCATED_CALL.to_string())),
                    Err(e) => (
                        json!({}),
                        Some(format!("arguments were not valid JSON ({e}) — nothing ran; send the call again")),
                    ),
                };
                ToolCall { id: f.id, name: f.name, arguments, malformed }
            })
            .collect();
        Turn { text: self.text, tool_calls, usage: self.usage }
    }
}

/// What the model hears when its output hit the token limit mid-call.
const TRUNCATED_CALL: &str = "this call was cut off: your output hit the model's token limit before \
     its arguments were complete — nothing ran. Split the work into smaller calls (e.g. write a \
     large file in parts: a write, then edits that append).";

/// Parse a tool call's streamed `arguments` into JSON, tolerantly.
///
/// Models occasionally emit raw control characters (a literal newline, tab, or
/// NUL) *inside* JSON string values — common when an `edit`/`write` argument
/// carries code. Strict JSON (RFC 8259) forbids unescaped control chars in
/// strings, so `serde_json` rejected the call and the error propagated out,
/// killing the whole task (the "clack" regression). We first escape any raw
/// control char that appears inside a string, then parse.
///
/// If it still won't parse (cut off at the token limit, genuinely malformed),
/// we do NOT fail the task: the caller marks the call malformed and the
/// executor answers it with an error result saying why, so the model retries —
/// one bad tool call must never sink a run. Empty arguments parse to `{}`.
fn parse_arguments(name: &str, raw: &str) -> std::result::Result<Value, serde_json::Error> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    let sanitized = escape_raw_control_chars(raw);
    serde_json::from_str(&sanitized).inspect_err(|e| {
        tracing::warn!(tool = %name, "tool-call arguments unparseable after sanitizing ({e})");
    })
}

/// Escape raw control characters (U+0000–U+001F) that appear *inside* JSON
/// string literals, leaving structural characters between tokens untouched.
/// A backslash escape sequence is passed through verbatim so already-escaped
/// content (`\n`, `\uXXXX`) is never double-escaped.
fn escape_raw_control_chars(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false; // previous char (in a string) was a backslash
    for ch in raw.chars() {
        if !in_string {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
            continue;
        }
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => {
                out.push(ch);
                escaped = true;
            }
            '"' => {
                out.push(ch);
                in_string = false;
            }
            c if (c as u32) < 0x20 => match c {
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0C}' => out.push_str("\\f"),
                other => out.push_str(&format!("\\u{:04x}", other as u32)),
            },
            _ => out.push(ch),
        }
    }
    out
}

/// Our Message enum → the OpenAI wire shape. The model id drops its provider
/// prefix ("deepseek/deepseek-chat" → "deepseek-chat").
fn build_body(req: &ChatRequest) -> Value {
    let model = req.model.split_once('/').map(|(_, m)| m).unwrap_or(&req.model);
    let messages: Vec<Value> = req.messages.iter().map(to_wire).collect();
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "function": {
                        "name": t.name, "description": t.description, "parameters": t.parameters,
                    }})
                })
                .collect(),
        );
    }
    body
}

fn to_wire(msg: &Message) -> Value {
    match msg {
        Message::System(s) => json!({"role": "system", "content": s}),
        Message::User(s) => json!({"role": "user", "content": s}),
        Message::Assistant { text, tool_calls } => {
            let mut m = json!({"role": "assistant", "content": text});
            if !tool_calls.is_empty() {
                m["tool_calls"] = Value::Array(
                    tool_calls
                        .iter()
                        .map(|c| {
                            json!({"id": c.id, "type": "function", "function": {
                                "name": c.name, "arguments": c.arguments.to_string(),
                            }})
                        })
                        .collect(),
                );
            }
            m
        }
        Message::ToolResult { call_id, content, is_error } => {
            // The wire has no error flag; the prefix is the contract.
            let content = if *is_error { format!("ERROR: {content}") } else { content.clone() };
            json!({"role": "tool", "tool_call_id": call_id, "content": content})
        }
    }
}

/// OpenAI reports cached tokens inside prompt_tokens; DeepSeek also exposes
/// prompt_cache_hit_tokens. Normalize to: input = uncached, cached = cached.
fn parse_usage(u: &Value) -> Usage {
    let prompt = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cached = u
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| u.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    Usage {
        input_tokens: prompt.saturating_sub(cached),
        cached_tokens: cached,
        output_tokens: u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_control_chars_inside_strings_are_escaped_and_parse() {
        // The clack shape: an `edit` whose `new` value carries a LITERAL newline
        // and tab — invalid JSON as-is, previously fatal to the whole task.
        let raw = "{\"path\":\"a.rs\",\"new\":\"line1\nline2\tend\"}";
        let v = parse_arguments("edit", raw).unwrap();
        assert_eq!(v["path"], "a.rs");
        // Content round-trips with the real control characters intact.
        assert_eq!(v["new"], "line1\nline2\tend");
    }

    #[test]
    fn already_escaped_sequences_are_not_double_escaped() {
        let raw = r#"{"new":"a\nb","u":"A"}"#;
        let v = parse_arguments("edit", raw).unwrap();
        assert_eq!(v["new"], "a\nb"); // \n stays a single newline, not \\n
        assert_eq!(v["u"], "A");
    }

    #[test]
    fn control_chars_between_tokens_do_not_break_parsing() {
        // Whitespace/newlines OUTSIDE strings are legal JSON — leave them be.
        let raw = "{\n  \"path\": \"a.rs\"\n}";
        assert_eq!(parse_arguments("edit", raw).unwrap()["path"], "a.rs");
    }

    fn sse(event: Value) -> Vec<u8> {
        format!("data: {event}\n\n").into_bytes()
    }

    #[test]
    fn a_multibyte_char_split_across_chunks_survives() {
        let mut bytes = sse(json!({"choices": [{"delta": {"content": "a — ✗ b"}}]}));
        bytes.extend(b"data: [DONE]\n\n");
        // Split inside the 3-byte '—' (and every other position).
        for cut in 1..bytes.len() {
            let mut turn = SseTurn::default();
            turn.feed(&bytes[..cut]);
            turn.feed(&bytes[cut..]);
            assert!(turn.done);
            assert_eq!(turn.finish().text, "a — ✗ b", "cut at {cut}");
        }
    }

    #[test]
    fn a_call_cut_off_at_the_token_limit_is_marked_malformed_with_the_reason() {
        let mut turn = SseTurn::default();
        turn.feed(&sse(json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "c1", "function": {"name": "write", "arguments": "{\"file\":\"a.rs\",\"content\":\"fn ma"}}
        ]}}]})));
        turn.feed(&sse(json!({"choices": [{"delta": {}, "finish_reason": "length"}]})));
        let call = &turn.finish().tool_calls[0];
        assert_eq!(call.name, "write");
        assert!(call.malformed.as_deref().is_some_and(|m| m.contains("token limit")), "{:?}", call.malformed);

        // Same broken JSON without a cut-off is reported as invalid JSON.
        let mut turn = SseTurn::default();
        turn.feed(&sse(json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "c1", "function": {"name": "write", "arguments": "{\"file\":"}}
        ]}}]})));
        let call = &turn.finish().tool_calls[0];
        assert!(call.malformed.as_deref().is_some_and(|m| m.contains("not valid JSON")));
    }

    #[test]
    fn truly_unparseable_args_are_an_error_not_a_panic() {
        // Truncated stream: not fatal — the caller marks the call malformed.
        assert!(parse_arguments("edit", "{\"path\":\"a").is_err());
        assert_eq!(parse_arguments("edit", "").unwrap(), json!({}));
    }

    /// Live smoke test — costs a fraction of a cent. Run explicitly:
    /// `set -a; source .env; set +a; cargo test deepseek_smoke -- --ignored`
    #[tokio::test]
    #[ignore = "live: needs DEEPSEEK_API_KEY and network"]
    async fn deepseek_smoke_streams_text_and_reports_usage() {
        let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY not set");
        let client = Client::new(key, "https://api.deepseek.com/v1".to_string());
        let req = ChatRequest {
            model: "deepseek/deepseek-chat".to_string(),
            messages: vec![Message::User("Reply with exactly one word: pong".to_string())],
            tools: vec![],
        };
        let (tx, mut rx) = mpsc::channel(64);
        let turn = client.chat(req, tx).await.unwrap();
        assert!(turn.text.to_lowercase().contains("pong"), "got: {}", turn.text);
        assert!(turn.usage.output_tokens > 0);
        assert!(turn.tool_calls.is_empty());
        let mut saw_delta = false;
        while let Ok(e) = rx.try_recv() {
            if matches!(e, StreamEvent::TextDelta(_)) {
                saw_delta = true;
            }
        }
        assert!(saw_delta, "no streaming deltas arrived");
    }
}
