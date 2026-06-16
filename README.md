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
| `cyrus-chimera` | MCP + tool-relay + OAuth server the ChatGPT connector talks to. |
| `cyrus-lipsync` | The `/v1/responses` shim that drives the chatgpt.com tab over CDP. |
| `docs/` | [TUNNELING.md](docs/TUNNELING.md) — stable-tunnel setup. |

**One binary ships the whole system.** The `cyrus` binary embeds chimera and
lipsync as hidden subcommands (`cyrus chimera …`, `cyrus lipsync …`): `cyrus
setup` spawns the two servers as `cyrus` subprocesses of itself, so the only
file you need to ship or run is `cyrus`.

## Quick start

One command — install and run. codex and cloudflared are embedded, so there's
nothing else to fetch:

```sh
# Node
npm i -g @mundy/cyrus && cyrus
```
```sh
# macOS / Linux — no Node required
curl -fsSL https://github.com/dannyisbad/cyrus/releases/latest/download/install.sh | sh
```
```powershell
# Windows (PowerShell)
irm https://github.com/dannyisbad/cyrus/releases/latest/download/install.ps1 | iex
```

The first run of `cyrus` opens Chrome and asks you to log in to ChatGPT once —
the only manual step. After that, use `cyrus` anywhere you'd use `codex`:

```sh
cyrus                          # codex, on the plan you already pay for
cyrus exec "fix the failing test"
```

You'll need Google Chrome and a ChatGPT Plus/Pro account. Windows is the
most-tested platform; macOS and Linux builds are published too.

<details><summary>Build from source instead</summary>

Requirements: Rust (stable). The release binary embeds the pinned codex fork +
cloudflared; a plain `cargo build` does not (it resolves them from PATH).

```sh
cargo build --release --workspace
target/release/cyrus setup --repo /path/to/your/repo
```

For the embedded single-binary (what the installer ships), see
`scripts/release.ps1` (Windows) or `.github/workflows/release.yml` (all platforms).
</details>

`cyrus setup` walks six idempotent steps and tells you the one thing it needs
from you (a ChatGPT login in the Chrome window it opens). Re-running it is
always safe: healthy layers are reused, broken ones repaired. It writes
**nothing** to your codex config — the `cyrus` front door injects the model
provider as per-launch overrides, so a plain `codex` stays pristine. After it
finishes, run `cyrus` and your turns go through your ChatGPT session.

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
Point cyrus at a different codex binary with `CYRUS_CODEX_BIN`. (cyrus injects
the model provider as per-launch `-c` overrides — nothing is written to your
codex config, and a plain `codex` keeps running on its normal provider.)

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

## License

This repository is licensed under the [MIT License](LICENSE).

cyrus is designed to work with — and distributions of it may bundle —
[codex](https://github.com/openai/codex), which is licensed under the
Apache License 2.0. Bundled distributions include codex's `LICENSE` and
`NOTICE` files unmodified, and any changes to codex itself are published in a
separate fork under Apache-2.0. Nothing in this repository contains codex
source code.
