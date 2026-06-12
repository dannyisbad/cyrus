/* cyrus_connect.js — page-context connector automation for chatgpt.com
 *
 * Injected into the logged-in chatgpt.com tab via CDP Runtime.evaluate. Drives
 * the entire custom-MCP-connector lifecycle as authenticated same-origin
 * fetch() against /backend-api/aip/connectors/* — no settings-UI clicking.
 *
 * The bearer/REPO_AGENT_TOKEN NEVER enters the page. The one privileged step
 * (OAuth consent) is auto-issued by chimera via a nonce that cyrus arms over
 * loopback; the page only ever sees that single-use nonce.
 *
 * ── cyrus driver order (CDP) ───────────────────────────────────────────────
 *   1. inject this file once.                       Runtime.evaluate(<file>)
 *   2. r = await __cyrus.setup(P)                    // prep+dedupe+discover+create
 *        P = { base, name }   base = chimera publicUrl (no trailing /)
 *      -> { connectorId } | { reusedLinkId }         (reused => skip to step 6)
 *   3. cyrus LOCAL: nonce = random(24);              // bearer stays local
 *      POST http://127.0.0.1:8787/control/arm-consent
 *        Authorization: Bearer <REPO_AGENT_TOKEN>
 *        { "nonce": nonce, "ttl_sec": 90 }
 *   4. await __cyrus.linkAndNavigate(connectorId, nonce)
 *        -> page navigates to chimera /oauth/authorize?...&cyrus_nonce=nonce,
 *           chimera auto-issues the code, browser lands back on
 *           chatgpt.com/#settings/Connectors. (this eval's promise dies w/ nav)
 *   5. cyrus: wait until the tab is back on chatgpt.com, then RE-INJECT this
 *      file (navigation cleared window.__cyrus).
 *   6. await __cyrus.finishByConnector(connectorId)  // find link+refresh+lock
 *        -> { linkId, actions: [...] }
 *
 * Everything is idempotent enough to re-run: setup() dedupes by mcp_url.
 */
