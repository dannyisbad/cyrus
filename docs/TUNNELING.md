# Tunneling

ChatGPT's MCP connector runs **server-side** — OpenAI's backend, not your
browser, makes the tool calls. So the chimera server on your machine has to be
reachable at a **public HTTPS URL**. That URL is the one moving part in the
whole setup, and how stable it is decides how often the ChatGPT connector has to
be re-created.

`cyrus setup` picks a tunnel provider automatically, in this order. The first
one that's configured/available wins.

## 1. ngrok with a static domain — recommended ⭐

One free signup gives you a **permanent** `*.ngrok-free.app` URL. Because it
never changes, the ChatGPT connector is created **once** and reused forever,
across reboots. This is the smoothest experience and what we recommend.

```
1. Sign up at https://ngrok.com (free) and install the ngrok agent.
2. ngrok config add-authtoken <your-token>
3. Reserve your free static domain on the dashboard
   (Cloud Edge → Domains), e.g. my-name.ngrok-free.app
4. Tell cyrus to use it:
      setx CYRUS_NGROK_DOMAIN my-name.ngrok-free.app   (Windows)
      export CYRUS_NGROK_DOMAIN=my-name.ngrok-free.app  (macOS/Linux)
```

That's it — `cyrus setup` now brings the tunnel up on that domain every time.

## 2 & 3. cloudflared — the after-note (fallback)

If you don't set `CYRUS_NGROK_DOMAIN`, cyrus falls back to
[cloudflared](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/).
Two shapes:

- **Named tunnel** — if you own a domain and have a `~/.cloudflared/config.yml`
  with an ingress hostname, cyrus reuses it. Stable, like ngrok-static, but it
  requires a domain + Cloudflare DNS setup.
- **Quick tunnel** — zero config: cyrus runs `cloudflared tunnel --url ...` and
  gets an ephemeral `*.trycloudflare.com` URL. Works out of the box, **but the
  URL changes every time cloudflared restarts** (reboot, crash, or closing it).
  When the URL changes, cyrus re-creates the ChatGPT connector automatically
  (no password — auto-issued), so it's hands-off; it just costs ~15-30s on a
  cold start and a fresh consent.

ngrok-ephemeral (a random `*.ngrok-free.app`, no reserved domain) is used only
if cloudflared isn't installed at all.

## Why a changing URL is safe now

The ChatGPT connector is keyed on the exact tunnel URL. When the URL changes,
the old connector is stale. cyrus records every connector **it** creates in
`~/.cyrus/connector.json` and, on a URL change, deletes **only those recorded
ids** — never anything matched by domain. So even on a shared apex like
`trycloudflare.com` or `ngrok-free.app`, your other, unrelated MCP connectors
are never touched.

## Summary

| Provider | URL stability | Cost | Connector churn |
|---|---|---|---|
| **ngrok static** ⭐ | permanent | one free signup | created once, ever |
| cloudflared named | permanent | needs your own domain | created once, ever |
| cloudflared quick | changes on restart | none | auto re-created on cold start |
| ngrok ephemeral | changes on restart | free signup | auto re-created on cold start |

Override the binaries if they aren't on `PATH`: `CYRUS_NGROK_EXE`,
`CYRUS_CLOUDFLARED_EXE`.
