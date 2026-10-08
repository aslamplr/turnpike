//! The Anthropic ↔ OpenAI bridge.
//!
//! When a Claude-spec client (e.g. Claude Code via `/v1/messages`) targets a
//! route whose provider speaks the OpenAI spec (e.g. DeepSeek, GLM, Kimi via
//! OpenCode Go), this module translates the request before forwarding and the
//! response/stream after. It follows the same blueprint as Ollama's
//! `anthropic` crate: total conversion at the edges, native passthrough
//! inside.

pub mod stream;

use serde_json::{json, Map, Value};

/// Request intent that has to shape the *response*, carried into the
/// translation layer.
///
/// Both fields exist for the same reason: the Anthropic contract makes the
/// response depend on what the request asked for, and the OpenAI upstream
/// destroys the evidence on the way through. A thinking block is only part of
/// the contract when the client asked for thinking, and a stop-sequence hit is
/// only recoverable if the bridge kept the list instead of forwarding it —
/// OpenAI strips the sequence from the text it returns.
#[derive(Debug, Clone, Default)]
pub struct ResponseOptions {
    /// The client sent `thinking: {type: "enabled" | "adaptive"}`. An absent
    /// parameter and `{"type": "disabled"}` both mean no.
    pub thinking_requested: bool,
    /// The client's `stop_sequences`, which are **not** forwarded upstream.
    /// See `request_to_openai` for why, and `apply_stop_sequences` for what
    /// replaces them.
    pub stop_sequences: Vec<String>,
}

impl ResponseOptions {
    /// Read the options out of an Anthropic request body.
    pub fn from_request(payload: &Value) -> Self {
        Self {
            thinking_requested: matches!(
                payload.pointer("/thinking/type").and_then(Value::as_str),
                Some("enabled") | Some("adaptive")
            ),
            stop_sequences: payload
                .get("stop_sequences")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// Translate an Anthropic Messages request body into an OpenAI
/// chat-completions request. `model` is the upstream (already remapped) id.
pub fn request_to_openai(anthropic: &Value, model: &str) -> Result<Value, String> {
    let mut out = Map::new();
    out.insert("model".into(), json!(model));

    let mut messages: Vec<Value> = Vec::new();

    if let Some(system) = anthropic.get("system") {
        let text = match system {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    for msg in anthropic
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages array is required")?
    {
        translate_message(msg, &mut messages)?;
    }

    // Anthropic `tool_result` blocks arrive inside user messages; OpenAI
    // requires tool messages. translate_message pushes them before the
    // user's own text, which matches the assistant-tool_calls ordering
    // OpenAI expects.
    out.insert("messages".into(), Value::Array(messages));

    if let Some(max) = anthropic.get("max_tokens").and_then(Value::as_u64) {
        out.insert("max_tokens".into(), json!(max));
    }
    if let Some(t) = anthropic.get("temperature") {
        out.insert("temperature".into(), t.clone());
    }
    if let Some(t) = anthropic.get("top_p") {
        out.insert("top_p".into(), t.clone());
    }
    // `stop_sequences` is deliberately **not** forwarded as OpenAI's `stop`.
    //
    // OpenAI documents that "the returned text will not contain the stop
    // sequence", so once it fires the evidence is gone: `finish_reason` is
    // `"stop"` for a natural end and for a stop-sequence hit alike, and the
    // text no longer carries the sequence to match against. The bridge
    // therefore keeps the list and truncates locally (`apply_stop_sequences`,
    // and the stream converter's hold-back buffer), which is what Anthropic
    // does server-side and the only way `stop_reason: "stop_sequence"` can be
    // reported at all. The cost is that the model generates a few tokens past
    // the sequence, bounded by `max_tokens`.

    let mut tools: Vec<Value> = Vec::new();
    if let Some(list) = anthropic.get("tools").and_then(Value::as_array) {
        for t in list {
            // Server-side tool declarations (web_search_*) have no
            // input_schema — they were designed to be executed by the
            // provider. The gateway's agentic middleware executes them
            // instead, so they are kept and given a function schema the
            // model can call. turnpike intercepts the resulting tool calls.
            let mut parameters = t.get("input_schema").cloned().unwrap_or(Value::Null);
            let ttype = t.get("type").and_then(Value::as_str).unwrap_or("");
            if parameters.is_null() && ttype.starts_with("web_search") {
                parameters = json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "The web search query"}
                    },
                    "required": ["query"]
                });
            }
            let function = json!({
                "name": t.get("name"),
                "description": t.get("description").cloned().unwrap_or(Value::Null),
                "parameters": parameters,
            });
            tools.push(json!({"type": "function", "function": function}));
        }
    }
    if let Some(tc) = anthropic.get("tool_choice") {
        if let Some(mapped) = map_tool_choice(tc) {
            // A tool_choice pinned to a tool that didn't survive translation
            // (e.g. a stripped server tool), or any tool_choice with no tools
            // at all, makes upstreams 400 with "Tools cannot be empty if
            // tool choice is set to a specific tool". Drop it instead.
            let referenced = mapped
                .pointer("/function/name")
                .and_then(Value::as_str)
                .map(|name| {
                    tools
                        .iter()
                        .any(|t| t.pointer("/function/name") == Some(&json!(name)))
                });
            let keep = !tools.is_empty() && referenced != Some(false);
            if keep {
                out.insert("tool_choice".into(), mapped);
            } else {
                tracing::warn!(
                    "dropping tool_choice: tools empty or pinned tool absent from translated tools"
                );
            }
        }
    }

    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }

