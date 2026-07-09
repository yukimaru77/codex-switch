#!/usr/bin/env bash
# Detect a new upstream (openai/codex) release tag, rebase the custom
# feature branch (env_switch + Monitor) onto it in an isolated worktree,
# build + test, and on any failure hand the worktree to an AI agent
# (claude -p by default) that studies the feature diff and re-ports it.
#
# Designed to be run unattended (launchd / cron). Never touches the main
# checkout: all rebase/repair work happens in a dedicated git worktree.
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="${AUTOREBASE_REPO_DIR:-$(cd "$SCRIPT_DIR/../.." && pwd)}"
STATE_DIR="${AUTOREBASE_STATE_DIR:-$REPO_DIR/.upstream-autorebase}"
STATE_FILE="$STATE_DIR/state.jsonl"
LOG_DIR="$STATE_DIR/logs"
LOCK_DIR="$STATE_DIR/lock"
WORKTREE_ROOT="${AUTOREBASE_WORKTREE_ROOT:-$(dirname "$REPO_DIR")/codex-autorebase-worktrees}"

BRANCH_PREFIX="${AUTOREBASE_BRANCH_PREFIX:-env-switch-monitor-v}"
TAG_PREFIX="${AUTOREBASE_TAG_PREFIX:-rust-v}"
UPSTREAM_REMOTE="${AUTOREBASE_UPSTREAM_REMOTE:-upstream}"
UPSTREAM_REPO="${AUTOREBASE_UPSTREAM_REPO:-openai/codex}"
PUSH_REMOTE="${AUTOREBASE_PUSH_REMOTE:-origin}"
PUSH_ENABLED="${AUTOREBASE_PUSH:-1}"
MAX_REPAIR_ATTEMPTS="${AUTOREBASE_MAX_REPAIR_ATTEMPTS:-3}"
MAX_RUNS_PER_VERSION="${AUTOREBASE_MAX_RUNS_PER_VERSION:-2}"
AGENT_TIMEOUT="${AUTOREBASE_AGENT_TIMEOUT_SECONDS:-10800}"
PROMPT_TEMPLATE="${AUTOREBASE_PROMPT_TEMPLATE:-$SCRIPT_DIR/repair-prompt-template.txt}"
AGENT_CMD="${AUTOREBASE_AGENT_CMD:-codex exec --dangerously-bypass-approvals-and-sandbox}"
FORCE="${AUTOREBASE_FORCE:-0}"

# Share one cargo target dir across versions so unattended runs don't
# rebuild the world (or fill the disk) for every worktree.
export CARGO_TARGET_DIR="${AUTOREBASE_CARGO_TARGET_DIR:-$STATE_DIR/target-cache}"
export RUST_MIN_STACK=8388608

DRY_RUN=0
case "${1:-}" in
    --dry-run|check) DRY_RUN=1 ;;
esac

mkdir -p "$STATE_DIR" "$LOG_DIR"
RUN_LOG="$LOG_DIR/run-$(date '+%Y%m%d-%H%M%S').log"
exec > >(tee -a "$RUN_LOG") 2>&1

log() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"; }

notify() {
    # Best-effort macOS notification; harmless elsewhere.
    osascript -e "display notification \"$2\" with title \"$1\"" >/dev/null 2>&1 || true
}

record() {
    # record <version> <status> [extra-json-object]
    _extra="${3:-}"
    [ -n "$_extra" ] || _extra='{}'
    jq -cn --arg ts "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
           --arg version "$1" --arg status "$2" --arg log "$RUN_LOG" \
           --argjson extra "$_extra" \
           '{ts:$ts,version:$version,status:$status,log:$log} + $extra' >> "$STATE_FILE"
}

die() {
    log "ERROR: $*"
    exit 1
}

# ---------------------------------------------------------------- lock
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
    log "Another run is in progress ($LOCK_DIR exists). Exiting."
    exit 0
fi
trap 'rmdir "$LOCK_DIR" 2>/dev/null' EXIT

cd "$REPO_DIR"

for cmd in git jq curl cargo; do
    command -v "$cmd" >/dev/null 2>&1 || die "required command not found: $cmd"
done

