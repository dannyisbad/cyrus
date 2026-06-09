//! Configuration loading + permission profiles.
//!
//! Source: repo-agent-mcp/src/config.ts (private original)
//!         (+ shapes from .../src/types.ts)
//!
//! Port plan:
//!   - `RepoAgentConfig` struct mirroring types.ts (root/homeRoot, port/host,
//!     bearerToken/publicUrl, sandboxMode/approvalPolicy, command allow/deny
//!     prefix lists, blockedPathGlobs, autoCompact tuning, subagent caps).
//!   - DEFAULT_PERMISSION_PROFILES: read-only / auto / full-access.
//!   - loadConfig(): merge order is DEFAULTS < repo-agent.config.json < env <
//!     argv, with realpath canonicalization of root/homeRoot/writableRoots.
//!   - wantsHttp(): `--http` present OR `--stdio` absent.
//!
//! Hazards (preserved below):
//!   - The TS `realpathSync`'s the root; on Windows we mirror with `dunce` so a
//!     `\\?\` UNC/verbatim prefix never leaks into later path-containment checks.
//!   - Env precedence differs per field. `REPO_AGENT_TOKEN` (and
//!     `REPO_AGENT_PUBLIC_URL`) use JS `||` semantics: a present-but-empty env
//!     value falls through to the file/default. `port`/`host` use `??` (nullish)
//!     semantics: only an absent value falls through. Both are reproduced exactly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Enums (types.ts)
// ---------------------------------------------------------------------------

/// `SpiceLevel` — "mild" | "spicy" | "feral".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpiceLevel {
    Mild,
    Spicy,
    Feral,
}

/// `SandboxMode` — "read-only" | "workspace-write" | "danger-full-access".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// `ApprovalPolicy` — "untrusted" | "on-request" | "never".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    Untrusted,
    OnRequest,
    Never,
}

/// `ApprovalReviewer` — "user" | "auto_review".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReviewer {
    User,
    AutoReview,
}

// ---------------------------------------------------------------------------
// Nested config shapes (types.ts)
// ---------------------------------------------------------------------------

/// `AutoCompactConfig` from types.ts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoCompactConfig {
    pub enabled: bool,
    #[serde(rename = "eventSoftLimit")]
    pub event_soft_limit: i64,
    #[serde(rename = "eventHardLimit")]
    pub event_hard_limit: i64,
    #[serde(rename = "bytesSoftLimit")]
    pub bytes_soft_limit: i64,
    #[serde(rename = "hotEventCount")]
    pub hot_event_count: i64,
    #[serde(rename = "hotFileCount")]
    pub hot_file_count: i64,
    #[serde(rename = "capsuleBudgetChars")]
    pub capsule_budget_chars: i64,
    #[serde(rename = "returnCapsuleEveryNEvents")]
    pub return_capsule_every_n_events: i64,
}

/// `PermissionProfile` from types.ts. Mirrors `Partial<PermissionProfile>` —
/// every field is optional so file overrides can specify just a subset, exactly
/// like the TS `Record<string, Partial<PermissionProfile>>`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PermissionProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        rename = "sandboxMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sandbox_mode: Option<SandboxMode>,
    #[serde(
        rename = "approvalPolicy",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub approval_policy: Option<ApprovalPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<ApprovalReviewer>,
    #[serde(
        rename = "writableRoots",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub writable_roots: Option<Vec<String>>,
    #[serde(
        rename = "commandAllowPrefixes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub command_allow_prefixes: Option<Vec<String>>,
    #[serde(
        rename = "commandPromptPrefixes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub command_prompt_prefixes: Option<Vec<String>>,
    #[serde(
        rename = "commandDenyRegex",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub command_deny_regex: Option<Vec<String>>,
}

