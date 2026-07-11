# AGENTS.md — fyler for windows 実装ガイド

このファイルは実装エージェント(Codex等)向けの運用ルール。
**設計の正典は [docs/DESIGN.md](docs/DESIGN.md)**。判断に迷ったら必ずDESIGN.mdに従うこと。
このリポジトリの骨組み(型・トレイト・モジュール構成・仕様テスト)は設計書v2から生成済みで、
実装は `todo!()` スタブを埋める形で進める。

## 絶対ルール(違反コードは書かない)

1. **確認ダイアログの承認なしに実ファイルへ触れない。** M2まではdry-runのみ。
   実FSへの書き込みは `fyler-fsops` の `apply` モジュール(forward: `apply_plan`系、
   undo: `apply_undo_cancellable`)だけが行い、保存状態機械(`fyler_core::save`)の
   `Applying` / `ApplyingUndo` 状態からのみ呼ばれる。例外は fyler 自身のデータ
   ディレクトリ(config / undo journal・backup payload)のみ(ROADMAP_M12.md参照)。
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
- CIは `.github/workflows/ci.yml`。Linuxで fmt + clippy(-D warnings)+ 全テスト + Windows GNUクロスターゲットclippy、Windowsネイティブで check + 全テスト。
  リリースは `release.yml`(`v*` タグpushでタグ⇔ワークスペースバージョン照合 → Windowsでテスト → `fyler.exe` をzip + sha256でGitHub Releaseへ)。
  依存更新は dependabot(週次、nvim-rsはignore固定)+ patch/minor自動マージ(`dependabot-automerge.yml`)。

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
- [x] **M9 常用ファイラー化(2026-07-06)** — 「移動する・探す・衝突を裁く」の欠落を埋める(3セッション、docs/ROADMAP_M9.md): (1) **M9-1/M9-2 ナビゲーション**: `gd`でカーソル行ディレクトリを新ルート化(NavigateInto)、`:cd <path>`で絶対/相対/`~`パス移動(ChangeDirectory、cnoreabbrevで`:cd`乗っ取り・`:cdo`温存)、Windowsドライブ一覧(GetLogicalDrives、drives.rs新設)、`^`親移動後の子ディレクトリへのカーソル復元。すべて読み取り専用(実FS書き込みなし)。(2) **M9-3 上書きpreflight**: plan確定時に実FS衝突をpreflight走査(preflight.rs、読み取り専用・plan順シミュレーション・case-fold vacated除外)、既存ディレクトリ衝突はValidateError::TargetOccupiedByDirectoryで中断、既存ファイル/symlink衝突は確認ダイアログで警告し承認後ごみ箱退避して上書き(apply_plan_with_overwrites)。(3) **M9-4 apply非同期化**: apply_plan_cancellable(操作間キャンセル+進捗通知)、承認はStartApplyを返しfyler-applyワーカーで実行、進捗ダイアログ+キャンセルボタン、apply中の外部変更は遅延flush。fyler_core::save状態機械は無変更。Linuxで対象テスト185件・workspace clippy警告ゼロ、Windows GNUクロスターゲットでclippy pass、headless RPCスモーク6件pass(実nvim)。Windows実機動作確認は未実施
- [x] **M10 pane(分割ウィンドウ)(2026-07-10)** — issue #6対応(3セッション、codex実装、docs/ROADMAP_M10.md): (1) **M10-1 pane基盤**: 1 pane = 1 NvimEngine(独立プロセス)+ PaneSession(root/engine/SaveController/watcher/git装飾/deferred_changesをpane毎に独立所有)、fyler-coreにPaneId/PaneLayout(二分木)/PaneAction、AppEvent全系統へのPaneId付与、`<C-w>`系keymap(guard.rs内getcharstrディスパッチ→PaneAction変換、既存のnvim実window分割穴も封鎖)、GUIのPaneLayout再帰描画+active paneのみ入力転送、IdAllocatorセッション共有(pane間in-buffer IDエイリアシング防止)、GitRefresher pane別化、crashのpane単位降格、split/close(dirty/busy/最後の1pane拒否・最大4pane・直列spawn)。(2) **M10-2 TransferPlan純ロジック**: fyler-coreにTransferPlan(from_root/to_root+ops)、fsopsにpreflight_transfer(移動先衝突/自己子孫move・copy/to_root巻き込みmove/source消滅/op間干渉をblocked、絶対パスcase-fold vacated除外)とapply_transfer_plan_cancellable(volume分類・ごみ箱退避上書き・操作間cancel・CommitReport共用)。孤立していたSaveController::change_rootを削除。(3) **M10-3 transfer UX配線**: normal/visual `gm`/`gc`→TransferRequested、target=last_active pane、destination=targetカーソル行のdir/親、開始ゲート(dirty/非Idle/crash/apply・transfer中/ID無し行)、親子孫重複除外、確認ダイアログ(`MOVE a → [pane 2] b`+上書き警告)→両pane lock→worker apply(進捗+cancel)→両pane reconcile→deferred flush、確認中の外部変更でplan無効化。Linuxで275件pass・workspace clippy警告ゼロ、headless RPCスモーク9件pass(実nvim v0.12.3)。Windows実機動作確認は未実施
- [x] **M11 fuzzy finder / picker UI(2026-07-10)** — issue #7対応(2セッション、codex実装、docs/ROADMAP_M11.md): (1) **M11-1 純ロジック+エンジン語彙**: fyler-coreに`search`モジュール(SearchCandidate=正規化key/name_offsetキャッシュ、subsequence score+basename完全/前方/部分・segment境界・連続一致・開始位置加点、空白tokenのAND、上位K件・同点は元順安定、migemo用expand_query境界。50k実測約36ms)、`EditorCommand::SetCursorLine`(nvim_win_set_cursor相当・クランプ・undo/changedtick非汚染)、`EditorEvent::OpenFilePicker`+guard.rs `g/` buffer-local map、`SaveController::reveal_entry`(collapsed祖先展開+対象行index返却、非IdleはBusy)。(2) **M11-2 GUI picker+app配線**: GUI返却チャネルをConfirmChoice専用からGuiAction{Confirm, PickerSelect{pane_id, entry_id, action}}へ一般化(on_choice署名は不変)、DialogState::FilePicker(egui::Modal+TextEdit=IMEネイティブ・初回のみfocus・上位100件・選択追従スクロール・Esc/↑↓/C-p/C-n/Enter=jump/C-Enter=open)、active pane対象の起動ゲート(dialog/apply/transfer/crashed/非Idleは拒否・**dirtyは許可**)、選択はEntryIdで現baseline再解決(stale通知)、可視行jump=SetCursorLine+snapshot行ID照合(dirtyズレ誤爆防止)、collapsed配下はclean時のみreveal+SetLines(dirty拒否=既存collapse契約と同じ)、Open=既定アプリ起動(File/Symlink/Dir)、picker表示中の入力遮断とIME geometryゲート、対象paneのclose/crashで自動クローズ。Linuxで300件pass・workspace clippy警告ゼロ、headless RPCスモーク10件pass(実nvim v0.12.3)。Windows実機動作確認は未実施
- [x] **M12 apply後のundo(2026-07-10)** — issue #8対応(4セッション、codex実装、docs/ROADMAP_M12.md、stage 1+2): (1) **M12-A receipt基盤**: fyler-coreに`undo`モジュール(FileIdentity/Fingerprint/ManifestEntry/UndoStep 5種/BackupRef/UndoTransaction)、fsopsにidentity.rs(Windows: FILE_ID_INFO 128bit、unix: dev+ino、symlink非追跡)/backup.rs(payload退避・復元、復元先占有拒否)/UndoRecorder。`apply_plan_cancellable`にrecorder引数追加、Delete/overwrite退避は**backup完了後にのみ**recycle(backup失敗は元データ無傷)、RestoreOverwrittenは対のop stepより前に記録。(2) **M12-B undo実行系**: preflight_undo(read-only・UI用)+apply_undo_cancellable(逆順実行・実行直前stale再検証が正典・step間cancel・CommitReport<UndoStep>)。stale判定=identity+fingerprint(File: size/mtime、Dir: 空/manifest再採取比較、Symlink: link_target)、復元先占有拒否、case-only renameは2段rename(case-insensitive dirではfrom空き確認をスキップ=自己占有誤検出防止)。(3) **M12-C 状態機械+コントローラ**: save.rsにAwaitingUndoConfirmation/ApplyingUndo(Reconcilingはunit化)、`:FylerUndo`(cnoreabbrevなし=バッファ`u`と分離)→EditorEvent::UndoRequested、SaveController::request_undo(全stepRejectedは状態遷移せずUndoNothingLeft)/on_undo_finished、undo確認中の外部変更はUndoInvalidated(transaction返却)、plan_warningsにDelete/overwrite backup見積+placeholder hydration警告。(4) **M12-D app配線+journal**: undo_journal.rs(WAL: Preparing→Committed→Undoing→Undone、手書きtoml+atomic rename、%LOCALAPPDATA%\fyler\undo、起動時scan=Committed/Undone残骸purge・Preparing/Undoing復旧ダイアログ)、PaneSession.undo_slot(pane毎直近1件、transfer開始で全paneクリア)、undo workerはapply_owner共用(deferred changes機構有効)、GUI undo plan/report/recovery/進捗ダイアログ。Linuxで対象テスト349件・workspace clippy警告ゼロ、headless RPCスモーク11件pass(実nvim)。Windows実機動作確認は未実施
- [x] **issue #5 中規模改善(2026-07-10)** — 5セッション(codex/-issue-5ブランチ): (1) **z系折りたたみ**: `zc/zo/za/zC/zO/zR/zM`(FoldOp/EditorEvent::Fold、`SaveController::fold`。zcはファイル行から親の展開中dirへ遡るvim fold準拠、zMはトップレベル祖先へカーソル)、(2) **sortコマンド+補完UI**: `:sort name|date|size|ext`(`!`で降順、引数なしで現在値表示。FylerSort+cnoreabbrev乗っ取り、complete関数でTab補完)、SortKey/ScanOptions{key,reverse}(date/sizeのNoneはreverse時も末尾固定、ext小文字キーはスキャン時事前計算でcomparator内String確保ゼロ維持)、config.tomlにsort_key/sort_reverse追加、**ext_popupmenu有効化**でpopupmenu_show/select/hide→GUI cmdline直上に候補窓(最大8件)、(3) **open-with選択**: `go`でShell関連付けハンドラ列挙(SHAssocEnumHandlers/IAssocHandler::Invoke、COM RAIIガード、windows featureへWin32_System_Com追加)→GUI選択モーダル(j/k/Enter/Esc、末尾にopenasダイアログ委譲)、(4) **`<`/`>`構造インデント**: 組み込み`<`/`>`が行頭(IDプレフィックス前)へタブを挿入して行をNoId化する罠をoperatorfunc remapで回避(プレフィックス保持でタブ±1、count/motion/Visual対応)、CursorMoved(I)でカーソルを名前先頭へスナップバック(インデント領域はナビゲート不可)、(5) **装飾インデント方式**: conceal.rsでIDプレフィックス+行頭タブを隠しdepthを保持、tree_view.rsで深さ×空白2文字幅にアイコンを左端アンカー描画(カーソル/選択/検索ハイライトは自動追従)。Linuxで全テスト281件・workspace clippy警告ゼロ、Windows GNUクロスターゲットでclippy pass、headless RPCスモーク12件pass(実nvim v0.12.2)。Windows実機動作確認は未実施
- [x] **issue #9 keymapカスタマイズ + leader key(2026-07-11)** — 1セッション(codex、agent/issue-9-custom-keymapsブランチ、PR #16): (1) **fyler-core::keymap新設**: `KeySequence(Vec<KeyInput>)`(既存editor::KeyInputを1ストロークとして再利用)/`EditorAction` 26種(config名snake_case相互変換+日本語description)/`KeyBinding`/`default_bindings()`(現行キーと完全一致=後方互換テスト対象)/エンジン非依存key表記parser(`"g d"`・`"Ctrl+W v"`・`"Leader f"`。修飾付きASCII英字は小文字正規化、印字可能文字へのShiftは拒否、`Space`等named key対応)/`resolve_bindings`(デフォルト+ユーザー上書き、`"none"`でunmap、不正エントリは日本語警告して無視=起動は止めない、単独`Ctrl+W`と`Ctrl+W`内プレフィックス衝突は拒否)。(2) **config.toml**: トップレベル`leader`(単一・無修飾キーのみ。既定Space)+`[keymap.normal]`を読み込み(`[keymap.visual]`等未対応セクションは警告)、解決済み`Config.bindings`へ。(3) **エンジン配線**: `NvimConfig.bindings`+`NvimConfig::new`、guard.rsのaction mapをデータ駆動化(exec_lua引数でbinding表を渡しLuaループ設置。translate.rs `sequence_to_lhs`でnvim keycode変換をエンジン内に隔離)、`Ctrl+W`はgetcharstr trie常設ディスパッチャ(組み込みwindowコマンド封鎖を維持、未知キーはaction_blocked)、transferのvisual判定はdispatch時`mode()`判定へ。安全ガード(write autocmd/BufEnter/cmdline alias rewrite/インデントremap/gf系block)はカスタマイズ対象外で不変。(4) **動的ヘルプ**: `GuiOptions.help_lines`(fyler-appが解決済みbindingsから生成、エンジン非依存表記)。docs/CONFIGURATION.md新設(英語、設定リファレンス全項目)。Linuxで全テスト417件・workspace clippy警告ゼロ、Windows GNUクロスターゲットでclippy pass、headless RPCスモーク20件pass(実nvim、custom leader/unmap/Ctrl+W trie検証含む)。Windows実機動作確認は未実施

各マイルストーンの完了条件は DESIGN.md「マイルストーン」章を参照。
完了したらこのチェックリストを更新すること。