    // Anthropic's `disable_parallel_tool_use` has a direct OpenAI equivalent,
    // but it is a *sibling* of `tools` in chat-completions, not a field of
    // `tool_choice` — so it is read here rather than in `map_tool_choice`.
    // Emitted whenever the client set it, even if `tool_choice` itself was
    // dropped for referencing a tool that did not survive translation: the cap
    // is about how many calls may come back, not about which tool.
    if anthropic
        .pointer("/tool_choice/disable_parallel_tool_use")
        .and_then(Value::as_bool)
        == Some(true)
    {
        out.insert("parallel_tool_calls".into(), json!(false));
    }

    let stream = anthropic
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if stream {
        out.insert("stream".into(), json!(true));
        // Ask for usage in the final chunk when the provider supports it.
        out.insert("stream_options".into(), json!({"include_usage": true}));
    } else {
        out.insert("stream".into(), json!(false));
    }

    // Dropped by design: thinking/output_config (no portable OpenAI
    // equivalent), metadata, and per-block cache_control (upstream ignores
    // them; usage accounting differs).

    Ok(Value::Object(out))
}

fn map_tool_choice(tc: &Value) -> Option<Value> {
    match tc.get("type").and_then(Value::as_str) {
        Some("auto") => Some(json!("auto")),
        Some("none") => Some(json!("none")),
        Some("any") => Some(json!("required")),
        Some("tool") => tc
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        _ => None,
    }
}

