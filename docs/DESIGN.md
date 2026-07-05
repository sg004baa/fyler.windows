# fyler for windows — 実装設計書 v2（Claude Code向け）

> v2 (2026-07-03): 外部レビューを反映した全面改訂。主な変更点: (1) 行ID追跡をextmarkからin-buffer ID方式（oil.nvim方式）へ変更、(2) 保存処理を明示的な状態機械化、(3) EditorEngineをsnapshot + command channel型へ再設計、(4) parse/validateパイプライン追加、(5) M0スパイク追加。

> このファイルは本プロジェクトの**正典**。Notionページ「idea > fyler for windows」の設計書 v2 のコピー。実装判断に迷ったら必ずここに従うこと。

## 概要

fyler.nvim（ツリー表示のファイルシステムをバッファのように編集するNeovimファイラー）のコンセプトを、Windowsネイティブの**スタンドアロンGUIファイラー**としてRustで実装する。

- 編集エンジンとして組み込みNeovimを利用する（**方式A**）。Neovimは「Vim編集状態マシン」としてのみ使い、描画エンジンとしては使わない
- neovide方式（UIグリッドをそのまま描画）は採用しない。**描画はすべてRust側で自前**
- 将来、自前vimサブセット実装（方式B）へ移行できるよう、編集エンジンをトレイトで抽象化する

## 脅威モデル（先に確定）

本アプリの防御対象は**「ローカルの信頼済みユーザー自身の誤操作」のみ**とする。

- `:lua os.remove(...)` や `:!del` のような意図的な破壊操作への防御はスコープ外（PowerShellを開けば同じことができるため、防衛対象ではない）
- cmdline（`:` / `/`）は**ユーザーに開放する**。`:%s/old/new/` によるバルクリネームは本アプリの中核機能であり、Rust製ミニcmdlineへの置き換えは行わない
- 防御するのは「知らないうちに想定外の画面状態・保存経路に入ること」。詳細は「事故防止」の章

## アーキテクチャ

レイヤー構成:

1. **GUI層**: egui / eframe。ツリー描画、カーソル・モードライン・cmdline描画、確認ダイアログ
2. **EditorEngine層**: トレイトで抽象化。初期実装は `NvimEngine`（nvim-rs + tokio によるmsgpack-RPC）
3. **Parse / Validate / Diff層**: バッファテキスト → DesiredTree → OperationPlan
4. **FsOps層**: Windowsファイル操作の実行

### EditorEngineトレイト（snapshot + command channel型）

UIスレッドがRPC完了を同期待ちしない構造にする。GUIは常に**単一の整合したsnapshot**を描画し、入力はchannel経由でNvimEngineタスクへ送る。lines / cursor / modeを別々のRPCで取得すると異なるrevisionの状態が混ざるため、snapshotとして一括で受け取る。

```rust
trait EditorEngine: Send + Sync {
    fn send(&self, cmd: EditorCommand) -> anyhow::Result<()>;
    fn snapshot(&self) -> Arc<EditorSnapshot>;
}

struct EditorSnapshot {
    revision: u64,      // Rust側で単調増加
    changedtick: u64,   // nvimのb:changedtick
    lines: Arc<[EditorLine]>,
    cursor: Cursor,
    mode: Mode,
    visual_start: Option<Cursor>,
    dirty: bool,
}

enum EditorCommand {
    Key(KeyInput),   // 通常のキー入力
    Text(String),    // IME確定文字列・日本語入力
    Paste(String),   // ペースト（nvim_paste経由）
    RequestCommit,   // :w相当のトリガ
    Undo,
    Redo,
}
```

- `Key` だけでは Windows IME・日本語入力・デッドキーに対応できないため、確定文字列は `Text` として流す
- **重要**: nvim固有のAPI・概念をこのトレイト境界の外に漏らさないこと。方式B移行時に差し替えるのは `NvimEngine` 実装のみ

## 行ID追跡 — in-buffer ID方式

**extmarkは使用しない。** extmarkは周囲のテキストが削除されても消えず境界へ寄るだけで、`dd` したテキストと一緒にレジスタへ格納されない。`dd → p` で行に追従しないため、行移動の追跡には使えない。

