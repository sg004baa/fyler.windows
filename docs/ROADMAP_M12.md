# M12: apply後のundo(issue #8)

issue #8 コメント(2026-07-10)の対応方針を実装へ落とすための設計正典。
DESIGN.md・AGENTS.md 絶対ルールが常に優先。本書は issue コメントの方針を
現行コード(M11時点)へ接地させた決定事項を記す。

スコープ: issueコメントの **stage 1 + stage 2**。
- stage 1: create/copy/move/rename の session内undo(stale検知付き)
- stage 2: delete/overwrite 用 backup archive と永続journal(起動時復旧含む)

非スコープ(stage 3 = 後続):
- 複数transactionのundo stack(本実装は**直近1 transactionのみ**保持)
- backup の容量・期限管理
- transfer(`gm`/`gc`)のundo化 — transfer実行開始時に**全paneのundo slotをクリア**する
  (これを怠ると「最後のFS変更のundo」の意味論が壊れる)
- redo

## 絶対ルール1のスコープ明確化

- undo の実FS書き込み(逆操作・backup復元)は `fyler-fsops::apply` 配下の新API
  `apply_undo_cancellable` だけが行い、保存状態機械の `ApplyingUndo` 状態
  (=undo確認ダイアログ承認後)からのみ呼ばれる。絶対ルール1の「apply」に
  undo実行を含める(AGENTS.md更新は完了後のdocsコミットで行う)。
- backup payload の書き込み(delete/overwrite 対象の退避コピー)は forward apply
  実行の一部として fsops apply 内で行う(承認済みplanの副作用)。
- journal manifest(`manifest.toml`)は fyler-app が書く。`recent.toml` と同じ
  「アプリ自身のデータディレクトリ」カテゴリであり、表示ツリーへの書き込みではない。
  undo dir 解決: `FYLER_UNDO_DIR` env override → Windows `%LOCALAPPDATA%\fyler\undo`
  → unix `$XDG_STATE_HOME/fyler/undo` else `~/.local/state/fyler/undo`。

## 型(fyler-core/src/undo.rs 新設)

std のみ。derive は plan 型と同じ Debug/Clone/PartialEq/Eq。

```rust
/// volume + file の実体識別子。Windows: VolumeSerialNumber + FILE_ID_INFO(128bit)。
/// unix: dev + ino。
pub struct FileIdentity { pub volume: u64, pub file: u128 }

/// 属性のみのfingerprint。内容hashはしない(placeholder hydration防止)。
/// symlinkは link_target を照合に使う。
pub struct Fingerprint {
    pub kind: EntryKind,
    pub size: Option<u64>,          // Dir は None
    pub mtime: Option<SystemTime>,  // Dir は None(子変更で揺れるため照合に使わない)
    pub link_target: Option<PathBuf>, // Symlink のみ
}

/// ディレクトリ子孫のmanifest(Copy dir のundo可否判定用)。
pub struct ManifestEntry {
    pub rel_path: String,  // '/'区切り相対
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub mtime: Option<SystemTime>,
}

/// forward apply の成功1操作に対応する逆操作記述。パスはすべて絶対。
pub enum UndoStep {
    /// Create の逆: 作成物が未変更ならごみ箱へ。
    RemoveCreated { path: PathBuf, identity: Option<FileIdentity>, post: Fingerprint },
    /// Copy の逆: copy先が未変更ならごみ箱へ。Dir は manifest 照合。
    RemoveCopied { path: PathBuf, identity: Option<FileIdentity>, post: Fingerprint,
                   manifest: Option<Vec<ManifestEntry>> },
    /// Move/Rename の逆: to の実体が同一なら from へ戻す。
    MoveBack { from: PathBuf, to: PathBuf, identity: Option<FileIdentity>,
               post: Fingerprint, case_only: bool },
    /// Delete の逆: backup を元pathへ復元。
    RestoreDeleted { path: PathBuf, backup: BackupRef },
    /// overwrite退避の逆: 上書き前backupを target へ復元(対の逆操作が先に vacate する)。
    RestoreOverwritten { path: PathBuf, backup: BackupRef },
}

/// transaction dir 内の payload 相対パス参照。
pub struct BackupRef { pub payload_rel: String, pub kind: EntryKind }

/// forward apply 1回分。steps は実行順(undo時は逆順に処理)。
pub struct UndoTransaction {
    pub id: String,                 // "{unix_ms}-{pid}-{seq}"
    pub root: PathBuf,              // 表示用
    pub steps: Vec<UndoStep>,
    pub backup_dir: Option<PathBuf>, // payload を持つ場合のみ
}

/// preflight結果(表示用)。
pub enum UndoStepStatus { Ready, Rejected { reason: String } }
```

