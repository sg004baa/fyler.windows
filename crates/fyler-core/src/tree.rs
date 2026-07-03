//! ツリーの中間表現: baseline(実FSの最終同期状態)と DesiredTree(編集後バッファの意図)。

use std::collections::HashSet;
use std::path::PathBuf;

use crate::id::EntryId;
use crate::path::TreePath;

/// エントリ種別。
///
/// symlink / junction / reparse point はMVPでは追跡せず、リンク自体を
/// 1エントリとして扱う(中に潜らない。DESIGN.md「validateで弾くもの」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// symlink / junction / reparse point(まとめて1エントリ扱い)。
    Symlink,
}

/// baseline: 最後に実FSと同期した時点のツリー。
///
/// - `fyler-fsops::scan` が実FSから構築し、IDを採番する
/// - diff(fyler-pipeline)はこのbaselineとDesiredTreeを比較する
/// - reconcile(保存成功後)で更新される。部分失敗時は成功した操作のみ反映する
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineTree {
    /// 表示ルートの実FSパス。
    pub root: PathBuf,
    pub entries: Vec<BaselineEntry>,
}

impl BaselineTree {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            entries: Vec::new(),
        }
    }

    pub fn insert(&mut self, entry: BaselineEntry) {
        self.entries.push(entry);
    }

    pub fn get(&self, id: EntryId) -> Option<&BaselineEntry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineEntry {
    pub id: EntryId,
    pub path: TreePath,
    pub kind: EntryKind,
}

/// DesiredTree: 編集後バッファをparseして得られる「ユーザーが意図するツリー」。
///
/// - 同一IDの複数出現を**許す**(yy→p のCOPY表現。diff層が解釈する)
/// - collapsedなディレクトリの子孫はバッファに存在しないため、ここにも現れない。
///   その情報は [`EditContext::collapsed_dirs`] が補う
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DesiredTree {
    pub entries: Vec<DesiredEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredEntry {
    /// Noneの行はCREATE候補。
    pub id: Option<EntryId>,
    pub path: TreePath,
    pub kind: EntryKind,
    /// 由来するバッファ行番号(0始まり)。エラー表示・部分失敗時のdirty差分の対応付けに使う。
    pub line: usize,
}

/// バッファ以外に由来する編集セッションの文脈。validate / diff に渡す。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EditContext {
    /// collapsed(折りたたみ)状態のディレクトリID。
    ///
    /// collapsedなディレクトリの子孫はバッファに存在しないため、DesiredTreeに
    /// 現れなくても**DELETEしてはならない**。collapsed行のrename/moveは
    /// 子孫ごとの操作(planにはディレクトリ1件のMoveのみ)として扱う。
    /// 逆に、展開中(collapsedでない)ディレクトリの子孫がバッファから消えていれば
    /// それはDELETEである。(DESIGN.md「バッファ文法の決定事項」)
    pub collapsed_dirs: HashSet<EntryId>,
}
