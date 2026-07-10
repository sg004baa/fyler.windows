//! エンジン非依存のエディタ抽象(DESIGN.md「EditorEngineトレイト」)。
//!
//! **絶対ルール2**: nvim固有のAPI・概念(keycode表記、msgpack-RPC、autocmd名等)を
//! このモジュールの型に持ち込まないこと。ここにある型はすべて、将来の方式B
//! (自前vimサブセット実装)でもそのまま使える語彙で定義する。

use std::sync::Arc;

use crate::pane::PaneAction;

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
    /// Visual系モード中の選択起点(0始まり)。Visual系モード以外では`None`。
    pub visual_start: Option<Cursor>,
    /// 未保存の変更があるか(`modified` 相当)。
    pub dirty: bool,
    /// 現在ハイライトすべき検索状態(`/` 検索 + hlsearch/incsearch)。
    /// GUIはこれを使って可視行のマッチを描画する。検索なし・`:noh`後は`None`。
    pub search: Option<SearchHighlight>,
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
            visual_start: None,
            dirty: false,
            search: None,
        }
    }
}

/// 現在の検索ハイライト状態(エンジン非依存)。
///
/// - `pattern`: マッチさせる文字列。GUIはこれを**リテラル部分文字列**として
///   可視行に照合する(ファイル名検索用途。full Vim regexは解釈しない)。
///   `\c` / `\C` の大文字小文字フラグは [`SearchHighlight::resolve`] で
///   解釈・除去済み
/// - `case_sensitive`: smartcase / ignorecase / `\c` / `\C` を解決した実効値
///
/// nvim語彙(`@/` / `v:hlsearch` / `&smartcase`)には触れない。エンジン実装が
/// それらを読んで [`SearchHighlight::resolve`] に渡す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHighlight {
    pub pattern: String,
    pub case_sensitive: bool,
}

impl SearchHighlight {
    /// nvimの検索状態から実効ハイライトを解決する。
    ///
    /// - `raw_pattern`: `@/` レジスタまたはincsearch中のcmdline内容
    /// - `ignorecase` / `smartcase`: `&ignorecase` / `&smartcase`
    ///
    /// 大文字小文字の決定順(Vim準拠):
    /// 1. パターン中の `\C` → 常にcase-sensitive、`\c` → 常にcase-insensitive
    ///    (`\C` が優先)。フラグはパターンから除去する
    /// 2. フラグなし: `ignorecase` が偽ならcase-sensitive
    /// 3. `ignorecase` かつ `smartcase`: パターンに大文字が含まれれば
    ///    case-sensitive、なければcase-insensitive
    /// 4. `ignorecase` かつ `smartcase` でない: case-insensitive
    ///
    /// パターンが空になる場合は `None`(ハイライト対象なし)。
    pub fn resolve(raw_pattern: &str, ignorecase: bool, smartcase: bool) -> Option<Self> {
        let (pattern, forced) = strip_case_flags(raw_pattern);
        if pattern.is_empty() {
            return None;
        }
        let case_sensitive = match forced {
            Some(force) => force,
            None => {
                if !ignorecase {
                    true
                } else if smartcase {
                    pattern.chars().any(|c| c.is_uppercase())
                } else {
                    false
                }
            }
        };
        Some(Self {
            pattern,
            case_sensitive,
        })
    }

    /// `text` 中のパターンのマッチを、非重複・左から順に半開区間の
    /// バイトオフセット `(start, end)` で返す。
    ///
    /// リテラル部分文字列照合。case-insensitive時はUnicode小文字化で比較し、
    /// 返すオフセットは常に `text` の元バイト境界に乗る。
    pub fn match_spans(&self, text: &str) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        if self.pattern.is_empty() {
            return spans;
        }
        let mut cursor = 0;
        while cursor < text.len() {
            match self.match_at(&text[cursor..]) {
                Some(len) if len > 0 => {
                    spans.push((cursor, cursor + len));
                    cursor += len;
                }
                _ => {
                    // 次のUTF-8文字境界へ進める。
                    cursor += text[cursor..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(1);
                }
            }
        }
        spans
    }

