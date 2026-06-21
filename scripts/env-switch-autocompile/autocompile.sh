#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="${CODEX_REPO_DIR:-$(cd "$SCRIPT_DIR/../.." && pwd)}"
TEMPLATE="${ENV_SWITCH_PROMPT_TEMPLATE:-$SCRIPT_DIR/prompt-template.txt}"
PROMPT_DIR="${ENV_SWITCH_PROMPT_DIR:-$REPO_DIR/.env-switch-autocompile/prompts}"
STATE_FILE="${ENV_SWITCH_STATE_FILE:-$REPO_DIR/.env-switch-autocompile/state.jsonl}"
PUSH_REMOTE="${ENV_SWITCH_PUSH_REMOTE:-git@github.com:yukimaru77/codex-switch.git}"
TARGET_REMOTE="${ENV_SWITCH_TARGET_REMOTE:-upstream}"
BASE_REMOTE="${ENV_SWITCH_BASE_REMOTE:-origin}"
TIMEOUT="${ENV_SWITCH_TIMEOUT_SECONDS:-5400}"
INTERVAL="${ENV_SWITCH_POLL_INTERVAL_SECONDS:-30}"

mkdir -p "$PROMPT_DIR" "$(dirname "$STATE_FILE")"

log() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"; }

cd "$REPO_DIR"

if ! command -v cmux >/dev/null 2>&1; then
    log "ERROR: cmux is required"
    exit 1
fi

if ! command -v codex >/dev/null 2>&1; then
    log "ERROR: codex is required"
    exit 1
fi

if ! git remote get-url "$TARGET_REMOTE" >/dev/null 2>&1; then
    git remote add "$TARGET_REMOTE" https://github.com/openai/codex.git
fi

log "Fetching $TARGET_REMOTE tags..."
git fetch "$TARGET_REMOTE" --tags 2>&1 || true

log "Finding highest existing env-switch version..."
EXISTING_VERSIONS=$(
    {
        git branch --list 'env-switch-v0.*' | sed 's/^[* ]*//' | sed 's/^env-switch-v//'
        git ls-remote --heads "$BASE_REMOTE" 'refs/heads/env-switch-v0.*' 2>/dev/null | sed 's|.*refs/heads/env-switch-v||'
        git ls-remote --heads "$PUSH_REMOTE" 'refs/heads/env-switch-v0.*' 2>/dev/null | sed 's|.*refs/heads/env-switch-v||'
    } | sort -t. -k2 -n | tail -1
)

if [ -z "$EXISTING_VERSIONS" ]; then
    log "ERROR: No existing env-switch branches found. Need at least one to determine previous version."
    exit 1
fi

LATEST_EXISTING="$EXISTING_VERSIONS"
LATEST_MINOR=$(echo "$LATEST_EXISTING" | cut -d. -f2)
log "Highest existing env-switch version: $LATEST_EXISTING (minor=$LATEST_MINOR)"

log "Checking for new stable tags..."
NEW_TAGS=()
for tag in $(git tag | grep -E '^rust-v0\.[0-9]+\.[0-9]+$' | sort -V); do
    tag_minor=$(echo "$tag" | sed 's/^rust-v0\.//' | cut -d. -f1)
    if [ "$tag_minor" -le "$LATEST_MINOR" ]; then
        continue
    fi

    full_version=$(echo "$tag" | sed 's/^rust-v//')
    if echo "$full_version" | grep -qE '\.0$'; then
        short_version=$(echo "$full_version" | sed 's/\.0$//')
    else
        short_version="$full_version"
    fi
    branch="env-switch-v${short_version}"

    if git rev-parse --verify "$branch" >/dev/null 2>&1; then
        continue
    fi

    NEW_TAGS+=("$tag|$short_version|$branch")
done