代わりに**IDをバッファテキスト自体に埋め込む**（oil.nvimと同方式）。IDが文字列の一部であるため、`dd`/`p`/`yy`/`:m`/`:s`/visual mode/マクロなど**あらゆるVim操作で自動的にIDが行に付いて回る**。remapによるモグラ叩きが不要になる。

### 行フォーマット

```
/012 src/
/013   main.rs
/014   lib.rs
新規ファイル.txt
```

- 各行は `/{id} ` プレフィックス + インデント + 名前
- ユーザーが新規に打った行にはIDがない → CREATE候補
- GUI描画時に `/{id} ` プレフィックスを**完全に隠蔽**する（concealではなくRust側描画なので漏れない）。カーソル列はプレフィックス長ぶんオフセット補正して表示する
- カーソルがプレフィックス領域に入らないよう、可能な範囲でカーソル位置を補正する（M0で検証）

### diff判定ルール

| バッファの状態 | 操作 |
|---|---|
| ID一致・名前/親ディレクトリが変化 | RENAME / MOVE |
| baselineに存在したIDがバッファから消滅 | DELETE |
| IDのない行 | CREATE |
| 同一IDが複数行に出現（yy→p） | 1つを元位置とみなし、残りは COPY |

- ID重複 = COPY として扱えるため、`yy → p` がコピー操作として自然に表現できる（in-buffer方式の副産物）
- IDプレフィックスが部分的に破壊された行（例: `/0`だけ残っている等）は**validateエラーとして保存を中断**する。曖昧な状態から操作を推測して実行しない
- この方式はエンジン非依存のため、方式B移行時もそのまま流用できる

## 保存処理の状態機械

`BufWriteCmd` はハンドラ自身が書き込みを完了させる前提のイベントであり、`rpcnotify` を投げるだけでは `:w` が即座に「完了」してしまう。確認ダイアログ中の編集・再保存・部分失敗の扱いが未定義になるため、保存を明示的な状態機械にする。

```
Idle
  → Planning(changedtickスナップショット取得, modifiable=false)
  → AwaitingConfirmation(plan表示)
  → [承認] Applying → Reconcile → Idle
  → [キャンセル/失敗] modifiable=true, dirtyのまま → Idle
```

ルール:

- `BufWriteCmd` 発火時に changedtick 付きでバッファ全体をスナップショットし、直ちに `modifiable=false` にする（確認中の編集と再`:w`を構造的に防ぐ）
- **承認 → Applying**: OperationPlanを実行
- **Reconcile**: 実ファイルシステムを再スキャンし、バッファを実FSの状態から再構築、baselineを更新、`modified=false` に設定
- **キャンセル / 全件失敗**: バッファを `modifiable=true` に戻し、dirtyのまま。baselineは更新しない
- **部分失敗**: CommitReportで成功/失敗を操作単位で提示し、実FSを再スキャンしてbaselineとバッファを実際の状態へreconcileする
- `BufWriteCmd` に加えて `FileWriteCmd` / `FileAppendCmd`（部分書き込み・別名書き込み）、`BufFilePre`（`:file` / `:saveas`）もハンドルし、同一経路へ誘導またはエラーにする

## パイプライン

バッファから直接diffせず、中間表現を挟む:

```
編集後バッファ → parse → DesiredTree → validate → OperationPlan → apply → CommitReport → reconcile
```

### バッファ文法の決定事項

- nvimバッファに置くのは「**IDプレフィックス + インデント + 編集可能な名前**」のみ。アイコン・git status・ツリー罫線はバッファ文字列に含めず、Rust側の描画装飾とする
- インデント: 半角スペース2個 = 1階層
- ディレクトリは末尾 `/` で区別する
- 空行は無視する（validateで警告なしにスキップ）
- collapsedなディレクトリの子孫はバッファに存在しない。collapsed行のrename/moveは**子孫ごと**の操作として扱う
- ディレクトリ行の移動 = 子孫ごとの移動