    /// `text` の先頭がパターンに一致する場合、その一致バイト長を返す。
    fn match_at(&self, text: &str) -> Option<usize> {
        let mut text_chars = text.char_indices();
        let mut pattern_chars = self.pattern.chars();
        let mut end = 0;
        loop {
            let Some(pattern_char) = pattern_chars.next() else {
                return Some(end);
            };
            let (offset, text_char) = text_chars.next()?;
            if !chars_match(text_char, pattern_char, self.case_sensitive) {
                return None;
            }
            end = offset + text_char.len_utf8();
        }
    }
}

/// パターン中の `\c` / `\C` を除去し、強制ケースを返す。
/// `\C` が1つでもあれば `Some(true)`、なければ `\c` があれば `Some(false)`、
/// どちらもなければ `None`。他のバックスラッシュ列はそのまま残す。
fn strip_case_flags(pattern: &str) -> (String, Option<bool>) {
    let mut result = String::with_capacity(pattern.len());
    let mut forced_sensitive = false;
    let mut forced_insensitive = false;
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek().copied() {
                Some('c') => {
                    forced_insensitive = true;
                    chars.next();
                    continue;
                }
                Some('C') => {
                    forced_sensitive = true;
                    chars.next();
                    continue;
                }
                Some(next) => {
                    result.push(c);
                    result.push(next);
                    chars.next();
                    continue;
                }
                None => {
                    result.push(c);
                    continue;
                }
            }
        }
        result.push(c);
    }
    let forced = if forced_sensitive {
        Some(true)
    } else if forced_insensitive {
        Some(false)
    } else {
        None
    };
    (result, forced)
}

