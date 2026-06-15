//! The `repo_*` tool surface: schemas, handlers, and the standalone repo ops
//! (shell / apply_patch / file read-write-edit-glob-grep / background procs).
//!
//! Thin standalone impls — this module deliberately does NOT path-depend on
//! codex-rs (apply_patch / exec-sandbox reuse is a TODO).
//!
//! Behavioral notes:
//!   - `trim_middle` head/tail ratios are 0.58 / 0.32 (load-bearing).
//!   - the deny-regex floor is matched against quote-masked text so a dangerous
//!     token inside a quoted literal does not trip, while command substitution
//!     ($(...) / backticks) stays visible.
//!   - the Windows shell is PowerShell; the default deny set covers both *nix and
//!     Windows destructive forms (rm -rf /, format c:, del /s /q).

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read-only" => Some(SandboxMode::ReadOnly),
            "workspace-write" => Some(SandboxMode::WorkspaceWrite),
            "danger-full-access" => Some(SandboxMode::DangerFullAccess),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    Untrusted,
    OnRequest,
    Never,
}

impl ApprovalPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalPolicy::Untrusted => "untrusted",
            ApprovalPolicy::OnRequest => "on-request",
            ApprovalPolicy::Never => "never",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "untrusted" => Some(ApprovalPolicy::Untrusted),
            "on-request" => Some(ApprovalPolicy::OnRequest),
            "never" => Some(ApprovalPolicy::Never),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReviewer {
    User,
    AutoReview,
}

/// `ParsedCommandSummary`. `kind` is the classifier output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedCommandSummary {
    pub kind: String,
    pub cmd: String,
    pub safe: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

/// A permission profile (partial in TS — every field optional). Only the fields
/// the porting surface reads are kept.
#[derive(Debug, Clone, Default)]
pub struct PermissionProfile {
    pub sandbox_mode: Option<SandboxMode>,
    pub approval_policy: Option<ApprovalPolicy>,
    pub reviewer: Option<ApprovalReviewer>,
    pub writable_roots: Option<Vec<String>>,
    pub command_allow_prefixes: Option<Vec<String>>,
    pub command_prompt_prefixes: Option<Vec<String>>,
}

/// `RepoAgentConfig` (subset). Mutated in place by `set_permissions` /
/// `switch_project`, so the handler surface takes `&mut Config` where needed.
#[derive(Debug, Clone)]
pub struct Config {
    pub root: PathBuf,
    pub home_root: PathBuf,
    pub current_project: Option<String>,
    pub project_search_roots: Vec<PathBuf>,
    pub projects: Vec<ProjectConfig>,
    pub max_project_scan_depth: i64,
    pub spice_level: String,
    pub allow_model_write_file: bool,
    pub allow_model_dev_shell: bool,
    pub allow_secrets_read: bool,
    pub allow_hidden_files: bool,
    pub hide_hidden_dirs: bool,
    pub sandbox_mode: SandboxMode,
    pub approval_policy: ApprovalPolicy,
    pub approvals_reviewer: ApprovalReviewer,
    pub writable_roots: Vec<PathBuf>,
    pub command_allow_prefixes: Vec<String>,
    pub command_prompt_prefixes: Vec<String>,
    pub permission_profiles: BTreeMap<String, PermissionProfile>,
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
    pub max_command_output_bytes: usize,
    pub default_command_timeout_ms: u64,
    pub blocked_path_globs: Vec<String>,
    pub command_profiles: BTreeMap<String, String>,
    pub command_deny_regex: Vec<String>,
    pub env_passthrough: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ProjectConfig {
    pub name: Option<String>,
    pub root: PathBuf,
    pub description: Option<String>,
    pub tags: Vec<String>,
}

impl Config {
    /// Default config mirroring the PRODUCTION defaults in `config::load_config`
    /// (shared consts/fns from `crate::config` where they exist). This used to
    /// carry a divergent inline copy — 3 blocked globs instead of 12,
    /// `allow_hidden_files: false` instead of true, 5MB writes instead of 350KB —
    /// so tests and fallback paths exercised a posture production never runs.
    /// Live servers populate `tools::Config` via `wire::tools_config_view`; this
    /// constructor is for tests and standalone embedding only.
    pub fn with_root(root: PathBuf) -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "read-only".to_string(),
            PermissionProfile {
                sandbox_mode: Some(SandboxMode::ReadOnly),
                approval_policy: Some(ApprovalPolicy::OnRequest),
                reviewer: Some(ApprovalReviewer::User),
                ..Default::default()
            },
        );
        profiles.insert(
            "auto".to_string(),
            PermissionProfile {
                sandbox_mode: Some(SandboxMode::WorkspaceWrite),
                approval_policy: Some(ApprovalPolicy::OnRequest),
                reviewer: Some(ApprovalReviewer::User),
                ..Default::default()
            },
        );
        profiles.insert(
            "full-access".to_string(),
            PermissionProfile {
                sandbox_mode: Some(SandboxMode::DangerFullAccess),
                approval_policy: Some(ApprovalPolicy::Never),
                reviewer: Some(ApprovalReviewer::User),
                ..Default::default()
            },
        );
        Config {
            home_root: root.clone(),
            root,
            current_project: None,
            project_search_roots: Vec::new(),
            projects: Vec::new(),
            max_project_scan_depth: 3,
            spice_level: "spicy".to_string(),
            allow_model_write_file: true,
            allow_model_dev_shell: true,
            allow_secrets_read: false,
            // Production default (config.rs): hidden FILES are listable; hidden
            // DIRS stay hidden. The traversal denylist in walk_files holds anyway.
            allow_hidden_files: true,
            hide_hidden_dirs: true,
            // Production default profile is "full-access" (config.rs).
            sandbox_mode: SandboxMode::DangerFullAccess,
            approval_policy: ApprovalPolicy::Never,
            approvals_reviewer: ApprovalReviewer::User,
            writable_roots: Vec::new(),
            command_allow_prefixes: Vec::new(),
            command_prompt_prefixes: Vec::new(),
            permission_profiles: profiles,
            max_read_bytes: 180_000,
            max_write_bytes: 350_000,
            max_command_output_bytes: 160_000,
            default_command_timeout_ms: 120_000,
            blocked_path_globs: crate::config::DEFAULT_BLOCKED_PATH_GLOBS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            command_profiles: crate::config::default_command_profiles(),
            command_deny_regex: default_deny_regex(),
            env_passthrough: crate::config::DEFAULT_ENV_PASSTHROUGH
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// Default hard-deny regexes. Cover both POSIX and Windows destructive shapes
/// (the brief calls these the "deny-regex floor"). Matched case-insensitively
/// against quote-masked text.
pub fn default_deny_regex() -> Vec<String> {
    vec![
        r"rm\s+(-[a-z]*\s+)*-[a-z]*f[a-z]*\s+(/|~|\$HOME)".into(),
        r"rm\s+-rf\s+/".into(),
        r":\(\)\s*\{".into(), // fork bomb
        r"\bmkfs\b".into(),
        r"\bdd\s+if=".into(),
        r">\s*/dev/sd[a-z]".into(),
        r"\bshutdown\b".into(),
        r"\breboot\b".into(),
        r"\bdiskpart\b".into(),
        r"format\s+[a-z]:".into(),
        r"del\s+/[sq]".into(),
        r"rd\s+/s".into(),
        r"Remove-Item\s+.*-Recurse".into(),
        r"chmod\s+-r\s+777\s+/".into(),
    ]
}

// ---------------------------------------------------------------------------
// result.ts — textReply / errReply / trimMiddle
// ---------------------------------------------------------------------------

/// `ToolReply<T>` shape from types.ts. `structured_content` always carries `ok`.
#[derive(Debug, Clone, Serialize)]
pub struct ToolReply {
    #[serde(rename = "structuredContent")]
    pub structured_content: Value,
    pub content: Vec<TextContent>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextContent {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
}

/// `textReply(ok, structured, text, meta?)`. `structured` is merged with `{ok}`.
pub fn text_reply(ok: bool, mut structured: Value, text: impl Into<String>, meta: Option<Value>) -> ToolReply {
    if !structured.is_object() {
        structured = json!({});
    }
    let obj = structured.as_object_mut().expect("object");
    // ok is inserted first in TS ({ ok, ...structured }); serde_json preserves
    // insertion order only with the preserve_order feature. Functionally the
    // field set is identical, which is what callers read.
    obj.insert("ok".to_string(), Value::Bool(ok));
    ToolReply {
        structured_content: structured,
        content: vec![TextContent {
            kind: "text",
            text: text.into(),
        }],
        meta,
    }
}

/// `errReply(message, extra?)` => textReply(false, {error, ...extra}, "Error: ...").
pub fn err_reply(message: impl Into<String>, extra: Value) -> ToolReply {
    let message = message.into();
    let mut structured = if extra.is_object() { extra } else { json!({}) };
    structured
        .as_object_mut()
        .unwrap()
        .insert("error".to_string(), Value::String(message.clone()));
    text_reply(false, structured, format!("Error: {message}"), None)
}

/// `trimMiddle(s, maxChars)`. Head ratio 0.58, tail ratio 0.32 — load-bearing.
/// Operates on chars (the TS uses String.length, i.e. UTF-16 code units; we use
/// chars, which matches for the BMP text these budgets target).
pub fn trim_middle(s: &str, max_chars: usize) -> (String, bool) {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return (s.to_string(), false);
    }
    let head = (max_chars as f64 * 0.58).floor() as usize;
    let tail = (max_chars as f64 * 0.32).floor() as usize;
    let omitted = chars.len() - head - tail;
    let head_str: String = chars[..head].iter().collect();
    let tail_str: String = chars[chars.len() - tail..].iter().collect();
    (
        format!(
            "{head_str}\n\n…[trimmed {} chars]…\n\n{tail_str}",
            group_thousands(omitted)
        ),
        true,
    )
}

/// `Number.prototype.toLocaleString()` for the default (en-US) locale: group
/// digits in threes with commas. Used inside the trimMiddle banner.
fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::new();
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// `brief(s, n=240)` — collapse whitespace, trim, then trimMiddle to n.
pub fn brief(s: &str, n: usize) -> String {
    let collapsed = collapse_ws(s);
    trim_middle(collapsed.trim(), n).0
}

fn collapse_ws(s: &str) -> String {
    let re = ws_re();
    re.replace_all(s, " ").into_owned()
}

fn ws_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+").unwrap())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// (The pre-chrono `now_iso`/`civil_from_days` helpers that used to live here
// were superseded during server wiring: timestamping now goes through the
// chrono-backed `now_iso` impls in `state.rs` / `register.rs`.)

// ===========================================================================
// command.ts — quote-aware lexer, parsedCommand classifier, deny floor
// ===========================================================================

const SEARCH_TOOLS: &[&str] = &["rg", "grep", "ag", "ack", "ripgrep", "git-grep"];
const LIST_TOOLS: &[&str] = &["ls", "dir", "tree", "find", "fd", "du", "stat"];
const READ_TOOLS: &[&str] = &[
    "cat", "type", "bat", "head", "tail", "sed", "awk", "less", "more", "wc", "nl", "od", "xxd",
];
const NETWORK_TOOLS: &[&str] = &[
    "curl", "wget", "nc", "netcat", "ncat", "ssh", "scp", "sftp", "rsync", "ftp", "telnet",
];
const PKG_TOOLS: &[&str] = &[
    "npm", "pnpm", "yarn", "bun", "pip", "pip3", "uv", "cargo", "gem", "go", "poetry",
];
const PKG_VERBS: &[&str] = &[
    "install", "add", "update", "upgrade", "publish", "login", "release", "remove", "uninstall",
];
const INTERPRETERS: &[&str] = &[
    "python", "python3", "node", "tsx", "ts-node", "bash", "sh", "zsh", "powershell", "pwsh",
    "ruby", "perl", "deno",
];
const FS_MUTATORS: &[&str] = &["touch", "mkdir", "mv", "cp", "ln", "rsync"];

#[derive(Debug, Clone)]
pub struct CommandReview {
    pub allowed: bool,
    pub needs_approval: bool,
    pub reason: Option<String>,
    pub parsed: Vec<ParsedCommandSummary>,
    pub dangerous: bool,
    pub prompt: bool,
}

/// tab/LF/CR are legitimate shell syntax; only truly stealthy controls blocked.
/// the bidi/zero-width formatting block.
pub fn has_stealth_control_chars(command: &str) -> bool {
    command.chars().any(|c| {
        let u = c as u32;
        let c0 = u <= 0x1f && c != '\t' && c != '\n' && c != '\r';
        let del = u == 0x7f;
        let bidi = (0x200b..=0x200f).contains(&u)
            || (0x202a..=0x202e).contains(&u)
            || (0x2066..=0x2069).contains(&u);
        c0 || del || bidi
    })
}

/// `commandStartsWith` — matching prefix if normalized command starts with any
/// normalized prefix (trim, collapse whitespace, lowercase on both sides).
pub fn command_starts_with(command: &str, prefixes: &[String]) -> Option<String> {
    let normalized = collapse_ws(command.trim()).to_lowercase();
    prefixes
        .iter()
        .find(|prefix| normalized.starts_with(&collapse_ws(prefix.trim()).to_lowercase()))
        .cloned()
}

struct ShellSegment {
    raw: String,
    masked: String,
    words: Vec<String>,
}

/// Quote-aware lexer.
fn lex_segments(command: &str) -> Vec<ShellSegment> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments: Vec<(String, String)> = Vec::new();
    let mut raw = String::new();
    let mut masked = String::new();
    #[derive(PartialEq)]
    enum Q {
        None,
        Single,
        Double,
    }
    let mut quote = Q::None;
    let mut subst_depth: i32 = 0;

    macro_rules! flush {
        () => {{
            if !raw.trim().is_empty() {
                segments.push((raw.trim().to_string(), masked.trim().to_string()));
            }
            raw.clear();
            masked.clear();
        }};
    }

    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();

        if quote == Q::Single {
            raw.push(ch);
            if ch == '\'' {
                masked.push(ch);
                quote = Q::None;
            } else {
                masked.push('x');
            }
            i += 1;
            continue;
        }

        if quote == Q::Double {
            raw.push(ch);
            if ch == '$' && next == Some('(') {
                subst_depth += 1;
                masked.push_str("$(");
                raw.push('(');
                i += 2;
                continue;
            }
            if subst_depth > 0 {
                if ch == ')' {
                    subst_depth = (subst_depth - 1).max(0);
                }
                masked.push(ch);
                i += 1;
                continue;
            }
            if ch == '`' {
                masked.push(ch);
                i += 1;
                continue;
            }
            if ch == '"' {
                masked.push(ch);
                quote = Q::None;
                i += 1;
                continue;
            }
            masked.push('x');
            i += 1;
            continue;
        }

        // unquoted
        if ch == '"' || ch == '\'' {
            quote = if ch == '"' { Q::Double } else { Q::Single };
            raw.push(ch);
            masked.push(ch);
            i += 1;
            continue;
        }
        if (ch == '&' && next == Some('&')) || (ch == '|' && next == Some('|')) {
            flush!();
            i += 2;
            continue;
        }
        if ch == ';' || ch == '|' || ch == '\n' {
            flush!();
            i += 1;
            continue;
        }
        raw.push(ch);
        masked.push(ch);
        i += 1;
    }
    flush!();

