#!/usr/bin/env bash
# Install (or remove) a macOS launchd agent that runs autorebase.sh
# periodically. Usage:
#   ./install-scheduler.sh              # install / update (default: every 6h)
#   ./install-scheduler.sh --uninstall  # remove
# Env:
#   AUTOREBASE_INTERVAL_SECONDS  polling interval (default 21600 = 6h)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
LABEL="com.codex-switch.upstream-autorebase"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
INTERVAL="${AUTOREBASE_INTERVAL_SECONDS:-21600}"
LOG_DIR="$REPO_DIR/.upstream-autorebase/logs"

if [ "${1:-}" = "--uninstall" ]; then
    launchctl bootout "gui/$(id -u)" "$PLIST" 2>/dev/null || true
    rm -f "$PLIST"
    echo "Uninstalled $LABEL"
    exit 0
fi

# launchd starts jobs with a minimal PATH; bake in the dirs where the
# tools live right now (claude, cargo, just, jq, git, ...).
PATH_DIRS="/usr/bin:/bin:/usr/sbin:/sbin"
for tool in claude cargo just git jq curl codex; do
    p="$(command -v "$tool" 2>/dev/null || true)"
    if [ -n "$p" ]; then
        d="$(dirname "$p")"
        case ":$PATH_DIRS:" in
            *":$d:"*) ;;
            *) PATH_DIRS="$d:$PATH_DIRS" ;;
        esac
    fi
done

mkdir -p "$HOME/Library/LaunchAgents" "$LOG_DIR"

cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/bash</string>
        <string>$SCRIPT_DIR/autorebase.sh</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$PATH_DIRS</string>
    </dict>
    <key>StartInterval</key>
    <integer>$INTERVAL</integer>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>$LOG_DIR/launchd.log</string>
    <key>StandardErrorPath</key>
    <string>$LOG_DIR/launchd.log</string>
</dict>
</plist>
EOF

launchctl bootout "gui/$(id -u)" "$PLIST" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "Installed $LABEL (every $((INTERVAL / 3600))h). Logs: $LOG_DIR"
echo "Run now:      launchctl kickstart gui/$(id -u)/$LABEL"
echo "Check status: launchctl print gui/$(id -u)/$LABEL | head -20"
