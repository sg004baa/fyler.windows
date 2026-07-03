# AGENTS.md — fyler for windows 実装ガイド

このファイルは実装エージェント(Codex等)向けの運用ルール。
**設計の正典は [docs/DESIGN.md](docs/DESIGN.md)**。判断に迷ったら必ずDESIGN.mdに従うこと。
このリポジトリの骨組み(型・トレイト・モジュール構成・仕様テスト)は設計書v2から生成済みで、
実装は `todo!()` スタブを埋める形で進める。

## 絶対ルール(違反コードは書かない)

1. **確認ダイアログの承認なしに実ファイルへ触れない。** M2まではdry-runのみ。
   実FSへの書き込みは `fyler-fsops` の `apply::apply_plan` だけが行い、
   保存状態機械(`fyler_core::save`)の `Applying` 状態からのみ呼ばれる。
2. **nvim固有のAPI・概念を `EditorEngine` トレイト境界の外に漏らさない。**
   nvim-rs・keycode表記・msgpack-RPC等のnvim語彙に触れてよいのは
   `crates/fyler-engine-nvim` だけ。クレート間を跨ぐ型はすべて `fyler-core` の
   エンジン非依存型を使う(方式B移行のため。DESIGN.md「リスクと撤退ルート」)。
3. **パス変換(`\\?\` プレフィックス等)は `crates/fyler-fsops/src/long_path.rs` の1か所に閉じ込める。**
   他の場所に `\\?\` の文字列が現れたら違反。
4. **マイルストーンを順番に通す。** `crates/m0-spike` と `docs/M0_RESULTS.md` の
   全項目がpassするまで、M1以降の実装を始めない。

## ワークスペース構成(依存境界)

| クレート | 役割 | 依存してよいもの |
|---|---|---|
| `fyler-core` | 全レイヤー共有の「型の正典」+ バッファ文法(`grammar`)+ Windows名前規則(`win_naming`)+ 保存状態機械の型(`save`) | std / anyhow / thiserror のみ |
| `fyler-pipeline` | parse → DesiredTree → validate → OperationPlan(純粋ロジック) | fyler-core のみ。**FS・nvim・GUIに一切触れない** |
| `fyler-engine-nvim` | `EditorEngine` のNeovim実装 | nvim-rs / tokio / arc-swap。**nvim-rsに依存してよい唯一のクレート** |
| `fyler-fsops` | Windowsファイル操作・baselineスキャン・外部変更監視 | trash / notify / windows。**windowsクレートに依存してよい唯一のクレート** |
| `fyler-gui` | egui描画(tree_view / conceal / modeline / cmdline / confirm / input) | eframe |
| `fyler-app` | 配線・エントリポイント | 上記すべて |
| `m0-spike` | M0成立性スパイク(検証専用バイナリ) | fyler-core / fyler-engine-nvim |

- 新しい共有型が必要になったら `fyler-core` に置く。
- クレート境界を跨ぐシグネチャに nvim-rs / eframe / windows の型を出さない。

## 実装の進め方

- 各 `todo!("...")` とその直前のdocコメントが**実装契約**。契約に反する実装をしない。
  契約が曖昧なら DESIGN.md の該当章に従う。
- `crates/fyler-pipeline/tests/spec_m2.rs` に仕様テストが `#[ignore]` 付きで置いてある。
  これが **M2のacceptance criteria**。実装したら `#[ignore]` を外して通すこと。
  `fyler-core/src/save.rs` の状態機械テスト(M0項目)も同様。
- `fyler_core::grammar`(バッファ行フォーマット)と `fyler_core::win_naming`(予約文字・予約名)は
  **実装済みの正典**。パイプラインやGUIで同じロジックを再実装しない。必ずこれを呼ぶ。
- スタブだらけのクレートの先頭には `#![allow(unused_variables)]` が置いてある。
  そのクレートの実装が概ね済んだら削除して警告ゼロにする。
- コミット前に `cargo fmt --all` を実行する。

## ビルド・テスト

- **Linux / macOS**: `cargo test -p fyler-core -p fyler-pipeline`(純粋ロジックのみ)。
  `cargo check --workspace` も概ね通るが、実行・動作確認はWindowsのみ。
- **Windows**: `cargo check --workspace --all-targets` / `cargo test --workspace`。
  アプリの実行は `cargo run -p fyler-app`(M1以降)。
- Neovim本体と nvim-rs のバージョンは**固定**(nvim-rsはAPI unstable宣言。DESIGN.md参照)。
  安易に `cargo update` しない。`Cargo.lock` はコミット対象。
- CIは `.github/workflows/ci.yml`。Linuxで fmt + 純粋ロジックのテスト、Windowsで全体check。

## マイルストーン現況

- [ ] **M0 成立性スパイク** ← いまここ。`crates/m0-spike` に検証コードを書き、`docs/M0_RESULTS.md` を埋める
- [ ] M1 read-only表示
- [ ] M2 rename限定dry-run(`spec_m2.rs` を全部通す)
- [ ] M3 create / delete / rename 実行
- [ ] M4 構造編集(move / copy・クロスボリューム)
- [ ] M5 統合・装飾

各マイルストーンの完了条件は DESIGN.md「マイルストーン」章を参照。
完了したらこのチェックリストを更新すること。