    segments
        .into_iter()
        .map(|(raw, masked)| {
            let words = split_words(&raw);
            ShellSegment { raw, masked, words }
        })
        .collect()
}

/// Quote-aware word split.
fn split_words(segment: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for ch in segment.chars() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                cur.push(ch);
            }
            has = true;
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            has = true;
            continue;
        }
        if ch.is_whitespace() {
            if has {
                words.push(std::mem::take(&mut cur));
                has = false;
            }
            continue;
        }
        cur.push(ch);
        has = true;
    }
    if has {
        words.push(cur);
    }
    words
}

/// Basename of the command head, lowercased, surrounding quotes/path stripped.
fn head_of(words: &[String]) -> String {
    let first = words.first().map(|s| s.as_str()).unwrap_or("");
    let first = first.trim_matches(|c| c == '\'' || c == '"');
    first
        .rsplit(|c| c == '\\' || c == '/')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// `maskQuotedLiterals` — join masked segments with " ; " for deny scanning.
pub fn mask_quoted_literals(command: &str) -> String {
    lex_segments(command)
        .iter()
        .map(|s| s.masked.clone())
        .collect::<Vec<_>>()
        .join(" ; ")
}

pub fn split_shell_sequence(command: &str) -> Vec<String> {
    lex_segments(command).into_iter().map(|s| s.raw).collect()
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static regex")
}

fn parse_one(seg: &ShellSegment) -> ParsedCommandSummary {
    let cmd = seg.raw.clone();
    let lower = collapse_ws(&seg.masked.to_lowercase());
    let head = head_of(&seg.words);
    let sub = seg.words.get(1).map(|s| s.to_lowercase()).unwrap_or_default();

    let mk = |kind: &str, safe: bool, reason: Option<&str>| ParsedCommandSummary {
        kind: kind.to_string(),
        cmd: cmd.clone(),
        safe,
        reason: reason.map(|s| s.to_string()),
        path: None,
        query: None,
    };

    if cmd.is_empty() {
        return mk("unknown", false, Some("empty command"));
    }

    // 1. Destructive / host-level.
    if re(r"(^|\s)(rm|del|erase|rmdir)\s").is_match(&lower)
        || re(r"(^|\s)(shutdown|reboot|mkfs|diskpart|format)\b").is_match(&lower)
        || re(r">\s*/(etc|bin|usr|var|system32)\b").is_match(&lower)
    {
        return mk("danger", false, Some("destructive/system command"));
    }

    // 2. git — identified by head, classified by subcommand.
    if head == "git" {
        if re(r"^(push|commit|reset|clean|checkout|switch|merge|rebase|cherry-pick|tag|stash|am|apply)$")
            .is_match(&sub)
            || (sub == "branch" && re(r"(?i)\s-d\b").is_match(&lower))
        {
            return mk(
                "write",
                false,
                Some("git command can mutate history or remote state"),
            );
        }
        if re(r"^(status|diff|log|show|branch|ls-files|grep|rev-parse|describe|remote|blame|cat-file|shortlog)$")
            .is_match(&sub)
            || sub.is_empty()
        {
            return mk("git", true, Some("read-only git inspection"));
        }
        return mk("git", true, Some("git inspection"));
    }

    // 3. Read / list / search — identified by HEAD.
    if SEARCH_TOOLS.contains(&head.as_str()) {
        let mut p = mk("search", true, None);
        p.query = seg
            .words
            .iter()
            .skip(1)
            .find(|w| !w.starts_with('-'))
            .cloned();
        return p;
    }
    if LIST_TOOLS.contains(&head.as_str()) {
        return mk("list", true, None);
    }
    if READ_TOOLS.contains(&head.as_str()) {
        return mk("read", true, None);
    }

    // 4. Routine validation (test/build).
    let pkg_run = PKG_TOOLS.contains(&head.as_str())
        && re(r"^(run|test|lint|typecheck|check|build)$").is_match(&sub);
    if pkg_run
        || re(r"^(pytest|jest|vitest|mocha)$").is_match(&head)
        || (head == "cargo" && re(r"^(test|check|build|clippy)$").is_match(&sub))
        || (head == "go" && sub == "test")
        || (head == "dotnet" && sub == "test")
        || (head == "mvn" && sub == "test")
        || (head == "gradle" && sub == "test")
    {
        let is_build = re(r"^(build|compile|check|typecheck|clippy)$").is_match(&sub)
            || (head == "cargo" && sub == "check");
        return mk(
            if is_build { "build" } else { "test" },
            true,
            Some("routine project validation"),
        );
    }

    // 5. Network.
    if NETWORK_TOOLS.contains(&head.as_str()) {
        return mk("network", false, Some("network or remote host access"));
    }

    // 6. Package manager mutation.
    if PKG_TOOLS.contains(&head.as_str()) && PKG_VERBS.contains(&sub.as_str()) {
        return mk(
            "package",
            false,
            Some("package manager/network or publish action"),
        );
    }

    // 7. Formatters.
    if re(r"^(prettier|eslint|ruff|black|gofmt|biome|standardrb)$").is_match(&head)
        || (head == "cargo" && sub == "fmt")
        || (head == "dotnet" && sub == "format")
        || (head == "go" && sub == "fmt")
    {
        return mk("format", false, Some("formatter can rewrite files"));
    }

    // 8. Writes / arbitrary execution: an UNQUOTED redirect, fs mutator, or interpreter.
    //    TS regex: /(^|[^0-9>])>{1,2}(?![=>])\s*\S/ — a `>`/`>>` not part of `>=`
    //    or `>>>` style, followed by optional ws and a non-space target.
    if redirect_to_target(&lower)
        || FS_MUTATORS.contains(&head.as_str())
        || INTERPRETERS.contains(&head.as_str())
    {
        return mk(
            "write",
            false,
            Some("may create, edit, or execute arbitrary code"),
        );
    }

    mk("unknown", false, Some("not recognized as safe"))
}

/// Port of `/(^|[^0-9>])>{1,2}(?![=>])\s*\S/` (the negative lookahead `(?!...)`
/// is unavailable in the `regex` crate, so it is expressed procedurally).
fn redirect_to_target(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    while i < n {
        if chars[i] == '>' {
            // prev must be start-of-string or not a digit / '>'
            let prev_ok = if i == 0 {
                true
            } else {
                let p = chars[i - 1];
                !(p.is_ascii_digit() || p == '>')
            };
            if prev_ok {
                // consume 1 or 2 '>'
                let mut j = i + 1;
                if j < n && chars[j] == '>' {
                    j += 1;
                }
                // lookahead: next char must NOT be '=' or '>'
                let la = chars.get(j).copied();
                if la != Some('=') && la != Some('>') {
                    // skip whitespace, require a non-space char
                    let mut k = j;
                    while k < n && chars[k].is_whitespace() {
                        k += 1;
                    }
                    if k < n {
                        return true;
                    }
                }
            }
        }
        i += 1;
    }
    false
}

pub fn parse_command(command: &str) -> Vec<ParsedCommandSummary> {
    let segments = lex_segments(command);
    let parsed: Vec<ParsedCommandSummary> = segments.iter().map(parse_one).collect();
    let mut deduped: Vec<ParsedCommandSummary> = Vec::new();
    for item in parsed {
        if let Some(prev) = deduped.last() {
            if prev.kind == item.kind && prev.cmd == item.cmd {
                continue;
            }
        }
        deduped.push(item);
    }
    if deduped.is_empty() {
        deduped.push(ParsedCommandSummary {
            kind: "unknown".into(),
            cmd: command.to_string(),
            safe: false,
            reason: Some("could not parse command".into()),
            path: None,
            query: None,
        });
    }
    deduped
}

#[derive(Default)]
pub struct ReviewOpts {
    pub approved: bool,
    pub task_profile: bool,
}

pub fn review_command(config: &Config, command: &str, opts: ReviewOpts) -> CommandReview {
    let parsed = parse_command(command);
    let dangerous = parsed.iter().any(|p| p.kind == "danger");
    let prompt = command_starts_with(command, &config.command_prompt_prefixes).is_some()
        || parsed.iter().any(|p| {
            !p.safe
                && ["network", "package", "write", "danger", "unknown", "format"]
                    .contains(&p.kind.as_str())
        });
    let explicitly_allowed = command_starts_with(command, &config.command_allow_prefixes).is_some();
    let _ = opts.task_profile; // parity with TS signature

    let review = |allowed, needs_approval, reason: Option<String>, prompt| CommandReview {
        allowed,
        needs_approval,
        reason,
        parsed: parsed.clone(),
        dangerous,
        prompt,
    };
    let danger_reason = || {
        parsed
            .iter()
            .find(|p| p.kind == "danger")
            .and_then(|p| p.reason.clone())
            .unwrap_or_else(|| "dangerous command".into())
    };

    if has_stealth_control_chars(command) {
        return CommandReview {
            allowed: false,
            needs_approval: false,
            reason: Some("Command contains hidden/control Unicode characters.".into()),
            parsed,
            dangerous: true,
            prompt: false,
        };
    }

    let deny_scan = mask_quoted_literals(command);
    for pattern in &config.command_deny_regex {
        if let Ok(rx) = Regex::new(&format!("(?i){pattern}")) {
            if rx.is_match(&deny_scan) {
                return CommandReview {
                    allowed: false,
                    needs_approval: !opts.approved,
                    reason: Some(format!("Command denied by regex {pattern}")),
                    parsed,
                    dangerous: true,
                    prompt,
                };
            }
        }
    }

    if opts.approved {
        return review(true, false, None, prompt);
    }
    if config.sandbox_mode == SandboxMode::DangerFullAccess {
        return review(true, false, None, prompt);
    }
    if config.approval_policy == ApprovalPolicy::Never {
        if dangerous {
            return review(false, false, Some(danger_reason()), prompt);
        }
        return review(true, false, None, prompt);
    }
    if config.sandbox_mode == SandboxMode::ReadOnly {
        let all_read_only = parsed
            .iter()
            .all(|p| p.safe && ["read", "list", "search", "git"].contains(&p.kind.as_str()));
        if all_read_only || explicitly_allowed {
            return review(true, false, None, prompt);
        }
        return review(
            false,
            true,
            Some("read-only sandbox blocks edits and arbitrary command execution".into()),
            true,
        );
    }
    if config.sandbox_mode == SandboxMode::WorkspaceWrite {
        if dangerous {
            return review(false, true, Some(danger_reason()), true);
        }
        if explicitly_allowed {
            return review(true, false, None, prompt);
        }
        if prompt {
            let reason = parsed
                .iter()
                .find(|p| !p.safe)
                .and_then(|p| p.reason.clone())
                .unwrap_or_else(|| "command is outside routine workspace-write actions".into());
            return review(false, true, Some(reason), prompt);
        }
        return review(true, false, None, prompt);
    }

    review(false, true, Some("unknown sandbox policy".into()), true)
}

// ===========================================================================
// path.ts — containment, glob->regex, hidden-dir detection, binary sniff
// ===========================================================================

#[derive(Debug)]
pub struct GuardError(pub String);
impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for GuardError {}

pub fn to_posix(path: &str) -> String {
    path.replace('\\', "/")
}

/// True when the path lives inside (or is) a hidden directory. Top-level
/// dotfiles (e.g. .gitignore) are kept. Port of path.ts `inHiddenDir`.
pub fn in_hidden_dir(rel: &str) -> bool {
    let stripped = rel.trim_end_matches('/');
    let parts: Vec<&str> = stripped.split('/').collect();
    let dir_count = if rel.ends_with('/') {
        parts.len()
    } else {
        parts.len().saturating_sub(1)
    };
    for p in parts.iter().take(dir_count) {
        if p.starts_with('.') && *p != "." && *p != ".." {
            return true;
        }
    }
    false
}

fn escape_regex(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if "|\\{}()[]^$+?.".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// `globToRegExp`. `**` -> `(?:.*)`, `*` -> `[^/]*`, `?` -> `[^/]`.
/// The post-join replace lets a `**` segment optionally span `/` boundaries.
///
/// Process-wide memoized: `Regex::new` is ~tens of microseconds, and the hot
/// callers (filter_files, the walk's blocked-glob check, the repo_grep fallback)
/// hit the SAME handful of globs once per file across thousands of files. Without
/// the cache a single repo_glob over ~4869 files recompiled ~90k regexes and
/// spun the worker for ~3 minutes (the live hang). The cache key is the raw glob
/// string; compiled `Regex` is cheap to clone (it's `Arc`-backed internally).
pub fn glob_to_regex(glob: &str) -> Regex {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, Regex>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some(re) = map.get(glob) {
            return re.clone();
        }
    }
    let re = compile_glob_to_regex(glob);
    if let Ok(mut map) = cache.lock() {
        map.entry(glob.to_string()).or_insert_with(|| re.clone());
    }
    re
}

/// Uncached compile (the body of the old `glob_to_regex`). Used by the cache and
/// by the precompiled matcher set in [`filter_files`].
fn compile_glob_to_regex(glob: &str) -> Regex {
    let posix = to_posix(glob);
    let parts: Vec<String> = posix
        .split('/')
        .map(|part| {
            if part == "**" {
                return "(?:.*)".to_string();
            }
            let mut piece = String::new();
            for ch in part.chars() {
                match ch {
                    '*' => piece.push_str("[^/]*"),
                    '?' => piece.push_str("[^/]"),
                    _ => piece.push_str(&escape_regex(&ch.to_string())),
                }
            }
            piece
        })
        .collect();
    let joined = parts.join("/");
    let joined = joined.replace("/(?:.*)/", "(?:/.*?/|/)");
    Regex::new(&format!("^{joined}$")).unwrap_or_else(|_| Regex::new("^$").unwrap())
}

/// One glob precompiled into the (at most two) regexes `matches_any_glob` tests:
/// the glob itself plus, when it begins with `**/`, the head-stripped fallback.
/// Build these ONCE and reuse across every file instead of recompiling per file.
struct GlobMatcher {
    glob: String,
    primary: Regex,
    /// The `**/`-stripped fallback form, if the glob had that head.
    fallback: Option<Regex>,
}

impl GlobMatcher {
    fn new(glob: &str) -> Self {
        GlobMatcher {
            glob: glob.to_string(),
            primary: glob_to_regex(glob),
            fallback: glob
                .strip_prefix("**/")
                .map(glob_to_regex),
        }
    }

    /// Does `posix` (already `./`-stripped, posix-separated) match this glob,
    /// honouring the `**/` head fallback? Same semantics as one iteration of the
    /// old `matches_any_glob` loop.
    fn matches(&self, posix: &str) -> bool {
        if self.primary.is_match(posix) {
            return true;
        }
        if let Some(fb) = &self.fallback {
            if fb.is_match(posix) {
                return true;
            }
        }
        false
    }
}

/// Normalize a path for glob matching exactly as `matches_any_glob` does.
fn normalize_for_glob(path: &str) -> String {
    let posix = to_posix(path);
    posix.strip_prefix("./").unwrap_or(&posix).to_string()
}

/// Precompile a glob list into reusable matchers (build once, match many files).
fn compile_glob_matchers(globs: &[String]) -> Vec<GlobMatcher> {
    globs.iter().map(|g| GlobMatcher::new(g)).collect()
}

/// First matcher in `matchers` that matches `posix`, returning its glob string —
/// the precompiled equivalent of `matches_any_glob`.
fn matches_any_compiled<'a>(posix: &str, matchers: &'a [GlobMatcher]) -> Option<&'a str> {
    matchers
        .iter()
        .find(|m| m.matches(posix))
        .map(|m| m.glob.as_str())
}