/// `ProjectConfig` from types.ts (post-normalization: `root` is canonicalized,
/// `tags` is always present).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `RepoAgentConfig` from types.ts — the fully-merged runtime config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoAgentConfig {
    pub root: String,
    #[serde(rename = "homeRoot")]
    pub home_root: String,
    #[serde(
        rename = "currentProject",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub current_project: Option<String>,
    #[serde(rename = "projectSearchRoots")]
    pub project_search_roots: Vec<String>,
    pub projects: Vec<ProjectConfig>,
    #[serde(rename = "maxProjectScanDepth")]
    pub max_project_scan_depth: f64,
    pub port: f64,
    pub host: String,
    #[serde(
        rename = "bearerToken",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub bearer_token: Option<String>,
    #[serde(rename = "publicUrl", default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    #[serde(rename = "spiceLevel")]
    pub spice_level: SpiceLevel,
    #[serde(rename = "allowModelWriteFile")]
    pub allow_model_write_file: bool,
    #[serde(rename = "allowModelDevShell")]
    pub allow_model_dev_shell: bool,
    #[serde(rename = "allowSecretsRead")]
    pub allow_secrets_read: bool,
    #[serde(rename = "allowHiddenFiles")]
    pub allow_hidden_files: bool,
    #[serde(rename = "hideHiddenDirs")]
    pub hide_hidden_dirs: bool,
    #[serde(rename = "sandboxMode")]
    pub sandbox_mode: SandboxMode,
    #[serde(rename = "approvalPolicy")]
    pub approval_policy: ApprovalPolicy,
    #[serde(rename = "approvalsReviewer")]
    pub approvals_reviewer: ApprovalReviewer,
    #[serde(rename = "writableRoots")]
    pub writable_roots: Vec<String>,
    #[serde(rename = "commandAllowPrefixes")]
    pub command_allow_prefixes: Vec<String>,
    #[serde(rename = "commandPromptPrefixes")]
    pub command_prompt_prefixes: Vec<String>,
    #[serde(rename = "permissionProfiles")]
    pub permission_profiles: BTreeMap<String, PermissionProfile>,
    #[serde(rename = "maxReadBytes")]
    pub max_read_bytes: i64,
    #[serde(rename = "maxWriteBytes")]
    pub max_write_bytes: i64,
    #[serde(rename = "maxCommandOutputBytes")]
    pub max_command_output_bytes: i64,
    #[serde(rename = "defaultCommandTimeoutMs")]
    pub default_command_timeout_ms: i64,
    #[serde(rename = "blockedPathGlobs")]
    pub blocked_path_globs: Vec<String>,
    #[serde(rename = "commandProfiles")]
    pub command_profiles: BTreeMap<String, String>,
    #[serde(rename = "commandDenyRegex")]
    pub command_deny_regex: Vec<String>,
    #[serde(rename = "envPassthrough")]
    pub env_passthrough: Vec<String>,
    #[serde(rename = "autoCompact")]
    pub auto_compact: AutoCompactConfig,
    #[serde(rename = "maxSubagents")]
    pub max_subagents: f64,
    #[serde(rename = "maxSubagentSpawns")]
    pub max_subagent_spawns: f64,
    #[serde(rename = "subagentIdleTimeoutMs")]
    pub subagent_idle_timeout_ms: f64,
}

/// Back-compat alias so `http.rs` / `main.rs` keep referring to `config::Config`.
pub type Config = RepoAgentConfig;

// ---------------------------------------------------------------------------
// DEFAULT_PERMISSION_PROFILES (config.ts)
// ---------------------------------------------------------------------------

fn vecs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// `DEFAULT_PERMISSION_PROFILES` — three named profiles. Insertion order
/// ("read-only", "auto", "full-access") is preserved via an ordered map so the
/// serialized shape matches the TS object literal.
fn default_permission_profiles() -> BTreeMap<String, PermissionProfile> {
    let mut map = BTreeMap::new();

    map.insert(
        "read-only".to_string(),
        PermissionProfile {
            name: Some("read-only".to_string()),
            description: Some(
                "Inspect files and git state. Writes and arbitrary shell commands require approval."
                    .to_string(),
            ),
            sandbox_mode: Some(SandboxMode::ReadOnly),
            approval_policy: Some(ApprovalPolicy::OnRequest),
            reviewer: Some(ApprovalReviewer::User),
            writable_roots: Some(vec![]),
            command_allow_prefixes: Some(vecs(&[
                "git status",
                "git diff",
                "git log",
                "git show",
                "git branch",
                "git ls-files",
                "ls",
                "dir",
                "cat",
                "type",
                "sed",
                "head",
                "tail",
                "rg",
                "grep",
                "find",
                "tree",
            ])),
            command_prompt_prefixes: Some(vec![]),
            command_deny_regex: Some(vec![]),
        },
    );

    map.insert(
        "auto".to_string(),
        PermissionProfile {
            name: Some("auto".to_string()),
            description: Some(
                "Codex-like default: edit within the workspace and run routine commands, ask for risky/network/out-of-scope actions."
                    .to_string(),
            ),
            sandbox_mode: Some(SandboxMode::WorkspaceWrite),
            approval_policy: Some(ApprovalPolicy::OnRequest),
            reviewer: Some(ApprovalReviewer::User),
            writable_roots: Some(vec![]),
            command_allow_prefixes: Some(vecs(&[
                "git status",
                "git diff",
                "git log",
                "git show",
                "git branch",
                "git ls-files",
                "npm test",
                "npm run test",
                "npm run build",
                "npm run lint",
                "npm run typecheck",
                "pnpm test",
                "pnpm build",
                "pnpm lint",
                "pnpm typecheck",
                "yarn test",
                "yarn build",
                "yarn lint",
                "yarn typecheck",
                "bun test",
                "cargo test",
                "cargo check",
                "go test",
                "pytest",
                "python -m pytest",
                "uv run pytest",
            ])),
            command_prompt_prefixes: Some(vecs(&[
                "npm install",
                "pnpm install",
                "yarn install",
                "bun install",
                "pip install",
                "uv pip install",
                "cargo install",
                "curl",
                "wget",
                "git push",
                "git commit",
                "git reset",
                "git clean",
            ])),
            command_deny_regex: Some(vec![]),
        },
    );

    map.insert(
        "full-access".to_string(),
        PermissionProfile {
            name: Some("full-access".to_string()),
            description: Some(
                "Maximum authority. Still blocks obvious host-destroying commands unless explicitly approved."
                    .to_string(),
            ),
            sandbox_mode: Some(SandboxMode::DangerFullAccess),
            approval_policy: Some(ApprovalPolicy::Never),
            reviewer: Some(ApprovalReviewer::User),
            writable_roots: Some(vec![]),
            command_allow_prefixes: Some(vec![]),
            command_prompt_prefixes: Some(vec![]),
            command_deny_regex: Some(vec![]),
        },
    );

    map
}

