# upstream-autorebase

本家 [openai/codex](https://github.com/openai/codex) の **latest リリースタグを自動検知**し、追加機能ブランチ(`env-switch-monitor-v*`)を新タグへ **rebase → build → test** まで無人で行う仕組み。rebase のコンフリクトやテスト失敗が起きた場合は、AI エージェント(デフォルト: `codex exec`)が **旧ブランチと旧タグの差分を精査し、新 latest から生やしたブランチ上で移植・改良** する。

## 仕組み

```
launchd (6時間ごと)
  └─ autorebase.sh
       1. GitHub API で openai/codex の latest リリースタグを取得
          (API 不通時は git ls-remote の安定版タグ rust-vX.Y.Z にフォールバック)
       2. 手元の最新 env-switch-monitor-v* ブランチのバージョンと比較。新しくなければ終了
       3. 専用 worktree を作成(メインの checkout には一切触らない)
          ../codex-autorebase-worktrees/env-switch-monitor-v<NEW>
       4. git rebase --onto rust-v<NEW> rust-v<OLD> で追加機能コミットを載せ替え
       5. cargo build --bin codex + app-server 統合テスト用 code-mode host +
          MCP stdio helper の事前ビルド + cargo nextest run
          (追加機能が触っているクレートを diff から自動算出してスコープ)
       6. 失敗した場合:
          - rebase コンフリクト → worktree を rust-v<NEW> にリセットし、
            旧差分 (rust-v<OLD>..env-switch-monitor-v<OLD>) を提示して
            AI エージェントに一から移植させる
          - テスト失敗 → rebase 結果を保持したまま、失敗ログを提示して修復させる
          エージェント実行後に再検証。最大 3 回まで繰り返す
       7. 成功 → origin へ push(HTTPS が workflow スコープで拒否されたら SSH URL に
          フォールバック)、install-local.sh で codex + codex-code-mode-host を
          ~/.local/share/codex-binaries/ にインストールしてシンボリックリンクを更新、
          worktree を削除(ブランチは残る)、macOS 通知
          失敗 → worktree を残して通知(手動調査用)。同一バージョンの自動再試行は
          2 回まで(それ以降は AUTOREBASE_FORCE=1 が必要)
```

状態・ログはすべて `<repo>/.upstream-autorebase/`(gitignore 済み)に残る:

- `state.jsonl` — 各バージョンの処理履歴(started / rebased / repaired / pushed / failed …)
- `logs/run-*.log` — 実行ログ、`logs/verify-*.log` — build/test 失敗ログ、`logs/agent-*.log` — エージェントの出力
- `target-cache/` — worktree 間で共有する cargo target(毎回のフルビルド回避)。検証前の空き容量が既定 20 GiB 未満なら自動的に clean される

## セットアップ

```bash
# 動作確認(何をするかだけ表示して終了)
scripts/upstream-autorebase/autorebase.sh --dry-run

# 手動で 1 回実行
scripts/upstream-autorebase/autorebase.sh

# 6 時間ごとの自動実行を登録(macOS launchd)
scripts/upstream-autorebase/install-scheduler.sh

# 解除
scripts/upstream-autorebase/install-scheduler.sh --uninstall
```

前提: `git` / `jq` / `curl` / `cargo`(+ `cargo-nextest` 推奨)/ `just`、修復用に `codex` CLI(ログイン済み)。

## 設定(環境変数)

| 変数 | デフォルト | 説明 |
|---|---|---|
| `AUTOREBASE_BRANCH_PREFIX` | `env-switch-monitor-v` | 追加機能ブランチの接頭辞 |
| `AUTOREBASE_PUSH` | `1` | 成功時に push するか(`0` でローカルのみ) |
| `AUTOREBASE_PUSH_REMOTE` | `origin` | push 先リモート |
| `AUTOREBASE_PUSH_FALLBACK_URL` | origin の SSH URL を自動導出 | HTTPS push 失敗時の再試行先 |
| `AUTOREBASE_INSTALL` | `1` | 成功時に install-local.sh でローカルへインストールするか |
| `AUTOREBASE_AGENT_CMD` | `codex exec --dangerously-bypass-approvals-and-sandbox` | 修復エージェント。`claude --dangerously-skip-permissions -p` 等に変更可 |
| `AUTOREBASE_MAX_REPAIR_ATTEMPTS` | `3` | 1 回の実行内での修復試行回数 |
| `AUTOREBASE_MAX_RUNS_PER_VERSION` | `2` | 同一バージョンで failed になった後の自動再実行上限 |
| `AUTOREBASE_AGENT_TIMEOUT_SECONDS` | `10800` | エージェント 1 回の実行タイムアウト(3h) |
| `AUTOREBASE_WORKTREE_ROOT` | `../codex-autorebase-worktrees` | worktree の置き場所 |
| `AUTOREBASE_CARGO_TARGET_DIR` | `.upstream-autorebase/target-cache` | worktree 間で共有する cargo target |
| `AUTOREBASE_MIN_FREE_KB` | `20971520` | 検証前に共有 cargo target を clean する空き容量の下限(KiB) |
| `CARGO_INCREMENTAL` | `0` | 共有 target の incremental artifact 蓄積を抑止。明示指定時はその値を使用 |
| `AUTOREBASE_INTERVAL_SECONDS` | `21600` | (installer 用)ポーリング間隔 |
| `AUTOREBASE_FORCE` | `0` | `1` で失敗上限を無視して再実行 |

## 失敗したときの調べ方

1. macOS 通知「v X.Y.Z port FAILED」が出る
2. `.upstream-autorebase/logs/verify-<ver>.log` にビルド/テストの失敗内容、`agent-*.log` にエージェントの作業記録
3. worktree `../codex-autorebase-worktrees/env-switch-monitor-v<ver>` がそのまま残っているので、そこで手動修正 → push
4. もう一度自動に任せたい場合: `AUTOREBASE_FORCE=1 scripts/upstream-autorebase/autorebase.sh`
