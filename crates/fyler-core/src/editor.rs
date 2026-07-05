//! エンジン非依存のエディタ抽象(DESIGN.md「EditorEngineトレイト」)。
//!
//! **絶対ルール2**: nvim固有のAPI・概念(keycode表記、msgpack-RPC、autocmd名等)を
//! このモジュールの型に持ち込まないこと。ここにある型はすべて、将来の方式B
//! (自前vimサブセット実装)でもそのまま使える語彙で定義する。

use std::sync::Arc;

/// 編集エンジンの抽象(snapshot + command channel型)。
///
/// - GUIスレッドはRPC完了を**同期待ちしない**。入力は [`EditorEngine::send`] で
///   channelへ投げ、描画は常に [`EditorEngine::snapshot`] の単一の整合した
///   スナップショットを使う
/// - lines / cursor / mode を別々に取得すると異なるrevisionが混ざるため、
///   必ずsnapshotとして一括で受け取る
pub trait EditorEngine: Send + Sync {
    /// 入力・コマンドをエンジンへ送る(ノンブロッキング)。
    /// エンジンタスクが終了している場合はエラー。
    fn send(&self, cmd: EditorCommand) -> anyhow::Result<()>;

    /// 現在のスナップショットを返す(ロックフリーで常に即座に返ること)。
    fn snapshot(&self) -> Arc<EditorSnapshot>;
}

/// エンジンへ送るコマンド(DESIGN.mdの `EditorCommand` そのまま)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorCommand {
    /// 通常のキー入力。
    Key(KeyInput),
    /// IME確定文字列・日本語入力。`Key` だけではWindows IME・デッドキーに
    /// 対応できないため、確定文字列はこちらで流す。
    Text(String),
    /// ペースト(NvimEngineでは `nvim_paste` 経由)。
    Paste(String),
    /// reconcile等でバッファ全体を差し替える。投入後は `dirty=false` に戻す。
    ///
    /// `cursor_line` は差し替え後にカーソルを置く0始まりの行番号。`None` なら
    /// 先頭行。折りたたみトグルのように「操作した行に留まる」べき差し替えで使う
    /// (行数を超える指定は最終行へクランプする)。
    SetLines {
        lines: Vec<EditorLine>,
        cursor_line: Option<usize>,
    },
    /// バッファの `modifiable` を設定する。保存フロー中のユーザー編集すり抜けを
    /// 防ぐ(状態機械 [`crate::save`] の`SetModifiable`効果の実行系)。
    SetModifiable(bool),
    /// `:w` 相当のトリガ(保存状態機械 [`crate::save`] の入口)。
    RequestCommit,
    Undo,
    Redo,
}

/// エンジン状態の整合スナップショット。GUIはこれだけを描画する。
#[derive(Debug, Clone)]
pub struct EditorSnapshot {
    /// Rust側で単調増加するリビジョン。
    pub revision: u64,
    /// エンジン側の変更カウンタ(NvimEngineでは `b:changedtick`)。
    /// 保存状態機械が「どの時点のバッファをplanしたか」の照合に使う。
    pub changedtick: u64,
    pub lines: Arc<[EditorLine]>,
    pub cursor: Cursor,
    pub mode: Mode,
    /// 未保存の変更があるか(`modified` 相当)。
    pub dirty: bool,
}

impl EditorSnapshot {
    /// エンジン起動前・初期化用の空スナップショット。
    pub fn empty() -> Self {
        Self {
            revision: 0,
            changedtick: 0,
            lines: Arc::from(Vec::new()),
            cursor: Cursor::default(),
            mode: Mode::Normal,
            dirty: false,
        }
    }
}

/// バッファの1行。生テキスト(IDプレフィックス含む)を保持する。
/// プレフィックスの解釈・隠蔽は [`crate::grammar`] 経由で行うこと。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorLine {
    pub text: String,
}

impl EditorLine {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// カーソル位置。**生のバッファ座標**(conceal補正前)。
///
/// - `line`: 0始まりの行番号
/// - `col`: 行内のUTF-8バイトオフセット、0始まり
///
/// GUIは描画時に表示列へ変換し、IDプレフィックスの隠蔽ぶんを
/// オフセット補正する(fyler-gui::conceal)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub line: usize,
    pub col: usize,
}

/// エディタのモード(エンジン非依存の語彙)。
/// NvimEngineが `mode()` の戻り値からこのenumへ正規化する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Replace,
    Visual,
    VisualLine,
    VisualBlock,
    OperatorPending,
    /// cmdline(`:` / `/` / `?`)入力中。内容は [`EditorEvent::CmdlineShow`] で届く。
    Cmdline,
    /// 未知のモード。正規化できなかった生のモード名を保持する(描画は生文字列)。
    Other(String),
}

/// キー入力(エンジン非依存)。GUI層がOS/eguiのキーイベントをこの形に正規化し、
/// NvimEngine内部(translateモジュール)でnvim keycode表記へ変換する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    pub key: Key,
    pub mods: Modifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// 印字可能文字(Shift適用済みの文字を入れる。`A` はShift+aではなく `Char('A')`)。
    Char(char),
    Enter,
    Esc,
    Backspace,
    Tab,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// ファンクションキー F1..=F12
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    /// 印字可能文字では通常 `Key::Char` に反映済みのため不要。特殊キー用。
    pub shift: bool,
}

/// エンジンからGUI/アプリ層へ届くイベント(エンジン非依存の語彙)。
///
/// NvimEngineでは BufWriteCmd / ext_cmdline / ext_messages / プロセス監視から
/// 生成されるが、その対応関係はエンジン実装の内部事情であり、ここには書かない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEvent {
    /// スナップショットが更新された(GUIは再描画すればよい)。
    SnapshotUpdated,
    /// ユーザーが指定行のエントリを開くよう要求した。`line` は0始まり。
    ///
    /// 行テキストの解釈と、ファイル・ディレクトリごとの動作選択はapp層が行う。
    ActivateLine {
        line: usize,
    },
    /// ユーザーが指定行のエントリの絶対パスをコピーするよう要求した。
    /// `line`は0始まり。行の解釈とパス解決はapp層が行う。
    YankPath {
        line: usize,
    },
    /// ユーザーが現在の表示ルートの親ディレクトリへの移動を要求した。
    NavigateParent,
    /// ユーザーが隠しファイル表示の切り替えを要求した。
    ToggleHidden,
    /// ユーザーが保存(`:w` 相当)を要求した。`lines` は保存要求時点のsnapshotに
    /// 属する行で、後続編集で更新されたsnapshotを誤ってplanしないため同梱する。
    /// [`crate::save::SaveEvent::CommitRequested`] へ接続する。
    CommitRequested {
        changedtick: u64,
        lines: Arc<[EditorLine]>,
    },
    /// cmdline表示の更新(`:` / `/` 入力中の内容)。GUIが自前描画する。
    CmdlineShow(CmdlineState),
    CmdlineHide,
    /// エディタからのメッセージ(例: `E486: Pattern not found`)。GUIが自前描画する。
    Message(EditorMessage),
    /// エンジンプロセスのクラッシュ・異常終了(M1: GUIに通知して操作を止める)。
    EngineCrashed {
        reason: String,
    },
}

/// cmdlineの表示状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdlineState {
    /// プロンプト文字(`:`, `/`, `?`)。
    pub prompt: char,
    pub content: String,
    /// content内のカーソル位置(バイトオフセット)。
    pub cursor: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorMessage {
    pub kind: MessageKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Info,
    Warn,
    Error,
}
