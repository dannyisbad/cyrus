# Cyrus Port Status

Rust port of two services from the private originals this project grew out of:

- **cyrus-chimera** — the chimera MCP + tool-relay + OAuth server (originally Node/TypeScript: `repo-agent-mcp-merged/src`).
- **cyrus-lipsync** — the lipsync "responses shim" that impersonates OpenAI `POST /v1/responses` for codex, drives a logged-in `chatgpt.com` tab over CDP, taps the page WebSocket, and re-emits answer tokens as codex-shaped SSE (originally Python: `idare/shadow`).

Status snapshot generated 2026-06-10 (post-wiring polish pass) against this workspace.

---

## TL;DR

- **Compile:** Clean. `cargo build --workspace` and `--bins` succeed with **zero warnings**, zero errors.
- **Tests:** `cargo test --workspace` → **139 passed / 0 failed / 0 ignored**:
  - **122 unit tests** (49 chimera + 73 lipsync)
  - **12 integration (boot) tests** (8 chimera + 4 lipsync) — these boot the REAL assembled servers on loopback ports
  - **5 differential tests** — every area **byte-identical** to the Python/Node originals (python 3.14.2, node v24.13.1)
- **The assembly/glue is DONE:** chimera `run_http` / `run_stdio` are live (no more `bail!`), and lipsync's `serve()` installs the real `TurnDriver` (`runtime::ConductorMux` over `ShimRuntime`) instead of the old `UnwiredDriver` stub.
- **MANUAL GATE: CLEARED 2026-06-10 (see below).** Both binaries validated live end-to-end: the Rust lipsync drove real chatgpt.com turns (plain answer AND a full connector tool round trip) and the Rust chimera served the live ChatGPT MCP connector through the cloudflared tunnel, accepting the previously-issued OAuth token. The full-Rust stack now runs on the production ports (chimera 8787, shim 8765).
- **Remaining (deliberate):** codex-rs tool reuse (apply_patch / exec-sandbox) is a stated TODO.

---

## Workspace layout

Single Cargo workspace, one lockfile, `resolver = "2"`, stable toolchain.

```
cyrus/
  Cargo.toml                # workspace: 3 members, shared dep versions
  rust-toolchain.toml       # channel = "stable"
  cyrus-chimera/            # MCP + tool-relay + OAuth server (port of repo-agent-mcp)
    src/{lib,main,config,http,oauth,mcp,tools,state,subagent,register,wire}.rs
    tests/boot.rs           # 8 integration tests against the real assembled HTTP server
  cyrus-lipsync/            # /v1/responses shim + CDP browser driver (port of idare/shadow)
    src/{lib,main,config,responses,cdp,tab_factory,wstap,v1delta,provider,conductor,subagent_mux,runtime}.rs
    tests/boot.rs           # 4 integration tests against the real axum app (mock TurnDriver)
  tests/
    differential/          # cross-language byte-diff harness (its own crate, publish=false)
      src/emit.rs          #   cyrus-diff-emit binary: drives the REAL port code
      tests/differential.rs#   cargo test surface: shells to python/node, byte-diffs
      drivers/{emit_python.py, emit_node.mjs}
      run_differential.{ps1,sh}
    fixtures/              # language-neutral JSON/JSONL inputs shared by all 3 sides
```

Module sizes (lines): chimera totals ~13.9k across 11 files (largest: `register.rs` 2927, `tools.rs` 2696, `state.rs` 1669, `oauth.rs` 1369, `mcp.rs` 1246). lipsync totals ~12.2k across 12 files (largest: `provider.rs` 2789, `subagent_mux.rs` 2491, `conductor.rs` 1682, `responses.rs` 1268, `runtime.rs` 1260).

Each crate root (`lib.rs`) declares one module per logical area of the original source tree and owns a crate-wide `Error`/`Result`. Module docs carry the absolute path of the original file they port and the behavioral hazards preserved from it.

