//! Streaming bridge: OpenAI chat-completions SSE chunks → Anthropic SSE
//! events. Stateful, mirroring Ollama's `StreamConverter`: block indexing,
//! thinking→text→tool_use transitions, `input_json_delta` accumulation, and
//! final usage/stop-reason reporting.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::{earliest_stop, format_sse, synthetic_signature, ResponseOptions};

/// One converted SSE event ready to be framed and flushed.
pub struct Event {
    pub name: String,
    pub data: Value,
}

enum OpenBlock {
    Thinking(usize),
    Text(usize),
    Tool(usize),
}

struct ToolState {
    anthropic_index: usize,
}

pub struct StreamConverter {
    id: String,
    model: String,
    input_estimate: u64,

    started: bool,
    block_index: usize,
    open_block: Option<OpenBlock>,
    tools: std::collections::HashMap<u64, ToolState>,
    finished: bool,
    /// Real usage from the provider, captured from usage-bearing chunks
    /// (stream_options.include_usage sends it in a final choices-less chunk).
    usage: Option<Value>,

    /// Emit thinking blocks only when the request asked for them. The upstream
    /// returns `reasoning_content` on essentially every call, so an ungated
    /// converter surfaces thinking to clients that never asked — including one
    /// that sent `thinking: {type: "disabled"}`.
    thinking_requested: bool,
    /// The request's `stop_sequences`. Deliberately not forwarded upstream:
    /// OpenAI strips the sequence from the text it returns, so a hit can only
    /// be recovered by detecting it here.
    stop_sequences: Vec<String>,
    /// `max(len(stop_sequences)) - 1`. A sequence can straddle two deltas, so
    /// that many characters are held back rather than emitted.
    hold_back: usize,
    /// Text received but not yet safe to emit.
    pending_text: String,
    /// The open thinking block's text so far, for its synthetic signature.
    thinking_text: String,
    /// The stop sequence that fired, if any. Its presence also means the
    /// caller must stop reading the upstream — the sequence was never
    /// forwarded, so the model is still generating.
    stop_override: Option<String>,

    /// Tool names the *gateway* executes (the search middleware). A call to
    /// one of these opens Anthropic's `server_tool_use` block rather than
    /// `tool_use`, because the client must not be asked to run it.
    server_tools: Vec<String>,
    /// When true, an iteration's `finish_reason` closes its blocks but leaves
    /// the message open: the middleware loop may follow it with another
    /// iteration on the same stream. See `end_iteration`/`finish_message`.
    defer_finish: bool,
    /// The iteration that just finished, held for `end_iteration`.
    iteration_stop: Option<String>,
    iteration_usage: Option<Value>,
}

impl StreamConverter {
    pub fn new(id: String, model: String, input_estimate: u64) -> Self {
        Self {
            id,
            model,
            input_estimate,
            started: false,
            block_index: 0,
            open_block: None,
            tools: HashMap::new(),
            finished: false,
            usage: None,
            thinking_requested: false,
            stop_sequences: Vec::new(),
            hold_back: 0,
            pending_text: String::new(),
            thinking_text: String::new(),
            stop_override: None,
            server_tools: Vec::new(),
            defer_finish: false,
            iteration_stop: None,
            iteration_usage: None,
        }
    }

    /// Apply the request's intent to how the response is shaped.
    pub fn with_options(mut self, opts: &ResponseOptions) -> Self {
        self.thinking_requested = opts.thinking_requested;
        self.stop_sequences = opts.stop_sequences.clone();
        self.hold_back = self
            .stop_sequences
            .iter()
            .map(|s| s.chars().count())
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        self
    }

    /// True once a stop sequence has fired. The caller must stop reading the
    /// upstream: `stop` was not forwarded, so the model is still generating.
    pub fn stopped(&self) -> bool {
        self.stop_override.is_some()
    }

    /// Names the gateway executes itself. Their tool calls open
    /// `server_tool_use`, Anthropic's block for a tool the *server* ran.
    pub fn with_server_tools(mut self, names: Vec<String>) -> Self {
        self.server_tools = names;
        self
    }