/// `matchesAnyGlob` — returns the matching glob (with the `**/` head fallback).
/// Now backed by the memoized [`glob_to_regex`], so repeated calls with the same
/// globs no longer recompile. Hot per-file loops should still prefer
/// [`compile_glob_matchers`] + [`matches_any_compiled`] to avoid the cache's
/// per-call lock entirely.
pub fn matches_any_glob(path: &str, globs: &[String]) -> Option<String> {
    let posix = normalize_for_glob(path);
    for glob in globs {
        if glob_to_regex(glob).is_match(&posix) {
            return Some(glob.clone());
        }
        if let Some(rest) = glob.strip_prefix("**/") {
            if glob_to_regex(rest).is_match(&posix) {
                return Some(glob.clone());
            }
        }
    }
    None
}

/// Lexical resolve of `base`+`path` then normalize `.`/`..` without touching the
/// filesystem (mirrors node's path.resolve for the cases the guards rely on).
fn lexical_resolve(base: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    };
    normalize_path(&joined)
}

fn normalize_path(p: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    let mut pb = PathBuf::new();
    for c in out {
        pb.push(c.as_os_str());
    }
    pb
}

/// Relative path from `from` to `to`, posix-joined. Empty if equal.
fn relative_path(from: &Path, to: &Path) -> String {
    let from = normalize_path(from);
    let to = normalize_path(to);
    let fc: Vec<_> = from.components().collect();
    let tc: Vec<_> = to.components().collect();
    let mut i = 0;
    while i < fc.len() && i < tc.len() && fc[i] == tc[i] {
        i += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in i..fc.len() {
        parts.push("..".to_string());
    }
    for c in &tc[i..] {
        parts.push(c.as_os_str().to_string_lossy().to_string());
    }
    parts.join("/")
}

/// `resolveInsideRoot` — containment + hidden + blocked-glob +
/// secret-ish guards. Returns the absolute path or a GuardError.
pub struct ResolveOpts {
    pub for_write: bool,
    pub allow_blocked: bool,
}
impl Default for ResolveOpts {
    fn default() -> Self {
        ResolveOpts {
            for_write: false,
            allow_blocked: false,
        }
    }
}

pub fn resolve_inside_root(
    config: &Config,
    path: &str,
    opts: ResolveOpts,
) -> Result<PathBuf, GuardError> {
    if path.is_empty() || path.contains('\0') {
        return Err(GuardError("Invalid path.".into()));
    }
    let absolute = lexical_resolve(&config.root, path);
    let rel = relative_path(&config.root, &absolute);
    if rel.starts_with("..") || rel == ".." || (absolute != config.root && rel.is_empty()) {
        return Err(GuardError(format!("Path escapes repo root: {path}")));
    }
    let rel_posix = if rel.is_empty() { ".".to_string() } else { rel.clone() };
    if !config.allow_hidden_files {
        let has_hidden = rel_posix
            .split('/')
            .any(|p| p.starts_with('.') && p != ".");
        if has_hidden {
            return Err(GuardError(format!(
                "Hidden path blocked by config: {rel_posix}"
            )));
        }
    }
    if !opts.allow_blocked {
        if let Some(matched) = matches_any_glob(&rel_posix, &config.blocked_path_globs) {
            return Err(GuardError(format!(
                "Path blocked by rule {matched}: {rel_posix}"
            )));
        }
    }
    if !config.allow_secrets_read && !opts.for_write {
        let secretish = Regex::new(
            r"(?i)(^|/)(\.env|\.npmrc|\.pypirc|id_rsa|id_ed25519|credentials|secrets?)(\.|/|$)",
        )
        .unwrap();
        if secretish.is_match(&rel_posix) {
            return Err(GuardError(format!("Secret-ish file blocked: {rel_posix}")));
        }
    }
    Ok(absolute)
}

fn is_probably_binary(buf: &[u8]) -> bool {
    let sample = &buf[..buf.len().min(8192)];
    sample.iter().any(|&b| b == 0)
}

#[derive(PartialEq)]
enum StatKind {
    File,
    Dir,
    Other,
    Missing,
}

fn stat_kind(path: &Path) -> StatKind {
    match std::fs::symlink_metadata(path) {
        Ok(md) => {
            let ft = md.file_type();
            if ft.is_file() {
                StatKind::File
            } else if ft.is_dir() {
                StatKind::Dir
            } else {
                StatKind::Other
            }
        }
        Err(_) => StatKind::Missing,
    }
}

// ===========================================================================
// permissions.ts — isInside, canWritePath, set_permissions, effective view
// ===========================================================================

/// `isInside(parent, candidate)` — true if candidate is inside or equal to
/// parent.
pub fn is_inside(parent: &Path, candidate: &Path) -> bool {
    let rel = relative_path(parent, candidate);
    rel.is_empty() || (!rel.starts_with("..") && rel != "..")
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectivePermissions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(rename = "sandboxMode")]
    pub sandbox_mode: String,
    #[serde(rename = "approvalPolicy")]
    pub approval_policy: String,
    pub reviewer: String,
    pub root: String,
    #[serde(rename = "writableRoots")]
    pub writable_roots: Vec<String>,
    #[serde(rename = "commandAllowPrefixes")]
    pub command_allow_prefixes: Vec<String>,
    #[serde(rename = "commandPromptPrefixes")]
    pub command_prompt_prefixes: Vec<String>,
    #[serde(rename = "hardDenyRules")]
    pub hard_deny_rules: Vec<String>,
    pub note: String,
}

fn reviewer_str(r: ApprovalReviewer) -> &'static str {
    match r {
        ApprovalReviewer::User => "user",
        ApprovalReviewer::AutoReview => "auto_review",
    }
}

pub fn sandbox_note(mode: SandboxMode, approval: ApprovalPolicy) -> String {
    match (mode, approval) {
        (SandboxMode::ReadOnly, _) => {
            "Read-only: file writes and non-inspection commands require approval.".into()
        }
        (SandboxMode::WorkspaceWrite, ApprovalPolicy::OnRequest) => {
            "Workspace-write: repo-local edits and routine checks are allowed; risky, network, or out-of-scope actions ask first.".into()
        }
        (SandboxMode::DangerFullAccess, _) => {
            "Danger full access: policy removes most repo-agent boundaries; hard-deny regexes still protect obvious host destruction.".into()
        }
        (m, a) => format!("{} / {}", m.as_str(), a.as_str()),
    }
}

pub fn effective_permissions(config: &Config) -> EffectivePermissions {
    let profile = config
        .permission_profiles
        .iter()
        .find(|(_, p)| {
            p.sandbox_mode == Some(config.sandbox_mode)
                && p.approval_policy == Some(config.approval_policy)
        })
        .map(|(k, _)| k.clone());
    let writable_roots = if config.sandbox_mode == SandboxMode::ReadOnly {
        Vec::new()
    } else {
        let mut v = vec![config.root.to_string_lossy().to_string()];
        v.extend(config.writable_roots.iter().map(|p| p.to_string_lossy().to_string()));
        v
    };
    EffectivePermissions {
        profile,
        sandbox_mode: config.sandbox_mode.as_str().to_string(),
        approval_policy: config.approval_policy.as_str().to_string(),
        reviewer: reviewer_str(config.approvals_reviewer).to_string(),
        root: config.root.to_string_lossy().to_string(),
        writable_roots,
        command_allow_prefixes: config.command_allow_prefixes.clone(),
        command_prompt_prefixes: config.command_prompt_prefixes.clone(),
        hard_deny_rules: config.command_deny_regex.clone(),
        note: sandbox_note(config.sandbox_mode, config.approval_policy),
    }
}

pub struct SetPermsOpts {
    pub profile: Option<String>,
    pub sandbox_mode: Option<SandboxMode>,
    pub approval_policy: Option<ApprovalPolicy>,
    pub reviewer: Option<ApprovalReviewer>,
}

pub fn set_permissions(
    config: &mut Config,
    opts: SetPermsOpts,
) -> Result<EffectivePermissions, GuardError> {
    let profile = match &opts.profile {
        Some(name) => match config.permission_profiles.get(name).cloned() {
            Some(p) => Some(p),
            None => return Err(GuardError(format!("Unknown permission profile: {name}"))),
        },
        None => None,
    };
    config.sandbox_mode = opts
        .sandbox_mode
        .or_else(|| profile.as_ref().and_then(|p| p.sandbox_mode))
        .unwrap_or(config.sandbox_mode);
    config.approval_policy = opts
        .approval_policy
        .or_else(|| profile.as_ref().and_then(|p| p.approval_policy))
        .unwrap_or(config.approval_policy);
    config.approvals_reviewer = opts
        .reviewer
        .or_else(|| profile.as_ref().and_then(|p| p.reviewer))
        .unwrap_or(config.approvals_reviewer);
    if let Some(p) = &profile {
        if let Some(wr) = &p.writable_roots {
            config.writable_roots = wr
                .iter()
                .map(|r| normalize_path(&config.root.join(r)))
                .collect();
        }
        if let Some(a) = &p.command_allow_prefixes {
            config.command_allow_prefixes = a.clone();
        }
        if let Some(pp) = &p.command_prompt_prefixes {
            config.command_prompt_prefixes = pp.clone();
        }
    }
    config.allow_model_write_file = config.sandbox_mode != SandboxMode::ReadOnly;
    config.allow_model_dev_shell =
        config.approval_policy != ApprovalPolicy::Untrusted || config.sandbox_mode != SandboxMode::ReadOnly;
    Ok(effective_permissions(config))
}

pub fn can_write_path(config: &Config, abs_path: &Path) -> Result<(), String> {
    match config.sandbox_mode {
        SandboxMode::ReadOnly => Err("read-only sandbox".into()),
        SandboxMode::DangerFullAccess => Ok(()),
        SandboxMode::WorkspaceWrite => {
            let mut roots = vec![config.root.clone()];
            roots.extend(config.writable_roots.iter().cloned());
            if roots.iter().any(|root| is_inside(root, abs_path)) {
                Ok(())
            } else {
                let joined = roots
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(format!("outside writable roots ({joined})"))
            }
        }
    }
}

// ===========================================================================
// shell.ts — spawn (real, async), env filter, cwd validation, truncation
// ===========================================================================

#[derive(Debug, Clone, Serialize)]
pub struct ShellResult {
    pub command: String,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
    #[serde(skip)]
    pub signal: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub combined: String,
    #[serde(rename = "durationMs")]
    pub duration_ms: u128,
    #[serde(rename = "timedOut")]
    pub timed_out: bool,
    pub truncated: bool,
}

/// `commandDenied` — stealth controls or a deny-regex hit (scanned on
/// masked text). Returns the offending pattern/reason.
pub fn command_denied(config: &Config, command: &str) -> Option<String> {
    if has_stealth_control_chars(command) {
        return Some("hidden/control Unicode characters".into());
    }
    let scan = mask_quoted_literals(command);
    for pattern in &config.command_deny_regex {
        if let Ok(rx) = Regex::new(&format!("(?i){pattern}")) {
            if rx.is_match(&scan) {
                return Some(pattern.clone());
            }
        }
    }
    None
}

/// `filteredEnv` — only envPassthrough keys plus the REPO_AGENT_* set.
pub fn filtered_env(config: &Config) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    for key in &config.env_passthrough {
        if let Ok(v) = std::env::var(key) {
            env.push((key.clone(), v));
        }
    }
    env.push(("REPO_AGENT".into(), "1".into()));
    env.push(("REPO_AGENT_ROOT".into(), config.root.to_string_lossy().to_string()));
    env.push(("REPO_AGENT_SANDBOX".into(), config.sandbox_mode.as_str().into()));
    env.push((
        "REPO_AGENT_APPROVAL_POLICY".into(),
        config.approval_policy.as_str().into(),
    ));
    env
}

