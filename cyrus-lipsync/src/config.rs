//! ShadowConfig — CDP target, repo-agent server URL, loop tuning, model/effort
//! axes, subagent caps, and the injected preambles.
//!
//! Source: idare/shadow/config.py (private original)
//!         (the `ShadowConfig` dataclass + `from_env`)
//!
//! This is a plain data carrier — a faithful port of the Python dataclass. It
//! holds defaults and reads the documented env vars in `from_env`; it does NOT
//! normalize anything.
//!
//! Hazards (kept verbatim from the source):
//!   - `model_slug` accepts BOTH a raw slug AND a friendly spec ("5.5 thinking",
//!     "5.5/pro"); the normalization lives in the conductor (chat.py in the
//!     original), NOT here. Config stays a dumb carrier.
//!   - The preamble strings are load-bearing — the connector-load instruction,
//!     the "empty result == success" / autonomy posture, and the AGENT_STATUS
//!     sentinel contract make chatgpt.com behave. They are copied byte-for-byte
//!     from the Python; do not paraphrase or reflow them.
//!   - `subagent_preamble` carries a literal `{agent_id}` placeholder. The Python
//!     fills it via `str.format(agent_id=...)`; the conductor does the
//!     substitution here. The stored text keeps the literal `{agent_id}`.

use std::env;

/// Configuration for the ChatGPT shadow harness (the "slavemaster").
///
/// Mirrors `ShadowConfig` in config.py field-for-field, including defaults.
#[derive(Debug, Clone)]
pub struct ShadowConfig {
    // CDP target (the puppeted ChatGPT tab)
    pub cdp_host: String,
    pub cdp_port: u16,
    pub tab_match: String,

    // repo-agent MCP server (source of truth for tool calls / file edits)
    pub server_url: String,

    // loop tuning
    /// seconds between tab polls
    pub poll_interval: f64,
    /// consecutive stable polls before a turn counts as complete
    pub stable_ticks: u32,
    /// safety cap on auto-continues within one task
    pub max_turns: u32,
    /// wall-clock safety cap per task (minutes)
    pub max_minutes: u32,
    pub continue_text: String,
    /// auto-click write-confirmation cards
    pub auto_approve: bool,

    // Make the automation read less like a metronome: jitter poll cadence, vary the
    // pause between pasting a prompt and sending, vary the nudge phrasing, stagger
    // tab creation. Bounded so it never breaks the loop, just blurs the timing.
    pub human_jitter: bool,
    pub continue_variants: Vec<String>,

    // OpenAI's connector moderation intermittently runs a tool server-side then
    // withholds the result/confirmation ("blocked by openai policy"), leaving the
    // model empty-handed. We detect (server-confirmed tool + empty model turn) and
    // re-deliver the server truth so it continues and doesn't re-apply done writes.
    // Cap per task to avoid loops if a turn is empty for some other reason.
    pub max_block_recoveries: u32,
    // In-turn loop: when a result is withheld mid-turn (moderation eats the tool-result
    // channel — likeliest on apply_patch, whose large payload gives the classifier more to
    // trip on), the model re-calls the SAME tool with the SAME args. The cached "already
    // ran" note rides the same eaten channel, so soft re-delivery NEVER breaks the loop —
    // only an out-of-band user-turn message does. The first identical re-call already
    // proves the loop (exact-arg match), so stop + nudge on it; waiting longer just burns
    // turns on re-deliveries that can't land. count==2 => first re-call.
    pub loop_repeat_threshold: u32,

