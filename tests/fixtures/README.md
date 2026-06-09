# Differential test fixtures

These fixtures are the **fixed inputs** fed to BOTH the original implementations
(Python `idare/shadow`, Node `repo-agent-mcp`) and the Rust port (`cyrus-lipsync`,
`cyrus-chimera`). The differential harness (`tests/differential`) byte-diffs the
canonical outputs each side produces.

Each `.json` / `.jsonl` file is loaded by all three sides identically, so a
divergence is a real behavioral difference, never a fixture-encoding artifact.

| file | area | feeds |
|------|------|-------|
| `v1delta_frames.jsonl`        | v1delta decoding              | one delta frame per line; parser emits token/thinking/turn_complete events |
| `adversarial_tokens.json`     | SSE frame emission + v1delta  | nasty token strings (newlines, quotes, backslashes, unicode, fake `<<<sentinel>>>`) |
| `sse_sequences.json`          | /v1/responses SSE emission    | extract_prompt / message_item / function_call / custom_tool_call / completed cases |
| `parse_tool_call.json`        | chimera relay shaping (ReAct) | ```run / shell-fence / final-answer texts -> parsed tool call or none |
| `oauth_jwt.json`              | OAuth+JWT outputs             | fixed-payload JWT sign + PKCE S256 + redirect allowlist + token-endpoint cases |
| `relay_items.json`            | chimera tool-call relay shaping | function_call / custom_tool_call item builders with fixed call_ids |

## Determinism contract

Outputs that embed a random id (`call_id`, `msg_…`, `resp_…`, JWT `jti`, OAuth
`code`) are made deterministic by EITHER supplying a fixed id in the fixture OR
canonicalizing it out before comparison. JWTs are signed over a **fixed**
`iat`/`exp` and a **fixed secret**, so the HMAC signature is reproducible and
cross-validates between Node and Rust.