/// `validateCwd` — abs cwd must be inside an allowed root unless
/// danger-full-access / bypass. Returns the resolved abs path or an error.
pub fn validate_cwd(config: &Config, cwd: &str, bypass_policy: bool) -> Result<PathBuf, String> {
    let abs = if Path::new(cwd).exists() {
        std::fs::canonicalize(cwd).unwrap_or_else(|_| lexical_resolve(&config.root, cwd))
    } else {
        lexical_resolve(&config.root, cwd)
    };
    if bypass_policy || config.sandbox_mode == SandboxMode::DangerFullAccess {
        return Ok(abs);
    }
    let mut roots = vec![config.root.clone()];
    roots.extend(config.project_search_roots.iter().cloned());
    roots.extend(config.writable_roots.iter().cloned());
    if roots.iter().any(|root| is_inside(root, &abs)) {
        Ok(abs)
    } else {
        Err(format!("Command cwd outside allowed roots: {}", abs.display()))
    }
}

pub struct RunShellOpts {
    pub timeout_ms: Option<u64>,
    pub cwd: Option<String>,
    pub max_output_bytes: Option<usize>,
    pub bypass_policy: bool,
}
impl Default for RunShellOpts {
    fn default() -> Self {
        RunShellOpts {
            timeout_ms: None,
            cwd: None,
            max_output_bytes: None,
            bypass_policy: false,
        }
    }
}

/// `runShell` — deny-check, cwd-validate, spawn through the platform
/// shell (PowerShell on Windows / `sh -c` elsewhere), collect+truncate output.
pub async fn run_shell(
    config: &Config,
    command: &str,
    opts: RunShellOpts,
) -> Result<ShellResult, String> {
    if let Some(denied) = command_denied(config, command) {
        return Err(format!("Command denied by regex/policy {denied}"));
    }
    let cwd = validate_cwd(
        config,
        opts.cwd.as_deref().unwrap_or_else(|| config.root.to_str().unwrap_or(".")),
        opts.bypass_policy,
    )?;
    let timeout_ms = opts.timeout_ms.unwrap_or(config.default_command_timeout_ms);
    let max_output_bytes = opts.max_output_bytes.unwrap_or(config.max_command_output_bytes);
    let started = Instant::now();

    let mut cmd = platform_shell(command);
    cmd.current_dir(&cwd);
    cmd.env_clear();
    for (k, v) in filtered_env(config) {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // Timed-out children must die, not detach (see wait_child_with_timeout).
    cmd.kill_on_drop(true);

    let child = cmd.spawn().map_err(|e| e.to_string())?;
    let (output, timed_out) = wait_child_with_timeout(child, timeout_ms).await?;

    Ok(shape_shell_result(command.to_string(), output, timed_out, started, max_output_bytes))
}

/// Wait for `child` under `timeout_ms`. On timeout: the dropped wait future kills
/// the direct child (callers MUST spawn with `.kill_on_drop(true)` — a dropped
/// tokio Child otherwise detaches and the process lives on), and on Windows the
/// whole tree is additionally `taskkill /T /F`ed so e.g. powershell's own child
/// (the actual command) doesn't survive its dead shell. Returns the synthetic
/// "[timed out]" output the old inline arms produced.
async fn wait_child_with_timeout(
    child: tokio::process::Child,
    timeout_ms: u64,
) -> Result<(std::process::Output, bool), String> {
    let pid = child.id();
    let fut = child.wait_with_output();
    match tokio::time::timeout(Duration::from_millis(timeout_ms), fut).await {
        Ok(Ok(out)) => Ok((out, false)),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => {
            // The wait future (owning the child) is dropped by the timeout —
            // kill_on_drop terminates the root process. Tree-kill is best-effort.
            if let Some(pid) = pid {
                kill_pid_tree(pid);
            }
            Ok((
                std::process::Output {
                    status: Default::default(),
                    stdout: Vec::new(),
                    stderr: b"[timed out]".to_vec(),
                },
                true,
            ))
        }
    }
}

/// SIGKILL the whole process group led by `pid`. Children are spawned as their
/// own group leader (`process_group(0)`, see [`platform_shell`]), so `pgid == pid`
/// and a negative pid signals the entire subtree — the `sh`, the `cargo` it ran,
/// and every `rustc` that forked from it. Without this, a timed-out build leaves
/// orphaned compilers running on macOS/Linux.
#[cfg(unix)]
fn unix_kill_group(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &format!("-{pid}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Best-effort process-tree kill by pid. Windows: `taskkill /T /F`. Unix: SIGKILL
/// the child's process group (see [`unix_kill_group`]) — `kill_on_drop` alone only
/// reaps the direct child and would orphan its descendants.
fn kill_pid_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(unix)]
    unix_kill_group(pid);
}

/// Shape a collected `Output` into the [`ShellResult`] contract (combined text,
/// 0.65/0.35 stdout/stderr caps, middle-trim). Shared by [`run_shell`] and
/// [`run_git`].
fn shape_shell_result(
    command: String,
    output: std::process::Output,
    timed_out: bool,
    started: Instant,
    max_output_bytes: usize,
) -> ShellResult {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code();
    let combined_raw = if stderr.is_empty() {
        stdout.clone()
    } else {
        format!("{stdout}\n[stderr]\n{stderr}")
    };
    let (combined, truncated) = trim_middle(&combined_raw, max_output_bytes);
    let out_cap = (max_output_bytes as f64 * 0.65).floor() as usize;
    let err_cap = (max_output_bytes as f64 * 0.35).floor() as usize;
    ShellResult {
        command,
        exit_code,
        signal: None,
        stdout: trim_middle(&stdout, out_cap).0,
        stderr: trim_middle(&stderr, err_cap).0,
        combined,
        duration_ms: started.elapsed().as_millis(),
        timed_out,
        truncated,
    }
}

/// Wall-clock budget for one direct `git` invocation (mirrors the 20s the old
/// PowerShell-mediated git calls used).
const GIT_TIMEOUT_MS: u64 = 20_000;

/// Run `git` directly — no platform shell in between. On this class of Windows
/// box PowerShell's managed host intermittently fails to load and returns
/// .NET "managed-loading" garbage, which the shell path would dutifully hand
/// back as tool output; pure-git call sites (repo_status / repo_diff / the
/// ls-files probe) must therefore spawn `git` itself. Result shape mirrors
/// [`run_shell`]'s [`ShellResult`].
pub async fn run_git(
    config: &Config,
    args: &[&str],
    max_output_bytes: usize,
) -> Result<ShellResult, String> {
    let command = format!("git {}", args.join(" "));
    let started = Instant::now();

    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args);
    cmd.current_dir(&config.root);
    cmd.env_clear();
    for (k, v) in filtered_env(config) {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // Timed-out children must die, not detach (see wait_child_with_timeout).
    cmd.kill_on_drop(true);

    let child = cmd.spawn().map_err(|e| format!("git failed to start: {e}"))?;
    let (output, timed_out) = wait_child_with_timeout(child, GIT_TIMEOUT_MS).await?;

    Ok(shape_shell_result(command, output, timed_out, started, max_output_bytes))
}

/// Build the platform shell invocation. Windows -> PowerShell;
/// elsewhere -> `sh -c`. `shell: true` in node maps to this.
fn platform_shell(command: &str) -> tokio::process::Command {
    if cfg!(windows) {
        let mut c = tokio::process::Command::new("powershell.exe");
        c.arg("-NoProfile").arg("-NonInteractive").arg("-Command").arg(command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        // Lead a fresh process group so a timeout can SIGKILL the whole subtree
        // (sh -> cargo -> rustc ...), not just the direct child. See kill_pid_tree.
        #[cfg(unix)]
        c.process_group(0);
        c
    }
}

/// Like [`platform_shell`] but on `std::process::Command` (used by the background
/// registry, which spawns long-lived children outside the tokio reactor).
fn platform_shell_std(command: &str) -> std::process::Command {
    if cfg!(windows) {
        let mut c = std::process::Command::new("powershell.exe");
        c.arg("-NoProfile").arg("-NonInteractive").arg("-Command").arg(command);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(command);
        // Own process group so repo_bg_stop can kill the whole subtree.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            c.process_group(0);
        }
        c
    }
}

// ===========================================================================
// bg.ts — long-running background process registry (start/read/stop/list/killAll)
// ===========================================================================
//
// A process-global registry of detached children the model launches via
// `repo_shell({ background: true })` and polls with repo_bg_output / repo_bg_stop
// / repo_bg_list. Ported from core/bg.ts: a front-trimmed tail buffer per process,
// an absolute read cursor so each repo_bg_output returns only newer output, and a
// tree-kill on stop (taskkill /T on Windows, process-group SIGTERM elsewhere).

const BG_MAX_BUFFER_BYTES: usize = 512_000;

#[derive(Debug, Clone, Serialize)]
pub struct BgStartResult {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BgOutputResult {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
    pub running: bool,
    pub output: String,
    pub bytes: u64,
    #[serde(rename = "newBytes")]
    pub new_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BgSummary {
    pub id: String,
    pub name: String,
    pub command: String,
    pub status: String,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(rename = "ageMs")]
    pub age_ms: u128,
    #[serde(rename = "bufferedBytes")]
    pub buffered_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BgStopResult {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
}

struct BgProc {
    id: String,
    name: String,
    command: String,
    pid: Option<u32>,
    started_at: Instant,
    ended_at: Option<Instant>,
    exit_code: Option<i32>,
    status: String, // "running" | "exited" | "killed"
    /// Shared with the reader thread: front-trimmed tail buffer + dropped count.
    buffer: Arc<Mutex<BgBuffer>>,
    /// Absolute stream position already returned by repo_bg_output.
    read_cursor: usize,
    child: Arc<Mutex<std::process::Child>>,
}

#[derive(Default)]
struct BgBuffer {
    text: String,
    dropped: usize,
}

impl BgBuffer {
    fn append(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        if self.text.len() > BG_MAX_BUFFER_BYTES {
            let drop = self.text.len() - BG_MAX_BUFFER_BYTES;
            // Drop on a char boundary at or past `drop` so the buffer stays valid.
            let mut cut = drop;
            while cut < self.text.len() && !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.replace_range(..cut, "");
            self.dropped += cut;
        }
    }
}

fn bg_registry() -> &'static Mutex<(u64, Vec<BgProc>)> {
    static REG: std::sync::OnceLock<Mutex<(u64, Vec<BgProc>)>> = std::sync::OnceLock::new();
    REG.get_or_init(|| Mutex::new((0, Vec::new())))
}

pub struct BgStartOpts {
    pub name: Option<String>,
    pub cwd: Option<String>,
}

/// `startBackground(config, command, opts)`.
pub fn start_background(config: &Config, command: &str, opts: BgStartOpts) -> Result<BgStartResult, String> {
    if let Some(denied) = command_denied(config, command) {
        return Err(format!("Command denied by regex/policy {denied}"));
    }
    let cwd = validate_cwd(
        config,
        opts.cwd.as_deref().unwrap_or_else(|| config.root.to_str().unwrap_or(".")),
        false,
    )?;

    let id = {
        let mut reg = bg_registry().lock().unwrap();
        reg.0 += 1;
        format!("bg{}", reg.0)
    };
    let name = opts
        .name
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| id.clone());

    let mut cmd = platform_shell_std(command);
    cmd.current_dir(&cwd);
    cmd.env_clear();
    for (k, v) in filtered_env(config) {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let pid = Some(child.id());
    let buffer = Arc::new(Mutex::new(BgBuffer::default()));

    // Stream stdout + stderr into the shared tail buffer on reader threads.
    if let Some(out) = child.stdout.take() {
        spawn_bg_reader(out, buffer.clone());
    }
    if let Some(err) = child.stderr.take() {
        spawn_bg_reader(err, buffer.clone());
    }

    let proc = BgProc {
        id: id.clone(),
        name: name.clone(),
        command: command.to_string(),
        pid,
        started_at: Instant::now(),
        ended_at: None,
        exit_code: None,
        status: "running".to_string(),
        buffer,
        read_cursor: 0,
        child: Arc::new(Mutex::new(child)),
    };
    bg_registry().lock().unwrap().1.push(proc);
    Ok(BgStartResult {
        id,
        name,
        pid,
        command: command.to_string(),
    })
}

fn spawn_bg_reader<R: std::io::Read + Send + 'static>(mut reader: R, buffer: Arc<Mutex<BgBuffer>>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&chunk[..n]);
                    buffer.lock().unwrap().append(&text);
                }
            }
        }
    });
}

/// Refresh a process's terminal status by polling its child (non-blocking).
fn bg_poll(proc: &mut BgProc) {
    if proc.status != "running" {
        return;
    }
    let mut child = proc.child.lock().unwrap();
    match child.try_wait() {
        Ok(Some(status)) => {
            proc.exit_code = status.code();
            proc.ended_at = Some(Instant::now());
            proc.status = "exited".to_string();
        }
        _ => {}
    }
}

pub struct BgReadOpts {
    pub full: bool,
    pub max_bytes: Option<usize>,
}

/// `readBackground(id, opts)`.
pub fn read_background(id: &str, opts: BgReadOpts) -> Result<BgOutputResult, String> {
    let mut reg = bg_registry().lock().unwrap();
    let proc = reg
        .1
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("No background process {id}. Use repo_bg_list to see active ones."))?;
    bg_poll(proc);
    let (buf_text, dropped) = {
        let b = proc.buffer.lock().unwrap();
        (b.text.clone(), b.dropped)
    };
    let abs_end = dropped + buf_text.len();
    let from = if opts.full {
        0
    } else {
        proc.read_cursor.saturating_sub(dropped).min(buf_text.len())
    };
    // Slice on a char boundary at/after `from`.
    let mut start = from;
    while start < buf_text.len() && !buf_text.is_char_boundary(start) {
        start += 1;
    }
    let mut output = buf_text[start..].to_string();
    let new_bytes = abs_end.saturating_sub(proc.read_cursor.max(dropped));
    proc.read_cursor = abs_end;
    if let Some(max) = opts.max_bytes {
        output = trim_middle(&output, max).0;
    }
    let running = proc.status == "running";
    Ok(BgOutputResult {
        id: proc.id.clone(),
        name: proc.name.clone(),
        status: proc.status.clone(),
        exit_code: proc.exit_code,
        running,
        bytes: output.len() as u64,
        new_bytes: new_bytes as u64,
        output,
    })
}