    // Model and reasoning effort are TWO independent axes on chatgpt.com, both
    // carried in the /backend-api/f/conversation turn body and forced via a
    // fetch-wrapper (see chat.py):
    //   model_slug      -> the lane: gpt-5-5 (auto) / -instant / -thinking / -pro.
    //                      Accepts a slug or friendly spec ("5.5 thinking", "5.5/pro").
    //   thinking_effort -> reasoning depth for thinking-lane models:
    //                      min (Light) / standard / extended / max (Heavy).
    //                      Accepts those or aliases (light/heavy/high/...). Ignored
    //                      by non-thinking models. None = account default for both.
    /// main agent thread
    pub model_slug: Option<String>,
    /// main agent reasoning depth
    pub thinking_effort: Option<String>,
    /// default for spawned subagent threads
    pub subagent_model_slug: Option<String>,
    /// default subagent reasoning depth
    pub subagent_thinking_effort: Option<String>,

    // ----- subagents (each = its own ChatGPT tab/conversation) -----
    // The server bearer is needed for the auth-gated /control endpoints (bind,
    // subagent registry). Match repo-agent.config.json's bearerToken.
    pub server_bearer: Option<String>,
    /// concurrent live subagent tabs (2-3 is a practical ceiling)
    pub max_subagents: u32,
    pub subagent_max_turns: u32,
    pub subagent_max_minutes: u32,
    /// seconds of no tokens + not generating -> stalled
    pub subagent_idle_timeout: f64,
    /// seconds between /control/subagents polls
    pub spawn_poll_interval: f64,
    pub subagent_preamble: String,

    // Injected once at the start of each ChatGPT conversation. MCP connectors do
    // not reliably honor a server's `instructions`, so we front-load the contract:
    // always use the repo_* tools, and end every turn with the AGENT_STATUS line.
    pub send_preamble: bool,
    pub preamble: String,
}

/// Subagent preamble — injected at the top of each spawned subagent conversation.
///
/// Copied verbatim from `ShadowConfig.subagent_preamble` in config.py. The
/// `{agent_id}` token is a literal placeholder filled by the conductor (the
/// Python used `str.format(agent_id=...)`).
pub const SUBAGENT_PREAMBLE: &str = concat!(
    "SETUP — you are subagent \"{agent_id}\" working in parallel under a main agent.\n",
    "FIRST ACTION (required): call repo_status once before anything else, so the system can ",
    "register your session. Do not run any shell/edit before it.\n",
    "You share the repo via the **repo-agent** MCP connector. Do the scoped task below and ",
    "report a concise result; the main agent will collect it.\n",
    "• ALWAYS use the repo_* connector tools — never guess. Stay within any files you were scoped to.\n",
    "• Default posture is full-access: act autonomously.\n",
    "• REQUIRED: end EVERY reply with a final line, exactly one of:\n",
    "  <<<AGENT_STATUS: CONTINUE>>> / <<<AGENT_STATUS: DONE>>> / <<<AGENT_STATUS: BLOCKED: reason>>>\n",
    "Your final DONE message should be a tight summary of what you changed/found.\n\n",
    "TASK:\n",
);

/// Main conversation preamble — injected once at the start of each ChatGPT
/// conversation.
///
/// Copied verbatim from `ShadowConfig.preamble` in config.py. Load-bearing: the
/// connector-load instruction, the autonomy posture, and the AGENT_STATUS
/// sentinel contract are what make the page behave.
pub const PREAMBLE: &str = concat!(
    "SETUP (read once, then do the task):\n",
    "You are a coding agent driving a real repository through the **repo-agent** MCP connector.\n",
    "• ALWAYS use the repo_* connector tools for anything about the code — never guess file contents, ",
    "paths, or results from memory. Find with repo_grep / repo_glob, read with repo_read, change with ",
    "repo_edit (or repo_write for new files), validate with repo_run / repo_shell.\n",
    "• If the repo tools are not loaded, load the repo-agent connector first, then call repo_status.\n",
    "• Default posture is full-access: act autonomously, do not ask for approval for routine edits/commands.\n",
    "• Long-running processes (servers, watchers): repo_shell({command, background:true}), then poll ",
    "repo_bg_output.\n",
    "• REQUIRED: end EVERY reply with a final line, on its own, exactly one of:\n",
    "  <<<AGENT_STATUS: CONTINUE>>>  (more work to do)\n",
    "  <<<AGENT_STATUS: DONE>>>  (task complete and verified)\n",
    "  <<<AGENT_STATUS: BLOCKED: reason>>>  (need a human decision)\n",
    "An external driver reads that line; omit it and the loop halts.\n\n",
    "TASK:\n",
);