### validateで弾くもの

- 同一ディレクトリ内の名前重複
- ディレクトリの自分自身の子孫への移動
- Windows予約文字（`< > : " / \ | ? *`）、予約名（CON, PRN, AUX, NUL, COM1-9, LPT1-9）、末尾のスペース・ピリオド
- 破壊されたIDプレフィックス
- symlink / junction / reparse pointはMVPでは**追跡せず、リンク自体を1エントリとして扱う**（中に潜らない）

## nvim統合の詳細

### プロセス管理

- 起動コマンド: `nvim --embed --headless -u NONE -i NONE --noplugin`
  - `--embed` 単体はUI attachを待ってブロックするため、`--headless` を併用して非UI embedderとして起動する
  - `--clean` はユーザー設定を除外するが組み込みプラグインはロードするため、`-u NONE -i NONE --noplugin` で挙動を完全固定する
- spawn時に `CREATE_NO_WINDOW` フラグを必ず付ける（コンソールの一瞬の表示を防ぐ）
- プロセスのクラッシュ・異常終了を検知し、GUIに通知する（M1で実装）
- 配布時は `nvim.exe`（Apache 2.0、約15MB）の同梱を前提とする
- **Neovimとnvim-rsのバージョンは固定する**（nvim-rsはAPIをunstableと明言している）。RPCクライアント部分は薄いモジュールに隔離する

### UI attach / ext_\*

- 起動後、`ext_cmdline` のイベントを取るために `nvim_ui_attach` を最小グリッドサイズで後から実行する（attach自体は後から可能な構造にする）
- 有効化するext_*は最初は **`ext_cmdline`** と **`ext_messages`** の2つ。grid描画を無視する以上、ext_messagesなしでは `E486: Pattern not found` 等のメッセージがgridに描かれて見えなくなる。`ext_popupmenu` は補完UIを描画する段階で追加する。全ext ONにはしない
- grid系描画イベントはすべて無視する

### バッファ設計

- バッファ名は `filer://C:/Users/...` 形式の架空URI、`buftype=acwrite`
- `:w` → `BufWriteCmd` → 保存状態機械へ（前章）
- 行内容は `nvim_buf_attach` によるプッシュ通知（`nvim_buf_lines_event`）で同期
- mode / cursor / changedtick はキー送信ごとに `nvim_call_atomic` で一括取得し、snapshotとして原子的に更新する

### 事故防止（想定外画面遷移の防止）

- `<CR>`（ファイルを開く）等のアクションはバッファローカルmapで `rpcnotify` に差し替える
- 想定外のバッファ（`gf` や `:e 実パス` によるもの）が開かれたことを autocmd で検知したら即座に閉じ、fylerバッファへ戻す
- 保存系autocmd（`BufWriteCmd` / `FileWriteCmd` / `FileAppendCmd` / `BufFilePre`）を網羅的にハンドルする
- 悪意ある操作（`:lua` 等）の防御はスコープ外（脅威モデル参照）

## Windowsファイル操作層（FsOps）

### 操作種別の内部分類

`std::fs::rename` は別ボリューム間で失敗する。MoveFileExWもディレクトリは同一ドライブが必要。操作を内部的に3分類する:

- `SameVolumeRename` — 原子的
- `CrossVolumeFileMove` — copy + delete。非原子的
- `CrossVolumeDirectoryMove` — 再帰copy + delete。非原子的で途中失敗時の挙動が異なる

非原子的な操作は途中失敗時に「どこまで完了したか」をCommitReportに含める。

### その他の対応事項

