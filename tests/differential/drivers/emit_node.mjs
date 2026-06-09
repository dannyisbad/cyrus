// Node-original side of the differential harness (OAuth + JWT area).
//
// Drives the ACTUAL repo-agent-mcp `oauth.ts` code (via the __difftest__
// re-export surface) over the shared fixtures and prints the SAME canonical,
// line-oriented report the Rust `cyrus-diff-emit` binary prints for the `oauth`
// area. The integration test byte-diffs the two.
//
// Usage:  node emit_node.mjs <area> <fixtures_dir>
//   <area> in: oauth | all   (Node owns ONLY the OAuth/JWT surface here; the
//   Python driver owns v1delta / sse / parse_tool_call / relay.)

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

// The Node original is NOT part of this repo. Point CYRUS_OAUTH_TS at the
// original repo-agent-mcp `src/oauth.ts` to run the node-vs-rust differential.
// Without it we exit 86, which the Rust harness reports as SKIP (not a
// failure) so `cargo test` passes everywhere. Importing .ts needs a node that
// strips types (>= 23.6, or --experimental-strip-types).
const oauthTsPath = process.env.CYRUS_OAUTH_TS ?? "";
if (!oauthTsPath || !existsSync(oauthTsPath)) {
  process.stderr.write(
    "set CYRUS_OAUTH_TS to the original repo-agent-mcp src/oauth.ts to enable this differential\n",
  );
  process.exit(86);
}
const { __difftest__ } = await import(pathToFileURL(oauthTsPath).href);

const OUT = [];

// Compact JSON matching serde_json's default (no spaces, non-ASCII verbatim).
// JSON.stringify is already compact and keeps non-ASCII verbatim — exactly the
// canonical form the Rust/Python sides use.
function cj(v) {
  return JSON.stringify(v);
}

function line(tag, payload) {
  OUT.push(tag + "\t" + payload + "\n");
}

function load(dir, name) {
  return JSON.parse(readFileSync(join(dir, name), "utf-8"));
}

// `isBothChatGPT` is a closure local to oauth.ts's token() handler, so it can't
// be imported. This is the SAME three-line logic, kept in lockstep with the
// original (oauth.ts ~line 415).
function isBothChatGPT(uri1, uri2) {
  try {
    const u1 = new URL(uri1);
    const u2 = new URL(uri2);
    const allowed = ["chatgpt.com", "openai.com"];
    return allowed.includes(u1.hostname) && allowed.includes(u2.hostname);
  } catch {
    return false;
  }
}

function buildPayloadObject(c) {
  // Insertion order == claims_order, so JSON.stringify(obj) byte-matches the
  // fixture-ordered payload string the Rust side signs.
  const obj = {};
  for (const k of c.claims_order) obj[k] = c.claims[k];
  return obj;
}

function emitOauth(dir) {
  const fx = load(dir, "oauth_jwt.json");
  const defaultSecret = fx.secret;

  for (const c of fx.jwt_sign) {
    const secret = c.secret_override ?? defaultSecret;
    const token = __difftest__.signJwt(buildPayloadObject(c), secret);
    line("oauth.jwt." + c.name, cj(token));
  }

  for (const c of fx.pkce_s256) {
    const challenge = __difftest__.pkceS256Challenge(c.verifier);
    line("oauth.pkce." + c.name, cj(challenge));
  }

  for (const c of fx.redirect_allowlist) {
    const got = __difftest__.isValidRedirectUri(c.uri, c.base);
    line("oauth.redir." + c.name, cj(got));
  }

  for (const c of fx.both_chatgpt) {
    line("oauth.both." + c.name, cj(isBothChatGPT(c.a, c.b)));
  }

  for (const c of fx.validate_access_token) {
    const token = __difftest__.signJwt(buildPayloadObject(c), defaultSecret);
    const got = __difftest__.validateAccessToken(token, defaultSecret);
    line("oauth.validate." + c.name, cj(got));
  }
}

const area = process.argv[2] ?? "all";
const dir = process.argv[3] ?? "../fixtures";
if (area === "oauth" || area === "all") {
  emitOauth(dir);
}
process.stdout.write(OUT.join(""));