    /// Leave the message open across iterations, for the middleware loop.
    ///
    /// Without this the first iteration's `finish_reason` would emit
    /// `message_delta`/`message_stop` and end the client's stream after the
    /// model's first search request — the loop could never answer it.
    pub fn defer_finish(mut self, on: bool) -> Self {
        self.defer_finish = on;
        self
    }

    /// The iteration that just finished: its stop reason and its usage.
    ///
    /// Also resets the per-iteration tool map, so the next iteration's
    /// `tool_calls[0]` does not inherit this one's block index.
    pub fn end_iteration(&mut self) -> (Option<String>, Value) {
        self.tools.clear();
        (
            self.iteration_stop.take(),
            self.iteration_usage.take().unwrap_or(Value::Null),
        )
    }

    /// Terminate the message once the loop is done, with usage summed across
    /// every iteration.
    pub fn finish_message(&mut self, stop_reason: &str, usage: &Value) -> String {
        if self.finished {
            return String::new();
        }
        let mut events = Vec::new();
        self.close_open_block(&mut events);
        let (stop, seq) = match self.stop_override.take() {
            Some(seq) => ("stop_sequence".to_string(), json!(seq)),
            None => (stop_reason.to_string(), Value::Null),
        };
        events.push(Event {
            name: "message_delta".into(),
            data: json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop, "stop_sequence": seq},
                "usage": usage,
            }),
        });
        events.push(Event {
            name: "message_stop".into(),
            data: json!({"type": "message_stop"}),
        });
        self.finished = true;
        events
            .iter()
            .map(|e| format_sse(&e.name, &e.data))
            .collect()
    }

    /// Emit a complete text block: start → one `text_delta` → stop.
    ///
    /// Distinct from `emit_block` because Anthropic's text grammar carries the
    /// content in a delta, not in `content_block_start` — a client assembling
    /// from deltas would otherwise see an empty block. The middleware loop uses
    /// this for its placeholder when a turn produced only searches.
    pub fn emit_text_block(&mut self, text: &str) -> Vec<Event> {
        let mut events = Vec::new();
        self.close_open_block(&mut events);
        let idx = self.block_index;
        self.block_index += 1;
        events.push(Event {
            name: "content_block_start".into(),
            data: json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {"type": "text", "text": ""},
            }),
        });
        events.push(Event {
            name: "content_block_delta".into(),
            data: json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {"type": "text_delta", "text": text},
            }),
        });
        events.push(Event {
            name: "content_block_stop".into(),
            data: json!({"type": "content_block_stop", "index": idx}),
        });
        events
    }

    /// Emit a complete block: one whose whole payload rides in
    /// `content_block_start`, with no deltas.
    ///
    /// `web_search_tool_result` is the shape this exists for, and it is what
    /// Anthropic's own server-tool grammar does with a search result.
    pub fn emit_block(&mut self, block: &Value) -> Vec<Event> {
        let mut events = Vec::new();
        self.close_open_block(&mut events);
        let idx = self.block_index;
        self.block_index += 1;
        events.push(Event {
            name: "content_block_start".into(),
            data: json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": block,
            }),
        });
        events.push(Event {
            name: "content_block_stop".into(),
            data: json!({"type": "content_block_stop", "index": idx}),
        });
        events
    }

    /// Consume one OpenAI chat chunk, returning the Anthropic events it maps to.
    pub fn process(&mut self, chunk: &Value) -> Vec<Event> {
        if self.finished {
            return Vec::new();
        }
        let mut events = Vec::new();

        if !self.started {
            self.started = true;
            events.push(Event {
                name: "message_start".into(),
                data: json!({
                    "type": "message_start",
                    "message": {
                        "id": self.id,
                        "type": "message",
                        "role": "assistant",
                        "model": self.model,
                        "content": [],
                        "usage": {
                            "input_tokens": self.input_estimate,
                            "output_tokens": 0,
                        }
                    }
                }),
            });
        }

        let choice = match chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            Some(c) => c.clone(),
            None => {
                // Usage-only chunk (include_usage): stash the real usage for
                // the terminating message_delta.
                if let Some(u) = chunk.get("usage") {
                    if !u.is_null() {
                        self.usage = Some(u.clone());
                    }
                }
                return events;
            }
        };
        if let Some(u) = chunk.get("usage") {
            if !u.is_null() {
                self.usage = Some(u.clone());
            }
        }
        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

        // Reasoning deltas (DeepSeek-style) map to Anthropic thinking blocks —
        // but only when the client asked for thinking.
        if self.thinking_requested {
            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
                if !reasoning.is_empty() && !self.finished {
                    if let Some(OpenBlock::Text(_)) = self.open_block {
                        self.close_open_block(&mut events);
                    }
                    if !matches!(self.open_block, Some(OpenBlock::Thinking(_))) {
                        let idx = self.block_index;
                        self.block_index += 1;
                        events.push(Event {
                            name: "content_block_start".into(),
                            data: json!({
                                "type": "content_block_start",
                                "index": idx,
                                "content_block": {"type": "thinking", "thinking": ""},
                            }),
                        });
                        self.open_block = Some(OpenBlock::Thinking(idx));
                        self.thinking_text.clear();
                    }
                    // Accumulated so the block can be closed with a signature
                    // over its own text.
                    self.thinking_text.push_str(reasoning);
                    if let Some(OpenBlock::Thinking(idx)) = self.open_block {
                        events.push(Event {
                            name: "content_block_delta".into(),
                            data: json!({
                                "type": "content_block_delta",
                                "index": idx,
                                "delta": {"type": "thinking_delta", "thinking": reasoning},
                            }),
                        });
                    }
                }
            }
        }

        // Text deltas, held back far enough that a stop sequence straddling two
        // deltas is still detected.
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() && !self.finished {
                self.push_text(text, &mut events);
            }
        }

        // Tool call fragments.
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let openai_idx = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                if !self.tools.contains_key(&openai_idx) {
                    // A new tool call closes any open text/thinking/tool block.
                    self.close_open_block(&mut events);
                    let anthropic_index = self.block_index;
                    self.block_index += 1;
                    self.tools.insert(openai_idx, ToolState { anthropic_index });
                    self.open_block = Some(OpenBlock::Tool(anthropic_index));
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .unwrap_or_else(|| format!("call_{openai_idx}"));
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    // A tool the gateway executes is Anthropic's
                    // `server_tool_use`; the client must not be asked to run it.
                    let block_type = if self.server_tools.iter().any(|t| t == name) {
                        "server_tool_use"
                    } else {
                        "tool_use"
                    };
                    events.push(Event {
                        name: "content_block_start".into(),
                        data: json!({
                            "type": "content_block_start",
                            "index": anthropic_index,
                            "content_block": {"type": block_type, "id": id, "name": name, "input": {}},
                        }),
                    });
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    if !args.is_empty() {
                        let idx = self.tools[&openai_idx].anthropic_index;
                        events.push(Event {
                            name: "content_block_delta".into(),
                            data: json!({
                                "type": "content_block_delta",
                                "index": idx,
                                "delta": {"type": "input_json_delta", "partial_json": args},
                            }),
                        });
                    }
                }
            }
        }

        // Finish.
        if let Some(finish) = choice.get("finish_reason") {
            if finish.is_null() {
                return events;
            }
            self.close_open_block(&mut events);
            let has_tools = !self.tools.is_empty();
            // A stop sequence that fired in this same chunk wins over
            // `finish_reason`: `stop` was never forwarded, so the upstream's
            // `"stop"` says nothing about *why* it stopped.
            let (stop, seq) = match self.stop_override.take() {
                Some(seq) => ("stop_sequence".to_string(), json!(seq)),
                None => (
                    super::map_finish(finish.as_str(), has_tools).to_string(),
                    Value::Null,
                ),
            };
            let usage = chunk
                .get("usage")
                .filter(|u| !u.is_null())
                .cloned()
                .or_else(|| self.usage.take())
                .unwrap_or(Value::Null);
            // Anthropic semantics via the shared splitter: `input_tokens` is
            // the uncached remainder, `cache_read_input_tokens` the reads —
            // omitted entirely when the upstream reported no cache data.
            let (input, cached, output) = super::split_usage(&usage);
            let mut usage_out = json!({"input_tokens": input, "output_tokens": output});
            if let Some(c) = cached {
                usage_out["cache_read_input_tokens"] = json!(c);
            }
            // The middleware loop drives the message's lifetime itself: this
            // iteration's blocks are closed, but a search round may follow on
            // the same stream, so the message must stay open.
            if self.defer_finish {
                self.iteration_stop = Some(stop);
                self.iteration_usage = Some(usage_out);
                return events;
            }
            events.push(Event {
                name: "message_delta".into(),
                data: json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": stop, "stop_sequence": seq},
                    "usage": usage_out,
                }),
            });
            events.push(Event {
                name: "message_stop".into(),
                data: json!({"type": "message_stop"}),
            });
            self.finished = true;
        }

        events
    }

    /// Close out the stream if the upstream ended without a finish_reason.
    pub fn finish(&mut self) -> String {
        if self.finished {
            return String::new();
        }
        let mut events = Vec::new();
        self.close_open_block(&mut events);
        let (stop, seq) = match self.stop_override.take() {
            Some(seq) => ("stop_sequence".to_string(), json!(seq)),
            None => ("end_turn".to_string(), Value::Null),
        };
        events.push(Event {
            name: "message_delta".into(),
            data: json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop, "stop_sequence": seq},
                "usage": {"input_tokens": self.input_estimate, "output_tokens": 0},
            }),
        });
        events.push(Event {
            name: "message_stop".into(),
            data: json!({"type": "message_stop"}),
        });
        self.finished = true;
        events
            .iter()
            .map(|e| format_sse(&e.name, &e.data))
            .collect()
    }

    /// Append upstream text and emit as much of it as is safe to emit.
    fn push_text(&mut self, text: &str, events: &mut Vec<Event>) {
        self.pending_text.push_str(text);
        if !self.stop_sequences.is_empty() {
            if let Some((at, matched)) = earliest_stop(&self.pending_text, &self.stop_sequences) {
                // The turn ends here, and the sequence itself is not part of
                // the answer — exactly Anthropic's server-side behavior.
                let head: String = self.pending_text[..at].to_string();
                self.pending_text.clear();
                self.stop_override = Some(matched);
                self.emit_text(&head, events);
                return;
            }
        }
        let chars: Vec<char> = self.pending_text.chars().collect();
        if chars.len() > self.hold_back {
            let split = chars.len() - self.hold_back;
            let emit: String = chars[..split].iter().collect();
            self.pending_text = chars[split..].iter().collect();
            self.emit_text(&emit, events);
        }
    }

    /// Emit text into the open text block, opening one if needed.
    fn emit_text(&mut self, text: &str, events: &mut Vec<Event>) {
        if text.is_empty() {
            return;
        }
        let idx = match self.open_block {
            Some(OpenBlock::Text(i)) => i,
            _ => {
                if matches!(self.open_block, Some(OpenBlock::Thinking(_))) {
                    self.close_open_block(events);
                }
                let i = self.block_index;
                self.block_index += 1;
                events.push(Event {
                    name: "content_block_start".into(),
                    data: json!({
                        "type": "content_block_start",
                        "index": i,
                        "content_block": {"type": "text", "text": ""},
                    }),
                });
                self.open_block = Some(OpenBlock::Text(i));
                i
            }
        };
        events.push(Event {
            name: "content_block_delta".into(),
            data: json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {"type": "text_delta", "text": text},
            }),
        });
    }

    fn close_open_block(&mut self, events: &mut Vec<Event>) {
        let Some(block) = self.open_block.take() else {
            return;
        };
        let is_thinking = matches!(block, OpenBlock::Thinking(_));
        let is_text = matches!(block, OpenBlock::Text(_));
        let idx = match block {
            OpenBlock::Thinking(i) | OpenBlock::Text(i) | OpenBlock::Tool(i) => i,
        };
        // The held-back tail belongs to the block being closed, so it is
        // emitted here rather than through `emit_text`, which would reopen it.
        if is_text && !self.pending_text.is_empty() {
            let text = std::mem::take(&mut self.pending_text);
            events.push(Event {
                name: "content_block_delta".into(),
                data: json!({
                    "type": "content_block_delta",
                    "index": idx,
                    "delta": {"type": "text_delta", "text": text},
                }),
            });
        }
        // A thinking block completes with a `signature_delta` in Anthropic's
        // grammar. The bridge has no real signature to carry, so this is a
        // clearly synthetic one — see `synthetic_signature`.
        if is_thinking && !self.thinking_text.is_empty() {
            events.push(Event {
                name: "content_block_delta".into(),
                data: json!({
                    "type": "content_block_delta",
                    "index": idx,
                    "delta": {
                        "type": "signature_delta",
                        "signature": synthetic_signature(&self.thinking_text),
                    },
                }),
            });
            self.thinking_text.clear();
        }
        events.push(Event {
            name: "content_block_stop".into(),
            data: json!({"type": "content_block_stop", "index": idx}),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk(delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": "chatcmpl-1", "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish.map(|f| json!(f))
            }]
        })
    }

    fn event_bodies(events: &[Event]) -> Vec<(String, Value)> {
        events
            .iter()
            .map(|e| (e.name.clone(), e.data.clone()))
            .collect()
    }

    #[test]
    fn text_stream_produces_full_event_sequence() {
        let mut c = StreamConverter::new("msg_1".into(), "claude-sonnet-5".into(), 42);
        let mut all = Vec::new();
        all.extend(c.process(&chunk(json!({"role": "assistant"}), None)));
        all.extend(c.process(&chunk(json!({"content": "Hel"}), None)));
        all.extend(c.process(&chunk(json!({"content": "lo"}), None)));
        all.extend(c.process(&chunk(json!({}), Some("stop"))));
        let seq: Vec<String> = event_bodies(&all).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            seq,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        let (name, data) = event_bodies(&all)[0].clone();
        assert_eq!(name, "message_start");
        assert_eq!(data["message"]["usage"]["input_tokens"], 42);

        let (_, delta1) = event_bodies(&all)[2].clone();
        assert_eq!(delta1["delta"]["text"], "Hel");
        let (_, delta2) = event_bodies(&all)[3].clone();
        assert_eq!(delta2["delta"]["text"], "lo");
    }

    #[test]
    fn tool_call_fragments_accumulate_into_input_json_delta() {
        let mut c = StreamConverter::new("msg_1".into(), "m".into(), 0);
        let mut all = Vec::new();
        all.extend(c.process(&chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_9", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":"}}]}),
            None,
        )));
        all.extend(c.process(&chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"Paris\"}"}}]}),
            None,
        )));
        all.extend(c.process(&chunk(json!({}), Some("tool_calls"))));

        let seq: Vec<String> = event_bodies(&all).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            seq,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        let (_, start) = event_bodies(&all)[1].clone();
        assert_eq!(start["content_block"]["type"], "tool_use");
        assert_eq!(start["content_block"]["id"], "call_9");
        assert_eq!(start["content_block"]["name"], "get_weather");
        let (_, frag1) = event_bodies(&all)[2].clone();
        assert_eq!(frag1["delta"]["partial_json"], "{\"city\":");
        let (_, frag2) = event_bodies(&all)[3].clone();
        assert_eq!(frag2["delta"]["partial_json"], "\"Paris\"}");
        let (_, message_delta) = event_bodies(&all)[5].clone();
        assert_eq!(message_delta["delta"]["stop_reason"], "tool_use");
    }

    /// The thinking-requested options, which is what gates the block.
    fn wants_thinking() -> ResponseOptions {
        ResponseOptions {
            thinking_requested: true,
            ..Default::default()
        }
    }

    #[test]
    fn reasoning_content_maps_to_thinking_then_text() {
        let mut c =
            StreamConverter::new("msg_1".into(), "m".into(), 0).with_options(&wants_thinking());
        let mut all = Vec::new();
        all.extend(c.process(&chunk(json!({"reasoning_content": "hmm"}), None)));
        all.extend(c.process(&chunk(json!({"content": "answer"}), None)));
        all.extend(c.process(&chunk(json!({}), Some("stop"))));

        let seq: Vec<String> = event_bodies(&all).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            seq,
            vec![
                "message_start",
                "content_block_start", // thinking
                "content_block_delta", // thinking_delta
                "content_block_delta", // signature_delta, before the block closes
                "content_block_stop",  // thinking closed
                "content_block_start", // text
                "content_block_delta", // text
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        let (_, think_start) = event_bodies(&all)[1].clone();
        assert_eq!(think_start["content_block"]["type"], "thinking");
        let (_, think_delta) = event_bodies(&all)[2].clone();
        assert_eq!(think_delta["delta"]["type"], "thinking_delta");
        // Anthropic completes a thinking block with a signature_delta. The
        // bridge has no real signature, so this one is clearly synthetic.
        let (_, sig) = event_bodies(&all)[3].clone();
        assert_eq!(sig["delta"]["type"], "signature_delta");
        assert!(
            sig["delta"]["signature"].as_str().unwrap().len() > 20,
            "signature should be a base64 digest, got {:?}",
            sig["delta"]["signature"]
        );
        let (_, text_delta) = event_bodies(&all)[6].clone();
        assert_eq!(text_delta["delta"]["text"], "answer");
    }

    #[test]
    fn thinking_is_dropped_unless_the_request_asked_for_it() {
        // The upstream returns reasoning_content on essentially every call, so
        // an ungated converter surfaces thinking to a client that never asked —
        // including one that sent `thinking: {type: "disabled"}`.
        let mut c = StreamConverter::new("msg_1".into(), "m".into(), 0);
        let mut all = Vec::new();
        all.extend(c.process(&chunk(json!({"reasoning_content": "hmm"}), None)));
        all.extend(c.process(&chunk(json!({"content": "answer"}), None)));
        all.extend(c.process(&chunk(json!({}), Some("stop"))));

        let seq: Vec<String> = event_bodies(&all).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            seq,
            vec![
                "message_start",
                "content_block_start", // text — no thinking block at all
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        let (_, start) = event_bodies(&all)[1].clone();
        assert_eq!(start["content_block"]["type"], "text");
    }

    #[test]
    fn stop_sequence_truncates_and_reports_the_match() {
        // The bridge does not forward `stop` upstream (OpenAI strips the
        // sequence from the text it returns), so the hit is recovered here.
        let opts = ResponseOptions {
            stop_sequences: vec!["BANANA".into()],
            ..Default::default()
        };
        let mut c = StreamConverter::new("msg_1".into(), "m".into(), 0).with_options(&opts);
        let mut all = Vec::new();
        all.extend(c.process(&chunk(json!({"content": "one two "}), None)));
        all.extend(c.process(&chunk(json!({"content": "BANANA three four"}), None)));
        assert!(c.stopped(), "the caller must stop reading the upstream");

        let tail = c.finish();
        let seq: Vec<String> = event_bodies(&all).into_iter().map(|(n, _)| n).collect();
        // "BANANA" straddled the two deltas and was held back, not emitted.
        let text: String = all
            .iter()
            .filter(|e| e.name == "content_block_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert_eq!(text, "one two ");
        assert_eq!(seq[0], "message_start");
        assert!(tail.contains(r#""stop_reason":"stop_sequence""#));
        assert!(tail.contains(r#""stop_sequence":"BANANA""#));
        assert!(!tail.contains("three"), "nothing past the match is emitted");
    }

    #[test]
    fn a_stop_sequence_that_never_fires_leaves_end_turn() {
        // The hold-back must not swallow the tail: with no match, everything
        // still arrives, in order.
        let opts = ResponseOptions {
            stop_sequences: vec!["BANANA".into()],
            ..Default::default()
        };
        let mut c = StreamConverter::new("msg_1".into(), "m".into(), 0).with_options(&opts);
        let mut all = Vec::new();
        all.extend(c.process(&chunk(json!({"content": "one two "}), None)));
        all.extend(c.process(&chunk(json!({"content": "three"}), None)));
        let tail = c.finish();
        let text: String = all
            .iter()
            .filter(|e| e.name == "content_block_delta")
            .filter_map(|e| e.data["delta"]["text"].as_str())
            .collect();
        assert_eq!(text, "one two ");
        // The held-back tail is flushed by `finish()`, which frames it as SSE —
        // so the remainder is asserted there, not in the delta text above.
        assert!(
            tail.contains(r#""text":"three""#),
            "the held-back tail must still be emitted: {tail}"
        );
        assert!(tail.contains(r#""stop_reason":"end_turn""#));
    }

    #[test]
    fn finish_uses_provider_usage_from_usage_only_chunk() {
        let mut c = StreamConverter::new("msg_1".into(), "m".into(), 0);
        let _ = c.process(&chunk(json!({"content": "x"}), None));
        // include_usage sends a final choices-less chunk with real usage.
        let _ = c.process(&json!({
            "id": "chatcmpl-1", "choices": [],
            "usage": {"prompt_tokens": 100, "completion_tokens": 5}
        }));
        let all = c.process(&chunk(json!({}), Some("stop")));
        let (_, message_delta) = event_bodies(&all)
            .into_iter()
            .rev()
            .find(|(n, _)| n == "message_delta")
            .unwrap();
        assert_eq!(message_delta["usage"]["input_tokens"], 100);
        assert_eq!(message_delta["usage"]["output_tokens"], 5);
    }

    #[test]
    fn streamed_usage_reports_the_uncached_remainder() {
        let mut c = StreamConverter::new("msg_1".into(), "m".into(), 0);
        let _ = c.process(&chunk(json!({"content": "x"}), None));
        // include_usage sends a final choices-less chunk; a cached read rides
        // alongside it there.
        let _ = c.process(&json!({
            "id": "chatcmpl-1", "choices": [],
            "usage": {"prompt_tokens": 100, "completion_tokens": 5,
                      "prompt_tokens_details": {"cached_tokens": 84}}
        }));
        let all = c.process(&chunk(json!({}), Some("stop")));
        let (_, message_delta) = event_bodies(&all)
            .into_iter()
            .rev()
            .find(|(n, _)| n == "message_delta")
            .unwrap();
        // Anthropic semantics: the remainder only, reads reported separately.
        assert_eq!(message_delta["usage"]["input_tokens"], 16);
        assert_eq!(message_delta["usage"]["cache_read_input_tokens"], 84);
        assert_eq!(message_delta["usage"]["output_tokens"], 5);
    }

    #[test]
    fn finish_closes_unclosed_stream() {
        let mut c = StreamConverter::new("msg_1".into(), "m".into(), 7);
        let _ = c.process(&chunk(json!({"content": "partial"}), None));
        let tail = c.finish();
        assert!(tail.contains("content_block_stop"));
        assert!(tail.contains("message_stop"));
        assert!(tail.contains("\"input_tokens\":7"));
        // Idempotent.
        assert_eq!(c.finish(), "");
    }

    #[test]
    fn sse_framing_is_anthropic_shaped() {
        let framed = format_sse("message_stop", &json!({"type": "message_stop"}));
        assert_eq!(
            framed,
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
    }
}