if ! git remote get-url "$UPSTREAM_REMOTE" >/dev/null 2>&1; then
    git remote add "$UPSTREAM_REMOTE" "https://github.com/${UPSTREAM_REPO}.git"
fi

# ------------------------------------------- detect latest upstream tag
log "Detecting latest release of ${UPSTREAM_REPO}..."
LATEST_TAG="$(curl -fsSL --max-time 30 \
    "https://api.github.com/repos/${UPSTREAM_REPO}/releases/latest" 2>/dev/null \
    | jq -r '.tag_name // empty' 2>/dev/null || true)"

if ! echo "$LATEST_TAG" | grep -qE "^${TAG_PREFIX}[0-9]+\.[0-9]+\.[0-9]+$"; then
    log "GitHub API unavailable or unexpected tag ('$LATEST_TAG'); falling back to git tags."
    LATEST_TAG="$(git ls-remote --tags "$UPSTREAM_REMOTE" "refs/tags/${TAG_PREFIX}*" \
        | awk -F/ '{print $NF}' \
        | grep -E "^${TAG_PREFIX}[0-9]+\.[0-9]+\.[0-9]+$" \
        | sort -V | tail -1)"
fi
[ -n "$LATEST_TAG" ] || die "could not determine latest upstream tag"
NEW_VERSION="${LATEST_TAG#$TAG_PREFIX}"
log "Latest upstream release: $LATEST_TAG"

# --------------------------------------- find our current ported version
# Branch suffixes may be full (0.144.0) or short (0.144, implying patch .0).
# Emit "<normalized> <verbatim>" pairs so comparisons/tags use X.Y.Z while
# the branch name is preserved as-is.
CUR_PICK="$(
    {
        git branch --list "${BRANCH_PREFIX}*" | sed 's/^[* ]*//'
        git ls-remote --heads "$PUSH_REMOTE" "refs/heads/${BRANCH_PREFIX}*" 2>/dev/null \
            | sed 's|.*refs/heads/||'
    } | sed "s/^${BRANCH_PREFIX}//" \
      | grep -E '^[0-9]+\.[0-9]+(\.[0-9]+)?$' \
      | awk -F. 'NF == 3 { print $0 " " $0 } NF == 2 { print $0 ".0 " $0 }' \
      | sort -V | tail -1
)"
[ -n "$CUR_PICK" ] || die "no existing ${BRANCH_PREFIX}* branch found"
CUR_VERSION="${CUR_PICK%% *}"
CUR_BRANCH_VERSION="${CUR_PICK##* }"
log "Current ported version: $CUR_VERSION (branch ${BRANCH_PREFIX}${CUR_BRANCH_VERSION})"

HIGHEST="$(printf '%s\n%s\n' "$CUR_VERSION" "$NEW_VERSION" | sort -V | tail -1)"
if [ "$NEW_VERSION" = "$CUR_VERSION" ] || [ "$HIGHEST" != "$NEW_VERSION" ]; then
    log "Up to date (ours: $CUR_VERSION, upstream: $NEW_VERSION). Nothing to do."
    exit 0
fi

OLD_TAG="${TAG_PREFIX}${CUR_VERSION}"
OLD_BRANCH="${BRANCH_PREFIX}${CUR_BRANCH_VERSION}"
NEW_BRANCH="${BRANCH_PREFIX}${NEW_VERSION}"
WT="$WORKTREE_ROOT/$NEW_BRANCH"

# ----------------------------------------------- give up after N failures
FAILED_RUNS=0
if [ -f "$STATE_FILE" ]; then
    FAILED_RUNS="$(jq -rs --arg v "$NEW_VERSION" \
        '[.[] | select(.version==$v and .status=="failed")] | length' \
        "$STATE_FILE" 2>/dev/null || echo 0)"
fi
if [ "$FORCE" != "1" ] && [ "$FAILED_RUNS" -ge "$MAX_RUNS_PER_VERSION" ]; then
    log "Version $NEW_VERSION already failed $FAILED_RUNS time(s) (max $MAX_RUNS_PER_VERSION)."
    log "Manual intervention needed. Worktree kept at: $WT"
    log "Re-run with AUTOREBASE_FORCE=1 to try again."
    notify "codex autorebase" "v$NEW_VERSION needs manual attention"
    exit 0