## stale検知の契約(fsops::undo::preflight_undo + 実行時再検証)

判定は preflight(ダイアログ表示用・read-only)と実行直前(正典)の2回。
identity が record 時に取得できなかった場合(None)は fingerprint のみで照合。
identity と fingerprint は**post-op時点・最終パス**で採取したものと比較する
(FATのfile ID不安定性はrename/move時のみで、未変更エントリでは安定)。

- RemoveCreated(File): identity一致 かつ size+mtime一致 → ごみ箱へ。不一致は拒否。
- RemoveCreated(Dir): 実在・Dir・identity一致・**空** → ごみ箱へ。
- RemoveCopied(File): RemoveCreated(File) と同じ。
- RemoveCopied(Dir): identity一致 かつ manifest一致(rel_path集合・kind・size・mtime。
  ディレクトリ自身のmtimeは照合しない)→ ごみ箱へ。
- MoveBack: to の identity一致(+ File は size+mtime、Symlink は link_target)
  → from が空いていることを確認 → 戻す。case_only は case.rs の2段rename。
  volume は classify::classify_move で再分類(クロスは copy+削除)。
- RestoreDeleted / RestoreOverwritten: path が空いている → backup から復元。
  占有されていれば拒否(RestoreOverwritten は対の MoveBack が拒否されると
  連鎖的に占有拒否になる = 正しいfail-safe)。
- 拒否は step単位。残りは続行し、結果は CommitReport<UndoStep> で操作単位報告。
- ごみ箱送りは必ず recycle 経由。復元failした実体はごみ箱に残っている旨を報告文に含める。

## receipt記録(fsops::apply 拡張)

- `apply_plan_cancellable(root, plan, overwrites, cancel, on_progress,
  recorder: Option<&mut UndoRecorder>) -> CommitReport` — シグネチャ拡張。
  CommitReport 型・順序契約は不変。recorder=None で従来挙動(テスト互換)。
- `UndoRecorder`(fsops::undo)は transaction dir を所有し、成功opごとに
  post-op identity/fingerprint を採取して UndoStep を実行順に蓄積。
- Delete: **backup完了後にのみ** recycle を実行。backupが失敗したら
  そのopは Failed とし元データへ一切触れない。
- overwrite退避(recycle_approved_target): 同様に backup → recycle。
  RestoreOverwritten step は退避時点(=対のopのstepより前)に記録する
  (逆順実行で MoveBack/Create/Copy の後に復元される)。
- 部分失敗・キャンセル時: 成功opのstepのみがtransactionに残る。
- identity/fingerprint 採取失敗は step を None付きで記録(undo不可にはしない)。

## 保存状態機械の拡張(fyler-core/src/save.rs)

追加variant(既存遷移・M0テストは不変):

```text
Idle + UndoRequested{transaction}
  → AwaitingUndoConfirmation{transaction} + [SetModifiable(false), ShowUndoConfirmDialog]
AwaitingUndoConfirmation + Approved
  → ApplyingUndo{transaction} + [ExecuteUndo]
AwaitingUndoConfirmation + Cancelled
  → Idle + [SetModifiable(true)]        // バッファはclean前提。KeepBufferDirtyなし
ApplyingUndo + UndoApplyFinished{report}
  → 全件失敗: Idle + [ShowUndoReport, SetModifiable(true)]
  → それ以外: Reconciling + [ShowUndoReport, ReconcileFromFs]
Reconciling + ReconcileFinished → Idle + [SetModifiable(true)]   // 既存arm共用
```

- `Reconciling { report }` の report フィールドは読み手が存在しない(検証済み)
  → **unit variant `Reconciling` へ変更**し forward/undo で共用する。
- `ApplyingUndo` 中の Cancel は Applying と同じく apply_cancel フラグ経由
  (on_choice の Applying arm を ApplyingUndo にも適用)。
- AwaitingUndoConfirmation 中の外部変更 → PlanInvalidated と同様に
  ダイアログ破棄・Idle復帰。transaction は slot へ返す(次回 :FylerUndo で再preflight)。
- is_idle() 系の判定・deferred changes・watcherの自己apply抑制は
  「Idle以外」判定に新stateが自然に含まれることで機能する。

## journal(fyler-app/src/undo_journal.rs 新設)

- WAL: `Preparing`(apply承認直後・worker起動前に書く)→ `Committed`
  (apply完了・receipt確定後)→ `Undoing`(undo承認直後)→ `Undone`(undo完了後、
  結果に関わらず。payload dir を purge — 復元fail分もごみ箱に実体があるため安全)。
