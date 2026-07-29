# Codex env_switch + Monitor

This project is a fork of Codex that adds two built-in features:

1. **env_switch** — Switch tool execution targets to SSH hosts, Docker containers, or nested environments
2. **Monitor** — Observe background processes with real-time event delivery (Claude Code compatible)

## env_switch

`env_switch` lets Codex switch the execution target of its tools to an SSH host, a Docker container, or a nested environment.

`env_switch` changes the execution environment for these Codex tools:

- shell execution: `exec_command`, `write_stdin`
- file changes: `apply_patch`
- image reading: `view_image`

## Monitor

`Monitor` gives Codex the ability to observe long-running background processes and receive their output as real-time events — even while the agent is idle or executing other tools.

- **Idle wake**: When Codex is idle, a monitor event automatically starts a new turn
- **Active turn attach**: During tool execution, monitor events are injected into the next model request alongside tool results
- **Real-time TUI notifications**: Monitor output is displayed immediately, even during long-running commands
- **Persistent processes**: Monitor processes survive across turns (session-scoped)

### Monitor tools

- `monitor_start` — Launch a background process and stream its stdout as events
- `monitor_stop` — Stop a running monitor by name
- `monitor_list` — List all running monitors

### TUI slash commands

- `/monitor start <name> <command>` — Start a monitor from the TUI
- `/monitor stop <name>` — Stop a monitor
- `/monitor emit <text>` — Manually inject a monitor event

### agmsg integration

With Monitor, Codex can use [agmsg](https://github.com/fujibee/agmsg) in `monitor` delivery mode for real-time cross-agent messaging with idle wake support.

## Build

```shell
cd codex-rs
cargo build -p codex-cli --bin codex
```

Run the built Codex binary with:

```shell
./target/debug/codex --yolo
```

## Bug reports

https://github.com/yukimaru77/codex-switch/issues