/// Convert one Anthropic message into zero or more OpenAI messages.
fn translate_message(msg: &Value, out: &mut Vec<Value>) -> Result<(), String> {
    let role = msg
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user")
        .to_string();

    let mut text = String::new();
    let mut thinking = String::new();
    let mut images: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();
    // Thinking blocks are preserved for OpenAI-spec providers that require
    // reasoning_content to be passed back in the conversation history.

    match msg.get("content") {
        Some(Value::String(s)) => text.push_str(s),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
                match btype {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            text.push_str(t);
                        }
                    }
                    "image" => {
                        let url = image_to_url(block).ok_or("invalid image source")?;
                        images.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                    "tool_use" => {
                        let input = block.get("input").cloned().unwrap_or(json!({}));
                        tool_calls.push(json!({
                            "id": block.get("id").cloned().unwrap_or(json!("call_0")),
                            "type": "function",
                            "function": {
                                "name": block.get("name"),
                                "arguments": serde_json::to_string(
                                    &input).unwrap_or_else(|_| "{}".into()),
                            }
                        }));
                    }
                    "server_tool_use" => {
                        tool_calls.push(json!({
                            "id": block.get("id").cloned().unwrap_or(json!("srv_tool")),
                            "type": "function",
                            "function": {
                                "name": block.get("name"),
                                "arguments": serde_json::to_string(
                                    &block.get("input").cloned().unwrap_or(json!({})))
                                    .unwrap_or_else(|_| "{}".into()),
                            }
                        }));
                    }
                    "tool_result" | "web_search_tool_result" => {
                        let content = match btype {
                            "tool_result" => tool_result_text(block.get("content")),
                            _ => web_result_text(block.get("content")),
                        };
                        tool_results.push(json!({
                            "role": "tool",
                            "tool_call_id": block.get("tool_use_id").cloned().unwrap_or(json!("")),
                            "content": content,
                        }));
                    }
                    "thinking" => {
                        if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                            thinking.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    // Tool results precede the user's own text so they land directly after
    // the assistant's tool_calls message.
    if role == "user" {
        out.append(&mut tool_results);
    }

    // A message that carries only tool_result blocks has no OpenAI-side
    // payload of its own — don't emit an empty user message.
    let has_payload = !tool_calls.is_empty() || !text.is_empty() || !images.is_empty();
    if !has_payload {
        return Ok(());
    }

    let mut message = Map::new();
    message.insert("role".into(), json!(role));
    if !thinking.is_empty() {
        message.insert("reasoning_content".into(), json!(thinking));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
        if text.is_empty() && images.is_empty() {
            message.insert("content".into(), Value::Null);
        }
    }
    if !text.is_empty() || !images.is_empty() {
        let content = if images.is_empty() {
            Value::String(std::mem::take(&mut text))
        } else {
            let mut parts = vec![json!({"type": "text", "text": text})];
            parts.extend(images);
            Value::Array(parts)
        };
        message.insert("content".into(), content);
    }
    out.push(Value::Object(message));

    if !tool_results.is_empty() {
        out.extend(tool_results);
    }
    Ok(())
}

fn image_to_url(block: &Value) -> Option<String> {
    let source = block.get("source")?;
    match source.get("type").and_then(Value::as_str)? {
        "base64" => Some(format!(
            "data:{};base64,{}",
            source.get("media_type")?.as_str()?,
            source.get("data")?.as_str()?
        )),
        "url" => source.get("url").and_then(Value::as_str).map(String::from),
        _ => None,
    }
}

fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(String::from),
                Some(other) => Some(format!("[{other}]")),
                None => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn web_result_text(content: Option<&Value>) -> String {
    let mut out = String::new();
    if let Some(Value::Array(items)) = content {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("web_search_result") {
                out += &format!(
                    "- {}: {}\n",
                    item.get("title").and_then(Value::as_str).unwrap_or(""),
                    item.get("url").and_then(Value::as_str).unwrap_or("")
                );
            }
        }
    }
    out
}

/// Map an OpenAI chat-completions (non-streaming) response to an Anthropic
/// Messages response. `requested_model` is the client-facing id to echo back.
pub fn response_to_anthropic(
    openai: &Value,
    requested_model: &str,
    id: &str,
    opts: &ResponseOptions,
) -> Value {
    let choice = openai
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or(Value::Null);
    let message = choice.get("message").cloned().unwrap_or(Value::Null);

    let mut content: Vec<Value> = Vec::new();
    // Gated on the request: the upstream returns `reasoning_content` on
    // essentially every call, so an ungated bridge surfaces thinking blocks to
    // clients that never asked for them — and to one that explicitly sent
    // `thinking: {type: "disabled"}`.
    if opts.thinking_requested {
        if let Some(thinking) = message.get("reasoning_content").and_then(Value::as_str) {
            if !thinking.is_empty() {
                content.push(json!({
                    "type": "thinking",
                    "thinking": thinking,
                    "signature": synthetic_signature(thinking),
                }));
            }
        }
    }
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let args = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": call.get("id").cloned().unwrap_or(json!("call_0")),
                "name": call.pointer("/function/name").cloned().unwrap_or(json!("")),
                "input": args,
            }));
        }
    }

    let stop_reason = map_finish(
        choice.get("finish_reason").and_then(Value::as_str),
        content
            .iter()
            .any(|c| c.get("type") == Some(&json!("tool_use"))),
    );

    let usage = openai.get("usage").cloned().unwrap_or(Value::Null);
    let input = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64);
    let output = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut response = json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input,
            "cache_read_input_tokens": cached,
            "output_tokens": output,
        }
    });
    apply_stop_sequences(&mut response, &opts.stop_sequences);
    response
}