- 配置: `<undo_dir>/<transaction-id>/manifest.toml` + `payload/<step-idx>/...`。
- manifest は手書き toml::Table(config.rs の record_recent_root と同型)、
  temp file + atomic rename。serde は導入しない。非UTF-8パスを含む場合は
  journal書き込みをスキップし通知(session内undoは可能なまま)。
- 起動時スキャン:
  - `Committed`(前セッション残骸)・`Undone` → 黙ってpurge(stage 3で跨セッション復元を検討)。
  - `Preparing`/`Undoing`(クラッシュ痕跡)→ 自動実行せず復旧ダイアログ:
    「破棄」= purge / 「保持して閉じる」= 残す(次回起動で再提示)。
- pane close 時: その pane の slot の journal を Undone 化 + purge。

## app配線(fyler-app)

- 起動コマンド: `:FylerUndo`(buffer-local user command → rpcnotify "fyler_undo"
  → `EditorEvent::UndoRequested`)。keymapなし。`:undo`/`u` の乗っ取りは**しない**
  (バッファundoと明確に分離)。
- ゲート(CommitRequested arm を模倣): apply_owner/dialog_owner/transfer中 → 拒否。
  crashed は既存ゲートで遮断済み。**dirty → 拒否**(undo完了後の reconcile が
  バッファを書き換えるため)。slot空 → 「undoできる操作がありません」。
- `PaneSession.undo_slot: Option<UndoTransaction>`(pane毎・直近1件)。
  apply完了(1件以上成功)で置き換え、全件失敗なら旧slot維持。
  transfer worker 起動時に全pane slot クリア(+journal purge)。
- undo worker: apply worker と同型(`apply_owner = Some(pane_id)` を立てる →
  deferred changes 機構がそのまま効く)。AppEvent に UndoProgress/UndoFinished を追加。
  完了armは ApplyFinished arm と同じく reconcile → deferred flush → git.request。
- 確認応答: SaveController が AwaitingUndoConfirmation を保持するため、
  既存 Confirm arm(dialog_owner → on_choice)がそのまま流れる。
  SaveFlowResult に StartUndo{transaction, cancel} / ShowUndoPlan / UndoReport 等を追加。
- plan_warnings 拡張: Delete のbackup見積(baseline meta sidecarの子孫size合算・
  不明サイズ明示)と placeholder hydration警告、overwrite対象(単発stat)を
  forward確認ダイアログに出す。

## GUI(fyler-gui)

- DialogState::UndoPlan(app側で整形済み行 + step毎の Ready/Rejected 理由)/
  UndoReport(CommitReport<UndoStep>)/ 起動時復旧ダイアログ。
- 進捗は既存 Progress dialog を共用(current は整形済みString)。
- draw関数は confirm.rs の draw_plan / draw_report を模倣(y/n/Esc)。

## セッション分割(codex、直列)

- **A: 基盤** — fyler-core undo.rs / fsops identity.rs / fsops backup.rs /
  apply.rs recorder配線(backup-before-recycle含む)
- **B: undo実行** — fsops undo.rs(preflight_undo / apply_undo_cancellable)+ 逆操作・
  stale拒否のテストマトリクス
- **C: 状態機械+コントローラ** — save.rs拡張 / editor.rs UndoRequested /
  guard.rs+engine.rs FylerUndo / save_flow.rs(request_undo等 + plan_warnings拡張)/
  gui app.rs の網羅match arm / headless RPCスモーク
- **D: app配線+journal+GUI** — undo_journal.rs / pane_runtime・main配線 /
  fyler-gui ダイアログ / 起動時復旧

## 検証ゲート(各セッション)

```
cargo fmt --all --check
cargo test -p fyler-core -p fyler-pipeline -p fyler-fsops -p fyler-app -p fyler-gui
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p fyler-engine-nvim --test headless_rpc -- --ignored
cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings
```

主なテスト(issueコメントのリストを接地):
- 各opのinverse、複数opの逆順実行、case-only rename、cross-volume MoveBack
- delete/overwriteでbackup完了前に元データへ触れないこと(backup失敗注入)
- apply部分成功/キャンセルから成功stepだけのtransactionが残ること
- file置換(identity不一致)、内容変更(size/mtime不一致)、dir子孫追加(manifest不一致)、
  復元先占有、の各stale拒否
- undo確認cancel・実行cancel・部分失敗・reconcile、AwaitingUndoConfirmation中の外部変更無効化
- journal WAL遷移・書込み途中クラッシュ相当(手動でPreparing残骸を作る)の起動時復旧
- バッファ`u`とFsUndoの非干渉(undoフローが SetLines/SetModifiable 以外の
  エンジンコマンドを発行しないこと)
