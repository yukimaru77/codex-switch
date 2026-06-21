# Codex env_switch

このプロジェクトは Codex に `env_switch` ツールを持たせるための fork です。

`env_switch` は、人間が `ssh hoge` や `docker exec -it hoge bash` で別環境のシェルに入る体験を、Codex の組み込みツールにも与えるものです。

Codex にはファイル編集、画像読み取り、検索などの便利な組み込みツールがあります。しかし通常、それらはローカル環境にしか使えません。たとえば SSH 先のファイルを書き換える場合、Codex は `ssh host "..."` のような shell コマンドを書く必要があります。

この方法には次の問題があります。

- 毎回 `ssh` や `docker exec` を書くのでトークン効率が悪い
- shell の出力が曖昧になりやすい
- ファイル編集や画像読み取りなど、Codex の組み込みツールの強みを活かしにくい

`env_switch` は、Codex のツール実行先を SSH 先、Docker コンテナ内、さらにそのネスト環境へ切り替えられるようにします。

## Demo

![env_switch demo](docs/assets/env-switch-demo.gif)

## Build

```shell
cd codex-rs
cargo build -p codex-cli --bin codex
```

ビルドした Codex を起動するには:

```shell
./target/debug/codex --yolo
```

## Supported tools

`env_switch` changes the execution environment for these Codex tools:

- shell execution: `exec_command`, `write_stdin`
- file changes: `apply_patch`
- image reading: `view_image`
