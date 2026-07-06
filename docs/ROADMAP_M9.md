# fyler for windows — 常用ファイラー化フェーズ計画(M9)

> 2026-07-06 作成。M6〜M8完了(open/navigate/collapse、Windows堅牢性・性能、快適性)を受けて、
> 「見る・編集する」は完成した一方で残っていた「**移動する・探す・衝突を裁く**」の欠落を埋める。
> 設計の正典は [DESIGN.md](DESIGN.md)。本書はその「マイルストーン」章の延長であり、
> 矛盾した場合はDESIGN.mdを優先する。

## 棚卸し(2026-07-06、M8完了時点の欠落を致命度順)

1. **下位ディレクトリへrootを移せない** — `<CR>`は折りたたみトグル、`^`は親のみ。深い場所へ潜れない
2. **ドライブ切替がない(Windows)** — `^`はドライブルートで止まり、C:→D:は事前ブックマークのみ
3. **生パス移動がない** — `:b`はブックマーク名/recent番号のみ受理。`:cd <path>`相当なし
4. **上書き衝突の解決がない** — baselineに現れないFS実体(隠しファイル等)との衝突がapply時まで露見しない
5. **applyが同期・キャンセル不可・進捗不可視** — 数GB copyでイベントループが固まる

> 常用ファイラーとしての残欠落の全体像は managed skill `fyler-windows-m9-gap-catalog` を参照。
> 本フェーズは致命度上位(1→2→4→5に相当)を実装。3(finder)・プレビューはUI設計が重くM10候補。

## M9: 実装済み(2026-07-06)

> 3セッション、codex実装。Linuxゲート(fmt/test/clippy/クロスターゲット)+ headless RPCスモークpass。
> Windows実機動作確認のみ未実施。

### M9-1: ファイルを開く先=ディレクトリへ潜る(`gd`)

- guard.rs: `gd`(normal)→ `fyler_navigate_into`(カーソル行番号付きrpcnotify)
- engine.rs: → `EditorEvent::NavigateInto { line }`(nvim語彙はここで消える)
- main.rs: 行を`resolve_line`で解決し、ディレクトリなら既存 `change_root_to` へ。
  `<CR>`は従来どおり折りたたみトグルのまま(潜るのは`gd`に分離)
- 読み取り専用(実FS書き込みなし)。dirty/非idleは`change_root_to`が既存ゲートで拒否

### M9-2: 生パス移動と ドライブ切替(`:cd` / ドライブ一覧)

- guard.rs: buffer-local `FylerCd` user command + `cnoreabbrev cd`(`getcmdline() ==# 'cd'`
  ガードで`:cdo`等を温存。`:b`と同じ乗っ取りパターン)
- engine.rs: → `EditorEvent::ChangeDirectory { query: Option<String> }`
- main.rs `resolve_cd_target`(純粋関数): 絶対パスはそのまま / `~`・`~/...`はホーム基準 /
  それ以外は現在ルートからの相対。正規化は`change_root_to`の`normalize_root`が担当
- fyler-fsops/drives.rs 新設: `GetLogicalDrives`のビットマスク→`C:\`形式(Windows専用、他OSは空)。
  `^`がドライブルートで止まったとき / `:cd`単独でドライブ候補を提示
- `^`親移動後、元いた子ディレクトリへカーソル復元(`find_top_level_line`、grammar再利用)

### M9-3: 上書き衝突の事前検出と承認付き上書き(overwrite preflight)

diff層の順序契約で「可視エントリの置換」は既に1回の`:w`で可能(Delete→Move順序保証)。
穴は**baselineに現れないFS実体**(隠しファイル、外部生成)との衝突。

- fyler-fsops/preflight.rs 新設: plan確定時に各操作の移動先を`symlink_metadata`で照合
  (読み取り専用=hydration非誘発)。plan順シミュレーションで先行Delete/Moveが空ける移動先は
  衝突除外(case-fold vacated set。Windows case-insensitive解決の誤検出回避)。case-only renameも除外
- 既存ディレクトリ衝突 → `ValidateError::TargetOccupiedByDirectory`で保存中断(上書き不可)
- 既存ファイル/symlink衝突 → 確認ダイアログに退避対象を警告表示、承認ボタンを「上書きして実行」に。
  承認後 `apply_plan_with_overwrites` が実行直前に再確認してごみ箱経由で退避(直接削除しない=Delete規律)
- 全体承認(all-or-nothing)。操作単位skip等はやらない。applyの`ensure_target_vacant`はTOCTOU最終防衛線として残る
- `pending_overwrites`はcancel / PlanInvalidated / apply完了でクリア。`fyler_core::save`状態機械は無変更

### M9-4: applyの非同期化 + 進捗ダイアログ + 操作間キャンセル

- fyler-fsops: `apply_plan_cancellable(cancel: &AtomicBool, on_progress)`。操作間でcancel判定し
  残りをSkipped、各操作前と完了後に進捗通知。`apply_plan`/`apply_plan_with_overwrites`は委譲
- save_flow: 承認は `StartApply { plan, overwrites, cancel }` を返す(状態はApplyingへ遷移済み)。
  workerがapply→`on_apply_finished`で状態機械へ反映しreconcile。Applying中の`on_choice(Cancel)`は
  cancelフラグを立て`ApplyCancelRequested`、Approve/on_commitは無視
- main.rs: `fyler-apply`ワーカーspawn(GitRefresherパターン、spawn失敗は全Failed report合成)。
  `ApplyProgress`/`ApplyFinished`イベント。apply中の外部変更は`deferred_changes`へ退避し
  完了後flush(reconcile済みでNoChangesに畳まれる)
- GUI: 進捗ダイアログ(ProgressBar + N/M件 + current操作 + キャンセルボタン)。表示中は入力ゲート維持
- キャンセルは操作間でのみ有効(巨大ファイル1件のcopy途中では効かない=v1の割り切り)

## M9の完了条件

- `gd`で下位ディレクトリへ、`:cd`で任意パスへ、`^`/`:cd`でドライブ間を移動できる ← 実装済み(実機確認残)
- 隠しファイル等との衝突がapply前に検出され、ファイルは承認付き上書き・ディレクトリは中断 ← 実装済み
- 大量copyでUIが固まらず、進捗が見え、操作間でキャンセルできる ← 実装済み(実機体感確認残)
- 既存の保存フロー(rename→:w→確認→apply→reconcile)が非同期化後も同じ契約で動く ← テスト済み

## このフェーズでやらないこと(M10候補)

- fuzzy finder / ファイル名検索(`/`はバッファ可視行のみ。専用UIが必要)
- ファイル内容プレビュー(hydrationリスクと表示設計が重い)
- 離れた場所へのcross-root copy/move(タブ/ペインなしのクリップボード方式は中量)
- ソート選択肢の拡充(mtime/size/逆順)・フィルタ表示・ls -l列表示
- apply後のundo / open-with選択 / ここでターミナル / ごみ箱閲覧・復元 / `g?`ヘルプ