/// `stopBackground(id)` — tree-kill if still running.
pub fn stop_background(id: &str) -> Result<BgStopResult, String> {
    let mut reg = bg_registry().lock().unwrap();
    let proc = reg
        .1
        .iter_mut()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("No background process {id}. Use repo_bg_list to see active ones."))?;
    bg_poll(proc);
    if proc.status == "running" {
        bg_kill_tree(proc);
        proc.status = "killed".to_string();
        proc.ended_at = Some(Instant::now());
    }
    Ok(BgStopResult {
        id: proc.id.clone(),
        name: proc.name.clone(),
        status: proc.status.clone(),
        exit_code: proc.exit_code,
    })
}

fn bg_kill_tree(proc: &BgProc) {
    let pid = match proc.pid {
        Some(p) => p,
        None => return,
    };
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(unix)]
    {
        // Kill the whole process group (the child leads its own; see
        // platform_shell_std), then the direct child as a backstop.
        unix_kill_group(pid);
        let _ = proc.child.lock().unwrap().kill();
    }
}

/// `listBackground()`.
pub fn list_background() -> Vec<BgSummary> {
    let mut reg = bg_registry().lock().unwrap();
    let procs = &mut reg.1;
    for p in procs.iter_mut() {
        bg_poll(p);
    }
    procs
        .iter()
        .map(|p| {
            let end = p.ended_at.unwrap_or_else(Instant::now);
            BgSummary {
                id: p.id.clone(),
                name: p.name.clone(),
                command: p.command.clone(),
                status: p.status.clone(),
                exit_code: p.exit_code,
                pid: p.pid,
                age_ms: end.duration_since(p.started_at).as_millis(),
                buffered_bytes: p.buffer.lock().unwrap().text.len() as u64,
            }
        })
        .collect()
}

/// `killAllBackground()` — tree-kill every still-running child (signal handler).
pub fn kill_all_background() {
    let mut reg = bg_registry().lock().unwrap();
    for p in reg.1.iter_mut() {
        bg_poll(p);
        if p.status == "running" {
            bg_kill_tree(p);
            p.status = "killed".to_string();
        }
    }
}

// ===========================================================================
// files.ts — read / write / edit / glob / grep
// ===========================================================================

#[derive(Debug, Clone, Serialize)]
pub struct ReadFileResult {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub content: String,
    pub truncated: bool,
    pub binary: bool,
    #[serde(rename = "startLine", skip_serializing_if = "Option::is_none")]
    pub start_line: Option<usize>,
    #[serde(rename = "endLine", skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<usize>,
}

pub struct ReadOpts {
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub max_bytes: Option<usize>,
}

/// Hard ceiling on how many bytes one repo_read pulls off disk. The reply text
/// is separately capped by `max_read_bytes` (trim_middle); this bounds the read
/// I/O itself so a multi-GB file is never slurped whole into memory first.
/// `max(8MB, 4×max_read_bytes)` stays consistent with a config that raises
/// `max_read_bytes` past the 8MB floor.
fn read_ceiling_bytes(config: &Config) -> u64 {
    (8 * 1024 * 1024u64).max(config.max_read_bytes.saturating_mul(4) as u64)
}

/// Read a repo file (bounded). The blocking FS work runs under `spawn_blocking`
/// so the dispatcher's per-tool timeout stays pollable, mirroring
/// [`list_repo_files`].
pub async fn read_repo_file(
    config: &Config,
    path: &str,
    opts: ReadOpts,
) -> Result<ReadFileResult, String> {
    let config = config.clone();
    let path = path.to_string();
    tokio::task::spawn_blocking(move || read_repo_file_blocking(&config, &path, opts))
        .await
        .unwrap_or_else(|_| Err("read task aborted".to_string()))
}

fn read_repo_file_blocking(
    config: &Config,
    path: &str,
    opts: ReadOpts,
) -> Result<ReadFileResult, String> {
    let abs = resolve_inside_root(config, path, ResolveOpts::default()).map_err(|e| e.0)?;
    if stat_kind(&abs) != StatKind::File {
        return Err(format!("Not a file: {path}"));
    }
    // Size FIRST (symlink_metadata: no link-follow), then read at most the
    // ceiling — never the whole of an arbitrarily large file.
    let total_len = std::fs::symlink_metadata(&abs).map_err(|e| e.to_string())?.len();
    let ceiling = read_ceiling_bytes(config);
    let prefix_only = total_len > ceiling;
    let buf: Vec<u8> = if prefix_only {
        use std::io::Read;
        let f = std::fs::File::open(&abs).map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        f.take(ceiling)
            .read_to_end(&mut buf)
            .map_err(|e| e.to_string())?;
        buf
    } else {
        std::fs::read(&abs).map_err(|e| e.to_string())?
    };
    // Binary detection + sha256 run over what was read; when only a prefix was
    // read the reply text says so (the hash is of the prefix, not the file).
    let prefix_note = format!(
        "\n[truncated: file is {total_len} bytes; only the first {ceiling} bytes were read — sha256/lines reflect that prefix]"
    );
    let rel = to_posix(&relative_path(&config.root, &abs));
    if is_probably_binary(&buf) {
        let mut content = "[binary file blocked from text read]".to_string();
        if prefix_only {
            content.push_str(&prefix_note);
        }
        return Ok(ReadFileResult {
            path: rel,
            bytes: total_len,
            sha256: sha256_hex(&buf),
            content,
            truncated: true,
            binary: true,
            start_line: None,
            end_line: None,
            lines: None,
        });
    }
    let text = String::from_utf8_lossy(&buf);
    let all_lines: Vec<&str> = split_lines(&text);
    let total = all_lines.len();
    let start = opts.start_line.unwrap_or(1).max(1);
    let end = opts.end_line.unwrap_or(total).min(total);
    let mut numbered = String::new();
    if start <= end {
        for (i, line) in all_lines[start - 1..end].iter().enumerate() {
            if i > 0 {
                numbered.push('\n');
            }
            numbered.push_str(&format!("{}\t{}", start + i, line));
        }
    }
    let (mut content, trimmed) = trim_middle(&numbered, opts.max_bytes.unwrap_or(config.max_read_bytes));
    if prefix_only {
        content.push_str(&prefix_note);
    }
    Ok(ReadFileResult {
        path: rel,
        bytes: total_len,
        sha256: sha256_hex(&buf),
        content,
        truncated: trimmed || prefix_only,
        binary: false,
        start_line: Some(start),
        end_line: Some(end),
        lines: Some(total),
    })
}

/// Split on \r?\n like the TS `split(/\r?\n/)` (keeps a trailing empty element).
fn split_lines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let mut end = i;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            out.push(&s[start..end]);
            start = i + 1;
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteResult {
    pub path: String,
    pub bytes: usize,
    pub sha256: String,
}

pub struct WriteOpts {
    pub create_dirs: bool,
    pub expected_sha256: Option<String>,
}

pub fn write_repo_file(
    config: &Config,
    path: &str,
    content: &str,
    opts: WriteOpts,
) -> Result<WriteResult, String> {
    let abs = resolve_inside_root(
        config,
        path,
        ResolveOpts {
            for_write: true,
            allow_blocked: false,
        },
    )
    .map_err(|e| e.0)?;
    can_write_path(config, &abs).map_err(|r| format!("Write blocked by sandbox: {r}"))?;
    let rel = to_posix(&relative_path(&config.root, &abs));
    let bytes = content.len();
    if bytes > config.max_write_bytes {
        return Err(format!("Write too large: {bytes} > {}", config.max_write_bytes));
    }
    if let Some(expected) = &opts.expected_sha256 {
        if abs.exists() {
            let current = sha256_hex(&std::fs::read(&abs).map_err(|e| e.to_string())?);
            if &current != expected {
                return Err(format!(
                    "Expected sha mismatch for {rel}: current {} != expected {}",
                    &current[..12.min(current.len())],
                    &expected[..12.min(expected.len())]
                ));
            }
        }
    }
    if opts.create_dirs {
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    std::fs::write(&abs, content).map_err(|e| e.to_string())?;
    Ok(WriteResult {
        path: rel,
        bytes,
        sha256: sha256_hex(content.as_bytes()),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct EditResult {
    pub path: String,
    pub replacements: usize,
    pub bytes: usize,
    pub sha256: String,
}

pub fn edit_repo_file(
    config: &Config,
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<EditResult, String> {
    let abs = resolve_inside_root(
        config,
        path,
        ResolveOpts {
            for_write: true,
            allow_blocked: false,
        },
    )
    .map_err(|e| e.0)?;
    can_write_path(config, &abs).map_err(|r| format!("Edit blocked by sandbox: {r}"))?;
    let rel = to_posix(&relative_path(&config.root, &abs));
    if !abs.exists() {
        return Err(format!("File not found: {rel}. Use repo_write for new files."));
    }
    if old_string == new_string {
        return Err("old_string and new_string are identical.".into());
    }
    let original = std::fs::read_to_string(&abs).map_err(|e| e.to_string())?;
    let count = if old_string.is_empty() {
        0
    } else {
        original.matches(old_string).count()
    };
    if count == 0 {
        return Err(format!(
            "old_string not found in {rel}. Read the file and copy the exact text (including whitespace)."
        ));
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string is not unique in {rel} ({count} matches). Add surrounding context to make it unique, or set replace_all: true."
        ));
    }
    let updated = if replace_all {
        original.replace(old_string, new_string)
    } else {
        original.replacen(old_string, new_string, 1)
    };
    let bytes = updated.len();
    if bytes > config.max_write_bytes {
        return Err(format!("Result too large: {bytes} > {}", config.max_write_bytes));
    }
    std::fs::write(&abs, &updated).map_err(|e| e.to_string())?;
    Ok(EditResult {
        path: rel,
        replacements: if replace_all { count } else { 1 },
        bytes,
        sha256: sha256_hex(updated.as_bytes()),
    })
}

pub struct ListOpts {
    pub globs: Vec<String>,
    pub max: Option<usize>,
    pub sort: Option<String>, // "mtime" | "path"
}

/// Outcome of [`list_repo_files`]: the (filtered, sorted, capped) file list plus
/// whether the underlying scan tripped a budget — callers must NOT assume the
/// list is exhaustive when `truncated` is set.
#[derive(Debug, Clone)]
pub struct ListFilesResult {
    pub files: Vec<String>,
    /// True when the non-git walk hit its deadline / directory / depth budget.
    pub truncated: bool,
    /// Files collected by the scan, before glob filtering and the `max` cap.
    pub scanned: usize,
}

// Budgets for the non-git filesystem walk. A home-dir root (the incident shape)
// has junction loops ("Application Data" -> AppData\Roaming) and millions of
// entries; without these bounds the walk never returns.
const WALK_DEADLINE_MS: u64 = 10_000;
const WALK_MAX_DIRS: usize = 25_000;
const WALK_MAX_DEPTH: usize = 24;
const WALK_MAX_FILES: usize = 20_000;

/// Directory component names that are ALWAYS pruned from the walk, regardless of
/// `allow_hidden_files` / `blocked_path_globs` — pathologically-huge or loop-prone
/// trees that no repo listing should descend into. Compared case-insensitively.
const WALK_PRUNE_DIRS: &[&str] = &[
    "appdata",
    "application data",
    "node_modules",
    ".git",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".cache",
    ".gradle",
    ".m2",
    ".cargo",
    ".rustup",
    ".npm",
    ".nuget",
    "library",
];

/// Hard bounds threaded through [`walk_files`].
struct WalkBounds {
    deadline: Instant,
    max_dirs: usize,
    max_depth: usize,
}

/// Running totals for one walk; `truncated` flips when any bound trips.
#[derive(Default)]
struct WalkStats {
    dirs_visited: usize,
    truncated: bool,
}

pub async fn list_repo_files(config: &Config, opts: ListOpts) -> ListFilesResult {
    let max = opts.max.unwrap_or(800);
    let sort = opts.sort.unwrap_or_else(|| "mtime".into());
    let globs = opts.globs.clone();
    let mut files: Vec<String> = Vec::new();
    let truncated = false;
    // Direct git (no platform shell): the probe must not inherit PowerShell's
    // intermittent managed-host failures on Windows.
    if let Ok(git) = run_git(
        config,
        &["ls-files", "--cached", "--others", "--exclude-standard"],
        2_000_000,
    )
    .await
    {
        if git.exit_code == Some(0) && !git.stdout.trim().is_empty() {
            files = git
                .stdout
                .split('\n')
                .map(|s| s.trim_end_matches('\r'))
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
        }
    }
    // Everything below is synchronous CPU + blocking FS work (the walk, the
    // per-file glob filtering, the mtime stat pass + sort). It runs on a blocking
    // thread so the caller's async context — and the dispatcher's per-tool
    // `tokio::time::timeout` — stays pollable; a sync spin here can no longer hold
    // the worker thread and starve the timeout. If the blocking task is abandoned
    // (rare: runtime shutdown), degrade to an empty list rather than panicking.
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        list_repo_files_blocking(&config, files, truncated, &globs, max, &sort)
    })
    .await
    .unwrap_or_else(|_| ListFilesResult {
        files: Vec::new(),
        truncated: true,
        scanned: 0,
    })
}

/// The synchronous tail of [`list_repo_files`]: walk (if git was empty), filter,
/// sort. Pulled out so it can run under `spawn_blocking`. `files` is the git
/// ls-files result (possibly empty) and `truncated` carries any prior signal.
fn list_repo_files_blocking(
    config: &Config,
    mut files: Vec<String>,
    mut truncated: bool,
    globs: &[String],
    max: usize,
    sort: &str,
) -> ListFilesResult {
    if files.is_empty() {
        let bounds = WalkBounds {
            deadline: Instant::now() + Duration::from_millis(WALK_DEADLINE_MS),
            max_dirs: WALK_MAX_DIRS,
            max_depth: WALK_MAX_DEPTH,
        };
        let mut stats = WalkStats::default();
        let mut out: Vec<String> = Vec::new();
        walk_files(config, &config.root, 0, &bounds, &mut stats, &mut out);
        truncated = stats.truncated;
        if truncated {
            tracing::warn!(
                root = %config.root.display(),
                files = out.len(),
                dirs = stats.dirs_visited,
                "file walk truncated: scan budget exhausted"
            );
        }
        files = out;
    }
    let scanned = files.len();
    let filtered = filter_files(config, files, globs);
    if sort == "path" || filtered.len() > 8000 {
        let mut v: Vec<String> = filtered.into_iter().take(max).collect();
        v.sort();
        return ListFilesResult { files: v, truncated, scanned };
    }
    // The mtime-stat pass shares the walk's wall-clock budget: once the deadline
    // passes, remaining files keep mtime 0 instead of issuing more stats.
    let stat_deadline = Instant::now() + Duration::from_millis(WALK_DEADLINE_MS);
    let mut with_mtime: Vec<(String, u128)> = filtered
        .into_iter()
        .map(|f| {
            let mtime = if Instant::now() >= stat_deadline {
                0
            } else {
                std::fs::metadata(config.root.join(&f))
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            };
            (f, mtime)
        })
        .collect();
    with_mtime.sort_by(|a, b| b.1.cmp(&a.1));
    ListFilesResult {
        files: with_mtime.into_iter().take(max).map(|x| x.0).collect(),
        truncated,
        scanned,
    }
}

/// True when a directory component is on the always-prune denylist.
fn walk_pruned_component(name: &str) -> bool {
    WALK_PRUNE_DIRS.iter().any(|d| name.eq_ignore_ascii_case(d))
}

fn walk_files(
    config: &Config,
    dir: &Path,
    depth: usize,
    bounds: &WalkBounds,
    stats: &mut WalkStats,
    out: &mut Vec<String>,
) {
    if out.len() >= WALK_MAX_FILES {
        stats.truncated = true;
        return;
    }
    if Instant::now() >= bounds.deadline || stats.dirs_visited >= bounds.max_dirs {
        stats.truncated = true;
        return;
    }
    if depth > bounds.max_depth {
        stats.truncated = true;
        return;
    }
    stats.dirs_visited += 1;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for ent in entries.flatten() {
        if Instant::now() >= bounds.deadline {
            stats.truncated = true;
            return;
        }
        let abs = ent.path();
        let rel = to_posix(&relative_path(&config.root, &abs));
        if matches_any_glob(&rel, &config.blocked_path_globs).is_some() {
            continue;
        }
        if !config.allow_hidden_files && rel.split('/').any(|p| p.starts_with('.')) {
            continue;
        }
        let ft = match ent.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        // `DirEntry::file_type` does not follow links, so this also covers
        // Windows junctions — which is what makes home-dir walks loop forever
        // ("Application Data" -> AppData\Roaming -> ...).
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            let name = ent.file_name();
            if walk_pruned_component(&name.to_string_lossy()) {
                continue;
            }
            walk_files(config, &abs, depth + 1, bounds, stats, out);
        } else if ft.is_file() {
            out.push(rel);
        }
        if out.len() >= WALK_MAX_FILES {
            stats.truncated = true;
            return;
        }
    }
}