- **削除**: `trash` クレートでごみ箱へ（IFileOperation COM APIへの置き換えも検討可。その場合は**専用のCOM STAスレッド**が必要。tokioのワーカースレッドに直接投げられない）
- **case-onlyリネーム**（`Foo → foo`）: temp名経由の2段rename。ただしWindowsはディレクトリ単位でcase-sensitiveにできるため、衝突判定は対象ディレクトリの実際のcase sensitivityルール（`FILE_CASE_SENSITIVE_DIR`）に合わせる
- **長いパス**: `\\?\` prefixは絶対パス専用で `.` `..` `/` の解釈も変わる。アプリmanifestに `longPathAware` を入れ、パス変換ロジックはFsOps内部の1か所に閉じ込める
- **ロック中ファイル**: エラーは操作単位で報告。全体ロールバックはしない。部分成功を明示する
- **OneDriveプレースホルダ**: `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` 等の属性を確認し、サイズ取得やプレビューで不要なhydration（リモート取得）を発生させない
- **外部変更検知**: `notify` クレートで監視 → ツリー再描画。編集中バッファがdirtyの場合は上書きせず通知のみ

## GUI（egui）

- ツリー描画・ファイルアイコン・git status・インデントガイドはRust側で実装（バッファ文字列には含めない）
- モードライン（NORMAL / INSERT / VISUAL）・カーソル・cmdline・messagesの描画も自前
- IDプレフィックスの隠蔽とカーソル列オフセット補正を描画層で行う

## マイルストーン

**M0: 成立性スパイク（実装前に必ず通す）**

- in-buffer ID方式の検証: `dd`/`p`/`yy`/`p`/`:m`/`:s`/undo/redoでIDが行に追従すること、カーソル列補正と描画隠蔽が破綻しないこと
- `--embed --headless` 起動 + 後付け `nvim_ui_attach` + ext_cmdline / ext_messages のイベント疎通確認
- Windows IME（日本語入力）の入力経路確認（`EditorCommand::Text`）
- 保存状態機械の遷移をコードで確定
- バッファ文法（本書の決定事項）の最終確定

**M1: read-only表示**

- nvim spawn、RPC疎通、snapshot取得、ツリーのread-only描画
- nvimプロセスのクラッシュ・終了検知

**M2: rename限定dry-run（背骨）**

- 既存行の行内renameのみ対象
- `:w`（BufWriteCmd）→ parse → validate → OperationPlan を確認ダイアログに表示。**実行はしない**
- 「`i` でrenameを書いて `:w` するとダイアログに `RENAME a → b` が出る」がゴール

**M3: create / delete / rename 実行**

- 同一ボリューム限定。削除はごみ箱経由
- 部分失敗時のCommitReport
- commit後、実FSから再読込してreconcile

**M4: 構造編集**

- move / copy（ID重複 = COPY判定）
- クロスボリューム対応（3分類の実装）

**M5: 統合・装飾**

- notify監視、OneDrive属性対応、アイコン、git status

## 実装上の絶対ルール

1. 確認ダイアログの承認なしに実ファイルへ触れないこと（M2まではdry-runのみ）
2. nvim固有API・概念を `EditorEngine` トレイト境界の外に漏らさないこと
3. パス変換（`\\?\` 等）はFsOps内部の1か所に閉じ込めること
4. マイルストーンを順番に通すこと。M0のスパイク結果が出るまでM1以降の実装を始めないこと

## 主要クレート

| 用途 | クレート | 備考 |
|---|---|---|
| nvim RPC | nvim-rs | tokio前提。APIはunstableのためバージョン固定 |
| 非同期ランタイム | tokio | nvim-rsの要件 |
| GUI | egui / eframe | プロトタイプ速度優先 |
| ごみ箱削除 | trash | IFileOperation移行時はSTAスレッド必須 |
| ファイル監視 | notify | 外部変更検知 |
| Win32 API | windows | CREATE_NO_WINDOW、属性取得、case sensitivity判定等 |

## リスクと撤退ルート

- in-buffer ID方式はエンジン非依存のため、方式B（ropey + 自前modalキーマップ）へ移行してもdiff層はそのまま流用できる
- snapshot + command channel型のトレイトは方式Bでも同じ形で実装可能（同期API型より意味論の互換が取りやすい）
- 方式Bが必要になる条件: 単一バイナリ配布が絶対条件になった場合、またはRPC往復の遅延が許容できない場合のみ
- M0スパイクでin-buffer IDのカーソル補正が破綻した場合: IDプレフィックスを固定幅にする、またはカーソル移動をフックして補正するfallbackを検討する