### Assembly modules (the wiring added since the last snapshot)

- **`cyrus-chimera/src/register.rs`** — port of `tools/register.ts` (+ harness.ts, subagent.ts): `register_repo_tools` registers the full named tool surface (all 38 `repo_*` / codex-native tools: name + JSON schema + exposure + handler) on the MCP server builder, with the explicit-session threading replacing the TS AsyncLocalStorage capture.
- **`cyrus-chimera/src/wire.rs`** — port of `index.ts`'s `runHttp()`/`runStdio()` construction: concrete `StateAccess` / `OAuthProvider` / `McpDispatch` impls over the ported `state`/`oauth`/`mcp` modules, the config-view conversions, and the sync-over-async bridge (`block_in_place` + `Handle::block_on` — requires the multi-threaded runtime; do NOT drive from `current_thread`).
- **`cyrus-lipsync/src/runtime.rs`** — port of `responses_shim.py`'s `ShadowResponsesShim` runtime half: `ShimRuntime` (shared TabFactory rail, bind queue, project cache) + `ConductorMux` (`TurnDriver` impl owning the `thread-id -> ThreadConductor` map, eager-main adoption, `/control/*` routing) + `PageSurface` (one CDP client + one WsTap per tab, preserving per-thread WS isolation).

---

## What compiles / runs

- `cargo build --workspace` / `--bins` — **OK**, **0 warnings** (the former unused-import / dead-code stragglers are cleared).
- `cargo test --workspace --no-run` — **OK**; all test executables build.
- **chimera `run_http` is live:** boots state + jwt signing key + OAuth (iff `publicUrl`+`bearerToken`) + the registered MCP server and serves on axum with peer-IP `ConnectInfo` for the OAuth rate limiter. `run_stdio` runs the same registered server over the newline-delimited JSON-RPC stdin/stdout loop.
- **lipsync `serve()` is live:** constructs `ShimRuntime` + `ConductorMux`, applies the Python model/effort fallback chains, honors `--lazy` (eager-boots one MAIN tab otherwise; boot failure logs and serving continues), and serves `/v1/responses`, `/responses`, `/health`, `/control/*`.
- **Both binaries print useful `--help`** (`-h` too) and exit 0; lipsync's manual arg parser also reports unknown/missing-value flags as argparse-shaped errors.
- chimera `main.rs` signal path drains the real background-process registry (`tools::kill_all_background`) before exit; lipsync `main.rs` installs a `tracing-subscriber` fmt layer with env-filter. (Both were TODOs in the previous snapshot.)

---

## Test results

`cargo test --workspace` → **138 passed; 0 failed; 0 ignored** across all targets.

### Unit tests — 121 passing (48 chimera + 73 lipsync)

cyrus-chimera (48), by module:

- `config` (9): arg parsing (space/equals forms, flag-at-end), `as_string_array` filtering/fallback, dedup order, default profiles, JS-number empty-is-zero, auto-compact merge, lexical `..` collapse.
- `mcp` (13): Accept-header 406, JSON parse-error, DELETE 200, version negotiation + unknown-version fallback, tool widget meta/template, notification-only 202, resource URI mirror, tool-call reply, tools/list shape, unknown-tool invalid-params, unsupported-method 405+Allow, visibility-matches-harness.
- `oauth` (10): single-use armed nonce, base64url roundtrip vs Node, lenient both-chatgpt match, canonical JWT header, **JWT byte-for-byte vs Node**, sign/verify roundtrip, PKCE S256 challenge, rate-limit cap, signing-key default, redirect allowlist.
- `register` (4): all 28 required `repo_*` tools registered, codex-native relay tools registered, no duplicate tool names, `repo_ui` render meta.
- `subagent` (9): app-callable meta shape, duration-zero on garbage, JS-number stringify, summarize-result (no-capsule / not-collected / with-files / without-files), text-reply ok-splice, tool-defs cover all four names.
- `wire` (3): enum-str uses serde renames, job-patch maps present keys only, capsule strict-then-lenient lookup.