fn filter_files(config: &Config, files: Vec<String>, globs: &[String]) -> Vec<String> {
    // Precompile each glob's regex(es) ONCE, then match every file against the
    // reused set. The previous version called `matches_any_glob` per file, which
    // recompiled ~blocked×2 regexes for every one of thousands of files — the
    // live CPU hang. Semantics are unchanged: blocked-first, then hide_hidden,
    // then the user-glob `f.contains(g)` substring fallback.
    let blocked = compile_glob_matchers(&config.blocked_path_globs);
    let mut filtered: Vec<String> = files
        .into_iter()
        .filter(|f| matches_any_compiled(&normalize_for_glob(f), &blocked).is_none())
        .collect();
    if config.hide_hidden_dirs {
        filtered.retain(|f| !in_hidden_dir(f));
    }
    if globs.is_empty() {
        return filtered;
    }
    // One matcher per user glob, paired with its raw string for the substring
    // fallback (`f.contains(g)`) the TS kept.
    let user: Vec<(GlobMatcher, &String)> =
        globs.iter().map(|g| (GlobMatcher::new(g), g)).collect();
    filtered
        .into_iter()
        .filter(|f| {
            let posix = normalize_for_glob(f);
            user.iter()
                .any(|(m, g)| m.matches(&posix) || f.contains(g.as_str()))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub command: String,
    pub output: String,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
    pub used: String,
    #[serde(rename = "outputMode")]
    pub output_mode: String,
}

pub struct SearchOpts {
    pub globs: Vec<String>,
    pub typ: Option<String>,
    pub max_matches: Option<usize>,
    pub context: Option<usize>,
    pub output_mode: Option<String>,
    pub regex: bool,
}

/// Build the `git grep` argument vector for [`search_repo`]. Pulled out as a pure
/// function so the option→flag mapping is unit-testable without a live repo.
///
/// Mapping (mirrors the rg invocation's intent):
/// - `--no-color`, `-I` (skip binary files) always.
/// - fixed-strings vs regex: `-F` when `!opts.regex`, else `-E` (extended regex,
///   matching rg's default-ish ERE feel).
/// - smart-case: git grep has no smart-case, so emulate it — add `-i` only when
///   the query has no uppercase character (lowercase query ⇒ case-insensitive),
///   otherwise stay case-sensitive. This matches rg's `--smart-case` behavior.
/// - output mode: `-l` (files-with-matches) for "files"; `-c` (count per file)
///   for "count"; otherwise `-n` (line numbers) plus `-C<context>`.
/// - type filter (`opts.typ`): git grep has no `--type`, so translate a few common
///   names to a pathspec glob; unknown types are ignored (rg would filter, git
///   grep simply searches all — acceptable, still bounded).
/// - globs: appended as `--` pathspecs (one per glob). git grep treats a bare glob
///   like `*.rs` as a pathspec; we also wrap with `:(glob)` so `**` works.
/// - terminator: `-e <query>` so a leading-dash query isn't parsed as a flag.
fn build_git_grep_args(query: &str, opts: &SearchOpts, output_mode: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "grep".into(),
        "--no-color".into(),
        "-I".into(),
    ];
    if opts.regex {
        args.push("-E".into());
    } else {
        args.push("-F".into());
    }
    // smart-case emulation: case-insensitive only when the query is all-lowercase.
    if !query.chars().any(|c| c.is_uppercase()) {
        args.push("-i".into());
    }
    match output_mode {
        "files" => args.push("-l".into()),
        "count" => args.push("-c".into()),
        _ => {
            args.push("-n".into());
            let ctx = opts.context.unwrap_or(2);
            args.push(format!("-C{ctx}"));
        }
    }
    // Pattern (after `-e` so leading dashes are literal).
    args.push("-e".into());
    args.push(query.to_string());
    // Pathspecs: optional type→glob, then explicit globs. All after `--`.
    let mut pathspecs: Vec<String> = Vec::new();
    if let Some(t) = opts.typ.as_deref() {
        if let Some(glob) = git_type_to_pathspec(t) {
            pathspecs.push(glob);
        }
    }
    for g in &opts.globs {
        // `:(glob)` magic makes `**` behave; plain `*.rs` also works under it.
        pathspecs.push(format!(":(glob){g}"));
    }
    if !pathspecs.is_empty() {
        args.push("--".into());
        args.extend(pathspecs);
    }
    args
}

/// Map a handful of common rg `--type` names to a git pathspec glob. Returns
/// `None` for unrecognized types (the search then spans all tracked files).
fn git_type_to_pathspec(t: &str) -> Option<String> {
    let ext: &str = match t {
        "rust" => "rs",
        "js" => "js",
        "ts" => "ts",
        "py" | "python" => "py",
        "go" => "go",
        "c" => "c",
        "cpp" | "c++" => "cpp",
        "java" => "java",
        "json" => "json",
        "toml" => "toml",
        "md" | "markdown" => "md",
        "yaml" | "yml" => "yml",
        "sh" => "sh",
        _ => return None,
    };
    Some(format!(":(glob)**/*.{ext}"))
}

/// Probe whether a *real* ripgrep is on PATH (cached per process). On this class
/// of Windows box `rg.exe` is the App-Execution-Alias stub: a 0-byte reparse
/// point that, when spawned non-interactively, fails or produces nothing. A
/// genuine rg answers `rg --version` with a "ripgrep <ver>" banner; the stub does
/// not. We cache the verdict so the probe runs at most once per process.
async fn rg_available(config: &Config) -> bool {
    static RG_OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if let Some(v) = RG_OK.get() {
        return *v;
    }
    let probed = match run_shell(
        config,
        "rg --version",
        RunShellOpts {
            max_output_bytes: Some(4096),
            timeout_ms: Some(4_000),
            ..Default::default()
        },
    )
    .await
    {
        Ok(r) => {
            r.exit_code == Some(0)
                && r.stdout.to_ascii_lowercase().contains("ripgrep")
        }
        Err(_) => false,
    };
    // Racing probes are harmless (idempotent); first writer wins.
    let _ = RG_OK.set(probed);
    *RG_OK.get().unwrap_or(&probed)
}

/// Is `config.root` inside a git work tree? Uses the direct `run_git` helper
/// (no PowerShell), so it doesn't inherit the managed-host failures. `git
/// rev-parse --is-inside-work-tree` prints "true" on stdout with exit 0.
async fn root_is_git_repo(config: &Config) -> bool {
    match run_git(config, &["rev-parse", "--is-inside-work-tree"], 8_192).await {
        Ok(r) => r.exit_code == Some(0) && r.stdout.trim() == "true",
        Err(_) => false,
    }
}

pub async fn search_repo(config: &Config, query: &str, opts: SearchOpts) -> SearchResult {
    let max_matches = opts.max_matches.unwrap_or(80);
    let context = opts.context.unwrap_or(2);
    let output_mode = opts.output_mode.clone().unwrap_or_else(|| "content".into());

    // Strategy 1: ripgrep — but only if a real rg is present (not the WindowsApps
    // stub). The capability probe is cached per process.
    if rg_available(config).await {
        let glob_args = opts
            .globs
            .iter()
            .map(|g| format!("-g {}", shell_quote(g)))
            .collect::<Vec<_>>()
            .join(" ");
        let type_arg = opts
            .typ
            .as_ref()
            .map(|t| format!("--type {}", shell_quote(t)))
            .unwrap_or_default();
        let fixed = if opts.regex { "" } else { "--fixed-strings" };
        let hidden = if config.hide_hidden_dirs { "" } else { "--hidden" };
        let mode_args = match output_mode.as_str() {
            "files" => "--files-with-matches".to_string(),
            "count" => "--count".to_string(),
            _ => format!("--line-number --context {}", context.max(0)),
        };
        let raw = format!(
            "rg {hidden} --smart-case {fixed} {mode_args} {type_arg} {glob_args} {} .",
            shell_quote(query)
        );
        let cmd = ws_re().replace_all(&raw, " ").trim().to_string();
        if let Ok(rg) = run_shell(
            config,
            &cmd,
            RunShellOpts {
                max_output_bytes: Some(config.max_command_output_bytes),
                timeout_ms: Some(30_000),
                ..Default::default()
            },
        )
        .await
        {
            if rg.exit_code == Some(0) || rg.exit_code == Some(1) {
                let src = if rg.stdout.is_empty() { &rg.stderr } else { &rg.stdout };
                let output = if rg.exit_code == Some(1) && src.trim().is_empty() {
                    format!("no matches found for {query}")
                } else {
                    trim_matches(src, max_matches, &output_mode)
                };
                return SearchResult {
                    command: cmd,
                    output,
                    exit_code: rg.exit_code,
                    used: "rg".into(),
                    output_mode,
                };
            }
        }
        // rg claimed-available but this run failed hard — fall through to git/manual.
    }

    // Strategy 2: `git grep` on a git work tree (the fast path on this box, since
    // rg is the stub). exit 0 = matches, exit 1 = no matches (clean, NOT an
    // error), exit >1 = real error → fall through to the manual scan.
    if root_is_git_repo(config).await {
        let args = build_git_grep_args(query, &opts, &output_mode);
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        if let Ok(g) = run_git(config, &arg_refs, config.max_command_output_bytes).await {
            let cmd = format!("git {}", args.join(" "));
            match g.exit_code {
                Some(0) => {
                    return SearchResult {
                        command: cmd,
                        output: trim_matches(&g.stdout, max_matches, &output_mode),
                        exit_code: Some(0),
                        used: "git-grep".into(),
                        output_mode,
                    };
                }
                Some(1) => {
                    return SearchResult {
                        command: cmd,
                        output: format!("no matches found for {query}"),
                        exit_code: Some(1),
                        used: "git-grep".into(),
                        output_mode,
                    };
                }
                _ => {
                    tracing::warn!(
                        query = %query,
                        exit = ?g.exit_code,
                        "git grep failed; falling back to manual scan"
                    );
                }
            }
        }
    }

    // Strategy 3: JS-equivalent fallback when neither rg nor git is usable.
    // Inherits list_repo_files' scan
    // bounds: on a pathological root this list may be truncated, so the grep is
    // best-effort rather than exhaustive (better than never returning).
    let listed = list_repo_files(
        config,
        ListOpts {
            globs: opts.globs.clone(),
            max: Some(2000),
            sort: Some("path".into()),
        },
    )
    .await;
    let list_truncated = listed.truncated;
    let files = listed.files;
    let needle = query.to_lowercase();
    // The fallback read loop is blocking FS + CPU over up to ~640 files; run it
    // off the async worker so the dispatcher's per-tool timeout stays pollable,
    // and under the same wall-clock deadline the walk uses so a tree of huge
    // files can't pin the blocking thread either.
    let (hits, read_truncated): (Vec<(String, usize, String)>, bool) = {
        let config = config.clone();
        let needle = needle.clone();
        tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + Duration::from_millis(WALK_DEADLINE_MS);
            let mut truncated = false;
            let mut hits: Vec<(String, usize, String)> = Vec::new();
            for file in files {
                if hits.len() >= max_matches * 8 {
                    break;
                }
                if Instant::now() >= deadline {
                    truncated = true;
                    break;
                }
                if let Ok(abs) = resolve_inside_root(&config, &file, ResolveOpts::default()) {
                    if let Ok(md) = std::fs::metadata(&abs) {
                        if md.len() > 800_000 {
                            continue;
                        }
                    }
                    if let Ok(text) = std::fs::read_to_string(&abs) {
                        for (i, line) in split_lines(&text).iter().enumerate() {
                            if line.to_lowercase().contains(&needle) {
                                hits.push((file.clone(), i + 1, line.to_string()));
                            }
                        }
                    }
                }
            }
            (hits, truncated)
        })
        .await
        .unwrap_or((Vec::new(), true))
    };
    if read_truncated {
        tracing::warn!(query = %query, "fallback grep stopped at its scan deadline");
    }
    let output = match output_mode.as_str() {
        "files" => {
            let mut seen = Vec::new();
            for (f, _, _) in &hits {
                if !seen.contains(f) {
                    seen.push(f.clone());
                }
            }
            seen.into_iter().take(max_matches).collect::<Vec<_>>().join("\n")
        }
        "count" => {
            let mut counts: Vec<(String, usize)> = Vec::new();
            for (f, _, _) in &hits {
                if let Some(e) = counts.iter_mut().find(|(k, _)| k == f) {
                    e.1 += 1;
                } else {
                    counts.push((f.clone(), 1));
                }
            }
            counts
                .into_iter()
                .take(max_matches)
                .map(|(f, n)| format!("{f}:{n}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => hits
            .iter()
            .take(max_matches)
            .map(|(f, l, t)| format!("{f}:{l}:{t}"))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let exit_code = if hits.is_empty() { Some(1) } else { Some(0) };
    // Honesty note: a truncated file list or a deadline-stopped read loop means
    // the absence of a match proves nothing. But a *clean* empty result (no
    // truncation) is a real "no matches" — say so explicitly rather than handing
    // back a bare truncation line (the old confusing behavior).
    let mut output = output;
    if hits.is_empty() && !list_truncated && !read_truncated {
        output = format!("no matches found for {query}");
    } else if list_truncated || read_truncated {
        if !output.is_empty() {
            output.push('\n');
        }
        if hits.is_empty() {
            output.push_str(&format!(
                "no matches found for {query} (file list truncated — results may be incomplete)"
            ));
        } else {
            output.push_str("(file list truncated — results may be incomplete)");
        }
    }
    SearchResult {
        command: "fallback-js-search".into(),
        output,
        exit_code,
        used: "fallback".into(),
        output_mode,
    }
}

fn trim_matches(s: &str, max_matches: usize, output_mode: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let cap = if output_mode == "content" {
        max_matches * 4
    } else {
        max_matches
    };
    if lines.len() <= cap {
        return s.to_string();
    }
    let kind = if output_mode == "content" {
        "match blocks".to_string()
    } else {
        output_mode.to_string()
    };
    format!(
        "{}\n…[trimmed to {max_matches} {kind}]…",
        lines[..cap].join("\n")
    )
}

/// `shellQuote` — Windows (PowerShell/cmd) doubles inner double-quotes; POSIX
/// wraps in single quotes with the `'\''` escape.
pub fn shell_quote(s: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

// ===========================================================================
// projects.ts — discover / score / switch project (the repo_project surface)
// ===========================================================================

const PROJECT_SKIP_DIRS: &[&str] = &[
    ".git",
    ".repo-agent-mcp",
    "node_modules",
    ".next",
    "dist",
    "build",
    "target",
    ".venv",
    "venv",
    "__pycache__",
];
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "package.json",
    "pyproject.toml",
    "Cargo.toml",
    "go.mod",
    "deno.json",
    "bun.lockb",
    "pnpm-lock.yaml",
    "yarn.lock",
];

/// `ProjectSummary` (the subset repo_project surfaces). Optional fields are
/// omitted from the JSON exactly like the TS object spread drops `undefined`.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub root: String,
    pub selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<i64>,
}

pub struct DiscoverOpts {
    pub query: Option<String>,
    pub max: Option<usize>,
}

/// `discoverProjects(config, opts)` — seed from configured projects + the active
/// root, scan the search roots for project markers, summarize, score, sort.
pub fn discover_projects(config: &Config, opts: DiscoverOpts) -> Vec<ProjectSummary> {
    let max = opts.max.unwrap_or(24);
    // Insertion-ordered de-dup by realpath (JS Map order).
    let mut order: Vec<String> = Vec::new();
    let mut seeds: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    let mut add_seed = |root: String, name: Option<String>, desc: Option<String>| {
        if !seeds.contains_key(&root) {
            order.push(root.clone());
        }
        seeds.insert(root, (name, desc));
    };

    for project in &config.projects {
        let root = realpath_maybe(&to_posix_native(&project.root));
        add_seed(root, project.name.clone(), project.description.clone());
    }
    add_seed(
        realpath_maybe(&to_posix_native(&config.root)),
        config.current_project.clone(),
        None,
    );

    for base in &config.project_search_roots {
        scan_roots(
            &realpath_maybe(&to_posix_native(base)),
            config.max_project_scan_depth,
            &mut order,
            &mut seeds,
        );
        if order.len() >= max * 4 {
            break;
        }
    }

    let q = opts
        .query
        .as_ref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let config_root_real = realpath_maybe(&to_posix_native(&config.root));

    let mut summaries: Vec<ProjectSummary> = order
        .iter()
        .map(|root| {
            let (name, desc) = seeds.get(root).cloned().unwrap_or((None, None));
            summarize_project(root, name, desc, &config_root_real, q.as_deref())
        })
        .collect();

    if let Some(query) = &q {
        summaries.retain(|s| s.score.unwrap_or(0) > 0);
        let _ = query;
    }
    // Sort: selected first, then score desc, then name asc.
    summaries.sort_by(|a, b| {
        let sel = (b.selected as i64).cmp(&(a.selected as i64));
        if sel != std::cmp::Ordering::Equal {
            return sel;
        }
        let sc = b.score.unwrap_or(0).cmp(&a.score.unwrap_or(0));
        if sc != std::cmp::Ordering::Equal {
            return sc;
        }
        a.name.cmp(&b.name)
    });
    summaries.truncate(max);
    summaries
}

fn scan_roots(
    base: &str,
    depth: i64,
    order: &mut Vec<String>,
    seeds: &mut HashMap<String, (Option<String>, Option<String>)>,
) {
    let base_path = Path::new(base);
    if !base_path.exists() || !base_path.is_dir() || depth < 0 {
        return;
    }
    let real = realpath_maybe(base);
    if is_project(&real) && !seeds.contains_key(&real) {
        order.push(real.clone());
        seeds.insert(real.clone(), (None, None));
    }
    if depth == 0 {
        return;
    }
    let entries = match std::fs::read_dir(Path::new(&real)) {
        Ok(e) => e,
        Err(_) => return,
    };
    for ent in entries.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir || PROJECT_SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
            continue;
        }
        let child = ent.path();
        if child.is_dir() {
            scan_roots(&to_posix_native(&child), depth - 1, order, seeds);
        }
        if seeds.len() > 200 {
            return;
        }
    }
}

