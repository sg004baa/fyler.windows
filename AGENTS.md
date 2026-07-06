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

- [x] **M0 成立性スパイク** — `crates/m0-spike` で #1..#4 を実nvim(Windows実機)検証し ALL PASS。`docs/M0_RESULTS.md` 記録済み
- [x] **M1 read-only表示** — nvim spawn/RPC/snapshot同期 + eframe描画 + クラッシュ検知を実装。Windows実機(nvim v0.11.6)でGUI目視OK・コンソール窓なし。headless RPCスモークpass(実nvim)
- [x] **M2 rename限定dry-run** — `spec_m2.rs` 全pass(parse/validate/diff)。確認ダイアログ + 保存フロー配線(fyler-app/save_flow.rs、dry-run保証=承認しても実FS不可)。Windows実機で `:w`→`RENAME a→b` 表示・重複表示バグ修正を目視OK。SaveControllerテスト5件 + headless RPCスモークpass
- [x] **M3 create / delete / rename 実行** — 同一ボリュームapply(create/delete/rename) + 操作単位CommitReport + 実FS再スキャンreconcileを実装。Linuxでworkspace check/clippy警告ゼロ、対象テスト全pass。Windows実機動作確認は未実施
- [x] **M4 構造編集** — move/copy + クロスボリューム3分類 + 非原子的操作の進捗付きCommitReportを実装。Linuxでworkspace check/clippy警告ゼロ、対象テスト全pass。Windows実機動作確認は未実施
- [ ] **M5 統合・装飾** — notify外部変更監視 / OneDriveプレースホルダ属性判定 / ツリーアイコン装飾(カーソル列オフセット補正込み) / longPathAware manifest(embed-manifest) / watch→再スキャン再描画のapp配線(dirty中は通知のみ・自己apply抑制)を実装。**git statusはユーザー判断で今回スコープ外(M5残件)**。Linuxでworkspace check/clippy警告ゼロ・対象テスト全pass(84件)、Windows GNUクロスターゲットでcheck/clippy pass(cfg(windows)のGetFileAttributesW判定を含む)。Windows実機動作確認は未実施
- [x] **M5.5 バグ修正セッション(2026-07-04)** — 全クレート机上レビューで6件修正: (1) 親ディレクトリrename+子操作同時実行でplanが逐次実行不能(diff.rsのpre-move座標書き換え+順序edge追加、再作成循環はErr(MoveCycle)化)、(2) 展開済みディレクトリブロックCopyの子孫Copy重複、(3) case-fold重複のvalidate欠落(`Foo.txt`/`foo.txt`が黙って上書き)、(4) apply.rsのMove/Copy/Createに移動先preflight(fs::renameのMOVEFILE_REPLACE_EXISTING上書き対策)、(5) 確認ダイアログ中の外部変更でplan陳腐化→PlanInvalidatedでキャンセル、(6) 非ASCII case-only rename判定。回帰テスト12件追加(mutationで牙を確認済み)。既知の残リスク: SetModifiable未実装(:w→ダイアログ表示間の入力すり抜け)、`long_path::to_extended`/`is_cloud_placeholder`/`dir_is_case_sensitive`が未配線、watchのdebounceなし — いずれも docs/ROADMAP_M6.md のM6〜M8で対応
- [x] **M6-1 ファイルを開く / M6-2 ルート移動** — `<CR>`から既定アプリ起動、`^`から親ルートへの安全な移動、root/baseline/watcher/バッファの差し替え、現在ルートのモードライン表示を実装。Linuxで対象テスト80件・workspace check/clippy警告ゼロ・headless RPCスモークpass(nvim v0.12.2)、Windows GNUクロスターゲットでcheck/clippy pass。Windows実機動作確認は未実施
- [x] **M6-3 ディレクトリ折りたたみ / M6-4 隠しファイルトグル + ソート改善** — `<CR>`での折りたたみ、初期/ルート移動時のトップレベル折りたたみ、折りたたみ状態を維持するreconcile、`g.`での隠し表示切り替え、dotfile/Windows hidden属性判定、ディレクトリ優先のcase-insensitive自然順を実装。Linuxで指定テスト91件・workspace clippy警告ゼロ、Windows GNUクロスターゲットでcheck/clippy pass。Windows実機動作確認は未実施
- [x] **M7-1 未配線コードの配線** — long path変換(long_path::to_fs を全FS操作直前に配線)/ OneDriveプレースホルダ警告(確認ダイアログ表示)/ ディレクトリcase sensitivity実測(FILE_CASE_SENSITIVE_INFO、apply preflight分岐)を実処理へ配線し、BaselineTreeのID検索をO(1)化(M7-3の一部)。trashクレートの拡張形式パス受け入れはWindows実機要検証(recycle.rs)。Linuxで対象テスト98件・workspace clippy警告ゼロ、Windows GNUクロスターゲットでclippy pass。Windows実機でテスト・挙動確認OK(2026-07-05)
- [x] **M7-2 watchのdebounce/coalesce** — notifyイベントを200ms固定ウィンドウでパス集合へ集約し、watcher drop時のスレッド終了とapp層の二段coalesceを実装。Linuxで指定テスト100件・workspace clippy警告ゼロ、Windows GNUクロスターゲットでclippy pass。Windows実機でテスト・挙動確認OK(2026-07-05)
- [x] **M7-4 git status装飾** — porcelain v1のGit状態取得とサブディレクトリ基準のパス解決、EntryIdへの対応付け、GUI装飾列と更新配線を実装。Linuxで指定テスト128件・workspace clippy警告ゼロ、Windows GNUクロスターゲットでclippy pass。Windows実機でテスト・挙動確認OK(2026-07-05)
- [x] **M8 快適性** — ファイル情報表示/確認ダイアログのキー操作/パスコピー/ブックマーク・最近使ったルート/設定ファイル/確認中のバッファロックを実装。`config.toml`から隠し表示・ソート・確認詳細度・ブックマークを読み込み、`:b`ジャンプと`recent.toml`の最大10件記録を配線。Linuxで指定テスト141件・workspace clippy警告ゼロ、Windows GNUクロスターゲットでclippy pass、headless RPCスモーク4件pass(nvim v0.12.2)。Windows実機動作確認は未実施
- [x] **性能改善セッション(2026-07-06)** — 大量fileを含むdirをrootにした際のFS層ボトルネックを根本対応(3コミット): (1) スキャンのエントリ毎2syscall(symlink_metadata+GetFileAttributesW)を`DirEntry::metadata()`のfind-data由来metadataへ統一し追加syscallゼロ化。hidden/placeholder判定も同属性ビットから導出、ソートキー事前計算で比較毎のString確保も排除、(2) BaselineTreeにEntryMetaサイドカー(size/mtime/placeholder、PartialEq非対象)を追加し`visible_file_infos`の表示中エントリ全statをインメモリ参照化、(3) `rescan_changed_preserving_ids_with`: watchティックの全ツリー再スキャンを廃止し、変更パスの影響dirだけ実FS列挙(全再スキャンとentries・順序・ID採番まで完全一致が契約。ルート外・非UTF-8・列挙レースは全再スキャンへフォールバック)。Linux実測50kエントリでwatchティック437ms→60ms、(4) git statusサブプロセスをappイベントスレッド外のworkerへ(同時実行1本+coalesce、rootミスマッチのstale badge破棄)。Linuxで対象テスト191件・workspace clippy警告ゼロ、Windows GNUクロスターゲットでcheck/clippy pass、headless RPCスモーク5件pass(実nvim)。Windows実機動作確認は未実施

各マイルストーンの完了条件は DESIGN.md「マイルストーン」章を参照。
完了したらこのチェックリストを更新すること。
