//! Headless `cyrus` CLI — same engine the codex TUI's option 4 runs, printing
//! guided progress instead of rendering it.
//!
//!   cyrus setup [--repo <dir>] [--json]   converge the whole stack
//!   cyrus check [--json]                   per-component diagnostics (exit 0 = healthy)
//!
//! `--json` emits one JSON object per line on stdout — the machine contract the
//! codex TUI parses (so it never has to scrape human labels):
//!   {"event":"step_started","step":"chrome","label":"..."}
//!   {"event":"step_done","step":"chrome","detail":"..."}
//!   {"event":"needs_user_action","step":"chrome","instruction":"..."}
//!   {"event":"user_action_resolved","step":"chrome"}
//!   {"event":"done","public_url":"...","shim_base_url":"...","tool_count":34}
//!   {"event":"error","step":"stack","message":"..."}
//! `cyrus check --json` emits a single
//!   {"event":"health","healthy":true,"components":[{"name","ok","detail"},...]}

use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use cyrus_engine::{
    diagnose, ensure_all, SetupEvent, SetupOptions, SetupOutcome, Step,
};

struct Cli {
    opts: SetupOptions,
    json: bool,
}

fn step_key(step: Step) -> &'static str {
    match step {
        Step::Secrets => "secrets",
        Step::Chrome => "chrome",
        Step::Tunnel => "tunnel",
        Step::Stack => "stack",
        Step::Connector => "connector",
        Step::CodexConfig => "codex_config",
    }
}

/// Parse the args for a cyrus-owned command (`setup`/`check`). The subcommand
/// itself was already classified by `main`; here we only read `--repo`/`--json`.
fn parse_args() -> Cli {
    let mut repo = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let mut json = false;
    // skip argv0 and the subcommand token.
    let mut args = std::env::args().skip(2);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--repo" => {
                if let Some(v) = args.next() {
                    repo = v.into();
                }
            }
            "--json" => json = true,
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }
    Cli {
        opts: SetupOptions::new(repo),
        json,
    }
}

fn emit_json(obj: serde_json::Value) {
    println!("{obj}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

// ---------------------------------------------------------------------------
// Presentation: a cargo-style gutter of right-aligned action words, tasteful
// color on a real terminal (honoring NO_COLOR), no emoji.
// ---------------------------------------------------------------------------

const DIM: &str = "2";
const BOLD: &str = "1";
const GREEN: &str = "1;32";
const YELLOW: &str = "1;33";
const RED: &str = "1;31";

/// Width of the right-aligned verb gutter; content begins at `GUTTER + 2`.
const GUTTER: usize = 12;

fn color_enabled() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Wrap `s` in an SGR color code when color is enabled, else return it plain.
fn paint(s: &str, code: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// A gutter row: the verb right-aligned in `GUTTER` columns (colored), then the
/// content. Padding is computed on the plain verb so color codes never skew it.
fn gutter_row(verb: &str, code: &str, content: &str) -> String {
    let pad = GUTTER.saturating_sub(verb.chars().count());
    format!("{}{}  {content}", " ".repeat(pad), paint(verb, code))
}

/// Indent used for content and continuation lines (aligns under a gutter row).
fn indent() -> String {
    " ".repeat(GUTTER + 2)
}

/// (present verb, past verb, in-progress hint) for a step.
fn step_verbs(step: Step) -> (&'static str, &'static str, &'static str) {
    match step {
        Step::Secrets => (
            "Preparing",
            "Prepared",
            "a local token that locks the servers to you",
        ),
        Step::Chrome => (
            "Connecting",
            "Connected",
            "your logged-in chatgpt.com tab (log in if asked)",
        ),
        Step::Tunnel => (
            "Opening",
            "Opened",
            "a public HTTPS URL for the connector",
        ),
        Step::Stack => (
            "Starting",
            "Started",
            "chimera + lipsync, the two local servers",
        ),
        Step::Connector => (
            "Wiring",
            "Wired",
            "registering the MCP connector on ChatGPT",
        ),
        Step::CodexConfig => (
            "Configuring",
            "Configured",
            "the `shadow` provider in your codex config",
        ),
    }
}

/// `cyrus` is the single front door. A small reserved set of subcommands is
/// cyrus's own; **everything else is passed straight through to `codex`**, with
/// the `shadow` model provider injected when cyrus is set up. So `cyrus` is a
/// drop-in alias for `codex` that runs on your ChatGPT subscription.
///
/// Reserved (cyrus-owned, never reaches codex):
///   setup, check        — bring up / diagnose the stack
///   chimera, lipsync    — the embedded servers (busybox; spawned by `setup`)
///
/// None of these collide with a real codex subcommand.
const CYRUS_SUBCOMMANDS: [&str; 4] = ["setup", "check", "chimera", "lipsync"];

fn main() -> ExitCode {
    let is_cyrus_cmd = std::env::args()
        .nth(1)
        .map(|a| CYRUS_SUBCOMMANDS.contains(&a.as_str()))
        .unwrap_or(false);

    if !is_cyrus_cmd {
        // Bare `cyrus`, or any non-cyrus subcommand → it's codex.
        return passthrough_to_codex();
    }

    // cyrus-owned commands need an async runtime (the embedded servers run on a
    // multi-thread runtime; see cyrus-chimera's wire bridge).
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cyrus: could not start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(run_cyrus_command())
}

/// Dispatch a cyrus-owned subcommand (already classified by `main`).
async fn run_cyrus_command() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        // Embedded servers. chimera reads its flags off `std::env::args()`
        // (flag-based, so the leading `chimera` positional is ignored); lipsync
        // is handed everything after the `lipsync` token.
        Some("chimera") => match cyrus_chimera::cli::run_cli().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e:?}");
                ExitCode::FAILURE
            }
        },
        Some("lipsync") => cyrus_lipsync::cli::run_cli(std::env::args().skip(2)).await,
        Some("check") => {
            let cli = parse_args();
            run_check(&cli.opts, cli.json).await
        }
        // "setup" (the only remaining reserved command).
        _ => {
            let cli = parse_args();
            run_setup(cli.opts, cli.json).await
        }
    }
}

