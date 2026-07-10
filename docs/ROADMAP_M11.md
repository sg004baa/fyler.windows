# fyler for windows — fuzzy finder / picker UI フェーズ計画(M11)

> 2026-07-10 作成。[issue #7](https://github.com/sg004baa/fyler.windows/issues/7) 対応。
> 設計の正典は [DESIGN.md](DESIGN.md)。矛盾した場合はDESIGN.mdを優先する。
> issue #7 コメントの対応方針を4クレート実地調査で机上検証した結果、大筋妥当。
> ただしコメントは **M10(pane分割)以前のsingle-root前提** で書かれており、
> pane対応とdirtyバッファ契約の2系統の補正を織り込む。

> **2026-07-10 実装済み**(2セッション、codex実装。コミット: 515ba5a / cf21d1f)。
> Linuxゲート: cargo test --workspace 300件pass、workspace clippy(-D warnings)警告ゼロ、
> headless RPCスモーク10件pass(実nvim v0.12.3)。**Windows実機動作確認は未実施**。
> 既知: 既存スモーク set_initial_lines_with_multiple_lines_has_no_duplication は
> 環境負荷依存で稀にflaky(M10 tipでも再現、本フェーズ起因ではない)。

## 方針検証の結論(2026-07-10)

### 採用(コメント通り)

- **GUI所有のmodal picker + 表示中の通常入力遮断**。
  根拠: 入力転送は `fyler-gui/src/app.rs:388` の `self.dialog.is_none()` ゲートに一元化済みで、
  pickerを `DialogState` の新variantにすればnvimへの転送停止が構造的に成立する。
  `egui::Modal` の雛形は既に6箇所(confirm.rs / app.rs help・ValidationErrors)。
  egui 0.35はWindows IMEの確定Enter誤発火を統合層でフィルタ済み
  (egui-winit `try_on_ime_processed_keyboard_input`)で、`TextEdit` がIME
  preedit/commitをネイティブ処理する — 検索欄のIME入力はTextEditに任せられる。
- **`fyler-core::search` にエンジン/GUI/FS非依存の純ロジック**。
  fyler-coreの依存はanyhow/thiserrorのみ(絶対ルール)であり、外部fuzzyクレートを
  使わず線形subsequence scoreで開始し50k実測で判断する方針はこの制約と整合する。
- **正規化済み検索keyのcandidate構築時キャッシュ**(毎keystroke再正規化しない)。
- **選択結果は `EntryId + action` で返し、app側で現baselineへ再解決**。
  根拠: `BaselineTree::get(id)` はO(1)、watchの `rescan_changed_preserving_ids_with` は
  既存パスのID維持を契約済みのため、消えたエントリは `get→None` でstale検出できる。
  root変更後は全エントリが再採番されるため旧IDは必ずNone=安全側に倒れる。
- **起動はエンジン非依存語彙 `EditorEvent::OpenFilePicker`、nvim mapは
  fyler-engine-nvim(guard.rs)内に閉じる**。引数なし通知の定石
  (fyler_parent / fyler_toggle_hidden / fyler_help)がそのまま使える。
- **migemoは初期スコープ外、query展開フックだけ用意**(下記設計の `expand_query`)。

### 補正(コメントから変更)

1. **pane対応(最重要)**。「現在root」「単一snapshot」の記述はM10で古くなった。
   pickerは **active pane** を対象にし、`GuiEvent::ShowFilePicker { pane_id, candidates }`、
   選択返却も `{ pane_id, entry_id, action }` でpaneを固定する。表示中に当該paneが
   close/crashした場合はGUIがpickerを閉じる。
2. **「dirtyでも利用可能」は部分制限**。起動・絞り込み・**open** はdirtyでも可
   (読み取り専用)。ただし **jump** は:
   - 対象が可視行にある場合のみ、カーソル移動(非破壊)で実行。dirtyバッファでは
     行が編集されうるため、移動前に対象行のIDプレフィックスがtarget EntryIdと
     一致することをsnapshotで照合し、不一致なら拒否メッセージ(誤爆防止。cleanなら常に一致)。
   - collapsed配下でreveal(=`SetLines` 全行差し替え)が必要な場合、**dirtyでは拒否**。
     dirty中のSetLinesは編集を破棄するため。既存のcollapse
     (main.rs:470)・ToggleHidden(pane_runtime.rs:613)と同一契約。
3. **`EditorCommand::SetCursorLine(usize)` を新設**。単独カーソル移動コマンドが現存せず
   (`SetLines{cursor_line}` のみ)、可視行へのjumpに全行差し替えを使うとdirtyを壊し
   undo履歴・changedtickも汚すため。範囲外は最終行へクランプ(SetLinesと同契約)。
4. **reveal相当は `SaveController` の新API `reveal_entry(id)`**。
   `EditContext::collapsed_dirs` はprivateで、「collapsed_dirs変更後は必ず全行を
   SetLinesし直す(食い違うとdiffがDELETE誤解釈: tree.rs:134-135)」という不変条件を
   守る場所はSaveController(toggle_collapse / collapse_all_dirs と同型)。
   祖先dir列挙は `baseline.entries()` 1パス + `TreePath::is_strict_ancestor_of`
   (path→id逆引きは存在しないため。visible_file_infosと同イディオム)。
5. **GUI→app返却チャネルは `ConfirmChoice` 専用から `GuiAction` へ最小差分で一般化**。
   `GuiAction { Confirm(ConfirmChoice), PickerSelect { pane_id, entry_id, action } }`。
   `SaveController::on_choice` / `TransferController::on_choice` の署名は変えない
   (pane_runtime.rsのブリッジスレッドでvariant別に `AppEvent::Confirm` /
   `AppEvent::PickerSelect` へfan-outする)。チャネル型が現れるのは
   fyler-gui app.rs:153/172/684、pane_runtime.rs:165/1066 の5箇所+ブリッジ。
6. **起動ゲート**: `dialog_owner.is_some() || apply_owner.is_some() ||
   transfer.is_awaiting() || transfer.is_running() || session.crashed ||
   !save_controller.is_idle()` なら開かずメッセージ(CommitRequestedゲート
   pane_runtime.rs:343-347 と同型)。dirtyは拒否条件に **入れない**(補正2の範囲で許可)。

## 設計

- `fyler-core::search`(新モジュール。lib.rs の `pane` と `path` の間に配置):
  - `SearchCandidate { id: EntryId, path: TreePath, kind: EntryKind, display: String,
    key: String, name_offset: usize }`。`display` はTreePathのDisplay(`/`区切り相対)、
    `key` は `display` のUnicode小文字化、`name_offset` は `key` 上のbasename開始
    バイトオフセット(小文字化後に `rfind('/')+1` で計算)。
  - `build_candidates(&BaselineTree) -> Vec<SearchCandidate>`(baseline表示順のまま)。
  - `expand_query(&str) -> Vec<String>`: 空白区切りtoken列を返す(現状は小文字化のみ。
    migemo対応時にtoken→複数パターン展開へ差し替える境界。docコメントで明記)。
  - `search(&[SearchCandidate], query: &str, limit: usize) -> Vec<SearchHit>`
    (`SearchHit { index: usize, score: u32 }` 等)。全tokenがsubsequence一致(AND)した
    候補のみ、スコア降順・**同点は候補の元順(index昇順)で安定**、上位 `limit` 件
    (GUIからは100で呼ぶ)。空queryは先頭から `limit` 件を元順で返す。
  - score契約(テストで順位を固定。重みの絶対値はcodex裁量):
    basename完全一致 > basename前方一致 > basename部分一致 > パスsegment境界一致 >
    散在subsequence。連続一致長・一致開始の早さで加点。case比較は `key` で行う
    (大文字小文字無視)。
- `EditorCommand::SetCursorLine(usize)`: NvimEngineは `nvim_win_set_cursor` 相当を
  実行(行数超過は最終行へクランプ)。
- `EditorEvent::OpenFilePicker`(引数なし)+ guard.rs normalモードbuffer-local map
  `g/` → `rpcnotify(channel, "fyler_open_picker")` → engine.rs通知ハンドラで変換
  (`_ => {}` より前)。`g/` は未使用で衝突なし(観測済み)。
- `SaveController::reveal_entry(&mut self, id: EntryId) -> RevealResult`:
  `RevealResult { AlreadyVisible { line }, Revealed { lines, line }, NotFound, Busy }`。
  非IdleはBusy。collapsed祖先を全て展開し、全行と対象IDの0始まり行indexを返す
  (行index = `visible_entries` 上のposition。baseline_to_linesとの1:1は
  save_flow.rs:809-829の構築で保証済み)。dirty判定は呼び出し側(app層)の契約
  (toggle_collapseと同じ)。
- GUI(fyler-gui):
  - `DialogState::FilePicker { pane_id, candidates: Vec<SearchCandidate>,
    query: String, selected: usize, hits: Vec<SearchHit> }`。
    `GuiEvent::ShowFilePicker { pane_id, candidates }` で開く(hitsは空queryで初期化)。
  - 描画は `egui::Modal`(新規Id)。検索欄は `TextEdit::singleline`
    (**初回フレームのみ** `request_focus`。毎フレーム呼びはegui既知問題回避)、
    query変更フレームだけ `search` を再実行。結果リストは上位100件+選択行
    ハイライト+選択追従スクロール。行表示は `display`+Dirなら `/` サフィックス。
  - キー: `Esc`=閉じる、`↑/↓` と `Ctrl-p/Ctrl-n`=選択移動、`Enter`=Jump、
    `Ctrl-Enter`=Open。printable文字はTextEditが消費するため、グローバル
    `key_pressed` 読みは上記キーのみに限定(既存modalのy/n方式を流用しない)。
  - 選択確定で `GuiAction::PickerSelect` を送信し `dialog=None`。
    `GuiEvent::RemovePane` / `EngineCrashed` が対象paneに来たらpickerを閉じる。
  - IME geometry書き込み(app.rs:429-437)を `self.dialog.is_none()` でゲート
    (tree側IMEとTextEdit側IMEの衝突防止。1行)。
  - help(draw_help)へ `g/    Find file` を `g.` の直後に追加。
- app(fyler-app):
  - `AppEvent::PickerSelect { pane_id, entry_id, action }`(action: `PickerAction { Jump, Open }`。
    型はfyler-gui側にConfirmChoiceと同格で置く)。
  - `EditorEvent::OpenFilePicker` ハンドラ(pane_runtime.rs:341の内側match、
    catch-allより前): 補正6のゲート→ `build_candidates(&session.save_controller...)`
    → `GuiEvent::ShowFilePicker`。
  - `AppEvent::PickerSelect` ハンドラ: `panes.get(pane_id)`(無ければ無視)→
    `baseline.get(entry_id)` 再解決(None→「候補が見つかりません(外部変更の可能性)」)→
    - Open: `TreePath::to_fs_path(root)` → `open_with_default_app`
      (handle_activate_lineのFile armと同経路。Dirも同APIでexplorerが開く)。
    - Jump: 可視行position算出→可視なら snapshot行のIDプレフィックス照合→
      `SetCursorLine`。collapsed配下なら dirtyチェック→ `reveal_entry` →
      `SetLines { lines, cursor_line: Some(line) }` + `send_view_state`
      (カーソル追従スクロールはtree_view既存機構が自動処理: follow_cursor)。

## M11-1: 純ロジック+エンジン語彙(GUI・app配線なし)

- fyler-core: `search` モジュール新設(上記設計の全API+単体テスト)。
  `EditorCommand::SetCursorLine` / `EditorEvent::OpenFilePicker` 追加。
- fyler-engine-nvim: SetCursorLineのnvim実装(クランプ込み)、guard.rs `g/` map、
  engine.rs通知変換。headless_rpc.rsへ `g/` → OpenFilePicker 受信スモーク追加
  (pane_keymap_emits_split_actionと同形、NVIM_TEST_SERIAL + #[ignore])。
- fyler-gui: app.rs:235-260の網羅matchへ `OpenFilePicker => {}` 空arm追加のみ
  (コンパイルを通す。実装はM11-2)。
- fyler-app: `SaveController::reveal_entry` + `RevealResult` 追加(単体テスト付き)。
  イベント配線はM11-2(OpenFilePickerはcatch-all素通しのままでよい)。
- テスト(主要契約):
  - score順位: basename完全一致 / basename prefix / basename部分一致 / segment境界 /
    散在subsequence の順位関係、連続一致・開始位置の加点、case差(`Foo` で `foo.txt` が
    当たる)、AND token(`src main` で `src/main.rs`)、空query=元順先頭N件、0件、
    同点時の元順安定、日本語・非ASCII名。
  - `build_candidates` がbaseline表示順を保つこと・hidden除外がscan設定に従うこと
    (hiddenはscan時に除外済み=baselineに無い、の確認)。
  - `reveal_entry`: 可視(AlreadyVisible+正しい行)/1段collapsed/多段collapsed/
    NotFound/Busy、展開後の行indexが `visible_lines` と一致。
  - 50k候補の絞り込み性能: `#[ignore]` 付きテストで経過時間をログ出力
    (環境依存のためassertは緩く、例: 1秒以内)。
- 受け入れ: `cargo test --workspace` 全pass、clippy(-D warnings)ゼロ、
  headless RPCスモーク(実nvim)全pass。

## M11-2: GUI picker + app配線

- fyler-gui: `GuiAction` 導入(チャネル型変更5箇所+テストfixture)、
  `DialogState::FilePicker` + 描画・キー処理・TextEdit(上記設計)、
  `GuiEvent::ShowFilePicker`、IME geometryゲート、help行追加。
- fyler-app: ブリッジのfan-out(`GuiAction` → `AppEvent::Confirm` / `AppEvent::PickerSelect`)、
  `OpenFilePicker` ハンドラ(起動ゲート+候補構築)、`PickerSelect` ハンドラ
  (stale再解決 / Open合流 / Jump: SetCursorLine・ID照合・reveal+SetLines+send_view_state /
  dirty時reveal拒否)。
- テスト(主要契約):
  - 起動ゲート: dialog/apply/transfer/crashed/非Idleの各条件で拒否メッセージ、
    dirtyでは起動できること。
  - stale選択: baselineに無いEntryIdの選択が実行されず通知になること。
  - Jump: 可視行→SetCursorLine送信、collapsed配下→reveal+SetLines(cursor_line)+
    CollapsedDirs更新送信、dirty+collapsed配下→拒否、dirty+行ID不一致→拒否。
  - Open: ファイル/シンボリックリンク/ディレクトリでopen経路へ到達すること。
  - GUI: query更新でhitsが再計算されること、Esc/選択移動/Enter/Ctrl-Enterの
    キー割り当て、picker表示中に `forward_input` が呼ばれないこと(ゲート成立)、
    対象paneのRemovePane/EngineCrashedでpickerが閉じること
    (FylerAppロジックの単体テスト。既存testsパターン)。
- 受け入れ: `cargo test --workspace` 全pass、clippy(-D warnings)ゼロ、
  headless RPCスモーク全pass(M11-1分含む)。

## このフェーズでやらないこと

- migemo実装(`expand_query` のidentityフックまで。実装時はtoken→複数パターン展開)
- ファイル内容検索・内容プレビュー(M6ロードマップの後回し項目と同じ扱い)
- 全pane横断・複数root横断の検索(対象はactive paneのbaselineのみ)
- picker表示中の候補ライブ更新(候補は起動時スナップショット。外部変更は選択時の
  再解決でstale検出する)
- 外部fuzzyクレート(fzf-like matcher)導入 — 50k実測で線形scoreが不足した場合の後続判断
- `:`cmdlineからの起動コマンド(keymapカスタマイズ issue #9 と合わせて検討)
- picker内でのファイル操作(rename/delete等) — pickerは検索・移動・openのみ
