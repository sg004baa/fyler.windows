# fyler for windows

fyler.windows は、[fyler.nvim](https://github.com/FylerOrg/fyler.nvim) にインスパイアされたwindows用Windows 用のスタンドアロン GUI ファイラーです。ツリー表示されたファイルシステムを Neovim のバッファのように編集することができます。

## 主な機能

- バッファを編集して rename / move / copy / delete / create をまとめて実行
- 実行前の操作内容・上書き・外部変更の確認と、直前の操作の undo
- 独立した最大 4 pane の分割表示と pane 間の move / copy
- 大規模ツリーにも対応した fuzzy finder と、ディレクトリ移動・折りたたみ・自然順ソート
- Git status、ファイル情報、OneDrive プレースホルダー、隠しファイルの表示
- ブックマークと最近使ったルート
- キーマップと leader key のカスタマイズ
- `:terminal` による現在位置での外部ターミナル起動

## インストール

[GitHub Releases](https://github.com/sg004baa/fyler.windows/releases) から、installer の `fyler-vX.Y.Z-windows-x64-setup.exe`、または portable zip をダウンロードしてください。

アンインストールは Windows の「インストールされているアプリ」から行えます。既定では設定や undo データなどのユーザーデータ(`%APPDATA%\fyler`, `%LOCALAPPDATA%\fyler`)も削除されます。対話アンインストール時は削除確認ダイアログが表示されます(既定は「はい」)。サイレントアンインストール(`/VERYSILENT` 等)ではダイアログは出ず既定で削除されますが、コマンドラインに `/KEEPDATA` を付けた場合はユーザーデータを保持します。確認ダイアログで「はい」を選んだ場合や `/KEEPDATA` を付けなかった場合、これらのフォルダーは自動的に削除されます。

## 起動

- スタートメニューから `fyler` を起動
- コマンドラインから `fyler.exe [ルートディレクトリ]` を実行
- installer で任意の Explorer コンテキストメニュー統合を選び、ディレクトリから起動

## 設定

`config.toml`、キーマップ、leader key、ブックマークなどの設定は [設定リファレンス](docs/CONFIGURATION.md) を参照してください。

## フィードバック

fyler 内で `:feedback` を実行すると匿名フィードバックを送信できます。
感想、機能リクエストなどなんでも送って頂けると嬉しいです。
送信内容と取り扱いは [プライバシー情報](docs/PRIVACY.md) を参照してください。不具合報告や機能提案は [GitHub Issues](https://github.com/sg004baa/fyler.windows/issues) でも受け付けています。

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