/// Truncate a translated response at the first stop sequence, the way
/// Anthropic does server-side.
///
/// This is the whole reason the bridge does not forward `stop` upstream: by
/// the time the text comes back, OpenAI has already removed the sequence, so
/// there is nothing left to detect a hit with. Doing it here keeps
/// `stop_reason` and `stop_sequence` truthful instead of reporting `end_turn`
/// for both outcomes.
///
/// A sequence that straddles two adjacent text blocks is not detected — the
/// search is per block. In practice a bridged answer is one text block.
pub fn apply_stop_sequences(response: &mut Value, stops: &[String]) {
    if stops.is_empty() {
        return;
    }
    let Some(blocks) = response.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for i in 0..blocks.len() {
        if blocks[i].get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = blocks[i].get("text").and_then(Value::as_str) else {
            continue;
        };
        let Some((at, matched)) = earliest_stop(text, stops) else {
            continue;
        };
        blocks[i]["text"] = json!(&text[..at]);
        // The turn ended here: anything after the truncation point — a
        // further text block, a tool call — was never part of the answer.
        blocks.truncate(i + 1);
        response["stop_reason"] = json!("stop_sequence");
        response["stop_sequence"] = json!(matched);
        return;
    }
}

/// The earliest-occurring stop sequence in `text`, with its byte offset.
pub fn earliest_stop(text: &str, stops: &[String]) -> Option<(usize, String)> {
    stops
        .iter()
        .filter_map(|s| {
            if s.is_empty() {
                return None;
            }
            text.find(s.as_str()).map(|at| (at, s.clone()))
        })
        .min_by_key(|(at, _)| *at)
}

/// A stand-in for Anthropic's `signature` on a thinking block.
///
/// The bridge synthesizes thinking from the upstream's `reasoning_content`, so
/// there is no real signature to carry — Anthropic's is a cryptographic
/// attestation only it can produce. Emitting a **clearly synthetic** value
/// keeps the block's shape valid for typed consumers, which is the actual
/// divergence; the `turnpike-synthetic:` prefix is what keeps it from ever
/// being mistaken for real provenance.
///
/// Replaying it to a real Anthropic endpoint fails exactly as a missing
/// signature does, so nothing gets worse — and the bridge never forwards a
/// signature upstream anyway (`translate_message` reads only `thinking`).
pub fn synthetic_signature(thinking: &str) -> String {
    let mut input = b"turnpike-synthetic:".to_vec();
    input.extend_from_slice(thinking.as_bytes());
    let digest = ring::digest::digest(&ring::digest::SHA256, &input);
    base64_encode(digest.as_ref())
}

/// Minimal standard-alphabet base64. Hand-rolled rather than pulling in a
/// crate for the one place the bridge needs it.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn map_finish(finish: Option<&str>, has_tools: bool) -> &'static str {
    if has_tools {
        return "tool_use";
    }
    match finish {
        Some("length") => "max_tokens",
        Some("stop") | None => "end_turn",
        _ => "end_turn",
    }
}

/// Translate an OpenAI error body into the Anthropic error shape.
pub fn error_to_anthropic(openai_body: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(openai_body).ok()?;
    let msg = v
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("upstream error")
        .to_string();
    Some((msg, v["type"].to_string()))
}

/// Best-effort unique message id (msg_<hex>), like Ollama's GenerateMessageID.
pub fn new_message_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mixed = nanos ^ (std::process::id() as u128) << 64;
    let hex = format!("{mixed:032x}");
    format!("msg_{}", &hex[..16])
}