(() => {
  const enc = encodeURIComponent;

  // ── auth: harvest the page's own headers off a live /backend-api call ──────
  // Robust against client-version churn (no hardcoded oai-* constants). Falls
  // back to /api/auth/session for the bearer if nothing is captured in time.
  function harvestHeaders(timeoutMs = 5000) {
    return new Promise((resolve) => {
      const orig = window.fetch;
      let done = false;
      const finish = (h) => { if (done) return; done = true; window.fetch = orig; resolve(h); };
      window.fetch = function (input, init) {
        try {
          const u = typeof input === "string" ? input : (input && input.url) || "";
          if (u.includes("/backend-api/")) {
            const h = new Headers((init && init.headers) || (typeof input === "object" && input.headers) || {});
            const out = {};
            for (const [k, v] of h.entries()) {
              if (k === "authorization" || k.startsWith("oai-")) out[k] = v;
            }
            if (out.authorization) finish(out);
          }
        } catch (_) { /* ignore */ }
        return orig.apply(this, arguments);
      };
      setTimeout(() => finish(null), timeoutMs);
    });
  }

  async function authHeaders() {
    let h = await harvestHeaders();
    if (!h || !h.authorization) {
      // Fallback: pull the access token directly from the session endpoint.
      const s = await fetch("/api/auth/session", { credentials: "include" }).then((r) => r.json()).catch(() => ({}));
      h = h || {};
      if (s && s.accessToken) h.authorization = "Bearer " + s.accessToken;
    }
    if (!h.authorization) throw new Error("cyrus: could not obtain ChatGPT access token");
    return h;
  }

  // Cached per-injection so we harvest once.
  let _base = null;
  async function base() { return _base || (_base = await authHeaders()); }

  async function api(method, path, body) {
    const pathname = path.split("?")[0];
    const headers = Object.assign({}, await base(), {
      "content-type": "application/json",
      // Per-request edge routing headers, matching the captured client.
      "x-openai-target-path": pathname,
      "x-openai-target-route": pathname,
    });
    const res = await fetch(path, {
      method,
      headers,
      credentials: "include",
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
    const text = await res.text();
    let json; try { json = text ? JSON.parse(text) : null; } catch (_) { json = text; }
    if (res.status >= 400) {
      throw new Error(`cyrus: ${method} ${pathname} -> ${res.status} ${typeof json === "string" ? json : JSON.stringify(json)}`);
    }
    return json;
  }

  const C = "/backend-api/aip/connectors";

  // ── individual API calls (mirror the HAR exactly) ─────────────────────────
  const setAccountSetting = (feature, value) =>
    api("PATCH", `/backend-api/settings/account_user_setting?feature=${enc(feature)}&value=${enc(value)}`);

  const listConnectors = () =>
    api("POST", `${C}/list_accessible?skip_actions=true&external_logos=true&skip_directory=true`, { principals: [] })
      .then((r) => (r && r.connectors) || []);

  const listLinks = () =>
    api("POST", `${C}/links/list_accessible`, { principals: [], link_refresh_strategy: "NONE" })
      .then((r) => (r && r.links) || []);

  const deleteConnector = (id) => api("DELETE", `${C}/${id}`);

  const discoverOAuth = (mcpUrl) =>
    api("POST", `${C}/mcp/oauth_config`, { mcp_url: mcpUrl }).then((r) => r.oauth_config);

  const callbackId = (mcpUrl) =>
    api("GET", `${C}/oauth/callback_id?mcp_url=${enc(mcpUrl)}`).then((r) => r.callback_id).catch(() => null);

  function createConnector(name, mcpUrl, oauthConfig) {
    return api("POST", `${C}/mcp`, {
      name,
      mcp_url: mcpUrl,
      description: "",
      logo_url: null,
      auth_request: {
        supported_auth: [oauthConfig],
        oauth_client_params: { client_id: "repo-agent-mcp-client", client_secret: "", token_endpoint_auth_method: "none" },
        default_scopes: ["mcp"],
        oidc_enabled: true,
      },
    }).then((r) => r.connector);
  }

  const linkOAuth = (connectorId, name) =>
    api("POST", `${C}/links/oauth`, {
      connector_id: connectorId,
      oauth_client_id: null,
      name,
      action_names: null,
      requested_scopes: null,
      callback_url: "https://chatgpt.com/connector_platform_oauth_redirect",
      post_auth_url: "#settings/Connectors",
      tool_settings: { personalized: "NO_PERSONALIZATION" },
    });

  const refreshActions = (linkId) =>
    api("POST", `${C}/mcp/refresh_actions`, { link_id: linkId }).then((r) => (r && r.actions) || []);

  const lockPerms = (linkId) =>
    api("PATCH", `${C}/links/${linkId}`, { apps_privacy_control: "full_access" });

  // ── orchestration steps cyrus calls ───────────────────────────────────────

  // 1) account prep + dedupe + discover + create. Returns {connectorId} for a
  //    fresh connector, or {reusedLinkId} if a live one for this URL exists.
  async function setup({ base: baseUrl, name = "repo" }) {
    const mcpUrl = baseUrl.replace(/\/$/, "") + "/mcp";

    // account-level: tool default -> "important changes", enable dev mode
    await setAccountSetting("apps_privacy_control", "review_important_actions");
    await setAccountSetting("developer_mode", "true");

    // dedupe: a connector for this EXACT mcp_url is reused. We key on exact
    // base_url match only — never a domain/host heuristic. On a shared tunnel
    // apex (trycloudflare.com, ngrok-free.app) a host-based sweep would delete
    // the user's UNRELATED MCP connectors; cleanup of OUR stale connectors from
    // a previous tunnel URL is done by cyrus (Rust) against ids it recorded, so
    // this function only ever touches the exact-match connector.
    const connectors = await listConnectors();
    const mine = connectors.filter((c) => c && c.connector_type === "MCP" && c.base_url === mcpUrl);
    if (mine.length) {
      const links = await listLinks();
      const link = links.find((l) => l.connector_id === mine[0].id);
      if (link) return { reusedLinkId: link.id, connectorId: mine[0].id };
      // connector exists but no link yet -> drop it and recreate cleanly
      await deleteConnector(mine[0].id);
    }

    const oauthConfig = await discoverOAuth(mcpUrl);   // requires server SSE + WWW-Authenticate fixes
    await callbackId(mcpUrl);                           // hydrate (not strictly required)

    let connector;
    try {
      connector = await createConnector(name, mcpUrl, oauthConfig);
    } catch (e) {
      // ChatGPT enforces unique connector NAMES. A 409 here means a connector
      // with this name already exists at a DIFFERENT url — a stale cyrus
      // connector from a previous tunnel whose local record we lost (so the
      // exact-url dedupe above couldn't retire it). The name is cyrus's own
      // (default "repo"), so reclaim it: delete the duplicate and recreate at
      // the current url. We match on name AND mcp type AND a different base_url,
      // so a user's unrelated connector is never touched.
      if (!/ -> 409\b/.test(String(e && e.message))) throw e;
      const dup = (await listConnectors()).find(
        (c) => c && c.connector_type === "MCP" && c.name === name && c.base_url !== mcpUrl
      );
      if (!dup) throw e;
      await deleteConnector(dup.id);
      connector = await createConnector(name, mcpUrl, oauthConfig);
    }
    return { connectorId: connector.id };
  }

  // 2) kick OAuth and navigate. cyrus MUST have armed `nonce` over loopback
  //    first. This eval's promise will not resolve — the page navigates away.
  async function linkAndNavigate(connectorId, nonce, name = "repo") {
    const r = await linkOAuth(connectorId, name);
    const sep = r.redirect_url.includes("?") ? "&" : "?";
    window.location.assign(r.redirect_url + sep + "cyrus_nonce=" + enc(nonce));
    return { navigating: true };
  }

  // 3) after the tab returns to chatgpt.com (re-inject this file first):
  //    find the freshly-created link, refresh its tools, lock perms to never-ask.
  async function finishByConnector(connectorId) {
    const links = await listLinks();
    const link = links.find((l) => l.connector_id === connectorId);
    if (!link) throw new Error("cyrus: no link found for connector " + connectorId + " (OAuth may not have completed)");
    const actions = await refreshActions(link.id);
    await lockPerms(link.id);
    return { linkId: link.id, actions: actions.map((a) => a.name) };
  }

  window.__cyrus = { setup, linkAndNavigate, finishByConnector,
    // exposed for debugging / manual driving
    _api: { listConnectors, listLinks, deleteConnector, discoverOAuth, refreshActions, lockPerms } };
  return "cyrus_connect ready";
})();