if [ ${#NEW_TAGS[@]} -eq 0 ]; then
    log "No new versions to process."
    exit 0
fi

log "Found ${#NEW_TAGS[@]} new version(s)"
for entry in "${NEW_TAGS[@]}"; do
    IFS='|' read -r tag sv br <<< "$entry"
    log "  -> $tag ($br)"
done

IFS='|' read -r TAG SHORT_VERSION BRANCH <<< "${NEW_TAGS[0]}"
PREV_VERSION="$LATEST_EXISTING"

log "=== Processing $TAG -> $BRANCH (prev: $PREV_VERSION) ==="

log "Creating branch $BRANCH from $TAG..."
git checkout -b "$BRANCH" "$TAG"

PROMPT_FILE="${PROMPT_DIR}/prompt-v${SHORT_VERSION}.txt"
sed -e "s/__VERSION__/${SHORT_VERSION}/g" \
    -e "s/__PREV_VERSION__/${PREV_VERSION}/g" \
    "$TEMPLATE" > "$PROMPT_FILE"
log "Prompt written to $PROMPT_FILE"

log "Launching codex in new cmux pane..."
PANE_OUTPUT=$(cmux new-pane --direction right 2>&1)
SURFACE=$(echo "$PANE_OUTPUT" | grep -oE 'surface:[0-9]+')
log "Created pane: $SURFACE"

cmux send-panel --panel "$SURFACE" "cd $REPO_DIR && git checkout $BRANCH && codex --yolo"
sleep 3
cmux send-key-panel --panel "$SURFACE" Enter

log "Waiting for codex to start..."
for _ in $(seq 1 12); do
    sleep 5
    PANE_CONTENT=$(cmux capture-pane --surface "$SURFACE" --lines 30 2>/dev/null || true)
    if echo "$PANE_CONTENT" | grep -qi "trust"; then
        cmux send-key-panel --panel "$SURFACE" Enter
        sleep 5
        break
    fi
    if echo "$PANE_CONTENT" | grep -q "OpenAI Codex"; then
        break
    fi
done

sleep 3
PANE_CONTENT=$(cmux capture-pane --surface "$SURFACE" --lines 10 2>/dev/null || true)
if ! echo "$PANE_CONTENT" | grep -qi "codex"; then
    log "ERROR: Codex did not start properly"
    echo "{\"ts\":\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\",\"version\":\"$SHORT_VERSION\",\"branch\":\"$BRANCH\",\"status\":\"launch_failed\"}" >> "$STATE_FILE"
    exit 1
fi

log "Sending migration prompt..."
PROMPT_CONTENT=$(cat "$PROMPT_FILE")
cmux send-panel --panel "$SURFACE" "$PROMPT_CONTENT"
sleep 2
cmux send-key-panel --panel "$SURFACE" Enter

echo "{\"ts\":\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\",\"version\":\"$SHORT_VERSION\",\"tag\":\"$TAG\",\"branch\":\"$BRANCH\",\"surface\":\"$SURFACE\",\"status\":\"started\"}" >> "$STATE_FILE"

log "Monitoring codex for completion..."
ELAPSED=0
SUCCESS=false

while [ "$ELAPSED" -lt "$TIMEOUT" ]; do
    sleep "$INTERVAL"
    ELAPSED=$((ELAPSED + INTERVAL))

    PANE_CONTENT=$(cmux capture-pane --surface "$SURFACE" --lines 50 --scrollback 2>/dev/null || true)

    if echo "$PANE_CONTENT" | grep -q "Goal achieved"; then
        log "Goal achieved! Migration successful."
        SUCCESS=true
        break
    fi

    MINS=$((ELAPSED / 60))
    log "Still running... (${MINS}m elapsed)"
done

if $SUCCESS; then
    log "Pushing $BRANCH to $PUSH_REMOTE..."
    git checkout "$BRANCH"
    git push "$PUSH_REMOTE" "$BRANCH"
    echo "{\"ts\":\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\",\"version\":\"$SHORT_VERSION\",\"branch\":\"$BRANCH\",\"status\":\"pushed\",\"remote\":\"$PUSH_REMOTE\"}" >> "$STATE_FILE"
    log "=== $BRANCH completed and pushed ==="
else
    log "=== $BRANCH timed out or failed ==="
    echo "{\"ts\":\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\",\"version\":\"$SHORT_VERSION\",\"branch\":\"$BRANCH\",\"status\":\"failed\"}" >> "$STATE_FILE"
    exit 1
fi

log "All done."
