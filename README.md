# cyrus

Run [codex](https://github.com/openai/codex) against the ChatGPT subscription
you already pay for — your Plus/Pro web session becomes the model backend, with
real, structured tool calls.

cyrus is **not** an API reverse-engineering project and it does **not** get you
anything for free. It automates a real, logged-in `chatgpt.com` browser tab on
your own machine, using the subscription you already have. The value
proposition is *"use the capacity you're already paying for from your
terminal"* — nothing more.

> **⚠️ Read [Transparency & Terms of Service](#transparency--terms-of-service)
> before using this.** Automating the ChatGPT web app is almost certainly
> against OpenAI's Terms of Use and can get your account suspended or banned.

## How it works

```
                 your machine                                │   OpenAI
                                                             │
  codex ──HTTP /v1/responses──▶ cyrus-lipsync ──CDP──▶ Chrome tab ──▶ chatgpt.com
   ▲                              (the "shim")        (logged-in you)     │
   │                                  ▲                                   │
   │ executes tools locally           │ relays tool calls                 │
   │                                  │                                   ▼
   └────────────────────────── cyrus-chimera ◀──HTTPS tunnel──── ChatGPT MCP
                               (MCP server)                      connector
```

- **cyrus-lipsync** impersonates the OpenAI `POST /v1/responses` endpoint on
  `127.0.0.1:8765`. When codex sends a turn, lipsync types the prompt into a
  real Chrome tab over the DevTools protocol (CDP), taps the page's own
  WebSocket for the streamed answer tokens, and re-emits them to codex as
  codex-shaped SSE. Your credentials never leave the browser — lipsync drives
  the session you already opened.
- **cyrus-chimera** is an MCP server on `127.0.0.1:8787`. ChatGPT's connector
  framework calls tools **server-side**, so chimera is exposed through a public
  HTTPS tunnel (ngrok/cloudflared, see [docs/TUNNELING.md](docs/TUNNELING.md))
  protected by OAuth + a bearer token. When the model makes a tool call,
  chimera relays it back to lipsync, codex executes it locally in your repo,
  and the result flows back up.
- **cyrus-setup** (the `cyrus` binary) is the one-shot orchestrator:
  `cyrus setup` verifies-or-repairs every layer — secrets, Chrome/CDP, tunnel,
  servers, the ChatGPT connector itself (created programmatically, no UI
  clicking), and the codex `shadow` model provider. The only human step is
  logging in to ChatGPT in the window it opens. `cyrus check` reports health.

## Repository layout

| Crate | What it is |
|---|---|
| `cyrus-setup` | The `cyrus` CLI: one-shot setup/repair engine (`cyrus setup`, `cyrus check`), connector automation, tunnel management. Library name: `cyrus_engine`. |
| `cyrus-chimera` | MCP + tool-relay + OAuth server the ChatGPT connector talks to. Rust port of a private Node/TypeScript original (`repo-agent-mcp`). |
| `cyrus-lipsync` | The `/v1/responses` shim that drives the chatgpt.com tab over CDP. Rust port of a private Python original (`idare/shadow`). |
| `tests/differential` | Byte-diff harness that compares this port's output against the private originals (skips gracefully when you don't have them — see below). |
| `docs/` | [TUNNELING.md](docs/TUNNELING.md) (read this), [PORT_STATUS.md](docs/PORT_STATUS.md) (port provenance + validation history). |

**One binary ships the whole system.** The `cyrus` binary embeds chimera and
lipsync as hidden subcommands (`cyrus chimera …`, `cyrus lipsync …`, the
[busybox](https://en.wikipedia.org/wiki/BusyBox) pattern): `cyrus setup` spawns
the two servers as `cyrus` subprocesses of itself, so the only file you need to
ship or run is `cyrus`. The standalone `cyrus-chimera` / `cyrus-lipsync`
binaries are also built, but only as a convenience for running a server
directly during development — nothing in the product depends on them.

## Quick start

Requirements: Rust (stable), Google Chrome, a ChatGPT Plus/Pro account, and
[ngrok](https://ngrok.com) or
[cloudflared](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/)
for the tunnel. Windows is the validated platform; macOS/Linux should work but
are less tested.

```sh
cargo build --release --workspace

# the single self-contained binary is all you need to run or ship
target/release/cyrus setup --repo /path/to/your/repo
```

`cyrus setup` walks six idempotent steps and tells you the one thing it needs
from you (a ChatGPT login in the Chrome window it opens). Re-running it is
always safe: healthy layers are reused, broken ones repaired. It finishes by
writing a `shadow` model provider into `~/.codex/config.toml`, after which
codex can run turns through your ChatGPT session.

### `cyrus` is a front door to `codex`

After setup, just use `cyrus` wherever you'd use `codex` — it's a drop-in
alias. A small reserved set of subcommands is cyrus's own; **everything else is
passed straight through to `codex`** with the `shadow` provider injected, so
your turns run on your ChatGPT subscription:

```sh
cyrus setup [--repo <dir>]   # cyrus: bring up / repair the stack
cyrus check                  # cyrus: per-component diagnostics

cyrus                        # → codex (interactive) on your ChatGPT subscription
cyrus exec "fix the test"    # → codex exec … on your ChatGPT subscription
cyrus resume --last          # → codex … (any codex command/flags work)
```

A one-line breadcrumb on stderr (`cyrus › codex …`) makes the handoff obvious.
Point cyrus at a different codex binary with `CYRUS_CODEX_BIN`. (Prefer the
plain `codex` command instead? The `shadow` provider is in your codex config —
select it with `codex -c model_provider=shadow`, or set `model_provider =
"shadow"` as your codex default.)

For a stable tunnel URL (strongly recommended — it means the ChatGPT connector
is created once, ever), set up a free ngrok static domain:
see [docs/TUNNELING.md](docs/TUNNELING.md).

## Transparency & Terms of Service

Be clear-eyed about what this does before you run it:

- **What it automates.** cyrus programmatically drives the ChatGPT **web app**
  (chatgpt.com) in a real browser session that you log in to. It sends prompts,
  reads streamed responses, and manages an MCP connector on your account — the
  same things you could do by hand, done by software.
- **It almost certainly violates OpenAI's Terms of Use.** OpenAI's consumer
  terms restrict automated or programmatic access to the ChatGPT service
  outside their official API. Using cyrus may lead to rate-limiting,
  suspension, or **permanent loss of your OpenAI account**. That risk is yours
  and yours alone.
- **No payment is bypassed.** cyrus only uses a subscription you already pay
  for, under that subscription's own usage limits. That does not make the
  automation permitted — it just means nothing is being stolen.
- **Nothing is hidden from you.** All traffic flows between your machine, your
  browser, your tunnel, and OpenAI. There is no third-party server, no
  telemetry, and no credential ever leaves your machine (see
  [Security model](#security-model)).
- **No affiliation.** This project is not affiliated with, endorsed by, or
  supported by OpenAI. "ChatGPT", "OpenAI", and "codex" are their respective
  owners' marks.

If that trade-off isn't acceptable to you, use the official codex + API-key
path instead. If it is: read OpenAI's current Terms of Use yourself, and use a
personal account you can afford to lose.

## Security model

- **The bearer token never enters the page.** The one privileged step of
  connector setup (OAuth consent) is auto-issued by chimera via a single-use
  nonce armed over loopback by the local `cyrus` process; page JavaScript only
  ever sees that nonce.
- **The tunnel endpoint is not open.** Everything behind the public URL
  requires OAuth (HS256 JWTs) and/or the bearer token; `/control/*` endpoints
  are loopback-only.
- **Connector cleanup is surgical.** cyrus records every connector *it*
  creates (`~/.cyrus/connector.json`) and only ever deletes those recorded ids
  — never anything matched by domain, so unrelated MCP connectors on your
  account are never touched.
- **Local state** (secrets, the dedicated Chrome profile, connector records)
  lives in `~/.cyrus/`. Treat it like credentials.

## Testing

```sh
cargo test --workspace
```

Everything passes on a clean machine. The differential suite additionally
byte-compares this Rust port against the private Node/Python originals it was
ported from; without access to those originals the comparisons are reported as
`SKIP` (not failures). If you do have them:

```sh
CYRUS_SHADOW_PY_ROOT=/path/to/dir-containing-idare-package \
CYRUS_OAUTH_TS=/path/to/repo-agent-mcp/src/oauth.ts \
cargo test -p cyrus-differential
```

## License

This repository is licensed under the [MIT License](LICENSE).

cyrus is designed to work with — and distributions of it may bundle —
[codex](https://github.com/openai/codex), which is licensed under the
Apache License 2.0. Bundled distributions include codex's `LICENSE` and
`NOTICE` files unmodified, and any changes to codex itself are published in a
separate fork under Apache-2.0. Nothing in this repository contains codex
source code.
