# fyler for windows

[fyler.nvim](https://github.com/A7Lavinraj/fyler.nvim) のコンセプト
(ツリー表示のファイルシステムをバッファのように編集する)を、
Windowsネイティブのスタンドアロン GUI ファイラーとして Rust で実装するプロジェクト。

- 編集エンジン: 組み込み Neovim(`--embed --headless`)を「Vim編集状態マシン」としてのみ利用
- 描画: egui / eframe で全部自前(neovide方式は不採用)
- 行の同一性追跡: in-buffer ID方式(oil.nvim方式)

## ドキュメント

- **[docs/DESIGN.md](docs/DESIGN.md)** — 実装設計書 v2(正典)
- **[AGENTS.md](AGENTS.md)** — 実装エージェント向け運用ルール(絶対ルール・依存境界・進め方)
- **[docs/M0_RESULTS.md](docs/M0_RESULTS.md)** — M0成立性スパイクの結果記録

## ステータス

骨組み(ワークスペース構成・共有型・トレイト・仕様テスト)のみ。ロジックは `todo!()` スタブ。
現在のマイルストーン: **M0(成立性スパイク)**。M0が全項目passするまでM1以降は着手しない。

## ビルド

対象プラットフォームは Windows。純粋ロジック部分はクロスプラットフォームでテスト可能:

```
cargo test -p fyler-core -p fyler-pipeline   # どのOSでも可
cargo check --workspace --all-targets        # フルcheck(Windows推奨)
```
