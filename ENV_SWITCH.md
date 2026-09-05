# Env switch with native Monitor

This local `rust-v0.153.4` build restores env switch from the committed
`env-switch-monitor-v0.153.0` implementation (`efd2ea6b95`). The original dirty
worktree was not modified. Its older Monitor implementation was not imported;
the native bounded Monitor described in `MONITOR.md` remains in use.

The tools `env_switch`, `env_status`, and `env_list` register and select a
thread-local default execution target without restarting the conversation.
Compatible command, patch, image, and Monitor calls use that target. Command,
patch, and image tools also accept explicit environment overrides. Existing
monitor processes stay on the environment where they started.

```json
{"target":"docker","container":"my-container","cwd":"/workspace"}
{"target":"ssh","host":"my-ssh-alias","cwd":"~/project"}
{"hops":[{"type":"ssh","host":"my-ssh-alias"},{"type":"docker","container":"worker"}],"cwd":"/workspace"}
{"target":"local"}
```

Remote provisioning uses a managed `~/.codex-server/env-switch` location and
matches the host Codex release. It does not replace arbitrary remote Codex
installations found on PATH. A local switch restores host execution. The TUI
badge reflects the selected SSH host/container, including after thread replay.
The transient environment cursor is not persisted across process restarts.

Both features must remain enabled in `~/.codex/config.toml`:

```toml
[features]
env_switch = true
monitor = true
```

The port preserves read-only remote policy rather than widening it to workspace
write access. Environment status output is capped at 8,000 bytes, with an
explicit truncation marker for unusually large registries.

## Verification

The scoped regression run passed all 377 tests, covering provisioning/quoting,
thread-scoped routing and metadata, local fallback, environment status, badge
updates/replay, native Monitor, unified exec, and read-only policy retention.

An opt-in real Docker integration test is provided at
`codex-rs/core/tests/suite/env_switch_docker.rs`. It requires
`CODEX_ENV_SWITCH_TEST_CONTAINER` to name a disposable, unmounted Linux
container. It checks remote provisioning, patch and command routing, quiet
Monitor waiting, remote completion delivery, and return to host execution.
It passed on this Mac against a disposable Linux arm64 Docker container in
6.8 seconds; the test container was removed afterward.

Build/test commands use `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`, and
`CARGO_PROFILE_TEST_DEBUG=0` to avoid large debug/incremental artifacts.

The upstream `just write-app-server-schema` recipe refers to a removed binary
in this release. Its replacement is the existing ignored test
`write_schema_fixtures_from_env` in `codex-app-server-protocol`, with
`CODEX_APP_SERVER_SCHEMA_ROOT` set to the absolute schema directory and
`CODEX_APP_SERVER_SCHEMA_EXPERIMENTAL` set to `0`/`1` for stable/experimental
exports. Generate with the protocol package alone to preserve canonical key
ordering rather than workspace-unified `serde_json/preserve_order` output.

All eight schema/export consistency tests passed. Scoped Clippy, formatting,
configuration schema generation, Bazel lock update, and the final CLI build
completed successfully. The full workspace suite was not rerun for this port;
the earlier full-suite failures remain documented in `MONITOR.md`.

## Local installation

The normal npm-backed `codex` executable now includes both features. New CLI
sessions expose `env_switch`, `env_status`, `env_list`, and `monitor`. Existing
processes retain their old tool definitions. The ChatGPT desktop app's separate
bundled executable is unchanged, and npm upgrades can overwrite this patch.

Saved binary: `~/.local/share/codex-monitor/v0.153.4/codex.env-switch-monitor`.
The preceding Monitor-only binary remains in the same directory as
`codex.monitor`, and the original official binary as `codex.stock`. Restore a
saved binary using the atomic copy-and-rename procedure in `MONITOR.md`.
Logs for this port are saved under `validation/env-switch/` in that directory.

The low-debug build artifacts are retained for future incremental work; the
large Rust incremental-compilation cache is disabled for these builds.
