# @mundy/cyrus

Run the **real** [Codex](https://github.com/openai/codex) CLI on your ChatGPT
subscription — one self-contained binary.

```sh
npx @mundy/cyrus setup    # one-time: connect your ChatGPT session
npx @mundy/cyrus          # codex on the plan you already pay for
```

Or install it so the command is just `cyrus`:

```sh
npm i -g @mundy/cyrus
cyrus setup
cyrus exec "fix the failing test"
```

This package is a thin launcher: on first run it downloads the matching
self-contained `cyrus` binary (which embeds codex + cloudflared) from the
[GitHub release](https://github.com/dannyisbad/cyrus/releases) and caches it
under `~/.cyrus/bin`. Prefer no Node at all? Use the shell installers:

```sh
curl -fsSL https://github.com/dannyisbad/cyrus/releases/latest/download/install.sh | sh   # macOS / Linux
```
```powershell
irm https://github.com/dannyisbad/cyrus/releases/latest/download/install.ps1 | iex        # Windows
```

> ⚠️ Automating the ChatGPT web app is almost certainly against OpenAI's Terms
> of Use and can get your account suspended. Read the
> [full project README](https://github.com/dannyisbad/cyrus) before using this.

License: MIT