// ---------------------------------------------------------------------------
// DEFAULTS (config.ts) — env-derived bits resolved at call time.
// ---------------------------------------------------------------------------

/// JS `||` truthiness over an env var: returns `Some(value)` only when the var
/// is set AND non-empty (an empty string is falsy in JS, so it falls through).
fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// `Number(process.env.PORT ?? 8787)`.
///
/// `??` is nullish: only an *absent* `PORT` falls through to 8787; an empty
/// `PORT` is passed to `Number("")` which is `0`. We reproduce that: present →
/// `js_number()`, absent → 8787.
fn default_port() -> f64 {
    match std::env::var("PORT") {
        Ok(v) => js_number(&v),
        Err(_) => 8787.0,
    }
}

/// `process.env.HOST ?? "127.0.0.1"` (nullish: empty HOST is kept).
fn default_host() -> String {
    std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Production default blocked-path globs. `pub(crate)` so `tools::Config::with_root`
/// mirrors the SAME list instead of a divergent (weaker) inline copy.
pub(crate) const DEFAULT_BLOCKED_PATH_GLOBS: &[&str] = &[
    ".git/**",
    ".repo-agent-mcp/**",
    "**/.env",
    "**/.env.*",
    "**/*id_rsa*",
    "**/*id_ed25519*",
    "**/*.pem",
    "**/*.key",
    "**/node_modules/**",
    "**/.next/**",
    "**/dist/**",
    "**/build/**",
];

const DEFAULT_COMMAND_DENY_REGEX: &[&str] = &[
    r"rm\s+-rf\s+/(\s|$)",
    r"rm\s+-rf\s+~(\s|$)",
    r"del\s+/s\s+/q",
    r"format\s+[a-z]:",
    r"mkfs\.",
    r"dd\s+if=",
    r":\(\)\s*\{",
    r"sudo\s+",
    r"shutdown\s",
    r"reboot\s*$",
    r"chmod\s+-R\s+777\s+/(\s|$)",
];

/// Production default env passthrough. `pub(crate)`: see DEFAULT_BLOCKED_PATH_GLOBS.
pub(crate) const DEFAULT_ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "NODE_OPTIONS",
    "npm_config_user_agent",
    "PNPM_HOME",
    "COREPACK_HOME",
];

/// Production default command profiles. `pub(crate)`: see DEFAULT_BLOCKED_PATH_GLOBS.
pub(crate) fn default_command_profiles() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert(
        "typecheck".to_string(),
        "pnpm typecheck || npm run typecheck".to_string(),
    );
    m.insert("test".to_string(), "pnpm test || npm test".to_string());
    m.insert("lint".to_string(), "pnpm lint || npm run lint".to_string());
    m.insert("build".to_string(), "pnpm build || npm run build".to_string());
    m.insert(
        "git_status".to_string(),
        "git status --short --branch".to_string(),
    );
    m
}

fn default_auto_compact() -> AutoCompactConfig {
    AutoCompactConfig {
        enabled: true,
        event_soft_limit: 28,
        event_hard_limit: 64,
        bytes_soft_limit: 220_000,
        hot_event_count: 12,
        hot_file_count: 12,
        capsule_budget_chars: 12_000,
        return_capsule_every_n_events: 10,
    }
}

// ---------------------------------------------------------------------------
// argv / path helpers (config.ts)
// ---------------------------------------------------------------------------

/// `process.argv` equivalent. Node's argv is `[node, script, ...args]`; Rust's
/// is `[program, ...args]`. The original scans the *whole* vector for flags, and
/// flag names never collide with the interpreter/program path, so scanning all
/// of `std::env::args()` reproduces the behavior.
fn argv() -> Vec<String> {
    std::env::args().collect()
}

/// `argValue(name)` — `--name <value>` or `--name=value` (first match wins).
fn arg_value(args: &[String], name: &str) -> Option<String> {
    if let Some(idx) = args.iter().position(|a| a == name) {
        // TS: process.argv[idx + 1] — may be undefined past the end.
        return args.get(idx + 1).cloned();
    }
    let prefix = format!("{name}=");
    args.iter()
        .find(|a| a.starts_with(&prefix))
        .map(|a| a[prefix.len()..].to_string())
}

/// `resolve(path)` — absolutize against the current working directory using
/// Node's lexical `path.resolve` rules (no filesystem access, no symlink
/// resolution). Trailing components like `.`/`..` are collapsed.
fn resolve(path: &str) -> PathBuf {
    resolve_from(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")), path)
}

/// `resolve(base, path)` — Node's two-arg `path.resolve`: if `path` is absolute
/// it wins outright, otherwise it is joined onto `base`. Result is normalized
/// lexically.
fn resolve_from(base: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    normalize_lexically(&joined)
}

/// Lexical normalization mirroring Node's `path` collapse of `.` and `..`
/// without touching the filesystem (so it works on non-existent paths).
fn normalize_lexically(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::ParentDir) | None => out.push(comp),
                // Don't pop past a root or prefix.
                _ => {}
            },
            other => out.push(other),
        }
    }
    let mut buf = PathBuf::new();
    for comp in out {
        buf.push(comp.as_os_str());
    }
    if buf.as_os_str().is_empty() {
        buf.push(".");
    }
    buf
}