fi

if [ "$DRY_RUN" = "1" ]; then
    log "[dry-run] Would port $OLD_BRANCH -> $NEW_BRANCH (onto $LATEST_TAG) in $WT"
    exit 0
fi

log "=== Porting $OLD_BRANCH -> $NEW_BRANCH (onto $LATEST_TAG) ==="
record "$NEW_VERSION" "started" "{\"tag\":\"$LATEST_TAG\"}"
notify "codex autorebase" "Started port to $LATEST_TAG"

# --------------------------------------------------------- fetch refs
git rev-parse -q --verify "refs/tags/$LATEST_TAG" >/dev/null \
    || git fetch "$UPSTREAM_REMOTE" tag "$LATEST_TAG" --no-tags \
    || die "failed to fetch $LATEST_TAG"
git rev-parse -q --verify "refs/tags/$OLD_TAG" >/dev/null \
    || git fetch "$UPSTREAM_REMOTE" tag "$OLD_TAG" --no-tags \
    || die "failed to fetch $OLD_TAG"

if git rev-parse -q --verify "refs/heads/$OLD_BRANCH" >/dev/null; then
    OLD_REF="$OLD_BRANCH"
elif git rev-parse -q --verify "refs/remotes/$PUSH_REMOTE/$OLD_BRANCH" >/dev/null; then
    OLD_REF="$PUSH_REMOTE/$OLD_BRANCH"
else
    git fetch "$PUSH_REMOTE" "$OLD_BRANCH" || die "cannot find $OLD_BRANCH locally or on $PUSH_REMOTE"
    OLD_REF="$PUSH_REMOTE/$OLD_BRANCH"
fi

# ------------------------------- which crates do the custom commits touch?
TEST_PKGS="$(
    git diff --name-only "$OLD_TAG..$OLD_REF" -- codex-rs | while read -r f; do
        d="$(dirname "$f")"
        while [ "${d#codex-rs/}" != "$d" ]; do
            if [ -f "$REPO_DIR/$d/Cargo.toml" ]; then
                sed -n 's/^name *= *"\(.*\)".*/\1/p' "$REPO_DIR/$d/Cargo.toml" | head -1
                break
            fi
            d="$(dirname "$d")"
        done
    done | sort -u
)"
PKG_ARGS=""
for p in $TEST_PKGS; do
    PKG_ARGS="$PKG_ARGS -p $p"
done
[ -n "$PKG_ARGS" ] || PKG_ARGS="-p codex-core -p codex-tui -p codex-protocol"
log "Test scope:$PKG_ARGS"

# --------------------------------------------------------- worktree setup
mkdir -p "$WORKTREE_ROOT"
NEEDS_REBASE=1
if [ -d "$WT" ]; then
    log "Reusing existing worktree at $WT (previous run left it behind)."
    NEEDS_REBASE=0
elif git rev-parse -q --verify "refs/heads/$NEW_BRANCH" >/dev/null; then
    log "Branch $NEW_BRANCH already exists; attaching worktree."
    git worktree add "$WT" "$NEW_BRANCH" || die "worktree add failed"
    NEEDS_REBASE=0
else
    git worktree add "$WT" -b "$NEW_BRANCH" "$OLD_REF" || die "worktree add failed"
fi

# ------------------------------------------------------------- rebase
MODE="test-failure"
if [ "$NEEDS_REBASE" = "1" ]; then
    log "Rebasing custom commits ($OLD_TAG..$OLD_REF) onto $LATEST_TAG..."
    if git -C "$WT" rebase --onto "$LATEST_TAG" "$OLD_TAG" "$NEW_BRANCH" \
            > "$LOG_DIR/rebase-$NEW_VERSION.log" 2>&1; then
        log "Rebase succeeded."
        record "$NEW_VERSION" "rebased"
    else
        log "Rebase hit conflicts; resetting worktree to $LATEST_TAG for a full AI port."
        git -C "$WT" rebase --abort >/dev/null 2>&1 || true
        git -C "$WT" reset --hard "$LATEST_TAG"
        MODE="rebase-conflict"
        record "$NEW_VERSION" "rebase_conflict"
    fi