// ---------------------------------------------------------------------------
// codex passthrough — cyrus as a front door to codex
// ---------------------------------------------------------------------------

/// Run `codex` with the user's args, injecting `-c model_provider=shadow` when
/// cyrus has written that provider into the codex config. A one-line breadcrumb
/// on stderr makes the handoff obvious. The exit code is codex's.
fn passthrough_to_codex() -> ExitCode {
    let user_args: Vec<String> = std::env::args().skip(1).collect();
    let configured = cyrus_engine::codex_config::current_provider_base_url().is_some();

    passthrough_banner(configured);

    let mut cmd = codex_command();
    if configured {
        // Injected before the user's args so an explicit `-c model_provider=...`
        // they pass still takes precedence.
        cmd.arg("-c").arg("model_provider=shadow");
    }
    cmd.args(&user_args);

    match cmd.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "cyrus: could not find `codex`. Install it, or point CYRUS_CODEX_BIN at the binary."
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("cyrus: failed to launch codex: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Build the `Command` that runs codex, resolving it robustly:
/// `CYRUS_CODEX_BIN` → a `codex` next to our exe (bundled layout) → a PATH
/// search that honors Windows `PATHEXT` (so an npm `codex.cmd` shim is found and
/// launched via `cmd /C`).
fn codex_command() -> std::process::Command {
    use std::path::PathBuf;
    use std::process::Command;

    if let Some(p) = std::env::var_os("CYRUS_CODEX_BIN") {
        let p = PathBuf::from(p);
        if p.exists() {
            return wrap_for_shim(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sib = dir.join(if cfg!(windows) { "codex.exe" } else { "codex" });
            if sib.exists() {
                return Command::new(sib);
            }
        }
    }
    if let Some(found) = which_in_path("codex") {
        return wrap_for_shim(found);
    }
    Command::new(if cfg!(windows) { "codex.exe" } else { "codex" })
}

/// On Windows a `.cmd`/`.bat` shim (e.g. npm's `codex.cmd`) can't be launched by
/// `CreateProcess` directly — run it through `cmd /C`. Everything else runs as-is.
fn wrap_for_shim(path: std::path::PathBuf) -> std::process::Command {
    use std::process::Command;
    #[cfg(windows)]
    {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "cmd" || ext == "bat" {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(path);
            return c;
        }
    }
    Command::new(path)
}

/// Minimal PATH search. On Windows, tries each `PATHEXT` extension (so `.exe` is
/// preferred over a `.cmd` shim); elsewhere matches the bare name.
fn which_in_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        #[cfg(windows)]
        {
            let exts =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            for ext in exts.split(';') {
                let cand = dir.join(format!("{name}{}", ext.to_ascii_lowercase()));
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
        #[cfg(not(windows))]
        {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// One dim line on stderr so it's always obvious cyrus handed off to codex, and
/// which provider is in play.
fn passthrough_banner(configured: bool) {
    use std::io::IsTerminal;
    let dim = std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal();
    let line = if configured {
        "cyrus › codex — on your ChatGPT subscription (shadow).  cyrus commands: setup · check"
    } else {
        "cyrus › codex — default provider.  run `cyrus setup` to use your ChatGPT subscription"
    };
    if dim {
        eprintln!("\x1b[2m{line}\x1b[0m");
    } else {
        eprintln!("{line}");
    }
}

// ---------------------------------------------------------------------------
// check — per-component diagnostics
// ---------------------------------------------------------------------------

async fn run_check(opts: &SetupOptions, json: bool) -> ExitCode {
    let dx = diagnose(opts).await;
    let healthy = dx.healthy();

    if json {
        let components: Vec<serde_json::Value> = dx
            .components
            .iter()
            .map(|c| serde_json::json!({"name": c.name, "ok": c.ok, "detail": c.detail}))
            .collect();
        emit_json(serde_json::json!({
            "event": "health", "healthy": healthy, "components": components
        }));
    } else {
        println!("{}\n", paint("cyrus check", BOLD));
        for c in &dx.components {
            let (word, code) = if c.ok { ("ok", GREEN) } else { ("down", RED) };
            let status = paint(&format!("{word:<4}"), code);
            println!("  {status}  {:<18}  {}", c.name, c.detail);
        }
        println!();
        if healthy {
            println!("{}", paint("healthy", GREEN));
        } else {
            println!(
                "{} — run `cyrus setup` to repair",
                paint("needs attention", YELLOW)
            );
        }
    }

    if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------------------
// setup — guided convergence
// ---------------------------------------------------------------------------

async fn run_setup(opts: SetupOptions, json: bool) -> ExitCode {
    if !json {
        print_intro(&opts);
    }

    // The printer tracks the most recently started step so a failure can name
    // the stage it happened in (the engine returns one flat error).
    let current: Arc<Mutex<Option<Step>>> = Arc::new(Mutex::new(None));
    let printer_step = current.clone();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SetupEvent>();
    let printer = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let SetupEvent::StepStarted { step } = &ev {
                *printer_step.lock().expect("step mutex") = Some(*step);
            }
            render_event(json, &ev);
        }
    });

    let result = ensure_all(&opts, &tx).await;
    drop(tx);
    let _ = printer.await;

    match result {
        Ok(out) => {
            render_success(json, &opts, &out);
            ExitCode::SUCCESS
        }
        Err(e) => {
            let failed = *current.lock().expect("step mutex");
            render_failure(json, &opts, failed, &e);
            ExitCode::FAILURE
        }
    }
}

/// The guided preamble (human mode only).
fn print_intro(opts: &SetupOptions) {
    println!("{}", paint("cyrus setup", BOLD));
    println!("  {}", opts.repo_root.display());
    println!();
    println!(
        "Wiring codex to your ChatGPT subscription — {} steps, each verified and repaired\n\
         as needed. The only thing it may ask of you is a one-time ChatGPT login.\n",
        Step::ALL.len()
    );
}

/// Render one progress event in the active mode.
fn render_event(json: bool, ev: &SetupEvent) {
    if json {
        let obj = match ev {
            SetupEvent::StepStarted { step } => serde_json::json!({
                "event": "step_started", "step": step_key(*step), "label": step.label()
            }),
            SetupEvent::StepDone { step, detail } => serde_json::json!({
                "event": "step_done", "step": step_key(*step), "detail": detail
            }),
            SetupEvent::NeedsUserAction { step, instruction } => serde_json::json!({
                "event": "needs_user_action", "step": step_key(*step), "instruction": instruction
            }),
            SetupEvent::UserActionResolved { step } => serde_json::json!({
                "event": "user_action_resolved", "step": step_key(*step)
            }),
        };
        emit_json(obj);
        return;
    }
    match ev {
        SetupEvent::StepStarted { step } => {
            let (present, _, hint) = step_verbs(*step);
            println!("{}", gutter_row(present, DIM, hint));
        }
        SetupEvent::StepDone { step, detail } => {
            let (_, past, _) = step_verbs(*step);
            println!("{}", gutter_row(past, GREEN, detail));
        }
        SetupEvent::NeedsUserAction { instruction, .. } => {
            let lines = wrap(instruction, 60);
            for (i, line) in lines.iter().enumerate() {
                if i == 0 {
                    println!("{}", gutter_row("Waiting", YELLOW, line));
                } else {
                    println!("{}{line}", indent());
                }
            }
        }
        // The following "…ed" done line confirms the resolution; no extra noise.
        SetupEvent::UserActionResolved { .. } => {}
    }
}

/// Final success summary (human) or the `done` event (JSON).
fn render_success(json: bool, opts: &SetupOptions, out: &SetupOutcome) {
    if json {
        emit_json(serde_json::json!({
            "event": "done",
            "public_url": out.public_url,
            "shim_base_url": out.shim_base_url,
            "connector_id": out.connector_id,
            "link_id": out.link_id,
            "tool_count": out.tool_count,
            "fully_reused": out.fully_reused,
        }));
        return;
    }
    println!();
    let msg = if out.fully_reused {
        "everything was already up — codex is wired to your ChatGPT subscription"
    } else {
        "codex is wired to your ChatGPT subscription"
    };
    println!("{}", gutter_row("Ready", GREEN, msg));
    println!();
    detail_row("tunnel", &out.public_url);
    detail_row("model", &format!("{}  (lipsync)", out.shim_base_url));
    detail_row(
        "connector",
        &format!("{} · {} tools", opts.connector_name, out.tool_count),
    );
    detail_row("config", "model_providers.shadow");
    println!();
    println!(
        "Select the \"shadow\" model provider in codex to use it. Run `cyrus check` to verify."
    );
}

/// A `label   value` line aligned under the gutter (success/summary blocks).
fn detail_row(label: &str, value: &str) {
    println!(
        "{}{} {value}",
        indent(),
        paint(&format!("{label:<10}"), DIM)
    );
}

/// Diagnose a failed run: name the stage, show the error, suggest a remedy, and
/// point at the relevant logs.
fn render_failure(json: bool, opts: &SetupOptions, failed: Option<Step>, e: &anyhow::Error) {
    if json {
        let mut obj = serde_json::json!({"event": "error", "message": format!("{e:#}")});
        if let Some(step) = failed {
            obj["step"] = serde_json::Value::String(step_key(step).to_string());
        }
        emit_json(obj);
        return;
    }

    let what = failed.map(|s| s.label()).unwrap_or("setup");
    eprintln!();
    eprintln!("{}", gutter_row("Failed", RED, what));
    eprintln!();
    for line in format!("{e:#}").lines() {
        eprintln!("{}{line}", indent());
    }
    if let Some(step) = failed {
        eprintln!();
        eprintln!("{}{} {}", indent(), paint(&format!("{:<10}", "try"), DIM), step.remedy());

        let logs = existing_logs(opts, step);
        if !logs.is_empty() {
            for (i, p) in logs.iter().enumerate() {
                let label = if i == 0 { "logs" } else { "" };
                eprintln!("{}{} {p}", indent(), paint(&format!("{label:<10}"), DIM));
            }
        }
    }
    eprintln!();
    eprintln!("Re-running `cyrus setup` is safe — it verifies and repairs, never duplicates.");
}

/// The log files that exist for a step (so we never point at a file that was
/// never written).
fn existing_logs(opts: &SetupOptions, step: Step) -> Vec<String> {
    let dir = opts.cyrus_home().join("logs");
    step.log_files()
        .iter()
        .map(|name| dir.join(name))
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect()
}

/// Wrap text to `width` columns on word boundaries (for the action box).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_on_word_boundaries_within_width() {
        let lines = wrap("the quick brown fox jumps", 10);
        assert!(lines.iter().all(|l| l.len() <= 10), "{lines:?}");
        // round-trips to the original words in order
        assert_eq!(lines.join(" "), "the quick brown fox jumps");
    }

    #[test]
    fn wrap_keeps_an_overlong_word_whole() {
        let lines = wrap("short supercalifragilistic x", 8);
        assert_eq!(lines[1], "supercalifragilistic");
    }

    #[test]
    fn wrap_empty_is_empty() {
        assert!(wrap("   ", 10).is_empty());
    }

    #[test]
    fn step_key_is_stable_for_every_step() {
        // The TUI contract keys on these exact strings — guard them.
        let keys: Vec<&str> = Step::ALL.iter().map(|s| step_key(*s)).collect();
        assert_eq!(
            keys,
            ["secrets", "chrome", "tunnel", "stack", "connector", "codex_config"]
        );
    }

    #[test]
    fn every_step_has_a_remedy_and_blurb() {
        for s in Step::ALL {
            assert!(!s.remedy().is_empty());
            assert!(!s.blurb().is_empty());
            assert!(!s.label().is_empty());
        }
    }

    #[test]
    fn render_helpers_do_not_panic() {
        // Smoke test: the human + JSON renderers run cleanly over representative
        // inputs (output goes to the test's captured stdout/stderr).
        let opts = SetupOptions::new("C:/tmp/repo");
        let out = SetupOutcome {
            public_url: "https://example.test".into(),
            shim_base_url: "http://127.0.0.1:8765/v1".into(),
            connector_id: "c".into(),
            link_id: "l".into(),
            tool_count: 34,
            fully_reused: false,
        };
        render_success(false, &opts, &out);
        render_success(true, &opts, &out);
        let err = anyhow::anyhow!("boom");
        render_failure(false, &opts, Some(Step::Stack), &err);
        render_failure(true, &opts, Some(Step::Stack), &err);
        render_failure(false, &opts, None, &err);
    }
}