fn is_project(root: &str) -> bool {
    PROJECT_MARKERS
        .iter()
        .any(|m| Path::new(root).join(m).exists())
}

fn summarize_project(
    root: &str,
    seed_name: Option<String>,
    seed_desc: Option<String>,
    config_root_real: &str,
    query: Option<&str>,
) -> ProjectSummary {
    let real = realpath_maybe(root);
    let name = seed_name
        .or_else(|| package_name(&real))
        .unwrap_or_else(|| basename(&real));
    let selected = config_root_real == real;
    let description = seed_desc.or_else(|| {
        let readme = read_first_readme(&real)?;
        Some(brief(&readme, 220))
    });
    let mut summary = ProjectSummary {
        id: stable_id(&real),
        name,
        root: real,
        selected,
        description,
        score: None,
    };
    score_project(&mut summary, query);
    summary
}

fn package_name(root: &str) -> Option<String> {
    let txt = std::fs::read_to_string(Path::new(root).join("package.json")).ok()?;
    let pkg: Value = serde_json::from_str(&txt).ok()?;
    pkg.get("name").and_then(|v| v.as_str()).map(String::from)
}

fn read_first_readme(root: &str) -> Option<String> {
    for name in ["README.md", "readme.md", "README.txt", "AGENTS.md"] {
        let path = Path::new(root).join(name);
        if path.exists() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                return Some(text);
            }
            return None;
        }
    }
    None
}

fn score_project(summary: &mut ProjectSummary, query: Option<&str>) {
    let query = match query {
        None => {
            summary.score = Some(if summary.selected { 999 } else { 1 });
            return;
        }
        Some(q) => q,
    };
    let hay = format!(
        "{}\n{}\n{}",
        summary.name,
        summary.root,
        summary.description.clone().unwrap_or_default()
    )
    .to_lowercase();
    let mut score = 0i64;
    for token in query.split_whitespace() {
        if summary.name.to_lowercase().contains(token) {
            score += 8;
        }
        if summary.root.to_lowercase().contains(token) {
            score += 4;
        }
        if hay.contains(token) {
            score += 2;
        }
    }
    if summary.selected {
        score += 3;
    }
    summary.score = Some(score);
}

/// `stableId(path)` — base64url of the path, first 24 chars.
fn stable_id(path: &str) -> String {
    let encoded = base64url_encode(path.as_bytes());
    encoded.chars().take(24).collect()
}

fn base64url_encode(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 63) as usize] as char);
        out.push(ALPHA[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHA[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHA[(n & 63) as usize] as char);
        }
    }
    out
}

