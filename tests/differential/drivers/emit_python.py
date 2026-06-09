#!/usr/bin/env python3
"""Python-original side of the differential harness.

Drives the ACTUAL `idare.shadow` code (v1delta.V1DeltaParser, responses_shim
helpers) over the shared fixtures and prints the SAME canonical, line-oriented
report the Rust `cyrus-diff-emit` binary prints. The integration test byte-diffs
the two.

Usage:  python emit_python.py <area> <fixtures_dir>
  <area> in: v1delta | sse | parse_tool_call | relay | oauth | all

The OAuth/JWT area is intentionally SKIPPED here: the JWT/PKCE/redirect logic
lives in the Node `repo-agent-mcp` original, not in the Python shadow. The Node
driver covers it. (Python owns v1delta + the responses-shim SSE/relay surface;
Node owns OAuth.)
"""
import importlib.util
import json
import os
import sys

# ---- locate the idare.shadow package -------------------------------------
# The python original is NOT part of this repo. Point CYRUS_SHADOW_PY_ROOT at
# the directory that contains the `idare` package (i.e. <root>/idare/shadow/...)
# to run the python-vs-rust differential. Without it we exit 86, which the Rust
# harness reports as SKIP (not a failure) so `cargo test` passes everywhere.
SHADOW_ROOT = os.environ.get("CYRUS_SHADOW_PY_ROOT", "")
if not SHADOW_ROOT or not os.path.isdir(os.path.join(SHADOW_ROOT, "idare")):
    sys.stderr.write(
        "set CYRUS_SHADOW_PY_ROOT to the directory containing the original "
        "`idare` package to enable this differential\n"
    )
    sys.exit(86)
if SHADOW_ROOT not in sys.path:
    sys.path.insert(0, SHADOW_ROOT)

from idare.shadow.v1delta import V1DeltaParser  # noqa: E402
from idare.shadow.responses_shim import (  # noqa: E402
    extract_prompt,
    _message_item,
    _completed,
    _function_call_item,
    _custom_tool_call_item,
    parse_tool_call,
)


def cj(v) -> str:
    """Compact JSON matching serde_json's default: no spaces, non-ASCII kept."""
    return json.dumps(v, ensure_ascii=False, separators=(",", ":"))


OUT = []


def line(tag: str, payload: str) -> None:
    OUT.append(tag + "\t" + payload + "\n")


def sse_frame(obj) -> str:
    """Mirror responses_shim._sse's wire bytes (without the socket write)."""
    return "data: " + json.dumps(obj, ensure_ascii=False, separators=(",", ":")) + "\n\n"


def load(dir_, name):
    with open(os.path.join(dir_, name), encoding="utf-8") as f:
        return json.load(f)


# ===== v1delta ==============================================================

def emit_v1delta(dir_):
    # (1) recorded frames
    with open(os.path.join(dir_, "v1delta_frames.jsonl"), encoding="utf-8") as f:
        frames = f.read()
    p = V1DeltaParser()
    for raw in frames.splitlines():
        if not raw.strip():
            continue
        for kind, val in p.feed(raw):
            line("v1delta.frames", cj({"kind": kind, "value": val}))
    line("v1delta.frames.answer", cj(p.answer_text()))

    # (2) adversarial tokens
    adv = load(dir_, "adversarial_tokens.json")
    for case in adv["cases"]:
        name = case["name"]
        p = V1DeltaParser()
        p.feed('{"v":{"message":{"id":"m1","author":{"role":"assistant"},"content":{"parts":[""]}}}}')
        for tok in case["tokens"]:
            frame = {"p": "/message/content/parts/0", "o": "append", "v": tok}
            for kind, val in p.feed(cj(frame)):
                line("v1delta.adv." + name, cj({"kind": kind, "value": val}))
        line("v1delta.adv." + name + ".answer", cj(p.answer_text()))


# ===== SSE ==================================================================

def emit_sse(dir_):
    fx = load(dir_, "sse_sequences.json")
    for case in fx["extract_prompt"]:
        got = extract_prompt(case["body"])
        line("sse.extract." + case["name"], cj(got))

    for case in fx["full_turn"]:
        name = case["name"]
        response_id = case["response_id"]
        item_id = case["item_id"]
        tokens = case["tokens"]
        frames = []
        frames.append({"type": "response.created", "response": {}})
        frames.append({"type": "response.output_item.added", "item": _message_item("", item_id)})
        acc = ""
        for tok in tokens:
            acc += tok
            frames.append({"type": "response.output_text.delta", "delta": tok})
        frames.append({"type": "response.output_item.done", "item": _message_item(acc, item_id)})
        frames.append(_completed(response_id))
        for fr in frames:
            line("sse.turn." + name, cj(sse_frame(fr)))


# ===== parse_tool_call ======================================================

def emit_parse_tool_call(dir_):
    fx = load(dir_, "parse_tool_call.json")
    for case in fx["cases"]:
        got = parse_tool_call(case["text"])
        if got is None:
            payload = None
        else:
            payload = {"name": got["name"], "command": got["arguments"]["command"]}
        line("parse_tool_call." + case["name"], cj(payload))


# ===== relay items ==========================================================

def emit_relay(dir_):
    fx = load(dir_, "relay_items.json")
    for case in fx["function_call"]:
        item = _function_call_item(case["tool"], case["arguments"], case["call_id"])
        line("relay.fc." + case["name"], cj(item))
    for case in fx["custom_tool_call"]:
        item = _custom_tool_call_item(case["tool"], case["input"], case["call_id"])
        line("relay.ctc." + case["name"], cj(item))


# ===== driver ===============================================================

def main():
    area = sys.argv[1] if len(sys.argv) > 1 else "all"
    dir_ = sys.argv[2] if len(sys.argv) > 2 else "../fixtures"
    if area in ("v1delta", "all"):
        emit_v1delta(dir_)
    if area in ("sse", "all"):
        emit_sse(dir_)
    if area in ("parse_tool_call", "all"):
        emit_parse_tool_call(dir_)
    if area in ("relay", "all"):
        emit_relay(dir_)
    # oauth: not owned by Python; the Node driver covers it.
    sys.stdout.buffer.write("".join(OUT).encode("utf-8"))


if __name__ == "__main__":
    main()