/// Canonicalize an *existing* path the way `realpathSync` does, but strip the
/// Windows `\\?\` verbatim/UNC prefix (via `dunce`) so containment checks done
/// later compare apples to apples. Returns the input on any failure.
fn realpath_existing(p: &Path) -> PathBuf {
    match dunce::canonicalize(p) {
        Ok(real) => real,
        Err(_) => p.to_path_buf(),
    }
}

/// `realpathMaybe(path)` — `resolve(path)`, then `realpathSync` it iff it exists,
/// else return the resolved path untouched. The TS guards with `existsSync`; we
/// fold that into `realpath_existing` (canonicalize fails → fall back).
fn realpath_maybe(path: &str) -> String {
    let resolved = resolve(path);
    if resolved.exists() {
        path_to_string(&realpath_existing(&resolved))
    } else {
        path_to_string(&resolved)
    }
}

/// `realpathMaybe` but resolving relative to an explicit base dir first
/// (`realpathMaybe(resolve(configDir, p))` and friends in loadConfig).
fn realpath_maybe_from(base: &Path, path: &str) -> String {
    let resolved = resolve_from(base, path);
    if resolved.exists() {
        path_to_string(&realpath_existing(&resolved))
    } else {
        path_to_string(&resolved)
    }
}

fn path_to_string(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

// ---------------------------------------------------------------------------
// JSON coercion helpers (mirror the TS `String(...)` / `Number(...)` / type
// guards used throughout loadConfig).
// ---------------------------------------------------------------------------

/// JS `Number(x)` for the JSON values we feed it (numbers, strings, bool, null).
/// Only the cases reachable from the config are handled precisely; anything else
/// becomes `NaN` like JS would.
fn js_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0; // Number("") === 0
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `Number(value)` where `value` arrives as a JSON `Value` (the `?? DEFAULT`
/// already resolved by the caller). Numbers pass through; strings go through
/// `js_number`; bool → 0/1; null → 0 (Number(null) === 0).
fn js_number_value(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => js_number(s),
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Null => 0.0,
        _ => f64::NAN,
    }
}

/// `String(value)` for a JSON value (used for `host`, project `root`, search
/// roots). Strings pass through verbatim; numbers/bools stringify JS-style.
fn js_string_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// `asStringArray(value, fallback)` — if `value` isn't an array, clone the
/// fallback; otherwise keep only the string elements.
fn as_string_array(value: Option<&Value>, fallback: &[String]) -> Vec<String> {
    match value {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => fallback.to_vec(),
    }
}

/// Look up a key on a JSON object, returning `None` for non-objects/missing.
fn obj_get<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    obj.as_object().and_then(|m| m.get(key))
}

/// `typeof v === "string" ? v : undefined`.
fn opt_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// normalizeProjects (config.ts)
// ---------------------------------------------------------------------------

