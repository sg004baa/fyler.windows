//! ツリーの中間表現: baseline(実FSの最終同期状態)と DesiredTree(編集後バッファの意図)。

use std::collections::{HashMap, HashSet};
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
#[derive(Debug, Clone)]
pub struct BaselineTree {
    /// 表示ルートの実FSパス。
    pub root: PathBuf,
    entries: Vec<BaselineEntry>,
    index: HashMap<EntryId, usize>,
}

impl PartialEq for BaselineTree {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root && self.entries == other.entries
    }
}

impl Eq for BaselineTree {}

impl BaselineTree {
    /// 空のbaselineを作成する。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            entries: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// エントリを末尾へ追加し、ID検索用インデックスも同時に更新する。
    ///
    /// 同一IDの重複はbaseline構築側の不具合であるため、デバッグビルドでは
    /// 即座に検出する。
    pub fn insert(&mut self, entry: BaselineEntry) {
        debug_assert!(
            !self.index.contains_key(&entry.id),
            "BaselineTreeへ同一IDが重複挿入されました: {}",
            entry.id
        );
        let position = self.entries.len();
        self.index.insert(entry.id, position);
        self.entries.push(entry);
    }

    /// IDに対応するエントリをO(1)で返す。
    pub fn get(&self, id: EntryId) -> Option<&BaselineEntry> {
        self.index
            .get(&id)
            .and_then(|position| self.entries.get(*position))
    }

    /// baselineの全エントリを表示順で返す。
    pub fn entries(&self) -> &[BaselineEntry] {
        &self.entries
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_insert_updates_get_and_entries() {
        let first = BaselineEntry {
            id: EntryId(1),
            path: TreePath::parse("first.txt"),
            kind: EntryKind::File,
        };
        let second = BaselineEntry {
            id: EntryId(2),
            path: TreePath::parse("second"),
            kind: EntryKind::Dir,
        };
        let mut tree = BaselineTree::new("C:/root");

        tree.insert(first.clone());
        tree.insert(second.clone());

        assert_eq!(tree.get(EntryId(1)), Some(&first));
        assert_eq!(tree.get(EntryId(2)), Some(&second));
        assert_eq!(tree.get(EntryId(3)), None);
        assert_eq!(tree.entries(), &[first, second]);
    }

    #[test]
    fn baseline_equality_uses_root_and_entries() {
        let entry = BaselineEntry {
            id: EntryId(1),
            path: TreePath::parse("file.txt"),
            kind: EntryKind::File,
        };
        let mut left = BaselineTree::new("C:/root");
        let mut right = BaselineTree::new("C:/root");
        left.insert(entry.clone());
        right.insert(entry);

        assert_eq!(left, right);
    }
}