/// Frame one Anthropic SSE event.
pub fn format_sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQUEST: &str = r#"{
        "model": "claude-sonnet-5",
        "max_tokens": 256,
        "temperature": 0.5,
        "system": "Be terse.",
        "tools": [
            {"name": "get_weather", "description": "Get weather",
             "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}
        ],
        "tool_choice": {"type": "any"},
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "Weather in Paris?"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_1", "name": "get_weather",
                 "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "18C, sunny"}
            ]}
        ]
    }"#;

    #[test]
    fn request_translation_shapes() {
        let req: Value = serde_json::from_str(REQUEST).unwrap();
        let out = request_to_openai(&req, "deepseek-v4-flash").unwrap();

        assert_eq!(out["model"], "deepseek-v4-flash");
        assert_eq!(out["max_tokens"], 256);
        assert_eq!(out["temperature"], 0.5);

        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "Be terse.");

        // user text + image
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"][0]["type"], "text");
        assert_eq!(
            msgs[1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );

        // assistant tool_call with JSON-stringified arguments
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], Value::Null);
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"city":"Paris"}"#
        );

        // tool result becomes a tool message; no empty user message is
        // emitted for a tool_result-only turn
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[3]["content"], "18C, sunny");

        assert_eq!(out["tools"][0]["type"], "function");
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(out["tools"][0]["function"]["parameters"]["type"], "object");
    }

    /// A non-`text` block inside a `tool_result` is reported, not dropped:
    /// Claude Code's Read-on-a-PNG sends `content: [{type:"image",…}]` with no
    /// text block, and the old filter turned that into `""` — telling the model
    /// the tool returned nothing. An empty result must still stay empty.
    #[test]
    fn tool_result_content_marks_non_text_blocks_and_keeps_empty_empty() {
        fn tool_content(content: &str) -> String {
            let req: Value = serde_json::from_str(&format!(
                r#"{{"model":"m","max_tokens":1,"messages":[
                     {{"role":"user","content":[
                       {{"type":"tool_result","tool_use_id":"call_1","content":{content}}}]}}]}}"#
            ))
            .unwrap();
            let out = request_to_openai(&req, "m").unwrap();
            let msgs = out["messages"].as_array().unwrap();
            assert_eq!(msgs[0]["role"], "tool");
            msgs[0]["content"].as_str().unwrap().to_string()
        }

        // The captured Claude Code shape: an image and nothing else.
        assert_eq!(
            tool_content(
                r#"[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}]"#
            ),
            "[image]"
        );
        // Text survives; the image is marked in place.
        assert_eq!(
            tool_content(r#"[{"type":"text","text":"here it is"},{"type":"image","source":{}}]"#),
            "here it is\n[image]"
        );
        // An unrecognized block type self-describes.
        assert_eq!(
            tool_content(r#"[{"type":"document","source":{}}]"#),
            "[document]"
        );
        // A genuinely empty result stays empty — no false marker.
        assert_eq!(tool_content("[]"), "");
        assert_eq!(tool_content(r#""""#), "");
    }

    #[test]
    fn tool_choice_mapping() {
        let tc = |v: Value| map_tool_choice(&v);
        assert_eq!(tc(json!({"type": "auto"})), Some(json!("auto")));
        assert_eq!(tc(json!({"type": "none"})), Some(json!("none")));
        assert_eq!(tc(json!({"type": "any"})), Some(json!("required")));
        assert_eq!(
            tc(json!({"type": "tool", "name": "f"})),
            Some(json!({"type": "function", "function": {"name": "f"}}))
        );
    }

    #[test]
    fn pinned_tool_choice_dropped_when_tool_absent_or_tools_empty() {
        // No tools at all + pinned tool_choice → tool_choice dropped
        // (regression: upstream 400 "Tools cannot be empty if tool choice is
        // set to a specific tool").
        let req: Value = serde_json::from_str(
            r#"{"model":"m","max_tokens":1,"messages":[],
                "tools":[{"type":"web_search_20250305","name":"web_search"}],
                "tool_choice":{"type":"tool","name":"web_search"}}"#,
        )
        .unwrap();
        let mut stripped = req.clone();
        stripped["tools"] = json!([]); // what strip_server_tools leaves behind
        let out = request_to_openai(&stripped, "m").unwrap();
        assert!(out.get("tools").is_none());
        assert!(out.get("tool_choice").is_none());

        // Pinned to a tool that IS present → kept.
        let kept: Value = serde_json::from_str(
            r#"{"model":"m","max_tokens":1,"messages":[],
                "tools":[{"name":"f","input_schema":{"type":"object"}}],
                "tool_choice":{"type":"tool","name":"f"}}"#,
        )
        .unwrap();
        let out = request_to_openai(&kept, "m").unwrap();
        assert_eq!(out["tool_choice"]["function"]["name"], "f");

        // Pinned to an unknown tool → dropped, tools kept.
        let unknown: Value = serde_json::from_str(
            r#"{"model":"m","max_tokens":1,"messages":[],
                "tools":[{"name":"f","input_schema":{"type":"object"}}],
                "tool_choice":{"type":"tool","name":"nope"}}"#,
        )
        .unwrap();
        let out = request_to_openai(&unknown, "m").unwrap();
        assert!(out["tools"].as_array().unwrap().len() == 1);
        assert!(out.get("tool_choice").is_none());
    }

    #[test]
    fn builtin_web_search_tools_become_function_tools() {
        let req: Value = serde_json::from_str(
            r#"{"model":"m","max_tokens":1,"messages":[],
                "tools":[{"type":"web_search_20250305","name":"web_search"},
                         {"type":"custom","name":"ping","input_schema":{"type":"object"}}]}"#,
        )
        .unwrap();
        let out = request_to_openai(&req, "m").unwrap();
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        // Server tool gets a synthesized query schema the model can call;
        // turnpike's middleware intercepts and executes it.
        assert_eq!(tools[0]["function"]["name"], "web_search");
        assert_eq!(tools[0]["function"]["parameters"]["required"][0], "query");
        assert_eq!(tools[1]["function"]["name"], "ping");
    }

    #[test]
    fn response_translation_shapes() {
        let openai: Value = serde_json::from_str(
            r#"{
            "id": "chatcmpl-9", "object": "chat.completion", "model": "deepseek-v4-flash",
            "choices": [{"index": 0, "finish_reason": "tool_calls",
                "message": {"role": "assistant", "content": "Let me check.",
                    "reasoning_content": "thinking hard",
                    "tool_calls": [{"id": "call_1", "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]}}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7,
                      "prompt_tokens_details": {"cached_tokens": 4}}
        }"#,
        )
        .unwrap();
        let out = response_to_anthropic(
            &openai,
            "claude-sonnet-5",
            "msg_x",
            &ResponseOptions {
                thinking_requested: true,
                ..Default::default()
            },
        );
        assert_eq!(out["type"], "message");
        assert_eq!(out["model"], "claude-sonnet-5");
        assert_eq!(out["content"][0]["type"], "thinking");
        assert_eq!(out["content"][1]["type"], "text");
        assert_eq!(out["content"][2]["type"], "tool_use");
        assert_eq!(out["content"][2]["input"]["city"], "Paris");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["usage"]["input_tokens"], 11);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 4);
        assert_eq!(out["usage"]["output_tokens"], 7);
    }

    #[test]
    fn disable_parallel_tool_use_maps_to_parallel_tool_calls() {
        // Anthropic's flag is a field of `tool_choice`; OpenAI's equivalent is
        // a *sibling* of `tools`. Reading only `type`/`name` (the old
        // `map_tool_choice`) dropped it entirely, so a client asking for
        // exactly one call got parallel calls instead.
        let req: Value = serde_json::from_str(
            r#"{
            "model": "claude-sonnet-5", "max_tokens": 64,
            "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any", "disable_parallel_tool_use": true},
            "messages": [{"role": "user", "content": "hi"}]
        }"#,
        )
        .unwrap();
        let out = request_to_openai(&req, "m").unwrap();
        assert_eq!(out["parallel_tool_calls"], false);
        assert_eq!(out["tool_choice"], "required");

        // Absent means absent: the key is not invented, so the upstream's own
        // default (parallel allowed) stands.
        let req: Value = serde_json::from_str(
            r#"{
            "model": "claude-sonnet-5", "max_tokens": 64,
            "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any"},
            "messages": [{"role": "user", "content": "hi"}]
        }"#,
        )
        .unwrap();
        let out = request_to_openai(&req, "m").unwrap();
        assert!(out.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(map_finish(Some("stop"), false), "end_turn");
        assert_eq!(map_finish(Some("length"), false), "max_tokens");
        assert_eq!(map_finish(Some("stop"), true), "tool_use");
        assert_eq!(map_finish(None, false), "end_turn");
    }
}