/// `normalizeProjects(projects, configDir)`.
fn normalize_projects(projects: Option<&Value>, config_dir: &Path) -> Vec<ProjectConfig> {
    let arr = match projects {
        Some(Value::Array(a)) => a,
        _ => return vec![],
    };
    arr.iter()
        .filter(|p| {
            // Boolean(p) && typeof p === "object" && typeof p.root === "string"
            p.is_object()
                && matches!(obj_get(p, "root"), Some(Value::String(_)))
        })
        .map(|p| {
            let root_str = match obj_get(p, "root") {
                Some(v) => js_string_value(v), // String(p.root)
                None => String::new(),
            };
            ProjectConfig {
                name: opt_string(obj_get(p, "name")),
                root: realpath_maybe_from(config_dir, &root_str),
                description: opt_string(obj_get(p, "description")),
                tags: as_string_array(obj_get(p, "tags"), &[]),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Permission-profile merge + field selection helpers
// ---------------------------------------------------------------------------

/// `{ ...DEFAULT_PERMISSION_PROFILES, ...(fromFile.permissionProfiles ?? {}) }`.
/// A spread *replaces* whole entries by key (it does not deep-merge a profile),
/// matching the TS object spread precisely.
fn merge_permission_profiles(from_file: Option<&Value>) -> BTreeMap<String, PermissionProfile> {
    let mut merged = default_permission_profiles();
    if let Some(Value::Object(map)) = from_file {
        for (k, v) in map {
            // Best-effort deserialize a Partial<PermissionProfile>; unknown
            // shapes degrade to an all-None profile (still a valid override).
            let profile: PermissionProfile = serde_json::from_value(v.clone()).unwrap_or_default();
            merged.insert(k.clone(), profile);
        }
    }
    merged
}

// ---------------------------------------------------------------------------
// wantsHttp (config.ts)
// ---------------------------------------------------------------------------

/// `wantsHttp()` — `--http` present OR `--stdio` absent.
pub fn wants_http() -> bool {
    let args = argv();
    args.iter().any(|a| a == "--http") || !args.iter().any(|a| a == "--stdio")
}

/// Warn — do NOT refuse — when the resolved repo root is the user's home
/// directory, a filesystem/drive root, or a direct parent of the home directory.
/// Serving repo tools over such a root is the incident shape: walks hit their
/// scan budgets and globs come back truncated.
fn warn_pathological_root(root: &Path) {
    // Normalize for comparison: lowercase (Windows paths are case-insensitive)
    // and without trailing separators.
    fn norm(p: &Path) -> String {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let root_n = norm(root);
    let is_home = home.as_deref().is_some_and(|h| norm(h) == root_n);
    let is_home_parent = home
        .as_deref()
        .and_then(Path::parent)
        .is_some_and(|p| norm(p) == root_n);
    // A filesystem/drive root has no parent ("C:\", "/").
    let is_fs_root = root.parent().is_none();
    if is_home || is_home_parent || is_fs_root {
        tracing::warn!(
            "repo root is {} — an entire home directory or filesystem root; repo tools will be slow and bounded; pass --repo <project>",
            root.display()
        );
    }
}

// ---------------------------------------------------------------------------
// loadConfig (config.ts)
// ---------------------------------------------------------------------------

/// `loadConfig()` — DEFAULTS < repo-agent.config.json < env < argv.
///
/// This is the byte-for-byte port of the TS merge. Where the TS would throw
/// (e.g. `realpathSync` on a missing `--repo`), we degrade gracefully to the
/// lexically-resolved path rather than panicking the whole server.
pub fn load_config() -> RepoAgentConfig {
    let args = argv();

    // root = realpathSync(resolve(--repo ?? REPO_AGENT_ROOT ?? cwd))
    let root_arg = arg_value(&args, "--repo")
        .or_else(|| env_var_present("REPO_AGENT_ROOT"))
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| path_to_string(&p))
                .unwrap_or_else(|_| ".".to_string())
        });
    let root_resolved = resolve(&root_arg);
    // TS uses realpathSync directly (no existence guard). We canonicalize when
    // possible and fall back to the resolved path otherwise.
    let root = path_to_string(&realpath_existing(&root_resolved));
    let root_path = PathBuf::from(&root);
    // Loud (but non-fatal) startup sanity check: an entire home directory or a
    // drive root makes every repo tool slow and budget-bounded.
    warn_pathological_root(&root_path);

    // configPath = --config ?? REPO_AGENT_CONFIG ?? resolve(root, "repo-agent.config.json")
    let config_path = arg_value(&args, "--config")
        .or_else(|| env_var_present("REPO_AGENT_CONFIG"))
        .map(PathBuf::from)
        .unwrap_or_else(|| resolve_from(&root_path, "repo-agent.config.json"));

    // fromFile = existsSync(configPath) ? loadJson(configPath) : {}
    let from_file: Value = if config_path.exists() {
        match std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        {
            Some(v) => v,
            None => Value::Object(Default::default()),
        }
    } else {
        Value::Object(Default::default())
    };

    // configDir = dirname(configPath)
    let config_dir = config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    // homeRoot = realpathMaybe(String(fromFile.homeRoot ?? REPO_AGENT_HOME_ROOT ?? root))
    let home_root_src = match obj_get(&from_file, "homeRoot") {
        Some(v) if !v.is_null() => js_string_value(v),
        _ => env_var_present("REPO_AGENT_HOME_ROOT").unwrap_or_else(|| root.clone()),
    };
    let home_root = realpath_maybe(&home_root_src);

    // defaultProjectSearchRoots = [root, homeRoot, dirname(root)]
    let dir_of_root = root_path
        .parent()
        .map(|p| path_to_string(p))
        .unwrap_or_else(|| root.clone());
    let default_project_search_roots = vec![root.clone(), home_root.clone(), dir_of_root];

    // permissionProfiles = { ...DEFAULTS, ...(fromFile.permissionProfiles ?? {}) }
    let permission_profiles = merge_permission_profiles(obj_get(&from_file, "permissionProfiles"));

    // selectedProfile = typeof fromFile.permissionProfile === "string" ? ... : "full-access"
    let selected_profile =
        opt_string(obj_get(&from_file, "permissionProfile")).unwrap_or_else(|| "full-access".to_string());
    // profile = permissionProfiles[selectedProfile] ?? permissionProfiles["full-access"]
    let profile: PermissionProfile = permission_profiles
        .get(&selected_profile)
        .or_else(|| permission_profiles.get("full-access"))
        .cloned()
        .unwrap_or_default();

    // --- merged object (DEFAULTS spread, then fromFile, then explicit fields) ---

    // currentProject: typeof fromFile.currentProject === "string" ? ... : undefined
    let current_project = opt_string(obj_get(&from_file, "currentProject"));

    // projectSearchRoots: [...new Set(asStringArray(fromFile.projectSearchRoots, default).map(p => realpathMaybe(resolve(configDir, p))))]
    let search_src = as_string_array(
        obj_get(&from_file, "projectSearchRoots"),
        &default_project_search_roots,
    );
    let project_search_roots = dedup_preserve_order(
        search_src
            .iter()
            .map(|p| realpath_maybe_from(&config_dir, p))
            .collect(),
    );

    // projects: normalizeProjects(fromFile.projects, configDir)
    let projects = normalize_projects(obj_get(&from_file, "projects"), &config_dir);

    // maxProjectScanDepth: Number(fromFile.maxProjectScanDepth ?? DEFAULTS.maxProjectScanDepth)
    let max_project_scan_depth =
        js_number_value(&nullish(obj_get(&from_file, "maxProjectScanDepth"), || Value::from(3)));

    // port: Number(--port ?? fromFile.port ?? DEFAULTS.port)
    let port = match arg_value(&args, "--port") {
        Some(p) => js_number(&p),
        None => match nullish_opt(obj_get(&from_file, "port")) {
            Some(v) => js_number_value(v),
            None => default_port(),
        },
    };

    // host: --host ?? String(fromFile.host ?? DEFAULTS.host)
    let host = match arg_value(&args, "--host") {
        Some(h) => h,
        None => match nullish_opt(obj_get(&from_file, "host")) {
            Some(v) => js_string_value(v),
            None => default_host(),
        },
    };

    // bearerToken: REPO_AGENT_TOKEN || fromFile.bearerToken || DEFAULTS.bearerToken
    // (|| semantics; DEFAULTS.bearerToken itself === REPO_AGENT_TOKEN || undefined)
    let bearer_token = env_nonempty("REPO_AGENT_TOKEN")
        .or_else(|| truthy_string(obj_get(&from_file, "bearerToken")))
        .or_else(|| env_nonempty("REPO_AGENT_TOKEN"));

    // publicUrl: REPO_AGENT_PUBLIC_URL || fromFile.publicUrl || DEFAULTS.publicUrl
    let public_url = env_nonempty("REPO_AGENT_PUBLIC_URL")
        .or_else(|| truthy_string(obj_get(&from_file, "publicUrl")))
        .or_else(|| env_nonempty("REPO_AGENT_PUBLIC_URL"));

    // spiceLevel: fromFile.spiceLevel ?? DEFAULTS ("spicy")
    let spice_level = obj_get(&from_file, "spiceLevel")
        .and_then(|v| serde_json::from_value::<SpiceLevel>(v.clone()).ok())
        .unwrap_or(SpiceLevel::Spicy);

    // Plain booleans spread from fromFile over DEFAULTS.
    let allow_model_write_file = bool_or(obj_get(&from_file, "allowModelWriteFile"), true);
    let allow_model_dev_shell = bool_or(obj_get(&from_file, "allowModelDevShell"), true);
    let allow_secrets_read = bool_or(obj_get(&from_file, "allowSecretsRead"), false);
    let allow_hidden_files = bool_or(obj_get(&from_file, "allowHiddenFiles"), true);
    let hide_hidden_dirs = bool_or(obj_get(&from_file, "hideHiddenDirs"), true);

    // sandboxMode: fromFile.sandboxMode ?? profile.sandboxMode ?? DEFAULTS (danger-full-access)
    let sandbox_mode = enum_pick::<SandboxMode>(obj_get(&from_file, "sandboxMode"))
        .or(profile.sandbox_mode)
        .unwrap_or(SandboxMode::DangerFullAccess);

    // approvalPolicy: fromFile ?? profile ?? DEFAULTS (never)
    let approval_policy = enum_pick::<ApprovalPolicy>(obj_get(&from_file, "approvalPolicy"))
        .or(profile.approval_policy)
        .unwrap_or(ApprovalPolicy::Never);

    // approvalsReviewer: fromFile.approvalsReviewer ?? profile.reviewer ?? DEFAULTS (user)
    let approvals_reviewer = enum_pick::<ApprovalReviewer>(obj_get(&from_file, "approvalsReviewer"))
        .or(profile.reviewer)
        .unwrap_or(ApprovalReviewer::User);

    // writableRoots: asStringArray(fromFile.writableRoots, profile.writableRoots ?? DEFAULTS [])
    //   .map(p => realpathMaybe(resolve(root, p)))
    let writable_roots_fallback = profile.writable_roots.clone().unwrap_or_default();
    let writable_roots = as_string_array(obj_get(&from_file, "writableRoots"), &writable_roots_fallback)
        .iter()
        .map(|p| realpath_maybe_from(&root_path, p))
        .collect();

    // commandAllowPrefixes: asStringArray(fromFile..., profile... ?? DEFAULTS.auto allow list)
    let allow_fallback = profile
        .command_allow_prefixes
        .clone()
        .unwrap_or_else(default_auto_allow_prefixes);
    let command_allow_prefixes =
        as_string_array(obj_get(&from_file, "commandAllowPrefixes"), &allow_fallback);

    // commandPromptPrefixes: asStringArray(fromFile..., profile... ?? DEFAULTS.auto prompt list)
    let prompt_fallback = profile
        .command_prompt_prefixes
        .clone()
        .unwrap_or_else(default_auto_prompt_prefixes);
    let command_prompt_prefixes =
        as_string_array(obj_get(&from_file, "commandPromptPrefixes"), &prompt_fallback);

    // commandProfiles: { ...DEFAULTS.commandProfiles, ...(fromFile.commandProfiles ?? {}) }
    let mut command_profiles = default_command_profiles();
    if let Some(Value::Object(map)) = obj_get(&from_file, "commandProfiles") {
        for (k, v) in map {
            // fromFile values typed as Record<string,string>; coerce via String().
            command_profiles.insert(k.clone(), js_string_value(v));
        }
    }

    // commandDenyRegex: [...DEFAULTS, ...asStringArray(profile.commandDenyRegex, []),
    //                    ...asStringArray(fromFile.commandDenyRegex, [])]
    let mut command_deny_regex: Vec<String> = DEFAULT_COMMAND_DENY_REGEX
        .iter()
        .map(|s| s.to_string())
        .collect();
    command_deny_regex.extend(profile.command_deny_regex.clone().unwrap_or_default());
    command_deny_regex.extend(as_string_array(obj_get(&from_file, "commandDenyRegex"), &[]));

    // autoCompact: { ...DEFAULTS.autoCompact, ...(fromFile.autoCompact ?? {}) }
    let auto_compact = merge_auto_compact(obj_get(&from_file, "autoCompact"));

    // maxSubagents / maxSubagentSpawns / subagentIdleTimeoutMs: Number(fromFile.x ?? DEFAULT)
    let max_subagents =
        js_number_value(&nullish(obj_get(&from_file, "maxSubagents"), || Value::from(2)));
    let max_subagent_spawns =
        js_number_value(&nullish(obj_get(&from_file, "maxSubagentSpawns"), || Value::from(12)));
    let subagent_idle_timeout_ms = js_number_value(&nullish(
        obj_get(&from_file, "subagentIdleTimeoutMs"),
        || Value::from(90_000),
    ));

    // Pass-through arrays that come straight from DEFAULTS unless the file
    // overrides them (plain object spread of fromFile over DEFAULTS).
    let blocked_path_globs = as_string_array(
        obj_get(&from_file, "blockedPathGlobs"),
        &DEFAULT_BLOCKED_PATH_GLOBS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    );
    let env_passthrough = as_string_array(
        obj_get(&from_file, "envPassthrough"),
        &DEFAULT_ENV_PASSTHROUGH
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    );

    // Scalar DEFAULTS overridable by a plain spread of fromFile.
    let max_read_bytes = int_or(obj_get(&from_file, "maxReadBytes"), 180_000);
    let max_write_bytes = int_or(obj_get(&from_file, "maxWriteBytes"), 350_000);
    let max_command_output_bytes = int_or(obj_get(&from_file, "maxCommandOutputBytes"), 160_000);
    let default_command_timeout_ms = int_or(obj_get(&from_file, "defaultCommandTimeoutMs"), 120_000);

    RepoAgentConfig {
        root,
        home_root,
        current_project,
        project_search_roots,
        projects,
        max_project_scan_depth,
        port,
        host,
        bearer_token,
        public_url,
        spice_level,
        allow_model_write_file,
        allow_model_dev_shell,
        allow_secrets_read,
        allow_hidden_files,
        hide_hidden_dirs,
        sandbox_mode,
        approval_policy,
        approvals_reviewer,
        writable_roots,
        command_allow_prefixes,
        command_prompt_prefixes,
        permission_profiles,
        max_read_bytes,
        max_write_bytes,
        max_command_output_bytes,
        default_command_timeout_ms,
        blocked_path_globs,
        command_profiles,
        command_deny_regex,
        env_passthrough,
        auto_compact,
        max_subagents,
        max_subagent_spawns,
        subagent_idle_timeout_ms,
    }
}

// ---------------------------------------------------------------------------
// Small merge/coercion helpers used only by loadConfig.
// ---------------------------------------------------------------------------

/// `process.env.X` with JS `??` (nullish) semantics: an env var that is unset
/// is `undefined`; a set-but-empty var is the empty string and is *kept*.
fn env_var_present(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// `value ?? fallback()` over an optional JSON `Value` reference, returning an
/// owned `Value` (nullish = `None` or JSON `null`).
fn nullish(value: Option<&Value>, fallback: impl FnOnce() -> Value) -> Value {
    match value {
        Some(v) if !v.is_null() => v.clone(),
        _ => fallback(),
    }
}

/// `value ?? undefined`-style: returns the reference only when non-nullish.
fn nullish_opt(value: Option<&Value>) -> Option<&Value> {
    match value {
        Some(v) if !v.is_null() => Some(v),
        _ => None,
    }
}

/// JS `||` over a JSON value treated as a candidate string: a present, truthy
/// (non-empty) string yields `Some`, everything else (missing/null/empty/non
/// -string) falls through.
fn truthy_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Plain spread of a boolean field: `fromFile.x` if it is a JSON bool, else the
/// default. (The TS spreads the raw value; configs always supply booleans here.)
fn bool_or(value: Option<&Value>, default: bool) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        _ => default,
    }
}

/// Plain spread of an integer scalar (`Number`-free path: these DEFAULTS fields
/// are not wrapped in `Number(...)`, so a file override passes through as-is
/// when it is a JSON number; otherwise the default stands).
fn int_or(value: Option<&Value>, default: i64) -> i64 {
    match value {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(default),
        _ => default,
    }
}

/// Deserialize a string-enum from a JSON value, returning `None` on mismatch.
fn enum_pick<T: for<'de> Deserialize<'de>>(value: Option<&Value>) -> Option<T> {
    match value {
        Some(v) if !v.is_null() => serde_json::from_value::<T>(v.clone()).ok(),
        _ => None,
    }
}

/// De-duplicate while preserving first-seen order (the `[...new Set(...)]` idiom
/// over `projectSearchRoots`).
fn dedup_preserve_order(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        if seen.insert(it.clone()) {
            out.push(it);
        }
    }
    out
}

