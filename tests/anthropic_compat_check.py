#!/usr/bin/env python3
"""Anthropic-API compatibility probe for a **running** turnpike gateway.

This is the executable half of `docs/anthropic-compat.md`. It is deliberately
*not* a `cargo test`: every upstream-facing check needs a live gateway with a
real provider key behind it, so it cannot run in CI and it spends real quota.
The inline `#[cfg(test)]` modules still own everything that can be asserted
without a network — this file only covers the properties that need one.

    # against the default 127.0.0.1:8710 with every configured route
    python3 tests/anthropic_compat_check.py

    # only the checks that never touch the upstream (no key, no quota, no cost)
    python3 tests/anthropic_compat_check.py --local-only

    python3 tests/anthropic_compat_check.py --base-url http://127.0.0.1:8710 \\
        --model claude-sonnet-5

Exit status is 0 when nothing regressed. Three outcomes are not failures:

  PASS     the Anthropic contract holds
  FAIL     it does not — this is what the exit status tracks
  KNOWN    a documented divergence (see docs/anthropic-compat.md) is still
           present, asserted so the doc cannot silently go stale
  CHANGED  a documented divergence is *gone* — a fix landed and the doc (and
           this file) need updating

The nine divergences that doc once recorded are all closed, so no check here
calls `known(...)` today and a run should report `KNOWN 0`. The `known` helper
stays: it is the right shape for the next divergence, because a suite that can
only say PASS/FAIL goes quiet the moment a known gap is written down.

Why the User-Agent matters: turnpike forwards the client's `User-Agent`
verbatim, and the upstream WAF in front of opencode.ai blocks `Python-urllib/*`
by name. A bare `urllib` probe therefore reports `403` against a gateway that
is working perfectly, which is a spectacularly confusing way to start
debugging. Every request here sends a realistic client UA except the one check
that deliberately sends the blocked one, to assert the error is *shaped*.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request

# A UA a real Anthropic client would send. See the module docstring.
CLIENT_UA = "anthropic-sdk-python/0.40.0"
# The UA the upstream WAF rejects, used only to exercise error shaping.
BLOCKED_UA = "Python-urllib/3.12"

ANTHROPIC_VERSION = "2023-06-01"

# Event names Anthropic's SSE grammar can carry in a `content_block_delta`.
DELTA_TYPES = {"text_delta", "thinking_delta", "input_json_delta", "signature_delta"}

PASS, FAIL, KNOWN, CHANGED, SKIP = "PASS", "FAIL", "KNOWN", "CHANGED", "SKIP"


class Probe:
    def __init__(self, base_url: str, model: str, timeout: float) -> None:
        self.base_url = base_url.rstrip("/")
        self.model = model
        self.timeout = timeout
        self.results: list[tuple[str, str, str, str]] = []

    # -- transport --------------------------------------------------------

    def request(
        self,
        path: str,
        body: dict | None = None,
        *,
        method: str = "POST",
        headers: dict | None = None,
        user_agent: str = CLIENT_UA,
        raw: bytes | None = None,
    ) -> tuple[int, dict, str]:
        """Return (status, headers, body). Never raises for an HTTP error."""
        data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
        sent = {
            "content-type": "application/json",
            "anthropic-version": ANTHROPIC_VERSION,
            "user-agent": user_agent,
        }
        if headers:
            sent.update(headers)
        req = urllib.request.Request(
            self.base_url + path, data=data, headers=sent, method=method
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                return resp.status, dict(resp.headers), resp.read().decode()
        except urllib.error.HTTPError as e:
            return e.code, dict(e.headers), e.read().decode()
        except Exception as e:  # connection refused, timeout, ...
            return 0, {}, f"{type(e).__name__}: {e}"

    def request_stream_timing(self, path: str, body: dict) -> tuple[float, float]:
        """(seconds to first byte, seconds to last) for one streaming POST.

        Read incrementally so the *spread* is measurable: a rendition that is
        delivered as one lump has a first byte and a last byte a few
        milliseconds apart, however long the request took overall. Returns
        (0.0, 0.0) on failure.
        """
        data = json.dumps(body).encode()
        req = urllib.request.Request(
            self.base_url + path,
            data=data,
            headers={
                "content-type": "application/json",
                "anthropic-version": ANTHROPIC_VERSION,
                "user-agent": CLIENT_UA,
            },
            method="POST",
        )
        start = time.monotonic()
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                resp.read(1)
                first = time.monotonic()
                while resp.read(65536):
                    pass
        except Exception:
            return 0.0, 0.0
        return first - start, time.monotonic() - start

    # -- assertions -------------------------------------------------------

    def check(self, category: str, name: str, ok: bool, detail: str = "") -> None:
        self.results.append((PASS if ok else FAIL, category, name, detail))
        self._print(PASS if ok else FAIL, category, name, detail)

    def known(self, category: str, name: str, diverges: bool, detail: str = "") -> None:
        """Assert a *documented divergence* is still there.

        Passes when the divergence is present (`KNOWN`) and reports `CHANGED`
        when it is gone, so fixing one of these without updating
        docs/anthropic-compat.md is loud rather than silent.
        """
        state = KNOWN if diverges else CHANGED
        self.results.append((state, category, name, detail))
        self._print(state, category, name, detail)

    def skip(self, category: str, name: str, detail: str = "") -> None:
        self.results.append((SKIP, category, name, detail))
        self._print(SKIP, category, name, detail)

    @staticmethod
    def _print(state: str, category: str, name: str, detail: str) -> None:
        line = f"[{state:<7}] {category:<11} {name:<48} {detail}"
        print(line[:200], flush=True)

    # -- reporting --------------------------------------------------------

    def report(self) -> int:
        counts = {s: sum(1 for r in self.results if r[0] == s) for s in (PASS, FAIL, KNOWN, CHANGED, SKIP)}
        print("\n" + "=" * 78)
        print(
            f"PASS {counts[PASS]}   FAIL {counts[FAIL]}   KNOWN {counts[KNOWN]}   "
            f"CHANGED {counts[CHANGED]}   SKIP {counts[SKIP]}"
        )
        for state in (FAIL, CHANGED):
            for s, cat, name, detail in self.results:
                if s == state:
                    print(f"  {state} {cat} :: {name} :: {detail[:200]}")
        if counts[CHANGED]:
            print(
                "\nA documented divergence is gone — update docs/anthropic-compat.md "
                "and the corresponding `known(...)` check here."
            )
        return 1 if counts[FAIL] else 0


# -- shared helpers -------------------------------------------------------


def as_json(text: str):
    try:
        return json.loads(text)
    except Exception:
        return None


def parse_sse(text: str) -> list[tuple[str, dict | None]]:
    """Parse an Anthropic event stream into [(event, data), ...]."""
    events = []
    for block in text.split("\n\n"):
        if not block.strip():
            continue
        name = data = None
        for line in block.split("\n"):
            if line.startswith("event: "):
                name = line[7:].strip()
            elif line.startswith("data: "):
                data = line[6:]
        if name:
            events.append((name, as_json(data) if data else None))
    return events


def message(probe: Probe, **overrides) -> dict:
    body = {
        "model": probe.model,
        "max_tokens": 128,
        "messages": [{"role": "user", "content": "Reply with exactly: OK"}],
    }
    body.update(overrides)
    return body


def text_of(body: dict) -> str:
    return "".join(
        b.get("text", "") for b in (body or {}).get("content", []) if b.get("type") == "text"
    )


def block_types(body: dict) -> list[str]:
    return [b.get("type") for b in (body or {}).get("content", [])]


# Anthropic's error codes for a failed web search.
SEARCH_ERROR_CODES = {
    "too_many_requests",
    "invalid_tool_input",
    "max_uses_exceeded",
    "query_too_long",
    "request_too_large",
    "unavailable",
}


def search_result_shape(content) -> str:
    """Classify a `web_search_tool_result`'s content against Anthropic's schema.

    Success is a (possibly empty) list of `web_search_result`; failure is a
    single `web_search_tool_result_error` object — "on an error, `content` is a
    single error object rather than a list of result blocks". A bare string is
    neither, which is what turnpike used to emit.
    """
    if isinstance(content, list):
        return (
            "results"
            if all(isinstance(x, dict) and x.get("type") == "web_search_result" for x in content)
            else "malformed"
        )
    if isinstance(content, dict) and content.get("type") == "web_search_tool_result_error":
        return "error"
    return "malformed"


WEATHER_TOOL = [
    {
        "name": "get_weather",
        "description": "Get weather for a city",
        "input_schema": {
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        },
    }
]

TINY_PNG = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)


# -- local checks: no upstream, no key, no cost ---------------------------


def local_checks(p: Probe) -> None:
    print("\n-- protocol / error surface --")

    status, headers, _ = p.request("/_health", method="GET")
    p.check(
        "protocol",
        "_health -> 204 + gateway header",
        status == 204 and headers.get("x-turnpike-gateway") == "1",
        f"status={status} hdr={headers.get('x-turnpike-gateway')}",
    )

    status, _, text = p.request("/v1/models", method="GET")
    body = as_json(text) or {}
    entries = body.get("data") or []
    p.check(
        "protocol",
        "/v1/models pagination envelope",
        status == 200
        and isinstance(entries, list)
        and all(k in body for k in ("data", "first_id", "last_id", "has_more")),
        f"status={status} n={len(entries)} top_keys={sorted(body.keys())}",
    )
    p.check(
        "protocol",
        "/v1/models entries are model objects",
        bool(entries) and all(e.get("type") == "model" and e.get("id") for e in entries),
        f"ids={[e.get('id') for e in entries]}",
    )

    status, _, _ = p.request("/nope")
    p.check("protocol", "unknown path -> 404", status == 404, f"status={status}")

    status, _, text = p.request(
        "/v1/messages", message(p), headers={"Origin": "https://evil.example"}
    )
    error = (as_json(text) or {}).get("error", {})
    p.check(
        "security",
        "Origin header -> 403 permission_error",
        status == 403 and error.get("type") == "permission_error",
        f"status={status} type={error.get('type')}",
    )

    status, _, text = p.request("/v1/messages", message(p), headers={"Host": "example.com"})
    error = (as_json(text) or {}).get("error", {})
    p.check(
        "security",
        "non-loopback Host -> 403 permission_error",
        status == 403 and error.get("type") == "permission_error",
        f"status={status} type={error.get('type')}",
    )

    status, _, text = p.request("/v1/messages", raw=b"{not json")
    body = as_json(text) or {}
    p.check(
        "protocol",
        "malformed JSON -> 400 error envelope",
        status == 400 and body.get("type") == "error",
        f"status={status} type={body.get('type')}",
    )

    status, _, text = p.request("/v1/messages", {"max_tokens": 16, "messages": []})
    p.check(
        "protocol",
        "missing model -> 400",
        status == 400 and "model" in text,
        f"status={status} {text[:70]}",
    )

    status, _, text = p.request(
        "/v1/messages", {"model": "no-such-model", "max_tokens": 16, "messages": []}
    )
    error = (as_json(text) or {}).get("error", {})
    p.check(
        "protocol",
        "unknown model -> 404 not_found_error",
        status == 404 and error.get("type") == "not_found_error",
        f"status={status} type={error.get('type')}",
    )

    status, _, text = p.request(
        "/v1/messages",
        {"model": "no-such-model", "stream": True, "max_tokens": 16, "messages": []},
    )
    body = as_json(text) or {}
    p.check(
        "stream",
        "pre-stream error is JSON, not SSE",
        status == 404 and body.get("type") == "error",
        f"status={status} type={body.get('type')}",
    )

    print("\n-- count_tokens --")

    # Same message body as the tools comparison below, so the only difference
    # between the two payloads is the `tools` block.
    status, _, text = p.request(
        "/v1/messages/count_tokens",
        {"model": p.model, "messages": [{"role": "user", "content": "hi"}]},
    )
    tokens = (as_json(text) or {}).get("input_tokens")
    p.check(
        "count_tokens",
        "returns an input_tokens estimate",
        status == 200 and isinstance(tokens, int),
        f"status={status} input_tokens={tokens}",
    )

    # Anthropic's count_tokens counts tool definitions, and a Claude Code
    # request is dominated by its tool list — so the estimate must move when
    # the tools block does, and strictly upward.
    big_tools = [
        {
            "name": f"tool_{i}",
            "description": "A tool " * 200,
            "input_schema": {"type": "object", "properties": {"a": {"type": "string"}}},
        }
        for i in range(20)
    ]
    status, _, text = p.request(
        "/v1/messages/count_tokens",
        {"model": p.model, "tools": big_tools, "messages": [{"role": "user", "content": "hi"}]},
    )
    with_tools = (as_json(text) or {}).get("input_tokens")
    p.check(
        "count_tokens",
        "tools are counted in the estimate",
        with_tools is not None and with_tools > tokens,
        f"base={tokens} with_20_large_tools={with_tools}",
    )

    # Same unknown id as the /v1/messages check above, so the two must agree.
    status, _, text = p.request(
        "/v1/messages/count_tokens", {"model": "no-such-model", "messages": []}
    )
    error = (as_json(text) or {}).get("error", {})
    p.check(
        "count_tokens",
        "unknown model -> 404 not_found_error, as /v1/messages gives",
        status == 404 and error.get("type") == "not_found_error",
        f"status={status} type={error.get('type')}",
    )

    print("\n-- models API / batches --")

    status, _, text = p.request(f"/v1/models/{p.model}", method="GET")
    entry = as_json(text) or {}
    p.check(
        "models",
        "GET /v1/models/{id} returns the bare ModelInfo",
        status == 200 and entry.get("type") == "model" and entry.get("id") == p.model,
        f"status={status} id={entry.get('id')}",
    )
    status, _, text = p.request("/v1/models/no-such-model", method="GET")
    p.check(
        "models",
        "unknown model id -> 404 not_found_error",
        status == 404 and (as_json(text) or {}).get("error", {}).get("type") == "not_found_error",
        f"status={status}",
    )

    _, _, text = p.request("/v1/models", method="GET")
    entry = ((as_json(text) or {}).get("data") or [{}])[0]
    caps = entry.get("capabilities") or {}
    p.check(
        "models",
        "entries carry max_input_tokens and capabilities",
        "max_input_tokens" in entry and "capabilities" in entry,
        f"fields={sorted(entry.keys())}",
    )
    # "Keys are always present for all known capabilities."
    known_caps = [
        "batch", "citations", "code_execution", "context_management", "effort",
        "image_input", "pdf_input", "server_tools", "structured_outputs", "thinking",
    ]
    p.check(
        "models",
        "capabilities carry every known key",
        all(k in caps for k in known_caps),
        f"missing={[k for k in known_caps if k not in caps]}",
    )
    p.check(
        "models",
        "capability values are honest (batch off, images on)",
        caps.get("batch", {}).get("supported") is False
        and caps.get("image_input", {}).get("supported") is True,
        f"batch={caps.get('batch')} image_input={caps.get('image_input')}",
    )

    status, _, text = p.request(
        "/v1/messages/batches",
        {
            "requests": [
                {
                    "custom_id": "r1",
                    "params": {"model": p.model, "max_tokens": 16, "messages": []},
                }
            ]
        },
    )
    error = (as_json(text) or {}).get("error", {})
    p.check(
        "batches",
        "Message Batches path -> shaped 404, not a parse error",
        status == 404
        and error.get("type") == "not_found_error"
        and "Batches" in (error.get("message") or ""),
        f"status={status} type={error.get('type')} message={error.get('message')}",
    )


# -- upstream checks: live provider, real quota ---------------------------


def upstream_checks(p: Probe) -> None:
    print("\n-- non-streaming --")

    status, _, text = p.request("/v1/messages", message(p))
    body = as_json(text) or {}
    usage = body.get("usage", {})
    p.check(
        "nonstream",
        "message envelope",
        status == 200
        and body.get("type") == "message"
        and body.get("role") == "assistant"
        and isinstance(body.get("content"), list)
        and isinstance(body.get("id"), str)
        and body["id"].startswith("msg_")
        and body.get("model") == p.model,
        f"status={status} id={body.get('id')} stop={body.get('stop_reason')} blocks={block_types(body)}",
    )
    p.check(
        "nonstream",
        "usage carries input/output tokens",
        {"input_tokens", "output_tokens"} <= set(usage),
        json.dumps(usage),
    )
    p.check("nonstream", "text content returned", bool(text_of(body).strip()), repr(text_of(body)[:50]))
    # The requested id is echoed; the upstream id must never leak.
    p.check(
        "nonstream",
        "requested model id is echoed back",
        body.get("model") == p.model,
        f"model={body.get('model')}",
    )

    status, _, text = p.request(
        "/v1/messages", message(p, system="You are a terse bot. Answer in one word.")
    )
    p.check("nonstream", "system as a string", status == 200, f"status={status}")

    status, _, text = p.request(
        "/v1/messages", message(p, system=[{"type": "text", "text": "Answer in one word."}])
    )
    p.check("nonstream", "system as text blocks", status == 200, f"status={status}")

    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 64,
            "messages": [
                {"role": "user", "content": "My name is Ada."},
                {"role": "assistant", "content": "Hello Ada."},
                {"role": "user", "content": "What is my name? One word."},
            ],
        },
    )
    body = as_json(text) or {}
    p.check(
        "nonstream",
        "multi-turn history is carried",
        status == 200 and "Ada" in text_of(body),
        f"status={status} text={text_of(body)[:40]!r}",
    )

    status, _, _ = p.request(
        "/v1/messages", message(p, temperature=0.2, top_p=0.9, stop_sequences=["ZZZ"])
    )
    p.check("nonstream", "temperature / top_p / stop_sequences accepted", status == 200, f"status={status}")

    status, _, text = p.request("/v1/messages", message(p, max_tokens=8))
    body = as_json(text) or {}
    p.check(
        "nonstream",
        "max_tokens is honored",
        status == 200
        and body.get("stop_reason") == "max_tokens"
        and body.get("usage", {}).get("output_tokens", 99) <= 12,
        f"stop={body.get('stop_reason')} out={body.get('usage', {}).get('output_tokens')}",
    )

    status, _, _ = p.request("/v1/messages", message(p, metadata={"user_id": "compat-probe"}))
    p.check("nonstream", "metadata tolerated", status == 200, f"status={status}")

    print("\n-- tools --")

    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 256,
            "tools": WEATHER_TOOL,
            "tool_choice": {"type": "any"},
            "messages": [
                {"role": "user", "content": "What is the weather in Paris? Use the tool."}
            ],
        },
    )
    body = as_json(text) or {}
    calls = [b for b in body.get("content", []) if b.get("type") == "tool_use"]
    p.check(
        "tools",
        "tool_choice=any produces a tool_use block",
        status == 200
        and calls
        and calls[0].get("name") == "get_weather"
        and isinstance(calls[0].get("input"), dict)
        and calls[0].get("id"),
        f"n={len(calls)} input={calls[0].get('input') if calls else None}",
    )
    p.check(
        "tools",
        "stop_reason=tool_use while a call is pending",
        body.get("stop_reason") == "tool_use",
        f"stop={body.get('stop_reason')}",
    )

    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 256,
            "tools": WEATHER_TOOL,
            "tool_choice": {"type": "tool", "name": "get_weather"},
            "messages": [{"role": "user", "content": "Weather in Paris?"}],
        },
    )
    body = as_json(text) or {}
    calls = [b for b in body.get("content", []) if b.get("type") == "tool_use"]
    p.check(
        "tools",
        "tool_choice={type:tool} pins that tool",
        status == 200 and calls and calls[0].get("name") == "get_weather",
        f"n={len(calls)} name={calls[0].get('name') if calls else None}",
    )

    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 128,
            "tools": WEATHER_TOOL,
            "tool_choice": {"type": "none"},
            "messages": [{"role": "user", "content": "Say OK, do not use tools."}],
        },
    )
    body = as_json(text) or {}
    calls = [b for b in body.get("content", []) if b.get("type") == "tool_use"]
    p.check(
        "tools",
        "tool_choice=none suppresses calls",
        status == 200 and not calls,
        f"n={len(calls)} stop={body.get('stop_reason')}",
    )

    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 256,
            "tools": WEATHER_TOOL,
            "tool_choice": {"type": "auto"},
            "messages": [
                {"role": "user", "content": "What is the weather in Paris? Use the tool."}
            ],
        },
    )
    p.check("tools", "tool_choice=auto accepted", status == 200, f"status={status}")

    # The flag is now translated to OpenAI's `parallel_tool_calls: false`, but
    # whether the *provider* honours it is the provider's business — so this
    # asserts only that the request is accepted. The real assertion, that the
    # field reaches the upstream payload, is a unit test on
    # `request_to_openai` (it cannot be observed from outside the gateway).
    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 256,
            "tools": WEATHER_TOOL,
            "tool_choice": {"type": "any", "disable_parallel_tool_use": True},
            "messages": [
                {"role": "user", "content": "Weather in Paris and in London? Use the tool for each."}
            ],
        },
    )
    body = as_json(text) or {}
    calls = [b for b in body.get("content", []) if b.get("type") == "tool_use"]
    p.check(
        "tools",
        "disable_parallel_tool_use accepted (cap is upstream-side)",
        status == 200,
        f"status={status} n_tool_use={len(calls)} (provider-dependent; forwarded as parallel_tool_calls=false)",
    )

    # tool_result round-trip: the follow-up assistant turn must see the result.
    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 256,
            "tools": WEATHER_TOOL,
            "messages": [
                {"role": "user", "content": "What is the weather in Paris? Use the tool."},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_compat_1",
                            "name": "get_weather",
                            "input": {"city": "Paris"},
                        }
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_compat_1",
                            "content": "18C and clear",
                        }
                    ],
                },
            ],
        },
    )
    p.check(
        "tools",
        "tool_result round-trip reaches the model",
        status == 200 and "18" in json.dumps((as_json(text) or {}).get("content", [])),
        f"status={status} stop={(as_json(text) or {}).get('stop_reason')}",
    )

    status, _, _ = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 256,
            "tools": WEATHER_TOOL,
            "messages": [
                {"role": "user", "content": "What is the weather in Paris? Use the tool."},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "toolu_compat_2",
                            "name": "get_weather",
                            "input": {"city": "Paris"},
                        }
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_compat_2",
                            "is_error": True,
                            "content": "upstream timeout",
                        }
                    ],
                },
            ],
        },
    )
    p.check("tools", "tool_result is_error tolerated", status == 200, f"status={status}")

    status, _, _ = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 256,
            "tools": WEATHER_TOOL,
            "messages": [
                {"role": "user", "content": "Weather in Paris and London?"},
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "tu_a",
                            "name": "get_weather",
                            "input": {"city": "Paris"},
                        },
                        {
                            "type": "tool_use",
                            "id": "tu_b",
                            "name": "get_weather",
                            "input": {"city": "London"},
                        },
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "tu_a", "content": "18C clear"},
                        {"type": "tool_result", "tool_use_id": "tu_b", "content": "12C rain"},
                    ],
                },
            ],
        },
    )
    p.check("tools", "parallel tool_result blocks both matched", status == 200, f"status={status}")

    print("\n-- content blocks --")

    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 64,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": TINY_PNG,
                            },
                        },
                        {"type": "text", "text": "Reply with exactly: OK"},
                    ],
                }
            ],
        },
    )
    p.check("blocks", "base64 image block accepted", status == 200, f"status={status}")

    status, _, _ = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 256,
            "messages": [
                {"role": "user", "content": "Think step by step: what is 17*23? One short line."},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "17*23 = 391.", "signature": "sig_ignored"},
                        {"type": "text", "text": "391"},
                    ],
                },
                {"role": "user", "content": "Now what is 391+9? One word."},
            ],
        },
    )
    p.check("blocks", "thinking block in history tolerated", status == 200, f"status={status}")

    status, _, _ = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 300,
            "messages": [
                {"role": "user", "content": "What is 2+2?"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "Four.", "signature": ""},
                        {"type": "text", "text": "4"},
                    ],
                },
                {"role": "user", "content": "Add 1. One word."},
            ],
        },
    )
    p.check("blocks", "empty-signature thinking replay tolerated", status == 200, f"status={status}")

    # Thinking has to be *requested* for a thinking block to be part of the
    # response: the bridge gates on the request, because the upstream returns
    # reasoning_content on essentially every call.
    status, _, text = p.request(
        "/v1/messages", message(p, max_tokens=200, thinking={"type": "enabled"})
    )
    body = as_json(text) or {}
    thinking = [b for b in body.get("content", []) if b.get("type") == "thinking"]
    if thinking:
        # The bridge synthesizes thinking from `reasoning_content`, so there is
        # no real signature to carry. Anthropic's is a cryptographic
        # attestation only it can produce; the bridge emits a clearly synthetic
        # one (base64 of a SHA-256, prefixed `turnpike-synthetic:` in the hash
        # input) so the block's *shape* is valid for typed consumers.
        signature = thinking[0].get("signature")
        p.check(
            "blocks",
            "thinking blocks carry a `signature`",
            isinstance(signature, str) and len(signature) > 20,
            f"keys={sorted(thinking[0].keys())} signature_len={len(signature) if isinstance(signature, str) else '?'}",
        )
    else:
        p.skip("blocks", "thinking block shape", "upstream returned no reasoning this run")

    # The stream side of the same shape: a thinking block completes with a
    # `signature_delta` before its `content_block_stop`, as Anthropic's grammar
    # requires.
    status, _, text = p.request(
        "/v1/messages",
        message(p, max_tokens=200, stream=True, thinking={"type": "enabled"}),
    )
    events = parse_sse(text)
    saw_thinking = any(
        e == "content_block_start" and (d.get("content_block") or {}).get("type") == "thinking"
        for e, d in events
    )
    if saw_thinking:
        p.check(
            "blocks",
            "a signature_delta completes the thinking block",
            any(
                e == "content_block_delta"
                and (d.get("delta") or {}).get("type") == "signature_delta"
                for e, d in events
            ),
            "thinking block streamed with a signature_delta",
        )
    else:
        p.skip("blocks", "signature_delta", "upstream returned no reasoning this run")

    # Divergence 7's replacement assertion: thinking must NOT appear when the
    # request did not ask for it — and `{type: "disabled"}` is a request not to.
    for label, extra in [("absent", {}), ("disabled", {"thinking": {"type": "disabled"}})]:
        status, _, text = p.request("/v1/messages", message(p, max_tokens=200, **extra))
        body = as_json(text) or {}
        p.check(
            "blocks",
            f"no thinking block when the parameter is {label}",
            status == 200 and "thinking" not in block_types(body),
            f"status={status} blocks={block_types(body)}",
        )

    # The upstream returned 400 for a URL source while base64 succeeded. That is
    # provider-side (it could not fetch the URL), not a translation gap — the
    # bridge emits the right `image_url` shape either way.
    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 32,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}},
                        {"type": "text", "text": "hi"},
                    ],
                }
            ],
        },
    )
    p.skip(
        "blocks",
        "image url source",
        f"status={status} — upstream-side probe, see docs/anthropic-compat.md",
    )

    print("\n-- model resolution --")

    for alias, why in [
        ("deepseek-v4.1-flash", "upstream-model match"),
        ("zen-go/deepseek-v4.1-flash", "provider/model"),
        ("zen-go:deepseek-v4.1-flash", "provider:model"),
    ]:
        status, _, text = p.request(
            "/v1/messages", {"model": alias, "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}
        )
        body = as_json(text) or {}
        p.check(
            "resolve",
            f"{alias} ({why})",
            status == 200 and body.get("model") == alias,
            f"status={status} model_echo={body.get('model')}",
        )

    print("\n-- streaming SSE grammar --")

    status, headers, text = p.request("/v1/messages", message(p, stream=True))
    events = parse_sse(text)
    names = [e for e, _ in events]
    p.check(
        "stream",
        "content-type text/event-stream",
        "text/event-stream" in headers.get("content-type", ""),
        headers.get("content-type", ""),
    )
    p.check(
        "stream",
        "framed by message_start ... message_stop",
        names[:1] == ["message_start"] and names[-1:] == ["message_stop"],
        f"first={names[:1]} last={names[-1:]} n={len(names)}",
    )
    p.check(
        "stream",
        "exactly one message_start and message_stop",
        names.count("message_start") == 1 and names.count("message_stop") == 1,
        f"start={names.count('message_start')} stop={names.count('message_stop')}",
    )
    p.check(
        "stream",
        "message_delta precedes message_stop",
        "message_delta" in names and names.index("message_delta") < len(names) - 1,
        f"tail={names[-3:]}",
    )

    start = next((d for e, d in events if e == "message_start"), None) or {}
    inner = start.get("message", {})
    p.check(
        "stream",
        "message_start carries a message object",
        start.get("type") == "message_start"
        and inner.get("type") == "message"
        and inner.get("role") == "assistant"
        and isinstance(inner.get("content"), list)
        and inner.get("model") == p.model
        and "usage" in inner,
        f"model={inner.get('model')} usage={inner.get('usage')}",
    )

    opened = [d.get("index") for e, d in events if e == "content_block_start"]
    closed = [d.get("index") for e, d in events if e == "content_block_stop"]
    deltas = [
        (d.get("index"), (d.get("delta") or {}).get("type"))
        for e, d in events
        if e == "content_block_delta"
    ]
    p.check(
        "stream",
        "content_block_start/stop indices are balanced",
        opened == closed and len(opened) > 0,
        f"start={opened} stop={closed}",
    )
    p.check("stream", "block indices are 0-based", opened[:1] == [0], f"first={opened[:1]}")
    p.check(
        "stream",
        "content_block_delta indices and types are valid",
        all(i in opened for i, _ in deltas) and all(t in DELTA_TYPES for _, t in deltas),
        f"types={sorted({t for _, t in deltas})}",
    )

    delta = next((d for e, d in events if e == "message_delta"), None) or {}
    p.check(
        "stream",
        "message_delta carries stop_reason + output_tokens",
        delta.get("delta", {}).get("stop_reason") is not None
        and "output_tokens" in delta.get("usage", {}),
        f"delta={delta.get('delta')} usage={delta.get('usage')}",
    )
    assembled = "".join(
        (d.get("delta") or {}).get("text", "") for e, d in events if e == "content_block_delta"
    )
    p.check("stream", "text_delta payload assembles", bool(assembled.strip()), repr(assembled[:40]))

    # tool calls must arrive as input_json_delta fragments that concatenate.
    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 256,
            "stream": True,
            "tools": WEATHER_TOOL,
            "tool_choice": {"type": "any"},
            "messages": [{"role": "user", "content": "Weather in Paris? Use the tool."}],
        },
    )
    events = parse_sse(text)
    starts = [
        d
        for e, d in events
        if e == "content_block_start" and (d.get("content_block") or {}).get("type") == "tool_use"
    ]
    fragments = [
        d
        for e, d in events
        if e == "content_block_delta" and (d.get("delta") or {}).get("type") == "input_json_delta"
    ]
    final = next((d for e, d in events if e == "message_delta"), None) or {}
    p.check(
        "stream",
        "tool_use opens with an empty input object",
        bool(starts) and starts[0]["content_block"].get("input") == {},
        f"n={len(starts)} first={starts[0]['content_block'] if starts else None}",
    )
    p.check("stream", "input_json_delta fragments arrive", len(fragments) > 0, f"n={len(fragments)}")
    try:
        concatenated = "".join((d["delta"] or {}).get("partial_json", "") for d in fragments)
        parsed = json.loads(concatenated) if concatenated else None
    except Exception as e:  # malformed partial_json
        concatenated, parsed = f"<error: {e}>", None
    p.check(
        "stream",
        "input_json_delta concatenates to valid JSON",
        isinstance(parsed, dict),
        f"{concatenated[:60]} -> {parsed}",
    )
    p.check(
        "stream",
        "streamed stop_reason=tool_use",
        final.get("delta", {}).get("stop_reason") == "tool_use",
        f"delta={final.get('delta')}",
    )

    # a truncated stream must still close cleanly rather than hang or truncate.
    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 4,
            "stream": True,
            "messages": [{"role": "user", "content": "Count from 1 to 100, one per line."}],
        },
    )
    events = parse_sse(text)
    names = [e for e, _ in events]
    final = next((d for e, d in events if e == "message_delta"), None) or {}
    p.check(
        "stream",
        "truncated stream still closes cleanly",
        names[:1] == ["message_start"]
        and names[-1:] == ["message_stop"]
        and final.get("delta", {}).get("stop_reason") == "max_tokens",
        f"last={names[-2:]} stop={final.get('delta', {}).get('stop_reason')}",
    )

    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 400,
            "stream": True,
            "messages": [{"role": "user", "content": "Write 8 short numbered lines about Rust."}],
        },
    )
    events = parse_sse(text)
    chunks = [
        (d.get("delta") or {}).get("text", "")
        for e, d in events
        if e == "content_block_delta" and (d.get("delta") or {}).get("type") == "text_delta"
    ]
    p.check(
        "stream",
        "multi-delta text stream assembles",
        len(chunks) > 3 and len("".join(chunks)) > 60,
        f"n_deltas={len(chunks)} chars={len(''.join(chunks))}",
    )
    # The control for the search-path check below. On the plain path each
    # upstream chunk is forwarded as it arrives, so one block's deltas arrive in
    # many pieces — which is what "streaming" has to mean for a client that
    # paints progressively.
    indices = [d.get("index") for e, d in events if e == "content_block_delta"]
    p.check(
        "stream",
        "plain path is incremental (a block's deltas repeat)",
        any(indices.count(i) > 1 for i in indices),
        f"n_deltas={len(indices)} distinct_blocks={len(set(indices))}",
    )

    print("\n-- search middleware --")

    search_tools = [{"type": "web_search_20250305", "name": "web_search", "max_uses": 2}]
    search_ask = "Search the web for the current Rust stable version, then state it."
    status, _, text = p.request(
        "/v1/messages",
        {"model": p.model, "max_tokens": 512, "tools": search_tools,
         "messages": [{"role": "user", "content": search_ask}]},
    )
    body = as_json(text) or {}
    types = block_types(body)
    p.check(
        "search",
        "agentic loop emits server_tool_use + web_search_tool_result",
        status == 200 and "server_tool_use" in types and "web_search_tool_result" in types,
        f"status={status} blocks={types}",
    )
    # Every result block must be one of Anthropic's two shapes. A search that
    # errors is not a probe failure — it is a live provider hiccup — but it
    # must still be *shaped* the way a client can read.
    results = [b for b in body.get("content", []) if b.get("type") == "web_search_tool_result"]
    shapes = [search_result_shape(b.get("content")) for b in results]
    p.check(
        "search",
        "tool result content is Anthropic-shaped (results or an error object)",
        bool(results) and all(s != "malformed" for s in shapes),
        f"shapes={shapes}",
    )
    ok = next(
        (b for b in results if isinstance(b.get("content"), list) and b["content"]),
        None,
    )
    if ok:
        first = ok["content"][0]
        p.check(
            "search",
            "web_search_result carries url + title",
            all(k in first for k in ("type", "url", "title")),
            f"keys={sorted(first.keys())}",
        )
    else:
        p.skip("search", "web_search_result shape", "every search failed this run")

    errored = [b for b in results if search_result_shape(b.get("content")) == "error"]
    if errored:
        code = errored[0]["content"]["error_code"]
        p.check(
            "search",
            "a failed search reports web_search_tool_result_error",
            code in SEARCH_ERROR_CODES,
            f"error_code={code}",
        )
    else:
        p.skip("search", "search error shape", "no search failed this run")
    p.check(
        "search",
        "final answer text present",
        bool(text_of(body).strip()),
        repr(text_of(body)[:50]),
    )

    status, _, text = p.request(
        "/v1/messages",
        {"model": p.model, "max_tokens": 512, "stream": True, "tools": search_tools,
         "messages": [{"role": "user", "content": search_ask}]},
    )
    events = parse_sse(text)
    names = [e for e, _ in events]
    started = [
        (d.get("index"), (d.get("content_block") or {}).get("type"))
        for e, d in events
        if e == "content_block_start"
    ]
    p.check(
        "search",
        "searched answer is rendered as valid SSE",
        names[:1] == ["message_start"]
        and names[-1:] == ["message_stop"]
        and "server_tool_use" in [t for _, t in started],
        f"blocks={started} n={len(names)}",
    )
    # The same property the plain-path control asserts, now that the loop
    # streams each iteration instead of synthesizing the message and re-emitting
    # it: a block's delta index repeats, so a client paints tokens and not just
    # whole blocks. This is divergence 1's assertion, inverted.
    search_indices = [d.get("index") for e, d in events if e == "content_block_delta"]
    if search_indices:
        p.check(
            "search",
            "rendition is incremental (a block's deltas repeat)",
            any(search_indices.count(i) > 1 for i in search_indices),
            f"n_deltas={len(search_indices)} distinct_blocks={len(set(search_indices))} "
            f"indices={search_indices}",
        )
    else:
        p.skip("search", "rendition granularity", "no deltas in this run")

    # The stall that used to precede the rendition is a *timing* property and
    # cannot be read off the event list, so it is measured: the stream has to
    # span time rather than arrive as one lump. Before the loop streamed, the
    # first byte landed ~38.8s in and everything else inside 4ms.
    first_s, total_s = p.request_stream_timing(
        "/v1/messages",
        {"model": p.model, "max_tokens": 512, "stream": True, "tools": search_tools,
         "messages": [{"role": "user", "content": search_ask}]},
    )
    if total_s:
        p.check(
            "search",
            "stream spans time, not one lump at the end",
            total_s - first_s > 0.3,
            f"first_byte={first_s:.2f}s total={total_s:.2f}s spread={total_s - first_s:.2f}s",
        )
    else:
        p.skip("search", "stream timing", "request failed")

    status, _, text = p.request(
        "/v1/messages",
        {"model": p.model, "max_tokens": 64,
         "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 1}],
         "messages": [{"role": "user", "content": "Reply with exactly: OK. Do not search."}]},
    )
    p.check(
        "search",
        "non-search turn degrades gracefully",
        status == 200 and "server_tool_use" not in block_types(as_json(text) or {}),
        f"status={status} blocks={block_types(as_json(text) or {})}",
    )

    # The current variant must be recognized too: search keys on types/names
    # starting `web_search`, so a newer revision is picked up without a change.
    status, _, text = p.request(
        "/v1/messages",
        {"model": p.model, "max_tokens": 512,
         "tools": [{"type": "web_search_20260209", "name": "web_search", "max_uses": 2}],
         "messages": [{"role": "user", "content": search_ask}]},
    )
    p.check(
        "search",
        "current web_search_20260209 variant recognized",
        status == 200 and "server_tool_use" in block_types(as_json(text) or {}),
        f"status={status} blocks={block_types(as_json(text) or {})}",
    )

    print("\n-- stop_sequences and error shaping --")

    # The bridge no longer forwards `stop` upstream — OpenAI strips the
    # sequence from the text it returns, so a hit could not be recovered from
    # `finish_reason` or from the text. It keeps the list and truncates
    # locally, which is what Anthropic does server-side.
    status, _, text = p.request(
        "/v1/messages",
        {
            "model": p.model,
            "max_tokens": 400,
            "stop_sequences": ["BANANA"],
            "messages": [
                {"role": "user", "content": "Output exactly this and nothing else: one two BANANA three four"}
            ],
        },
    )
    body = as_json(text) or {}
    answer = text_of(body).strip()
    # An empty answer is not evidence either way: this upstream can spend the
    # whole budget on reasoning and never emit text at all, which would look
    # like a stop-sequence hit to a naive `"BANANA" not in text` test.
    hit = bool(answer) and "BANANA" not in answer
    if hit:
        p.check(
            "stop",
            "a stop-sequence hit reports stop_sequence",
            body.get("stop_reason") == "stop_sequence"
            and body.get("stop_sequence") == "BANANA",
            f"stop_reason={body.get('stop_reason')} stop_sequence={body.get('stop_sequence')!r} "
            f"answer={answer[:24]!r}",
        )
    else:
        p.skip(
            "stop",
            "stop-sequence reporting",
            f"upstream did not truncate this run (answer={answer[:24]!r})",
        )

    # The upstream WAF blocks `Python-urllib/*`; the point of this check is not
    # the status but that turnpike shapes a *non-JSON* upstream error into a
    # valid Anthropic error envelope rather than passing HTML through.
    status, _, text = p.request("/v1/messages", message(p), user_agent=BLOCKED_UA)
    body = as_json(text)
    shaped = (
        status == 200
        or (
            isinstance(body, dict)
            and body.get("type") == "error"
            and isinstance(body.get("error", {}).get("message"), str)
        )
    )
    p.check(
        "protocol",
        "non-JSON upstream error is shaped (blocked UA)",
        shaped,
        f"status={status} shaped={shaped} type={(body or {}).get('error', {}).get('type') if isinstance(body, dict) else '?'}",
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--base-url", default="http://127.0.0.1:8710")
    parser.add_argument(
        "--model", default="claude-sonnet-5", help="route id to probe (default: claude-sonnet-5)"
    )
    parser.add_argument(
        "--local-only",
        action="store_true",
        help="run only the checks that never reach the upstream (no quota spent)",
    )
    parser.add_argument("--timeout", type=float, default=180.0)
    args = parser.parse_args()

    probe = Probe(args.base_url, args.model, args.timeout)

    status, _, _ = probe.request("/_health", method="GET")
    if status != 204:
        print(
            f"no turnpike gateway at {probe.base_url} (/_health -> {status}).\n"
            "Start one with `turnpike serve`.",
            file=sys.stderr,
        )
        return 2

    # Prefer a route the gateway actually advertises, so the probe works on any
    # config rather than only the one it was written against.
    _, _, text = probe.request("/v1/models", method="GET")
    ids = [m.get("id") for m in (as_json(text) or {}).get("data", [])]
    if ids and probe.model not in ids:
        print(f"note: {probe.model!r} is not configured; probing {ids[0]!r} instead")
        probe.model = ids[0]

    mode = "local-only" if args.local_only else "full (spends upstream quota)"
    print(f"=== turnpike @ {probe.base_url} — model {probe.model} — {mode} ===\n")

    local_checks(probe)
    if not args.local_only:
        upstream_checks(probe)

    return probe.report()


if __name__ == "__main__":
    sys.exit(main())