/// Default `continue_variants` — varied nudge phrasings so the cadence reads less
/// like a metronome. Mirrors the Python tuple default verbatim and in order.
const CONTINUE_VARIANTS: &[&str] = &[
    "continue",
    "go on",
    "keep going",
    "continue please",
    "next",
    "ok continue",
];

impl Default for ShadowConfig {
    /// Mirrors the dataclass field defaults in `ShadowConfig`.
    fn default() -> Self {
        Self {
            cdp_host: "127.0.0.1".to_string(),
            cdp_port: 9222,
            tab_match: "chatgpt.com".to_string(),

            server_url: "http://127.0.0.1:8787".to_string(),

            poll_interval: 1.2,
            stable_ticks: 3,
            max_turns: 50,
            max_minutes: 30,
            continue_text: "continue".to_string(),
            auto_approve: true,

            human_jitter: true,
            continue_variants: CONTINUE_VARIANTS.iter().map(|s| s.to_string()).collect(),

            max_block_recoveries: 4,
            loop_repeat_threshold: 2,

            model_slug: None,
            thinking_effort: None,
            subagent_model_slug: None,
            subagent_thinking_effort: None,

            server_bearer: None,
            max_subagents: 2,
            subagent_max_turns: 30,
            subagent_max_minutes: 12,
            subagent_idle_timeout: 90.0,
            spawn_poll_interval: 1.5,
            subagent_preamble: SUBAGENT_PREAMBLE.to_string(),

            send_preamble: true,
            preamble: PREAMBLE.to_string(),
        }
    }
}