/// `{ ...DEFAULTS.autoCompact, ...(fromFile.autoCompact ?? {}) }` — per-field
/// shallow override of the default AutoCompact tuning.
fn merge_auto_compact(value: Option<&Value>) -> AutoCompactConfig {
    let mut cfg = default_auto_compact();
    if let Some(Value::Object(map)) = value {
        if let Some(v) = map.get("enabled") {
            if let Value::Bool(b) = v {
                cfg.enabled = *b;
            }
        }
        let set_i = |key: &str, slot: &mut i64| {
            if let Some(Value::Number(n)) = map.get(key) {
                if let Some(i) = n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)) {
                    *slot = i;
                }
            }
        };
        set_i("eventSoftLimit", &mut cfg.event_soft_limit);
        set_i("eventHardLimit", &mut cfg.event_hard_limit);
        set_i("bytesSoftLimit", &mut cfg.bytes_soft_limit);
        set_i("hotEventCount", &mut cfg.hot_event_count);
        set_i("hotFileCount", &mut cfg.hot_file_count);
        set_i("capsuleBudgetChars", &mut cfg.capsule_budget_chars);
        set_i("returnCapsuleEveryNEvents", &mut cfg.return_capsule_every_n_events);
    }
    cfg
}

/// `DEFAULT_PERMISSION_PROFILES.auto.commandAllowPrefixes` — the DEFAULTS fall
/// back to the auto profile's allow list.
fn default_auto_allow_prefixes() -> Vec<String> {
    default_permission_profiles()
        .get("auto")
        .and_then(|p| p.command_allow_prefixes.clone())
        .unwrap_or_default()
}

