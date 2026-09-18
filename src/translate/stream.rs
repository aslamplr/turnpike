//! Streaming bridge: OpenAI chat-completions SSE chunks → Anthropic SSE
//! events. Stateful, mirroring Ollama's `StreamConverter`: block indexing,
//! thinking→text→tool_use transitions, `input_json_delta` accumulation, and
//! final usage/stop-reason reporting.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::format_sse;

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
        }
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

        // Reasoning deltas (DeepSeek-style) map to Anthropic thinking blocks.
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
                }
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

        // Text deltas.
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() && !self.finished {
                if let Some(OpenBlock::Thinking(_)) = self.open_block {
                    self.close_open_block(&mut events);
                }
                if !matches!(self.open_block, Some(OpenBlock::Text(_))) {
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
                    self.open_block = Some(OpenBlock::Text(idx));
                }
                if let Some(OpenBlock::Text(idx)) = self.open_block {
                    events.push(Event {
                        name: "content_block_delta".into(),
                        data: json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": {"type": "text_delta", "text": text},
                        }),
                    });
                }
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
                    events.push(Event {
                        name: "content_block_start".into(),
                        data: json!({
                            "type": "content_block_start",
                            "index": anthropic_index,
                            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
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
            let stop = super::map_finish(finish.as_str(), has_tools);
            let usage = chunk
                .get("usage")
                .filter(|u| !u.is_null())
                .cloned()
                .or_else(|| self.usage.take())
                .unwrap_or(Value::Null);
            let input = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            events.push(Event {
                name: "message_delta".into(),
                data: json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": stop},
                    "usage": {"input_tokens": input, "output_tokens": output},
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
        events.push(Event {
            name: "message_delta".into(),
            data: json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
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

    fn close_open_block(&mut self, events: &mut Vec<Event>) {
        if let Some(block) = self.open_block.take() {
            let idx = match block {
                OpenBlock::Thinking(i) | OpenBlock::Text(i) | OpenBlock::Tool(i) => i,
            };
            events.push(Event {
                name: "content_block_stop".into(),
                data: json!({"type": "content_block_stop", "index": idx}),
            });
        }
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

    #[test]
    fn reasoning_content_maps_to_thinking_then_text() {
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
                "content_block_start", // thinking
                "content_block_delta", // thinking_delta
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
        let (_, text_delta) = event_bodies(&all)[5].clone();
        assert_eq!(text_delta["delta"]["text"], "answer");
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