fi

# ----------------------------------------------------------- verification
verify() {
    # verify <logfile>; returns 0 when build + tests pass
    _vlog="$1"
    : > "$_vlog"
    log "Verify: cargo build --bin codex"
    if ! (cd "$WT/codex-rs" && cargo build --bin codex) >> "$_vlog" 2>&1; then
        log "Build FAILED (see $_vlog)"
        return 1
    fi
    if command -v cargo-nextest >/dev/null 2>&1 || cargo nextest --version >/dev/null 2>&1; then
        log "Verify: cargo nextest run$PKG_ARGS"
        if ! (cd "$WT/codex-rs" && NEXTEST_PROFILE=local \
                cargo nextest run --no-fail-fast $PKG_ARGS) >> "$_vlog" 2>&1; then
            log "Tests FAILED (see $_vlog)"
            return 1
        fi
    else
        log "Verify: cargo test$PKG_ARGS (nextest not installed)"
        if ! (cd "$WT/codex-rs" && cargo test $PKG_ARGS) >> "$_vlog" 2>&1; then
            log "Tests FAILED (see $_vlog)"
            return 1
        fi
    fi
    return 0
}

run_agent() {
    # run_agent <prompt-file> <agent-log>; portable timeout wrapper
    _prompt="$(cat "$1")"
    (cd "$WT" && $AGENT_CMD "$_prompt") > "$2" 2>&1 &
    _pid=$!
    _waited=0
    while kill -0 "$_pid" 2>/dev/null; do
        sleep 30
        _waited=$((_waited + 30))
        if [ "$_waited" -ge "$AGENT_TIMEOUT" ]; then
            log "Agent timed out after ${AGENT_TIMEOUT}s; killing."
            kill -TERM "$_pid" 2>/dev/null; sleep 10
            kill -KILL "$_pid" 2>/dev/null
            return 124
        fi
        if [ $((_waited % 300)) -eq 0 ]; then
            log "Agent still working... ($((_waited / 60))m)"
        fi
    done
    wait "$_pid"
}

VLOG="$LOG_DIR/verify-$NEW_VERSION.log"
ATTEMPT=0
if [ "$MODE" = "rebase-conflict" ]; then
    # Nothing to verify yet: the worktree is pristine upstream. Seed the
    # "failure" that the agent is asked to fix.
    echo "rebase of $OLD_TAG..$OLD_REF onto $LATEST_TAG failed with conflicts;" > "$VLOG"
    echo "worktree was reset to $LATEST_TAG. See $LOG_DIR/rebase-$NEW_VERSION.log" >> "$VLOG"
    VERIFIED=1   # 1 = not verified (shell-style false)
else
    verify "$VLOG"; VERIFIED=$?
fi

