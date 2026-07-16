# fyler for windows

fyler は、ツリー表示されたファイルシステムを Neovim のバッファのように編集できる、Windows 用のスタンドアロン GUI ファイラーです。Neovim は Vim の操作状態を処理する組み込みエンジンとして使い、画面は Rust と egui で描画します。

## 主な機能

- バッファを編集して rename / move / copy / delete / create をまとめて実行
- 実行前の操作内容・上書き・外部変更の確認と、直前の操作の undo
- 独立した最大 4 pane の分割表示と pane 間の move / copy
- 大規模ツリーにも対応した fuzzy finder と、ディレクトリ移動・折りたたみ・自然順ソート
- Git status、ファイル情報、OneDrive プレースホルダー、隠しファイルの表示
- ブックマークと最近使ったルート
- キーマップと leader key のカスタマイズ
- `:terminal` による現在位置での外部ターミナル起動
- `:feedback` による匿名フィードバック送信

## インストール

[GitHub Releases](https://github.com/sg004baa/fyler.windows/releases) から、installer の `fyler-vX.Y.Z-windows-x64-setup.exe`、または portable zip をダウンロードしてください。

配布物には Neovim v0.12.4 が同梱されているため、Neovim を別途インストールする必要はありません。fyler が利用する nvim-rs はこの固定バージョンの Neovim との組み合わせで検証します。

アンインストールは Windows の「インストールされているアプリ」から行えます。設定や undo データなどのユーザーデータはアンインストールでは削除されません。完全に削除する場合は、アンインストール後に `%APPDATA%\fyler` と `%LOCALAPPDATA%\fyler` を手動で削除してください。

## 起動

- スタートメニューから `fyler` を起動
- コマンドラインから `fyler.exe [ルートディレクトリ]` を実行
- installer で任意の Explorer コンテキストメニュー統合を選び、ディレクトリから起動

fyler は次の順序で Neovim の実行ファイルを解決します。

1. `FYLER_NVIM_EXE` で指定された実行ファイル
2. fyler と同じ配置に同梱された `nvim/bin/nvim.exe`
3. `PATH` 上の `nvim`

`FYLER_NVIM_EXE` は開発・診断用の override で、指定された場合はファイルの存在確認をせず使用します。

## 設定

`config.toml`、キーマップ、leader key、ブックマークなどの設定は [設定リファレンス](docs/CONFIGURATION.md) を参照してください。

## フィードバック

fyler 内で `:feedback` を実行すると匿名フィードバックを送信できます。送信内容と取り扱いは [プライバシー情報](docs/PRIVACY.md) を参照してください。不具合報告や機能提案は [GitHub Issues](https://github.com/sg004baa/fyler.windows/issues) でも受け付けています。

## 開発

Rust toolchain を用意し、リポジトリのルートで実行します。

Windows ではワークスペース全体をビルド・テストできます。

```powershell
cargo build --workspace
cargo test --workspace
```

Linux では純粋ロジックのテストとワークスペースの check / clippy を実行できます。アプリの実行確認は Windows で行ってください。

```sh
cargo test -p fyler-core -p fyler-pipeline
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

設計とクレート境界の詳細は [設計書](docs/DESIGN.md) を参照してください。

## ライセンス

MIT License または Apache License 2.0 のいずれかを選択できます。詳細は [LICENSE-MIT](LICENSE-MIT) と [LICENSE-APACHE](LICENSE-APACHE) を参照してください。

配布物には日本語とアイコン(Nerd Font)グリフを含む組み込みフォント **Moralerspace Argon HW** を同梱しています。このフォントは SIL Open Font License 1.1 で提供され、ライセンス全文は [crates/fyler-gui/assets/fonts/LICENSE-Moralerspace.txt](crates/fyler-gui/assets/fonts/LICENSE-Moralerspace.txt) を参照してください。