impl ShadowConfig {
    /// Build a config from the process environment.
    ///
    /// Mirrors `ShadowConfig.from_env` in config.py: it overrides only the
    /// documented subset of fields; everything else keeps its dataclass default.
    ///
    /// Parsing notes (preserved from the Python):
    ///   - Numeric vars use the same string defaults the Python passed to
    ///     `int(...)` / `float(...)`. A malformed value would raise in Python; we
    ///     fall back to the documented default instead of panicking, which is the
    ///     closest safe equivalent for a config carrier.
    ///   - `AUTO_APPROVE` is true unless the value is exactly "0" (`!= "0"`),
    ///     matching the Python's `os.environ.get("AUTO_APPROVE", "1") != "0"`.
    ///   - The `*_BEARER` / `SHADOW_*` model/effort vars use the Python
    ///     `os.environ.get(...) or None` idiom: an unset OR empty value => None.
    pub fn from_env() -> Self {
        let d = Self::default();

        ShadowConfig {
            cdp_host: env::var("CDP_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            cdp_port: parse_or(env::var("CDP_PORT").ok().as_deref(), 9222),
            tab_match: env::var("TAB_MATCH").unwrap_or_else(|_| "chatgpt.com".to_string()),
            server_url: env::var("REPO_AGENT_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8787".to_string()),

            poll_interval: parse_or(env::var("POLL_INTERVAL").ok().as_deref(), 1.2),
            stable_ticks: parse_or(env::var("STABLE_TICKS").ok().as_deref(), 3),
            max_turns: parse_or(env::var("MAX_TURNS").ok().as_deref(), 50),
            max_minutes: parse_or(env::var("MAX_MINUTES").ok().as_deref(), 30),
            continue_text: env::var("CONTINUE_TEXT").unwrap_or_else(|_| "continue".to_string()),
            auto_approve: env::var("AUTO_APPROVE").as_deref().unwrap_or("1") != "0",

            server_bearer: env_or_none("REPO_AGENT_BEARER"),
            model_slug: env_or_none("SHADOW_MODEL"),
            thinking_effort: env_or_none("SHADOW_EFFORT"),
            subagent_model_slug: env_or_none("SHADOW_SUBAGENT_MODEL"),
            subagent_thinking_effort: env_or_none("SHADOW_SUBAGENT_EFFORT"),
            max_subagents: parse_or(env::var("MAX_SUBAGENTS").ok().as_deref(), 2),

            // Fields not read by from_env keep their dataclass defaults.
            ..d
        }
    }
}

/// Parse an optional env string into `T`, falling back to `default` when unset,
/// empty, or unparseable. Mirrors the Python defaulting (an absent var uses the
/// documented default string passed to `int`/`float`).
fn parse_or<T: std::str::FromStr>(value: Option<&str>, default: T) -> T {
    match value {
        Some(s) => s.trim().parse().unwrap_or(default),
        None => default,
    }
}

/// `os.environ.get(NAME) or None`: unset OR empty-string => None.
fn env_or_none(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_dataclass() {
        let c = ShadowConfig::default();
        assert_eq!(c.cdp_host, "127.0.0.1");
        assert_eq!(c.cdp_port, 9222);
        assert_eq!(c.tab_match, "chatgpt.com");
        assert_eq!(c.server_url, "http://127.0.0.1:8787");
        assert_eq!(c.poll_interval, 1.2);
        assert_eq!(c.stable_ticks, 3);
        assert_eq!(c.max_turns, 50);
        assert_eq!(c.max_minutes, 30);
        assert_eq!(c.continue_text, "continue");
        assert!(c.auto_approve);
        assert!(c.human_jitter);
        assert_eq!(
            c.continue_variants,
            vec![
                "continue",
                "go on",
                "keep going",
                "continue please",
                "next",
                "ok continue",
            ]
        );
        assert_eq!(c.max_block_recoveries, 4);
        assert_eq!(c.loop_repeat_threshold, 2);
        assert_eq!(c.model_slug, None);
        assert_eq!(c.thinking_effort, None);
        assert_eq!(c.subagent_model_slug, None);
        assert_eq!(c.subagent_thinking_effort, None);
        assert_eq!(c.server_bearer, None);
        assert_eq!(c.max_subagents, 2);
        assert_eq!(c.subagent_max_turns, 30);
        assert_eq!(c.subagent_max_minutes, 12);
        assert_eq!(c.subagent_idle_timeout, 90.0);
        assert_eq!(c.spawn_poll_interval, 1.5);
        assert!(c.send_preamble);
    }

    #[test]
    fn preambles_are_verbatim() {
        // AGENT_STATUS sentinel contract and connector-load lines are load-bearing.
        assert!(PREAMBLE.contains("<<<AGENT_STATUS: CONTINUE>>>"));
        assert!(PREAMBLE.contains("<<<AGENT_STATUS: DONE>>>"));
        assert!(PREAMBLE.contains("<<<AGENT_STATUS: BLOCKED: reason>>>"));
        assert!(PREAMBLE.contains("load the repo-agent connector first"));
        assert!(PREAMBLE.ends_with("TASK:\n"));
        // The subagent preamble keeps the literal {agent_id} placeholder.
        assert!(SUBAGENT_PREAMBLE.contains("subagent \"{agent_id}\""));
        assert!(SUBAGENT_PREAMBLE.ends_with("TASK:\n"));
    }

    #[test]
    fn parse_or_falls_back_on_garbage() {
        assert_eq!(parse_or::<u32>(Some("7"), 50), 7);
        assert_eq!(parse_or::<u32>(Some("nope"), 50), 50);
        assert_eq!(parse_or::<u32>(None, 50), 50);
        assert_eq!(parse_or::<f64>(Some("2.5"), 1.2), 2.5);
    }
}