cyrus-lipsync (73), by module:

- `v1delta` (9): bare-continuation not moved by patch/replace, CoT emits thinking-not-token, ignores v1-declaration/blank, last-visible-assistant-wins, marker invisible-until-user-visible, patch-batch recursion, register+append visible assistant, turn_complete carries answer text, user-message not token.
- `responses` (30): extract_prompt variants (string / last-user / instructions-fallback / empty), SSE frame emission (created→added→delta→done→completed) for final-answer / tool-call / empty-injection sequences, `sse_frame` single-line + newline escaping, message_item / function_call_item / custom_tool_call_item shaping, `completed` usage shape, ReAct ` ```run ` / shell-fence parsing precedence (run-fence primary, lone-shell-fence dominates, two-shell-fences / long-prose are NOT calls), injection building (preamble-once, tool-result forwarding, plain-user).
- `provider` (12): block-recovery tool list, build-args command+files, event-id stringify, AGENT_STATUS parsing (blocked-detail, case/spacing, last-wins, strip/none), ISO-Z timestamp parse, version parse, effort-alias resolve, empty-args signature.
- `conductor` (10): blocking-tools long timeout, canonical-json key sort, conductor preamble (default forwards instructions / falls-back), cwd extraction (environment_context / text-field+case / none-when-absent), **preambles load-bearing verbatim**, project-name leaf folder, thread-bind directive substitutes `{thread_id}`.
- `runtime` (7): ConductorMux routing rules — same thread-id returns same conductor, missing/empty thread-id maps to default, first non-subagent thread becomes MAIN, eager-main adopted by first main thread only, subagent kind seeds once and never clobbers, control lookup falls back to main + strips the codex prefix, control toolcall/turn_complete round-trip.
- `config` (3): defaults-match-dataclass, parse-or-falls-back-on-garbage, **preambles verbatim**.
- `cdp` (2): chunked-transfer dechunk (basic + extension/array).

### Integration (boot) tests — 12 passing (NEW since last snapshot)

`cyrus-chimera/tests/boot.rs` (8) boots the REAL assembled HTTP server (`wire::build_app_state` + `http::serve`) on a free loopback port:

- root status route responds
- unauthenticated `/mcp` → 401 with the resource-metadata challenge
- `/mcp` via tunnel headers without bearer fails closed → 403
- loopback surfaces 404 through the tunnel but stays alive locally
- armed nonce auto-issues an OAuth code exactly once
- `.well-known` metadata uses the public URL
- **MCP handshake lists all 38 tools**
- MCP GET probe is an event-stream

`cyrus-lipsync/tests/boot.rs` (4) boots the REAL axum app (`responses::build_app` / `serve`):

- `serve(--lazy)`: `/health` answers and a turn without Chrome fails gracefully (no panic, codex-shaped failure)
- `/control/*` routes round-trip into the installed driver
- `/control/*` defaults to the Python 409 shapes when no conductor matches
- `POST /responses` streams codex-shaped SSE end-to-end via a mock `TurnDriver`

### Differential tests — 5 passing, 0 failing, 0 skipped

`cargo test -p cyrus-differential` → **5 passed; 0 failed**. The harness drives the *actual* port code via the `cyrus-diff-emit` binary and byte-diffs it against the *actual* originals (real python `idare/shadow`, real node `repo-agent-mcp/src/oauth.ts`) over shared fixtures — no browser, no network.

| area | vs | result |
|------|----|--------|
| `diff_v1delta_vs_python`         | python | MATCH (3360 bytes, 53 lines) |
| `diff_sse_vs_python`             | python | MATCH (3241 bytes, 25 lines) |
| `diff_parse_tool_call_vs_python` | python | MATCH (681 bytes, 10 lines) |
| `diff_relay_vs_python`           | python | MATCH (1025 bytes, 7 lines) |
| `diff_oauth_vs_node`             | node   | MATCH (1258 bytes, 19 lines) |

Covers: v1 delta decoding → token/thinking/turn_complete; `/v1/responses` SSE frame emission + `extract_prompt`; ReAct fence parsing; function_call / custom_tool_call relay item shaping; OAuth JWT HS256 sign + PKCE S256 + redirect allowlist + `validate_access_token`.

The previously documented `relay` divergence (compact vs Python-spaced inner-`arguments` JSON) was fixed on the Rust side (`json_dumps_default` mirrors CPython's default separators) and `tests/differential/README.md` now records it as **resolved** — the stale "currently FAILS" note is gone.

---

## TODO (deliberate, not regressions)

1. **codex-rs tool reuse — explicit TODO (tools.rs ~9).** Per the port brief, chimera's `repo_*` tools are **thin standalone Rust reimplementations** and deliberately do **not** path-depend on codex-rs. `apply_patch` and exec-sandbox reuse from codex-rs is a stated TODO; today's shell/file/patch ops are self-contained (with the load-bearing fidelity details preserved: `trim_middle` 0.58/0.32 ratios, quote-masked deny-regex, PowerShell as the Windows shell). If/when codex-rs is adopted, these are the call sites to swap.
2. **Cosmetic:** a few module docs still carry scaffold-era phrasing ("sibling modules are still stubs") that predates the wiring; the code they describe is current, the prose is not. Harmless, but worth sweeping on the next doc pass.

---

## MANUAL GATE — CLEARED 2026-06-10 (live validation against the running stack)

**Validated live, end-to-end, with zero Node/Python in the path:**

1. **Plain turn:** Rust lipsync opened a fresh chatgpt.com tab over CDP (boot→composer in ~0.9s), injected a prompt, tapped the token stream, and emitted the exact Responses SSE sequence (`created → output_item.added → output_text.delta → output_item.done → completed`).
2. **Full connector tool round trip (the production contract):** model loaded the `repo` connector and called `shell_command` → OpenAI backend → cloudflared tunnel → **Rust chimera** `/mcp` (the pre-existing connector OAuth token validated unchanged — JWT parity held in production) → relay POST → **Rust shim** `/control/toolcall` → conductor merged-stream → `function_call` SSE to codex → codex returned `function_call_output` → parked chimera call resolved → model streamed the final answer containing the real command output.
3. **HTTP/MCP parity vs the live Node server:** `GET /`, 401 `WWW-Authenticate`, all three `.well-known` docs byte-identical; `tools/list` identical **34/34** after gating the legacy subagent rail; arm-consent → `cyrus_nonce` → 302 auto-issue verified on the Rust binary.

**Live-gate fixes landed during validation (all covered by the suite, 139 green):**
- `cdp.rs http_get`: Chrome's DevTools HTTP endpoint **ignores `Connection: close`**, so the old read-to-EOF strategy hung forever on the very first page-socket attach. Now reads headers + exactly the framed body (Content-Length / chunked terminator), under a 15s deadline. (`BrowserControl` was immune — it uses reqwest.)
- `register.rs`: the four legacy subagent tools (`repo_await`/`repo_spawn_subagent`/`repo_subagent_kill`/`repo_subagent_list`) are now gated behind `CHIMERA_LEGACY_SUBAGENTS=1` exactly like register.ts:1200 — default tools/list is **34**, matching the live server (guard tests updated: 28-name completeness with the gate on, 34-with-legacy-absent by default).
- Boot-path `tracing::debug!` instrumentation in `runtime.rs::open_page` / `conductor.rs::boot`.

**OPS — launch lines that constitute the working production stack.** In normal
use `cyrus setup` spawns both of these for you (busybox: they run as `cyrus`
subprocesses of the setup process). The equivalent manual lines are:

```
# chimera (MCP connector server; tunnel ingress -> 8787)
CHIMERA_RELAY_URL=http://127.0.0.1:8765/control/toolcall \
  cyrus chimera --http --port 8787 --host 127.0.0.1 --repo <your-repo>

# lipsync (codex-facing /v1/responses shim)
SHIM_CONDUCTOR=1 REPO_AGENT_URL=http://127.0.0.1:8787 \
  cyrus lipsync --port 8765 --model gpt-5-5-thinking --effort max
```

(The standalone `cyrus-chimera` / `cyrus-lipsync` dev binaries accept the same
flags without the leading subcommand.)

**`SHIM_CONDUCTOR=1` is load-bearing** (faithful to `responses_shim.py:744`): without it the shim uses the legacy buffered/ReAct path, which never consumes chimera `/control/toolcall` dispatches — tool calls then hang for the full 300s hold and fail. The conductor path is the production path.

**Not yet exercised live:** subagent thread isolation under real load (multiple concurrent codex threads / `x-openai-subagent`), and long-haul stability (reconnect-by-target-id after Chrome restarts, auto-continue loops). The original caveats below remain relevant for those surfaces.

What the offline suite deliberately excludes (from `tests/differential/README.md`: *"no live browser, no Chrome, no network"*):

- **Driving a real `chatgpt.com` tab over CDP.** Opening a target, the arm-before-navigate sequence, one-page-socket-per-tab isolation, reconnect-by-target-id — none of this is exercised. The fixtures feed pre-recorded frames into the parser; they never touch Chrome.
- **The live token tap (`wstap`).** Subscribing to `Network.webSocketFrameReceived`, the inline `/f/conversation` SSE fetch-tee, the `FETCH_WRAPPER` JS injection (which also forces `supports_buffering=false` and the model/effort axes), and the separate-parser-per-transport guard are all reverse-engineered wire contracts that only a real session can confirm.
- **Real ChatGPT v1 delta streams.** The differential proves the decoder is byte-identical to the Python decoder *on fixed frames*. It does NOT prove those fixed frames still match what `chatgpt.com` actually emits today — the live encoding can drift independently of either port.
- **The full turn loop + auto-continue.** Paste → send → consume tap → AGENT_STATUS (CONTINUE/DONE/BLOCKED) termination → fan-in of chimera `/control` tool events, plus per-thread conductor routing and per-tab WS isolation under real timing/jitter. (The conductor/mux logic is unit-tested; the loop against a live page is not.)
- **codex ↔ shim ↔ chimera integration.** codex POSTing to `/responses` with `thread-id`, the `response.completed` hard-requirement, item-added-before-deltas ordering under real load, and the cloudflared-tunnel + OAuth bearer path against the live connector. (The boot tests cover the tunnel/OAuth HTTP semantics in-process; not the live connector.)

**Required human validation:** stand up the running stack (Chrome with a logged-in `chatgpt.com` session, the lipsync shim — now with the real `ConductorMux` driver — chimera serving MCP + `/control`, codex pointed at the shim) and confirm a real turn streams correct tokens end-to-end, tool calls round-trip, and subagent threads stay isolated. Until then, treat the green offline suite as **necessary but not sufficient**: the pure logic is proven byte-identical and the glue is exercised against mocks; the browser-fidelity surface is unproven.

---

## Remaining gaps (ranked)

1. **Subagent isolation + long-haul live coverage** — the single-thread live path is validated; multiple concurrent codex threads, `x-openai-subagent` tabs, reconnect-after-Chrome-restart, and auto-continue loops have not been exercised live.
2. **codex-rs tool reuse is a TODO** — `repo_*` tools are standalone reimplementations; apply_patch / exec-sandbox reuse from codex-rs is not yet done.
3. **Doc-comment sweep** — stale scaffold-era "still a stub" phrasing in a few module docs.
