# Differential test harness

Proves the Rust port (`cyrus-lipsync` + `cyrus-chimera`) produces output
**byte-identical** to the originals on fixed inputs — no live browser, no Chrome,
no network. Each fixture is fed to BOTH the original implementation and the Rust
port; the two canonical reports are byte-diffed.

| area | original driven | Rust port driven | covers |
|------|-----------------|------------------|--------|
| `v1delta`         | python `idare/shadow/v1delta.py` | `cyrus_lipsync::v1delta` | v1 delta-frame decoding -> token/thinking/turn_complete events |
| `sse`             | python `responses_shim.py`       | `cyrus_lipsync::responses` | extract_prompt + `/v1/responses` SSE frame emission |
| `parse_tool_call` | python `responses_shim.py`       | `cyrus_lipsync::responses` | ReAct ```run / shell-fence parsing (chimera relay decision) |
| `relay`           | python `responses_shim.py`       | `cyrus_lipsync::responses` | function_call / custom_tool_call item shaping (relayed chimera calls) |
| `oauth`           | node `repo-agent-mcp/src/oauth.ts` | `cyrus_chimera::oauth` | JWT HS256 sign, PKCE S256, redirect allowlist, validate_access_token |

## How it works

1. **Fixtures** (`cyrus/tests/fixtures/`) are language-neutral JSON/JSONL, loaded
   identically by all three sides.
2. **`src/emit.rs`** (binary `cyrus-diff-emit`) drives the actual port code and
   prints a canonical, line-oriented report (`<tag>\t<compact-json>`).
3. **`drivers/emit_python.py`** and **`drivers/emit_node.mjs`** drive the actual
   originals and print the SAME report format.
4. **`tests/differential.rs`** (the `cargo test` surface) shells out to
   python/node, runs the emit binary, and byte-diffs per area.

Random ids (`call_id`, `msg_*`, `resp_*`, JWT `jti`, OAuth codes) are made
deterministic by supplying fixed ids in the fixtures. JWTs are signed over fixed
`iat`/`exp` + a fixed secret, so the HMAC signature is reproducible and
cross-validates Node<->Rust.

## Run it

```
# as cargo test (per-area; prints MATCH/DIFFER/SKIP):
cargo test -p cyrus-differential --test differential -- --nocapture

# standalone (no cargo harness; prints a PASS/FAIL/SKIP summary):
pwsh tests/differential/run_differential.ps1     # Windows
bash tests/differential/run_differential.sh      # POSIX / git-bash
```

If python or node is missing, that area is **SKIPPED**, not failed.

## Locating the originals

The Python/Node originals are **not part of this repository** (they are the
private codebases this port was extracted from). To run the vs-original halves,
point the drivers at your local copies:

```
CYRUS_SHADOW_PY_ROOT = directory containing the original `idare` python package
CYRUS_OAUTH_TS       = path to the original repo-agent-mcp src/oauth.ts
```

When either is unset (the normal case for contributors), the driver exits with
the skip sentinel (code 86) and the corresponding areas are **SKIPPED** — the
Rust-side emitter still runs, so `cargo test` passes everywhere. Importing
`oauth.ts` directly requires a Node that strips types (>= 23.6, or
`--experimental-strip-types`).

## Test-only surface added to the originals/port

To drive the *actual* private JWT/PKCE/redirect functions (not a reimplementation),
two non-behavioral re-export shims were added:

- `cyrus-chimera/src/oauth.rs` — `pub mod difftest { ... }`
- `repo-agent-mcp-merged/src/oauth.ts` — `export const __difftest__ = { ... }`

Both only widen visibility; they add no logic.

## Resolved divergence (the harness's job)

All 5 areas — including `relay` — now **MATCH byte-for-byte**.

`relay` used to FAIL: the Rust `function_call_item` serialized the inner
`arguments` JSON string compactly (`{"command":"echo hi"}`), while the Python
`_function_call_item` uses `json.dumps(..)` with default separators, emitting
spaces (`{"command": "echo hi"}`). Both were valid JSON that codex re-parses,
so they were functionally equivalent — but not byte-identical on the wire.

This was fixed on the Rust side: `cyrus-lipsync/src/responses.rs` now serializes
non-string `arguments` through `json_dumps_default`, a custom `serde_json`
`Formatter` that mirrors CPython's default separators (`", "` between items,
`": "` after keys) and `or {}` falsy coercion, so the wire bytes match Python
exactly. The harness keeps guarding this: any regression in the separator
spacing flips `relay` back to DIFFER.
