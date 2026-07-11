# fyler for windows — pane(分割ウィンドウ)フェーズ計画(M10)

> 2026-07-10 作成。[issue #6](https://github.com/sg004baa/fyler.windows/issues/6) 対応。
> 設計の正典は [DESIGN.md](DESIGN.md)。矛盾した場合はDESIGN.mdを優先する。
> issue #6 コメントの対応方針を4クレート実地調査で机上検証した結果、大筋妥当。
> 本書は検証で見つかった補正を織り込んだ実装計画。

> **2026-07-10 実装済み**(3セッション、codex実装。コミット: aa2423d / cc85f2c / 77804cd)。
> Linuxゲート: cargo test --workspace 275件pass、workspace clippy(-D warnings)警告ゼロ、
> headless RPCスモーク9件pass(実nvim v0.12.3)。**Windows実機動作確認は未実施**。

## 方針検証の結論(2026-07-10)

### 採用(コメント通り)

- **1 pane = 1 `NvimEngine`(独立プロセス)+ 1 `PaneSession`**。
  根拠: fyler-engine-nvim にグローバル状態なし(static/OnceLock 0件)、RPCは子プロセス
  stdio直結(`nvim_rs::create::tokio::new_child_cmd`)で名前・ポート衝突が構造的に不在、
  tokio runtimeは呼び出し側所有で共有可(engine.rs / main.rs)。
  1プロセス複数buffer化は (a) snapshot取得が `buffer 0 / window 0` 前提(query_status)、
  (b) BufEnterガードがfyler外バッファを能動削除、(c) buffer_lines_eventが通知元buffer未検査、
  の3点で `EditorEngine` 境界の大改修になるため後続最適化へ回す。最大4 pane。
- **`OperationPlan` へ混ぜず `TransferPlan` を新設**。
  根拠: `OperationPlan` はroot情報を持たず(plan.rs)、`TreePath` はroot相対。
  preflightのvacatedキーもroot相対のcase-fold文字列でありroot跨ぎでキー衝突する。
  一方 `classify_move` / apply.rs内copy系 / `recycle` / `long_path` は絶対パスベースで
  root非依存 — transfer内部でそのまま共用できる。
- **`<C-w>` 系はエンジン内keymapで捕捉し `EditorEvent::PaneAction` へ変換**。
  normalモードのbuffer-local mapなのでinsertモードの `<C-w>`(単語削除)を壊さない。
  なお現状 `<C-w>` は未ブロックで、translate.rs が Ctrl+W → `<C-w>` を透過させるため
  実nvim windowが割れる(=snapshotの`window 0`前提が揺らぐ)穴がある。本対応はguard穴塞ぎを兼ねる。
- 実行フロー「source選択 → target決定 → 両root preflight → 確認 → lock → worker apply →
  両root reconcile」、dirty/busy paneのtransfer拒否、apply全体1本制約、いずれも妥当。

### 補正(コメントから変更)

1. **ID採番はpane内で閉じない — セッション共有の `IdAllocator` にする**。
   in-buffer ID(`/012 `)がpane間で別エントリを指しうると、diff層は既知IDを
   **無関係エントリのMove/Copyとして誤解釈**する(diff.rs は unknown ID を黙ってcontinue、
   validateにもUnknown-IDエラーなし)。共有カウンタならエイリアシングが構造的に消える。
   なお pane=別nvimプロセス=レジスタ非共有のため、pane跨ぎ `yy`→`p` 自体が不可能。
   transferは明示操作のみとするコメントの方針と整合する。
2. **`GitRefresher` はpane別化必須**。現行は inflight/queued 各1スロット+
   `AppEvent::GitStatus{root}` のroot等値相関で、同一rootの2paneや連続要求時に
   結果が黙って消える/取り違える。GitStatusへPaneIdを載せ、スロットをpane別に持つ。
3. **transfer preflightに絶対パスの祖先/子孫チェックを追加**。paneのrootは入れ子・重複
   しうるため、(a) dirの自分自身の子孫へのmove/**copy**(copyは無限再帰の実害)、
   (b) target paneのroot自体を消す・巻き込むmove(`from` が `to_root` の祖先or同一)を
   blockedにする。
4. **engine crashはpane単位に降格**。現行のグローバルengine_error(全pane停止)を、
   当該paneのみ操作不能表示+close可能へ。最後の1paneのクラッシュのみ従来通り全体エラー。
5. **spawn直列化**。並列spawnのflaky実績あり(tests/headless_rpc.rs の NVIM_TEST_SERIAL)。
   pane追加は1つずつ、engine起動+初期scan+watcher作成の全成功後にlayoutへ追加、
   途中失敗は生成済みリソースを破棄してメッセージ表示(部分paneを作らない)。

段階分けはコメントの3段階を微修正: 「(2) paneごとのroot移動/watch/git/保存フロー」は
PaneSessionが全状態を所有した時点でほぼ自動的に成立するためM10-1へ吸収し、
代わりにtransferを純ロジック(M10-2)と配線(M10-3)へ分割する
(Linux単体テストで先行検証可能にするため)。

## 設計

- `PaneId`(不透明u64) / `PaneLayout`(二分木: leaf=PaneId、node=分割方向+比率) /
  `PaneAction`(SplitHorizontal / SplitVertical / FocusLeft / FocusRight / FocusUp /
  FocusDown / FocusNext / FocusPrevious / Close)は **fyler-core** に置く(std-onlyの純データ。
  AGENTS.md「新しい共有型はfyler-coreへ」)。nvim語彙はfyler-engine-nvim内で消す。
- fyler-appに `PaneSession { id, root, engine, save_controller, watcher, git装飾状態,
  deferred_changes, ... }`。**イベントループ1本は維持**し、`AppEvent` の
  Editor / ExternalChange / GitStatus / ApplyProgress / ApplyFinished にPaneIdを付与。
  tag付けは各ブリッジスレッド(engine毎・watcher毎)が行う。ConfirmはグローバルのままでOK
  (ダイアログ所有PaneIdをapp側で記録して振り分け)。
- appが layout + active pane の正典を持ち、GUIへは GuiEvent でミラー
  (pane追加時にそのpaneの `Arc<dyn EditorEngine>` を渡す)。GUIはleafごとに
  snapshot / scroll viewport / 装飾mapを保持・描画し、入力(IME含む)は
  **active paneのengineのみ**へ送る。tree_viewのScrollAreaは `id_salt(pane_id)`。
  active paneは枠線等で視覚表示。
- モードラインはpane毎に描画。cmdline・メッセージ・確認/進捗ダイアログはグローバル1個。
  モーダル中の既存入力ゲートが全pane入力を止めるため、保存フローの同時進行は
  構造的に直列化される(pane Bはダイアログ表示中に `:w` を打てない)。
- applyはworkspace全体で同時1本(pane内save applyもtransferも同じ制約)。
  apply/transfer実行中は関係の有無を問わず新規save承認・transfer開始を拒否する。

## M10-1: pane基盤(分割・focus・close・複数ツリー描画・イベントPaneId化)

- fyler-core: `PaneId` / `PaneLayout` / `PaneAction` 追加。layoutは
  split(active, 方向) / close(id) / focus隣接解決(方向は分割木の幾何から近似) /
  leaf列挙 を純関数で提供し、単体テストを付ける。
- fyler-engine-nvim: guard.rsのexec_lua内で `<C-w>` をnormalモードbuffer-local mapし、
  `getcharstr()` で後続1文字を読んでディスパッチ(`s`/`S`=横split、`v`=縦split、
  `h/j/k/l`=方向focus、`w`=次、`p`=前、`q`/`c`=close、`<C-w>`=`w`と同義)。
  既知キーは `rpcnotify(ch, 'fyler_pane', action)`、未知キーは既存の
  fyler_action_blocked と同様に無害通知。engine.rsで `EditorEvent::PaneAction` へ変換。
  headless RPCスモークに `<C-w>s` → PaneAction受信を追加。
- fyler-app: `PaneSession` 導入、既存シングルトン(root / engine / save_controller /
  watcher / git / deferred_changes / 装飾)を全て移設。`panes: BTreeMap<PaneId, PaneSession>`
  + `layout` + `active` + `last_active`。AppEventへのPaneId付与と全イベントアームの
  PaneId→session解決への書き換え。`IdAllocator` は `Arc<Mutex<IdAllocator>>` で全pane共有。
  GitRefresherのinflight/queuedをpane別スロット化し、結果相関をroot等値からPaneIdへ変更。
  ExternalChangeの合流(try_recv)とdeferred_changesはpane別に。
  recent.toml記録・bookmarksは共有のまま。
- pane追加(split): active paneのrootを複製し、runtime上で新engine起動→初期scan→
  watcher作成の全成功後にlayoutへ追加しfocus移動。5 pane目は拒否メッセージ。
- pane close: `snapshot().dirty` または save状態がIdle以外、またはapply実行中は拒否。
  最後の1 paneは閉じない。成功時はPaneSessionをdrop(engineは既存の正常終了経路、
  watcherはDrop join)し、layoutから除去、siblingへfocus。
- crash: `EngineCrashed` を受けたpaneはerror表示+入力遮断+close許可。
  全paneがcrashしたら従来のFatalError。crashしたpaneがダイアログ所有中なら閉じる。
- fyler-gui: `FylerApp` を pane別状態(engine / snapshot描画 / viewport /
  last_cursor_line / git_badges / file_infos / collapsed_dirs / root / cmdline)の
  mapへ再構成し、`PaneLayout` を再帰描画。入力転送はactive paneのみ。
  ダイアログ・メッセージ・helpはグローバルのまま。
- テスト: layout純関数(split/close/focus)、close拒否条件、PaneAction変換
  (headless RPC)、GitRefresher pane別ルーティング、イベントがpane間で混ざらないこと
  (save_flow系の既存テストパターン踏襲)。

## M10-2: TransferPlan 純ロジック(FS配線なしでLinux検証可能)

- fyler-core: `TransferKind { Move, Copy }`、
  `TransferOp { kind, from: TreePath, to: TreePath, entry_kind }`、
  `TransferPlan { from_root: PathBuf, to_root: PathBuf, ops }`。
  v1のtransferは常に「1 source pane → 1 target pane」なのでrootはplanレベルで持つ
  (コメントはop毎rootだが、現時点で差が出ず単純な方を取る)。
- fyler-fsops: `preflight_transfer(&TransferPlan) -> TransferPreflight`。絶対パスで
  (a) 移動先衝突: 既存ファイル/symlink→overwritable(承認+ごみ箱退避対象)、
  既存ディレクトリ→blocked、(b) 自己子孫へのmove/copy→blocked、
  (c) `from` が `to_root` の祖先or同一のmove→blocked、(d) source消滅→blocked。
  plan順シミュレーションでvacated除外(キーは絶対パスのcase-fold。
  対象dirのcase sensitivity実測は既存ヘルパー流用)。読み取り専用・hydration非誘発。
- fyler-fsops: `apply_transfer_plan_cancellable(&TransferPlan, overwrites, cancel,
  on_progress) -> CommitReport`。既存private copy/move/recycleヘルパーを
  pub(crate)昇格して共用(`OpFailure` の可視性整理を含む)。Moveは `classify_move` で
  同一volume rename / cross-volume copy+delete を分岐。overwrite承認済み衝突は
  実行直前にごみ箱退避(M9-3と同じDelete規律・TOCTOU再確認)。
  cancel判定と進捗通知は操作間(既存 `apply_plan_cancellable` と同契約)。
- 順序契約: 親dirをmoveすると子孫の絶対パスが変わるため、v1は
  **選択の親子孫重複を除外(最上位祖先のみ残す)**した上でop間の依存を持たない
  平坦なplanとする(同一transfer内でopが他opのfrom/toに干渉する構成はpreflightでblocked)。
- テスト: 衝突各分類、自己子孫move/copy、to_root巻き込みmove、vacated除外、
  同一/クロスvolume分岐(クロスはLinuxではtmpfs等が無ければ単体で分類関数をモック不可のため
  分類器の呼び出し契約テストに留めてよい)、部分失敗のCommitReport、cancel。

## M10-3: transfer UX配線

- keymap(fyler-engine-nvim guard.rs、変更容易な placeholder として):
  normal/visual `gm`=active paneの行(範囲)を**move**、`gc`=**copy**。
  rpcnotify → `EditorEvent::TransferRequested { kind, lines }`(エンジン非依存)。
- target解決(fyler-app): target pane = `last_active`(vimの `wincmd p` 相当。
  2 paneなら他方)。paneが1つならメッセージで拒否。destination dir =
  target paneのカーソル行がdirならそのdir、fileならその親、空バッファならroot。
- 開始ゲート: source/targetのどちらかが dirty / save状態Idle以外 / crash /
  apply・transfer実行中 → 拒否メッセージ。選択行にID無し行(未保存の新規行)が
  含まれる場合も拒否(dirtyゲートで実質カバーされるが明示チェック)。
- 選択解決: 行→EntryId→baseline→TreePath。親子孫重複は最上位のみ採用。
- フロー: preflight_transfer → 確認ダイアログ(`MOVE a → [pane 2] b` 形式、
  overwrite警告はM9-3書式)→ 承認で両paneのバッファをlock(SetModifiable(false))+
  transfer実行中フラグ → workerで `apply_transfer_plan_cancellable`(進捗ダイアログ+
  操作間キャンセル)→ 完了で両paneを実FSから `rescan_preserving_ids_with` でreconcile
  (collapsed維持・SetLines・baseline差替)→ unlock。
  `fyler_core::save` 状態機械は**無変更**(transferは:w経路と独立のapp層フロー。
  両paneのSaveControllerがIdleであることを開始条件で保証)。
- watch: transfer実行中は両pane(および他pane含む全pane)の外部変更をdeferredへ退避し、
  完了後flush(reconcile済みでNoChangesに畳まれる=既存の自己apply抑制と同じ機構)。
- テスト: target/destination解決、開始ゲート各条件、親子孫除外、
  確認→承認→report→両pane reconcileの一連(save_flow系テストパターン)、
  headless RPCで `gm`/`gc` → TransferRequested 変換。

## 完了条件

- `<C-w>s/v` で分割(最大4)、`<C-w>h/j/k/l/w/p` でfocus移動、`<C-w>q/c` でclose
  (dirty/busy/最後の1paneは拒否)できる
- 2 paneが独立に編集・保存・root移動・watch・git装飾を持ち、イベントが混ざらない
- pane間 move/copy が確認→非同期apply→両pane reconcileまで一貫して動き、
  dirty/busy時は開始できず、衝突はpreflightで裁かれる
- 既存の単一paneフロー(rename→:w→確認→apply→reconcile)の契約が退行しない

## このフェーズでやらないこと

- 1 nvimプロセス複数buffer化(メモリ実測で問題化した場合の後続最適化)
- pane間のvimレジスタ共有(プロセス分離のため構造的に不可。transferは明示操作のみ)
- レイアウトの保存・復元、pane毎の設定差分、`<C-w>=` 等のリサイズ操作
- transfer中の操作内(ファイル単位未満)キャンセル(M9-4と同じ割り切り)
