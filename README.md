# Codex Monitor

This project is a fork of Codex that adds a built-in `Monitor` feature, inspired by Claude Code's Monitor tool.

`Monitor` gives Codex the ability to observe long-running background processes and receive their output as real-time events — even while the agent is idle or executing other tools.

Codex can run shell commands, but it normally blocks until they complete. For background observation tasks like test watchers, build systems, log tails, or inter-agent messaging (agmsg), you need a way to receive output asynchronously.

That's what `Monitor` provides:

- **Idle wake**: When Codex is idle, a monitor event automatically starts a new turn
- **Active turn attach**: During tool execution, monitor events are injected into the next model request alongside tool results
- **Real-time TUI notifications**: Monitor output is displayed immediately, even during long-running commands
- **Persistent processes**: Monitor processes survive across turns (session-scoped, not turn-scoped)

## Monitor tools

The following tools are available to the model:

- `monitor_start` — Launch a background process and stream its stdout as events
- `monitor_stop` — Stop a running monitor by name
- `monitor_list` — List all running monitors

## TUI slash commands

- `/monitor start <name> <command>` — Start a monitor from the TUI
- `/monitor stop <name>` — Stop a monitor
- `/monitor status` — List running monitors
- `/monitor emit <text>` — Manually inject a monitor event (for testing)

## agmsg integration

With the monitor feature, Codex can use [agmsg](https://github.com/fujibee/agmsg) in `monitor` delivery mode — the same mode Claude Code uses. This enables real-time cross-agent messaging with idle wake support.

```
/monitor start agmsg /tmp/agmsg-watch-poll.sh <team> <agent>
```

## Delivery semantics

| Agent state | Monitor event behavior |
|-------------|----------------------|
| Idle | New turn auto-started (idle wake) |
| Tool executing | Event queued, delivered in next model request after tool completes |
| Text generating | Event queued, delivered in next model request |
| AttachOnly policy | Event queued, no idle wake |

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
