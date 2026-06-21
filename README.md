# Codex env_switch

This project is a fork of Codex that adds a built-in `env_switch` tool.

`env_switch` gives Codex's built-in tools the same kind of experience a human gets by entering another environment with commands like `ssh host` or `docker exec -it container bash`.

Codex has useful built-in tools for shell execution, file editing, image reading, and more. However, those tools normally operate only on the local environment. For example, editing a file on an SSH host usually requires Codex to write shell commands such as `ssh host "..."`.

That approach has several problems:

- It wastes tokens because every command has to be wrapped in `ssh` or `docker exec`
- Shell output can become ambiguous or unstable
- It makes it harder to use Codex's built-in tools for file editing, image reading, and similar tasks

`env_switch` lets Codex switch the execution target of its tools to an SSH host, a Docker container, or a nested environment.

`env_switch` changes the execution environment for these Codex tools:

- shell execution: `exec_command`, `write_stdin`
- file changes: `apply_patch`
- image reading: `view_image`

## Demo

![env_switch demo](docs/assets/env-switch-demo.gif)

## Build

```shell
cd codex-rs
cargo build -p codex-cli --bin codex
```

Run the built Codex binary with:

```shell
./target/debug/codex --yolo
```