/// `DEFAULT_PERMISSION_PROFILES.auto.commandPromptPrefixes`.
fn default_auto_prompt_prefixes() -> Vec<String> {
    default_permission_profiles()
        .get("auto")
        .and_then(|p| p.command_prompt_prefixes.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_value_space_and_equals_forms() {
        let args = vec![
            "prog".into(),
            "--repo".into(),
            "/tmp/x".into(),
            "--port=9000".into(),
        ];
        assert_eq!(arg_value(&args, "--repo"), Some("/tmp/x".to_string()));
        assert_eq!(arg_value(&args, "--port"), Some("9000".to_string()));
        assert_eq!(arg_value(&args, "--missing"), None);
    }

    #[test]
    fn arg_value_flag_at_end_is_none() {
        let args = vec!["prog".into(), "--repo".into()];
        assert_eq!(arg_value(&args, "--repo"), None);
    }

    #[test]
    fn as_string_array_filters_non_strings() {
        let v: Value = serde_json::json!(["a", 1, "b", null, true]);
        let out = as_string_array(Some(&v), &["fallback".into()]);
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn as_string_array_uses_fallback_when_not_array() {
        let v: Value = serde_json::json!("not-an-array");
        let out = as_string_array(Some(&v), &["fb".into()]);
        assert_eq!(out, vec!["fb".to_string()]);
    }

    #[test]
    fn js_number_empty_is_zero() {
        assert_eq!(js_number(""), 0.0);
        assert_eq!(js_number("  "), 0.0);
        assert_eq!(js_number("42"), 42.0);
    }

    #[test]
    fn default_profiles_have_three_entries() {
        let p = default_permission_profiles();
        assert!(p.contains_key("read-only"));
        assert!(p.contains_key("auto"));
        assert!(p.contains_key("full-access"));
        assert_eq!(
            p["full-access"].sandbox_mode,
            Some(SandboxMode::DangerFullAccess)
        );
    }

    #[test]
    fn dedup_preserves_first_seen_order() {
        let out = dedup_preserve_order(vec![
            "a".into(),
            "b".into(),
            "a".into(),
            "c".into(),
            "b".into(),
        ]);
        assert_eq!(out, vec!["a", "b", "c"]);
    }

    #[test]
    fn normalize_lexically_collapses_dot_dot() {
        let p = normalize_lexically(Path::new("/a/b/../c/./d"));
        assert_eq!(p, PathBuf::from("/a/c/d"));
    }

    #[test]
    fn merge_auto_compact_overrides_subset() {
        let v = serde_json::json!({ "enabled": false, "hotFileCount": 99 });
        let cfg = merge_auto_compact(Some(&v));
        assert!(!cfg.enabled);
        assert_eq!(cfg.hot_file_count, 99);
        // Untouched field keeps the default.
        assert_eq!(cfg.event_soft_limit, 28);
    }
}