fn basename(path: &str) -> String {
    to_posix(path)
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

fn to_posix_native(p: &Path) -> String {
    to_posix(&p.to_string_lossy())
}

/// `realpathMaybe(path)` — canonicalize an existing path (posix-normalized), else
/// the lexically resolved input. Degrades to the input on any failure.
fn realpath_maybe(path: &str) -> String {
    let pb = PathBuf::from(path);
    if pb.exists() {
        match std::fs::canonicalize(&pb) {
            Ok(real) => to_posix(&strip_unc(&real).to_string_lossy()),
            Err(_) => to_posix(path),
        }
    } else {
        to_posix(&normalize_path(&pb).to_string_lossy())
    }
}

/// Strip a Windows `\\?\` verbatim prefix so containment comparisons line up.
fn strip_unc(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// `resolveProject(config, queryOrPath)` — a direct allowed-root dir, else the
/// top discovery match. Returns the resolved [`ProjectSummary`].
pub fn resolve_project(config: &Config, query_or_path: &str) -> Result<ProjectSummary, String> {
    let raw = query_or_path.trim();
    let direct = realpath_maybe(raw);
    let config_root_real = realpath_maybe(&to_posix_native(&config.root));
    let mut allowed: Vec<String> = vec![to_posix_native(&config.home_root)];
    allowed.extend(config.project_search_roots.iter().map(|p| to_posix_native(p)));
    allowed.extend(config.projects.iter().map(|p| to_posix_native(&p.root)));
    allowed.push(to_posix_native(&config.root));

    if Path::new(&direct).is_dir()
        && allowed
            .iter()
            .any(|root| is_inside(Path::new(&realpath_maybe(root)), Path::new(&direct)))
    {
        return Ok(summarize_project(&direct, None, None, &config_root_real, None));
    }
    let projects = discover_projects(
        config,
        DiscoverOpts {
            query: Some(raw.to_string()),
            max: Some(8),
        },
    );
    projects
        .into_iter()
        .next()
        .ok_or_else(|| format!("No project matched {raw}"))
}

/// `switchProject(config, state, queryOrPath)` — resolve, then mutate the active
/// root + project name on `config`. (The caller mirrors the root into RepoState.)
///
/// NOTE: this deliberately only mutates the `tools::Config` view. Callers that
/// switch the LIVE root (repo_project's "switch" action, the `/project` slash
/// command) MUST also call `state.switch_root(&config.root...)` afterwards —
/// both existing callers do — or events/blobs keep writing under the old root's
/// `.repo-agent-mcp`.
pub fn switch_project(config: &mut Config, query_or_path: &str) -> Result<ProjectSummary, String> {
    let mut project = resolve_project(config, query_or_path)?;
    let root = PathBuf::from(&project.root);
    config.root = root;
    config.current_project = Some(project.name.clone());
    project.selected = true;
    Ok(project)
}

/// `safeGitDiff(config, maxBytes?)` — working-tree + staged diff, trimmed; any
/// failure degrades to the error message text (never throws). Runs `git`
/// directly (two invocations, concatenated) instead of chaining through the
/// platform shell — see [`run_git`].
pub async fn safe_git_diff(config: &Config, max_bytes: Option<usize>) -> String {
    let max = max_bytes.unwrap_or(160_000);
    let mut combined = String::new();
    for args in [
        &["diff", "--no-ext-diff"][..],
        &["diff", "--cached", "--no-ext-diff"][..],
    ] {
        match run_git(config, args, max).await {
            Ok(r) => {
                if !combined.is_empty() && !r.combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(&r.combined);
            }
            Err(e) => return e,
        }
    }
    trim_middle(&combined, max).0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    /// Fresh temp root per test (pid + tag so parallel runs don't collide).
    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chimera-tools-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_config(root: &Path) -> Config {
        let mut c = Config::with_root(root.to_path_buf());
        // Prod default (config.rs) is true; the prune denylist must hold anyway.
        c.allow_hidden_files = true;
        c
    }

    #[test]
    fn walk_stops_at_deadline() {
        let root = temp_root("deadline");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/file.txt"), "x").unwrap();
        let config = test_config(&root);
        let bounds = WalkBounds {
            deadline: Instant::now() - Duration::from_millis(1), // already expired
            max_dirs: 1000,
            max_depth: 8,
        };
        let mut stats = WalkStats::default();
        let mut out = Vec::new();
        walk_files(&config, &config.root, 0, &bounds, &mut stats, &mut out);
        assert!(stats.truncated, "expired deadline must report truncation");
        assert!(out.is_empty(), "no files collected past the deadline");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walk_prunes_denylisted_dirs_and_depth() {
        let root = temp_root("prune");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        // Denylisted by component name (NOT in the default blocked_path_globs).
        std::fs::create_dir_all(root.join("AppData/Roaming")).unwrap();
        std::fs::write(root.join("AppData/Roaming/huge.dat"), "x").unwrap();
        // Deeper than max_depth.
        std::fs::create_dir_all(root.join("d1/d2/d3/d4/d5")).unwrap();
        std::fs::write(root.join("d1/d2/d3/d4/d5/deep.txt"), "x").unwrap();
        let config = test_config(&root);
        let bounds = WalkBounds {
            deadline: Instant::now() + Duration::from_secs(10),
            max_dirs: 1000,
            max_depth: 3,
        };
        let mut stats = WalkStats::default();
        let mut out = Vec::new();
        walk_files(&config, &config.root, 0, &bounds, &mut stats, &mut out);
        assert!(out.contains(&"src/main.rs".to_string()), "got: {out:?}");
        assert!(
            !out.iter().any(|f| f.starts_with("AppData/")),
            "AppData must be pruned even with allow_hidden_files=true: {out:?}"
        );
        assert!(!out.iter().any(|f| f.ends_with("deep.txt")), "depth bound: {out:?}");
        assert!(stats.truncated, "tripped depth bound must surface as truncation");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walk_skips_symlinks_and_junctions() {
        let root = temp_root("links");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/file.txt"), "x").unwrap();
        // A self-referential dir link: the incident loop shape
        // ("Application Data" -> AppData\Roaming). Junction on Windows (no
        // admin needed), symlink elsewhere. Skip the test if creation fails.
        let link = root.join("loopback");
        #[cfg(windows)]
        let created = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(&root, &link).is_ok();
        if !created {
            let _ = std::fs::remove_dir_all(&root);
            return; // environment can't make links — nothing to assert
        }
        let config = test_config(&root);
        let bounds = WalkBounds {
            deadline: Instant::now() + Duration::from_secs(10),
            max_dirs: 1000,
            max_depth: 8,
        };
        let mut stats = WalkStats::default();
        let mut out = Vec::new();
        walk_files(&config, &config.root, 0, &bounds, &mut stats, &mut out);
        assert_eq!(out, vec!["real/file.txt".to_string()], "link must not be followed");
        assert!(!stats.truncated, "a skipped link is not a budget trip");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_git_on_non_repo_dir_fails_cleanly() {
        let root = temp_root("nongit");
        let config = test_config(&root);
        match block_on(run_git(&config, &["status", "--short", "--branch"], 40_000)) {
            Ok(r) => {
                assert_ne!(r.exit_code, Some(0));
                assert!(
                    r.combined.to_lowercase().contains("not a git repository"),
                    "got: {}",
                    r.combined
                );
            }
            // git not on PATH in this environment — the helper still failed
            // cleanly (a short message, not a PowerShell stack).
            Err(e) => assert!(e.contains("git failed to start"), "got: {e}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression guard for the live hang: filtering thousands of files against
    /// the default blocked globs + a user glob must NOT recompile a regex per
    /// file. Pre-fix this was ~3 minutes for ~4869 real files; here 5000 paths
    /// must complete in well under half a second.
    #[test]
    fn filter_files_does_not_recompile_per_file() {
        let root = std::env::temp_dir(); // not touched — filter is path-only
        let mut config = Config::with_root(root);
        // The production blocked set (config.rs DEFAULT_BLOCKED_PATH_GLOBS) — the
        // realistic ~hot count, each with a `**/` head that adds a fallback regex.
        config.blocked_path_globs = vec![
            ".git/**".into(),
            ".repo-agent-mcp/**".into(),
            "**/.env".into(),
            "**/.env.*".into(),
            "**/*id_rsa*".into(),
            "**/*id_ed25519*".into(),
            "**/*.pem".into(),
            "**/*.key".into(),
            "**/node_modules/**".into(),
            "**/.next/**".into(),
            "**/dist/**".into(),
            "**/build/**".into(),
        ];
        config.hide_hidden_dirs = false;
        let files: Vec<String> = (0..5000)
            .map(|i| format!("crate{}/src/sub{}/mod{}.rs", i % 50, i % 17, i))
            .chain((0..200).map(|i| format!("crate{i}/Cargo.toml")))
            .collect();
        let start = Instant::now();
        let out = filter_files(&config, files, &["**/Cargo.toml".to_string()]);
        let elapsed = start.elapsed();
        assert_eq!(out.len(), 200, "user glob should match the 200 Cargo.toml files");
        assert!(
            elapsed < Duration::from_millis(500),
            "filter_files took {elapsed:?} — regex recompilation regression"
        );
    }

    /// The memoized `glob_to_regex` returns the same compiled pattern for repeat
    /// calls and preserves the `**/` fallback semantics through `matches_any_glob`.
    #[test]
    fn glob_cache_preserves_semantics() {
        // `**/Cargo.toml` matches both a nested and a root Cargo.toml (head fallback).
        assert!(matches_any_glob("a/b/Cargo.toml", &["**/Cargo.toml".to_string()]).is_some());
        assert!(matches_any_glob("Cargo.toml", &["**/Cargo.toml".to_string()]).is_some());
        assert!(matches_any_glob("Cargo.lock", &["**/Cargo.toml".to_string()]).is_none());
        // Repeat calls hit the cache and stay correct.
        for _ in 0..3 {
            assert!(matches_any_glob("x/node_modules/y.js", &["**/node_modules/**".to_string()]).is_some());
        }
    }

    /// Is `pid` still a live process? (tasklist on Windows, `kill -0` elsewhere.)
    fn pid_alive(pid: u32) -> bool {
        if cfg!(windows) {
            match std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output()
            {
                Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
                Err(_) => false,
            }
        } else {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
    }

    /// Fix: a timed-out child must be KILLED, not detached. Before the fix the
    /// timeout arm dropped the tokio Child without kill_on_drop and the
    /// powershell/git process lived on as an orphan.
    #[test]
    fn timed_out_child_is_killed_not_orphaned() {
        let (pid, output, timed_out) = block_on(async {
            let mut cmd = if cfg!(windows) {
                let mut c = tokio::process::Command::new("powershell.exe");
                c.args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 120"]);
                c
            } else {
                let mut c = tokio::process::Command::new("sh");
                c.args(["-c", "sleep 120"]);
                c
            };
            cmd.stdin(std::process::Stdio::null());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            cmd.kill_on_drop(true); // the contract run_shell/run_git now follow
            let child = cmd.spawn().expect("spawn sleeper");
            let pid = child.id().expect("pid before wait");
            let (output, timed_out) = wait_child_with_timeout(child, 1_200).await.unwrap();
            (pid, output, timed_out)
        });
        // The synthetic timed-out result shape is preserved.
        assert!(timed_out);
        assert_eq!(String::from_utf8_lossy(&output.stderr), "[timed out]");
        // The child dies (kill_on_drop + tree-kill are async; poll briefly).
        let mut alive = true;
        for _ in 0..20 {
            alive = pid_alive(pid);
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        assert!(!alive, "pid {pid} survived the run_shell timeout");
    }

    /// Fix: repo_read of an oversized file reads only a bounded prefix (never the
    /// whole file), reports the REAL size, and says so in the reply.
    #[test]
    fn oversized_file_read_is_bounded_and_marked_truncated() {
        let root = temp_root("bigread");
        let config = test_config(&root);
        let big = root.join("big.bin");
        // Sparse 9MB file (instant to create; reads back as zeros => binary).
        let total: u64 = 9 * 1024 * 1024;
        std::fs::File::create(&big).unwrap().set_len(total).unwrap();

        let started = Instant::now();
        let r = block_on(read_repo_file(
            &config,
            "big.bin",
            ReadOpts { start_line: None, end_line: None, max_bytes: None },
        ))
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(r.bytes, total, "reply reports the real on-disk size");
        assert!(r.truncated);
        assert!(r.binary, "an all-zero prefix classifies as binary");
        assert!(
            r.content.contains("[truncated: file is 9437184 bytes"),
            "got: {}",
            r.content
        );

        // A normal small text file keeps the old shape end-to-end.
        std::fs::write(root.join("small.txt"), "alpha\nbeta\n").unwrap();
        let small = block_on(read_repo_file(
            &config,
            "small.txt",
            ReadOpts { start_line: None, end_line: None, max_bytes: None },
        ))
        .unwrap();
        assert!(!small.truncated && !small.binary);
        assert_eq!(small.bytes, 11);
        assert!(small.content.contains("1\talpha"));

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- FIX A: git-grep fast path -------------------------------------------

    fn search_opts(globs: Vec<String>, regex: bool, typ: Option<&str>) -> SearchOpts {
        SearchOpts {
            globs,
            typ: typ.map(|s| s.to_string()),
            max_matches: None,
            context: None,
            output_mode: None,
            regex,
        }
    }

    /// The pure arg-builder maps options to git-grep flags as intended.
    #[test]
    fn git_grep_args_fixed_strings_smartcase_content() {
        // lowercase query => smart-case insensitive (-i); fixed-strings (-F);
        // content mode => -n and -C2 (default context).
        let args = build_git_grep_args("shim_conductor", &search_opts(vec![], false, None), "content");
        assert!(args.contains(&"-F".to_string()), "fixed strings: {args:?}");
        assert!(args.contains(&"-i".to_string()), "smart-case lowercase => -i: {args:?}");
        assert!(args.contains(&"-n".to_string()), "content => line numbers: {args:?}");
        assert!(args.contains(&"-C2".to_string()), "default context: {args:?}");
        assert!(!args.contains(&"-E".to_string()), "no regex flag: {args:?}");
        // pattern carried after -e
        let i = args.iter().position(|a| a == "-e").expect("has -e");
        assert_eq!(args[i + 1], "shim_conductor");
    }

    /// Uppercase query stays case-sensitive (no -i); regex uses -E; globs become
    /// `:(glob)` pathspecs after `--`; files mode uses -l.
    #[test]
    fn git_grep_args_regex_case_sensitive_files_globs() {
        let args = build_git_grep_args(
            "Foo.*Bar",
            &search_opts(vec!["src/**/*.rs".into()], true, Some("rust")),
            "files",
        );
        assert!(args.contains(&"-E".to_string()), "regex => -E: {args:?}");
        assert!(!args.contains(&"-i".to_string()), "uppercase => case-sensitive: {args:?}");
        assert!(args.contains(&"-l".to_string()), "files mode => -l: {args:?}");
        // pathspecs after `--`
        let dd = args.iter().position(|a| a == "--").expect("has --");
        let specs = &args[dd + 1..];
        assert!(specs.iter().any(|s| s == ":(glob)src/**/*.rs"), "glob pathspec: {specs:?}");
        assert!(specs.iter().any(|s| s.contains("*.rs")), "rust type pathspec: {specs:?}");
    }

    #[test]
    fn git_grep_args_count_mode() {
        let args = build_git_grep_args("x", &search_opts(vec![], false, None), "count");
        assert!(args.contains(&"-c".to_string()), "count mode => -c: {args:?}");
        assert!(!args.contains(&"-n".to_string()));
    }

    /// Is git on PATH? Skip the live integration test cleanly when it isn't.
    fn git_present() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn git_in(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Integration: in a real temp git repo, repo_grep finds a string via the
    /// git-grep path (used == "git-grep"), reports a clean "no matches" for an
    /// absent string, and honors files vs content output modes. Gated on `git`.
    #[test]
    fn search_repo_uses_git_grep_in_real_repo() {
        if !git_present() {
            return; // no git on PATH — nothing to assert
        }
        let root = temp_root("gitgrep");
        if !git_in(&root, &["init", "-q"]) {
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        // Minimal identity so commits don't depend on global config.
        let _ = git_in(&root, &["config", "user.email", "t@t"]);
        let _ = git_in(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.rs"), "fn main() {}\nlet SHIM_CONDUCTOR = 1;\n").unwrap();
        std::fs::write(root.join("b.txt"), "nothing interesting here\n").unwrap();
        // git grep without a commit needs the files tracked; add them.
        assert!(git_in(&root, &["add", "-A"]), "git add must succeed");

        let config = test_config(&root);

        // 1) content search finds the needle via git-grep.
        let r = block_on(search_repo(
            &config,
            "SHIM_CONDUCTOR",
            search_opts(vec![], false, None),
        ));
        // If a real rg happened to be installed in CI, used would be "rg"; on the
        // target boxes (and CI without rg) it is "git-grep". Either way it MUST
        // find the match — and where git-grep ran, assert the label.
        assert_eq!(r.exit_code, Some(0), "should find the match: {r:?}");
        assert!(r.output.contains("SHIM_CONDUCTOR"), "match text present: {}", r.output);
        if r.used != "rg" {
            assert_eq!(r.used, "git-grep", "expected git-grep path: {r:?}");
        }

        // 2) absent string => clean "no matches found", not a bare truncation note.
        let none = block_on(search_repo(
            &config,
            "zzz_definitely_absent_zzz",
            search_opts(vec![], false, None),
        ));
        assert_eq!(none.exit_code, Some(1), "no matches => exit 1: {none:?}");
        assert!(
            none.output.contains("no matches found"),
            "clean no-match message: {}",
            none.output
        );
        assert!(
            !none.output.contains("truncated"),
            "must not be a bare truncation line: {}",
            none.output
        );

        // 3) files mode returns the path, not file:line:text.
        let files = block_on(search_repo(
            &config,
            "SHIM_CONDUCTOR",
            SearchOpts {
                globs: vec![],
                typ: None,
                max_matches: None,
                context: None,
                output_mode: Some("files".into()),
                regex: false,
            },
        ));
        assert_eq!(files.exit_code, Some(0), "{files:?}");
        assert!(files.output.contains("a.rs"), "files mode lists path: {}", files.output);
        if files.used != "rg" {
            assert!(
                !files.output.contains(":1:") && !files.output.contains(":2:"),
                "files mode must not include line:text: {}",
                files.output
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