while [ "$VERIFIED" -ne 0 ]; do
    ATTEMPT=$((ATTEMPT + 1))
    if [ "$ATTEMPT" -gt "$MAX_REPAIR_ATTEMPTS" ]; then
        log "=== FAILED: still broken after $MAX_REPAIR_ATTEMPTS repair attempt(s). ==="
        log "Worktree kept for inspection: $WT"
        record "$NEW_VERSION" "failed" "{\"attempts\":$MAX_REPAIR_ATTEMPTS,\"mode\":\"$MODE\"}"
        notify "codex autorebase" "v$NEW_VERSION port FAILED — see $WT"
        exit 1
    fi

    command -v "${AGENT_CMD%% *}" >/dev/null 2>&1 \
        || die "repair agent '${AGENT_CMD%% *}' not found in PATH"

    case "$MODE" in
        rebase-conflict)
            MODE_TEXT="本家 $LATEST_TAG への rebase がコンフリクトで失敗したため、このワークツリーは $LATEST_TAG そのもの(追加機能なし)にリセット済みです。旧ブランチの差分コミットを一つずつ丁寧に移植してください(git cherry-pick と手作業の併用可)。"
            ;;
        *)
            MODE_TEXT="rebase 自体は成功し、追加機能のコミットは取り込まれていますが、ビルドまたはテストが失敗しています。失敗ログを読んで原因を特定し、修復してください。"
            ;;
    esac

    PROMPT_FILE="$STATE_DIR/prompt-$NEW_VERSION-attempt$ATTEMPT.txt"
    sed -e "s|__NEW_TAG__|$LATEST_TAG|g" \
        -e "s|__OLD_TAG__|$OLD_TAG|g" \
        -e "s|__OLD_REF__|$OLD_REF|g" \
        -e "s|__NEW_BRANCH__|$NEW_BRANCH|g" \
        -e "s|__MODE__|$MODE_TEXT|g" \
        -e "s|__FAIL_LOG__|$VLOG|g" \
        -e "s|__TEST_PKG_ARGS__|$PKG_ARGS|g" \
        -e "s|__ATTEMPT__|$ATTEMPT/$MAX_REPAIR_ATTEMPTS|g" \
        "$PROMPT_TEMPLATE" > "$PROMPT_FILE"

    AGENT_LOG="$LOG_DIR/agent-$NEW_VERSION-attempt$ATTEMPT.log"
    log "--- Repair attempt $ATTEMPT/$MAX_REPAIR_ATTEMPTS: launching agent ($AGENT_CMD ...) ---"
    record "$NEW_VERSION" "repair_started" "{\"attempt\":$ATTEMPT,\"mode\":\"$MODE\"}"
    run_agent "$PROMPT_FILE" "$AGENT_LOG"
    AGENT_RC=$?
    log "Agent finished (rc=$AGENT_RC). Log: $AGENT_LOG"

    # After the first agent pass the tree contains the (re)ported feature,
    # so subsequent failures are plain test failures.
    MODE="test-failure"
    verify "$VLOG"; VERIFIED=$?
done

log "Build and tests PASSED."

# ------------------------------------------------------------- finalize
if [ -n "$(git -C "$WT" status --porcelain)" ]; then
    log "Committing leftover uncommitted changes from the agent..."
    git -C "$WT" add -A
    git -C "$WT" commit -m "chore: autorebase leftovers for $NEW_VERSION" >/dev/null
fi

if [ "$ATTEMPT" -gt 0 ]; then
    record "$NEW_VERSION" "repaired" "{\"attempts\":$ATTEMPT}"
fi

if [ "$PUSH_ENABLED" = "1" ]; then
    # HTTPS pushes through an OAuth token without the `workflow` scope are
    # rejected whenever the rebase touches .github/workflows; fall back to
    # the SSH remote URL, which is not scope-restricted.
    PUSH_FALLBACK_URL="${AUTOREBASE_PUSH_FALLBACK_URL:-}"
    if [ -z "$PUSH_FALLBACK_URL" ]; then
        _origin_url="$(git remote get-url "$PUSH_REMOTE" 2>/dev/null || true)"
        case "$_origin_url" in
            https://github.com/*)
                PUSH_FALLBACK_URL="git@github.com:${_origin_url#https://github.com/}"
                ;;
        esac
    fi
    log "Pushing $NEW_BRANCH to $PUSH_REMOTE..."
    if git -C "$WT" push "$PUSH_REMOTE" "$NEW_BRANCH"; then
        record "$NEW_VERSION" "pushed" "{\"remote\":\"$PUSH_REMOTE\"}"
    elif [ -n "$PUSH_FALLBACK_URL" ] \
        && log "Push to $PUSH_REMOTE failed; retrying via $PUSH_FALLBACK_URL..." \
        && git -C "$WT" push "$PUSH_FALLBACK_URL" "$NEW_BRANCH"; then
        record "$NEW_VERSION" "pushed" "{\"remote\":\"$PUSH_FALLBACK_URL\"}"
    else
        log "WARNING: push failed; branch remains local."
        record "$NEW_VERSION" "push_failed"
    fi
else
    record "$NEW_VERSION" "succeeded_local"
fi

log "Removing worktree (branch $NEW_BRANCH is kept)..."
git worktree remove --force "$WT" 2>/dev/null || log "WARNING: could not remove worktree $WT"

log "=== SUCCESS: $NEW_BRANCH is ready ($LATEST_TAG + custom features) ==="
notify "codex autorebase" "v$NEW_VERSION port succeeded"