/// 1文字ずつの一致判定。case-insensitive時はUnicode小文字化で比較する。
fn chars_match(a: char, b: char, case_sensitive: bool) -> bool {
    if a == b {
        return true;
    }
    if case_sensitive {
        return false;
    }
    a.to_lowercase().eq(b.to_lowercase())
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
    /// ユーザーが指定行のディレクトリを新しい表示ルートにするよう要求した。
    /// `line`は0始まり。行の解釈とパス解決はapp層が行う。
    NavigateInto {
        line: usize,
    },
    /// ユーザーが現在の表示ルートの親ディレクトリへの移動を要求した。
    NavigateParent,
    /// ユーザーがパス指定での表示ルート変更を要求した。
    /// `query`は生パス文字列。`None`は現在ルートと候補の表示要求。
    /// パスの解決(絶対/相対/`~`)はapp層が行う。
    ChangeDirectory {
        query: Option<String>,
    },
    /// ユーザーが隠しファイル表示の切り替えを要求した。
    ToggleHidden,
    /// ユーザーがブックマークまたは最近使ったルートへのジャンプ、
    /// あるいは候補一覧の表示を要求した。
    JumpBookmark {
        /// 指定名または番号。一覧表示要求の場合は`None`。
        query: Option<String>,
    },
    /// ユーザーがヘルプ表示を要求した。
    ShowHelp,
    /// ユーザーがpaneの分割・focus移動・closeを要求した。
    PaneAction(PaneAction),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hl(pattern: &str, case_sensitive: bool) -> SearchHighlight {
        SearchHighlight {
            pattern: pattern.to_string(),
            case_sensitive,
        }
    }

    // ---- resolve: 空パターン ----
    #[test]
    fn resolve_empty_is_none() {
        assert_eq!(SearchHighlight::resolve("", false, false), None);
        // `\c` / `\C` のみ → 除去後に空 → None
        assert_eq!(SearchHighlight::resolve("\\c", false, false), None);
        assert_eq!(SearchHighlight::resolve("\\C", true, true), None);
    }

    // ---- resolve: ignorecase / smartcase マトリクス ----
    #[test]
    fn resolve_no_ignorecase_is_sensitive() {
        assert_eq!(
            SearchHighlight::resolve("foo", false, false),
            Some(hl("foo", true)),
        );
    }

    #[test]
    fn resolve_plain_ignorecase_ignores_uppercase() {
        // smartcaseなし: 大文字を含んでも case-insensitive
        assert_eq!(
            SearchHighlight::resolve("Foo", true, false),
            Some(hl("Foo", false)),
        );
    }

    #[test]
    fn resolve_smartcase_lowercase_is_insensitive() {
        assert_eq!(
            SearchHighlight::resolve("foo", true, true),
            Some(hl("foo", false)),
        );
    }

    #[test]
    fn resolve_smartcase_uppercase_is_sensitive() {
        assert_eq!(
            SearchHighlight::resolve("Foo", true, true),
            Some(hl("Foo", true)),
        );
    }

    // ---- resolve: フラグ優先順位(フラグはパターンから除去される)----
    #[test]
    fn resolve_lowercase_flag_forces_insensitive_and_strips() {
        // `\c` は ignorecase=false でも case-insensitive を強制し、フラグを除去
        assert_eq!(
            SearchHighlight::resolve("\\cFoo", false, false),
            Some(hl("Foo", false)),
        );
    }

    #[test]
    fn resolve_uppercase_flag_forces_sensitive_and_strips() {
        // `\C` は smartcase下でも case-sensitive を強制
        assert_eq!(
            SearchHighlight::resolve("\\Cfoo", true, true),
            Some(hl("foo", true)),
        );
    }

    #[test]
    fn resolve_uppercase_flag_wins_over_lowercase() {
        // `\c` と `\C` が両方 → `\C` が勝つ
        assert_eq!(
            SearchHighlight::resolve("\\c\\Cfoo", true, true),
            Some(hl("foo", true)),
        );
    }

    #[test]
    fn resolve_preserves_non_case_backslash() {
        // ケースフラグ以外のバックスラッシュ列はそのまま残る
        assert_eq!(
            SearchHighlight::resolve("a\\.b", false, false),
            Some(hl("a\\.b", true)),
        );
    }

    // ---- match_spans: リテラル・非重複・バイト境界 ----
    #[test]
    fn spans_case_sensitive_literal() {
        let h = SearchHighlight::resolve("foo", false, false).unwrap();
        let text = "foobar foo";
        let spans = h.match_spans(text);
        assert_eq!(spans, vec![(0, 3), (7, 10)]);
        assert_eq!(&text[spans[0].0..spans[0].1], "foo");
        assert_eq!(&text[spans[1].0..spans[1].1], "foo");
    }

    #[test]
    fn spans_case_insensitive_preserves_original_case() {
        let h = SearchHighlight::resolve("foo", true, false).unwrap();
        assert!(!h.case_sensitive);
        let text = "FOO Foo foo";
        let spans = h.match_spans(text);
        assert_eq!(spans, vec![(0, 3), (4, 7), (8, 11)]);
        // 返るスライスは元テキストのケース。小文字化したパターンではない
        let matched: Vec<&str> = spans.iter().map(|&(s, e)| &text[s..e]).collect();
        assert_eq!(matched, vec!["FOO", "Foo", "foo"]);
    }

    #[test]
    fn spans_non_overlapping() {
        let h = SearchHighlight::resolve("aa", false, false).unwrap();
        // 非重複: (1,3) は出さない
        assert_eq!(h.match_spans("aaaa"), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn spans_empty_text() {
        let h = SearchHighlight::resolve("foo", false, false).unwrap();
        assert_eq!(h.match_spans(""), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn spans_multibyte_boundaries() {
        let h = SearchHighlight::resolve("abc", true, false).unwrap();
        let text = "あabcあ"; // あ = 3バイト
        let spans = h.match_spans(text);
        assert_eq!(spans, vec![(3, 6)]);
        assert_eq!(&text[spans[0].0..spans[0].1], "abc");
    }

    #[test]
    fn spans_case_insensitive_multibyte_letter() {
        // é(U+00E9) と É(U+00C9) は char::to_lowercase で一致 → マッチする
        let h = SearchHighlight::resolve("é", true, false).unwrap();
        assert!(!h.case_sensitive);
        let text = "xÉy"; // É = 2バイト
        let spans = h.match_spans(text);
        assert_eq!(spans, vec![(1, 3)]);
        assert_eq!(&text[spans[0].0..spans[0].1], "É");
    }
}
