#!/bin/sh
# cyrus installer.
#
#   curl -fsSL https://mundy.sh/install/cyrus | sh
#
# Detects your OS/arch, downloads the matching self-contained `cyrus` binary from
# the latest GitHub release, drops it in ~/.cyrus/bin, and puts that on your PATH.
# One file, no build, no toolchain. Re-run any time to update.
#
# Env knobs:
#   CYRUS_INSTALL_DIR   where to put the binary   (default: $HOME/.cyrus/bin)
#   CYRUS_VERSION       a specific tag, e.g. v0.1.0 (default: latest)
#   CYRUS_REPO          owner/repo                (default: dannyisbad/cyrus)
set -eu

REPO="${CYRUS_REPO:-dannyisbad/cyrus}"
INSTALL_DIR="${CYRUS_INSTALL_DIR:-${HOME}/.cyrus/bin}"
VERSION="${CYRUS_VERSION:-latest}"

# --- pretty output (only colorize a real terminal) --------------------------
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m'); RED=$(printf '\033[1;31m')
    GREEN=$(printf '\033[1;32m'); RESET=$(printf '\033[0m')
else
    BOLD=''; DIM=''; RED=''; GREEN=''; RESET=''
fi
say()  { printf '%s\n' "$*"; }
info() { printf '  %s%s%s\n' "$DIM" "$*" "$RESET"; }
ok()   { printf '%s%s%s %s\n' "$GREEN" "ok" "$RESET" "$*"; }
err()  { printf '%scyrus install:%s %s\n' "$RED" "$RESET" "$*" >&2; }
die()  { err "$*"; exit 1; }

# --- detect platform --------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux)  vendor_os="unknown-linux-gnu" ;;
    Darwin) vendor_os="apple-darwin" ;;
    *)      die "unsupported OS '$os'. On Windows use the PowerShell installer:
       irm https://mundy.sh/install/cyrus.ps1 | iex" ;;
esac
case "$arch" in
    x86_64 | amd64)        cpu="x86_64" ;;
    aarch64 | arm64)       cpu="aarch64" ;;
    *) die "unsupported CPU '$arch'. Build from source: https://github.com/$REPO" ;;
esac
TARGET="${cpu}-${vendor_os}"
ASSET="cyrus-${TARGET}.tar.gz"

# --- need curl-or-wget and tar ----------------------------------------------
if command -v curl >/dev/null 2>&1; then
    dl() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    dl() { wget -qO "$2" "$1"; }
else
    die "need curl or wget to download."
fi
command -v tar >/dev/null 2>&1 || die "need tar to unpack."

# --- resolve the download URL -----------------------------------------------
if [ "$VERSION" = "latest" ]; then
    URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
else
    URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
fi

say "${BOLD}Installing cyrus${RESET}"
info "$TARGET · $VERSION"
say ""

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

info "downloading $ASSET"
dl "$URL" "$tmp/$ASSET" || die "download failed: $URL
       (no release asset for $TARGET yet? see https://github.com/$REPO/releases)"

info "unpacking"
tar -xzf "$tmp/$ASSET" -C "$tmp" || die "could not unpack $ASSET (corrupt download?)"
[ -f "$tmp/cyrus" ] || die "archive did not contain a 'cyrus' binary."

mkdir -p "$INSTALL_DIR"
mv -f "$tmp/cyrus" "$INSTALL_DIR/cyrus"
chmod +x "$INSTALL_DIR/cyrus"
# macOS quarantines downloaded binaries; clear it so Gatekeeper doesn't block the
# first run of this unsigned build (best-effort — harmless if xattr is absent).
[ "$os" = "Darwin" ] && xattr -d com.apple.quarantine "$INSTALL_DIR/cyrus" 2>/dev/null || true

ok "cyrus -> $INSTALL_DIR/cyrus"
say ""

# --- PATH ------------------------------------------------------------------
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*)
        on_path=1 ;;
    *)
        on_path=0 ;;
esac

if [ "$on_path" -eq 1 ]; then
    say "${BOLD}Done.${RESET} Next: ${BOLD}cyrus setup${RESET}  (then just run ${BOLD}cyrus${RESET})"
else
    # Add INSTALL_DIR to PATH in the user's shell profile, guarded against dupes.
    line="export PATH=\"${INSTALL_DIR}:\$PATH\""
    profile=""
    case "${SHELL:-}" in
        */zsh)  profile="${ZDOTDIR:-$HOME}/.zshrc" ;;
        */bash) [ "$os" = "Darwin" ] && profile="$HOME/.bash_profile" || profile="$HOME/.bashrc" ;;
        */fish) profile="$HOME/.config/fish/config.fish"
                line="set -gx PATH ${INSTALL_DIR} \$PATH" ;;
        *)      profile="$HOME/.profile" ;;
    esac
    if [ -n "$profile" ] && ! { [ -f "$profile" ] && grep -qF "$INSTALL_DIR" "$profile"; }; then
        mkdir -p "$(dirname "$profile")"
        printf '\n# added by cyrus installer\n%s\n' "$line" >> "$profile"
        info "added $INSTALL_DIR to your PATH in $profile"
    fi
    say "${BOLD}Done.${RESET} Open a new terminal (or: ${BOLD}export PATH=\"${INSTALL_DIR}:\$PATH\"${RESET}), then:"
    say "  ${BOLD}cyrus setup${RESET}   ${DIM}# one-time: connect your ChatGPT session${RESET}"
    say "  ${BOLD}cyrus${RESET}         ${DIM}# codex on the plan you already pay for${RESET}"
fi
