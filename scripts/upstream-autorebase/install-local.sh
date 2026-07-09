#!/usr/bin/env bash
# Build and install this fork's codex plus its required sibling binaries
# using the local convention:
#   ~/.local/share/codex-binaries/<name>/     (versioned real binaries)
#   /opt/homebrew/bin/<binary>                (symlinks; dir configurable)
#
# codex resolves helper binaries (codex-code-mode-host since v0.143) next to
# its own executable path, so every binary listed here must be installed and
# symlinked together — installing codex alone breaks shell execution.
#
# Usage: install-local.sh [repo-dir]
# Env:
#   CODEX_INSTALL_LINK_DIR   symlink dir (default /opt/homebrew/bin)
#   CODEX_INSTALL_BASE       versioned-dir base (default ~/.local/share/codex-binaries)
#   CARGO_TARGET_DIR         honored if set (autorebase shares its cache)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="${1:-$(cd "$SCRIPT_DIR/../.." && pwd)}"
LINK_DIR="${CODEX_INSTALL_LINK_DIR:-/opt/homebrew/bin}"
BASE_DIR="${CODEX_INSTALL_BASE:-$HOME/.local/share/codex-binaries}"
BINARIES="codex codex-code-mode-host"

log() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"; }

cd "$REPO_DIR/codex-rs"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_DIR/codex-rs/target}"

BIN_FLAGS=""
for b in $BINARIES; do
    BIN_FLAGS="$BIN_FLAGS --bin $b"
done
log "Building release binaries: $BINARIES"
# shellcheck disable=SC2086
cargo build --release $BIN_FLAGS

VERSION="$("$TARGET_DIR/release/codex" --version | awk '{print $2}')"
[ -n "$VERSION" ] || { echo "ERROR: could not determine codex version" >&2; exit 1; }
DEST="$BASE_DIR/additional-features-${VERSION}-release-$(date '+%Y%m%d-%H%M%S')"
mkdir -p "$DEST" "$LINK_DIR"

for b in $BINARIES; do
    cp "$TARGET_DIR/release/$b" "$DEST/$b"
    ln -sfn "$DEST/$b" "$LINK_DIR/$b"
done

log "Installed codex $VERSION"
log "  binaries: $DEST"
log "  symlinks: $LINK_DIR ($BINARIES)"
log "Rollback: repoint the symlinks at a previous dir under $BASE_DIR"
