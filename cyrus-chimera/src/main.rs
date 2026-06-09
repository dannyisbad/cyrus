//! cyrus-chimera standalone binary — a thin shim over [`cyrus_chimera::cli`].
//!
//! The full entry logic (argv/flag parsing, signal handlers, transport
//! dispatch) lives in the library's `cli` module so the single `cyrus` busybox
//! binary can run the same server as `cyrus chimera ...`. This binary remains
//! for development and for anyone who wants to run chimera directly.

#[tokio::main]
async fn main() {
    if let Err(error) = cyrus_chimera::cli::run_cli().await {
        // index.ts: `.catch((e) => { console.error(e); process.exit(1); })`.
        eprintln!("{error:?}");
        std::process::exit(1);
    }
}
