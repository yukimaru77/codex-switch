# Local Monitor implementation

Based on the official `rust-v0.153.4` release (3d2ee51ca2).
The watcher design was adapted from yaanfpv/codex commit
ae7dbe6aecf15dd7c1747a8512acda004d38d6a5, linked in openai/codex#29922.

Enable the tool in `~/.codex/config.toml`:

```toml
[features]
monitor = true
```

The model can call:

```json
{"action":"start","command":"tail -F app.log | grep --line-buffered ERROR","description":"application errors"}
{"action":"list"}
{"action":"stop","id":"mon_<id returned by start>"}
```

The command uses the selected environment's shell, working directory, and
unified-exec sandbox/approval path. Start calls also run Bash PreToolUse hooks.
Stdout and stderr share the monitored output stream. The initial yield hands
off the same buffered output to avoid missing early bytes. Immediately exiting
commands are supported.

Notifications enter the session's pending-work queue. Active turns receive them
at safe model-call boundaries; idle sessions resume through the existing
scheduler. Quiet monitors generate no model calls. Normal exit includes the exit
status; explicit stop aborts delivery and terminates the process without waking
the agent. Session shutdown cancels all monitors.

Resource bounds: eight monitors per session, 80-byte labels, 8192-byte commands,
32 queued notifications, and under 900 bytes per context fragment. Output is
batched over 200 ms; truncation and queue overflow are visible. A 5000-line flood
stops the monitor. Use commands that emit meaningful events rather than verbose
progress streams.

This is a native core tool, not an MCP server or a watcher subagent. It applies
to new sessions using the patched CLI/app-server binary. Existing processes
retain their original executable and tool definitions. An npm update may replace
the patched executable. The separately bundled ChatGPT desktop binary is not
modified by this CLI installation.

Validation commands:

```sh
cd codex-rs
just test -p codex-core -E 'test(monitor) | test(input_queue) | test(unified_exec)' --retries 0
cargo build -p codex-cli --bin codex
just write-config-schema
just fix -p codex-core
just fmt
```

The focused regression run passed 185 tests, including idle/no-inference,
exactly-once completion, immediate exit, partial lines, stdout/stderr, stopping,
registry cleanup, bounded context, overflow, and read-only sandbox enforcement.

The full workspace run completed: 16,917 passed, 58 failed, 4 timed out,
44 skipped (16,979 executed). All 12 Monitor integration tests passed in that
run too. A low-concurrency follow-up of selected failing families ran 33 tests:
19 passed and 14 failed. The complete workspace suite is **not green**.

Observed failures outside the added Monitor tests include release-version
expectations (`0.0.0` versus `0.153.4`) in snapshots/MCP initialization, personal
Mac skills appearing in skill discovery fixtures, Apple Python cache warnings
inside Seatbelt, startup/HTTP timing, and a V8 sandbox-feature assertion for
the official prebuilt V8 archive. Not every remaining failure has been isolated
against an unmodified upstream build; these are not claimed to be proven
pre-existing failures. No unrelated snapshots or feature behavior were changed
to make the suite pass.

Full-workspace tests used the checksummed OpenAI V8 artifacts resolved by
`scripts/codex_package/v8.py`, since the default denoland archive URL returned
404. The CLI build itself does not require that V8 override.

## Installed on this Mac

The npm CLI native executable was replaced atomically, leaving running sessions
intact. The signed executable reports `codex-cli 0.153.4`; `codex features list`
reports `monitor` as enabled. `~/.codex/config.toml` has `features.monitor = true`.

Source: `/Users/nonaka/tasks/codex-monitor-v0.153.4`, branch `monitor-v0.153.4`.
Backups: `/Users/nonaka/.local/share/codex-monitor/v0.153.4/codex.stock` and
`config.toml.before-monitor` in the same directory. The installed build is also
retained there as `codex.monitor` (a stripped, ad-hoc-signed development build).
Validation logs are in the `validation/` subdirectory and generated, unaccepted
snapshot results are in `test-snapshots/`. Generated Cargo build artifacts were
removed after validation to recover disk space; rebuild from source when needed.

To revert the CLI, copy `codex.stock` to a new sibling file in
`/opt/homebrew/lib/node_modules/@openai/codex/node_modules/@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/`,
then atomically rename that copy to `codex`. Remove only `monitor = true` from
the config, preserving subsequent configuration changes. Do not overwrite the
whole configuration from the backup unless you intend to discard later edits.
