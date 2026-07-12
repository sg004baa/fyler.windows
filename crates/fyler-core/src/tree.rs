//! ツリーの中間表現: baseline(実FSの最終同期状態)と DesiredTree(編集後バッファの意図)。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::id::EntryId;
use crate::path::TreePath;
use crate::scanwarn::{ScanErrorKind, ScanWarning};

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
///
/// メタデータ([`crate::fileinfo::EntryMeta`])はスキャン時に列挙情報から
/// 収集する表示用の付帯情報であり、ツリー構造の同一性判定([`PartialEq`])には
/// **含めない**。外部変更でサイズ・更新日時だけが変わってもバッファ行の
/// 再設定を誘発しないためである。
#[derive(Debug, Clone)]
pub struct BaselineTree {
    /// 表示ルートの実FSパス。
    pub root: PathBuf,
    entries: Arc<Vec<BaselineEntry>>,
    index: Arc<HashMap<EntryId, usize>>,
    path_index: Arc<HashMap<TreePath, usize>>,
    meta: Arc<HashMap<EntryId, crate::fileinfo::EntryMeta>>,
    incomplete_dirs: Arc<BTreeMap<TreePath, ScanErrorKind>>,
    warnings: Arc<Vec<ScanWarning>>,
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
            entries: Arc::new(Vec::new()),
            index: Arc::new(HashMap::new()),
            path_index: Arc::new(HashMap::new()),
            meta: Arc::new(HashMap::new()),
            incomplete_dirs: Arc::new(BTreeMap::new()),
            warnings: Arc::new(Vec::new()),
        }
    }

    /// エントリを末尾へ追加し、ID・パス検索用インデックスも同時に更新する。
    ///
    /// 同一IDの重複はbaseline構築側の不具合であるため、デバッグビルドでは
    /// 即座に検出する。
    pub fn insert(&mut self, entry: BaselineEntry) {
        debug_assert!(
            !self.index.contains_key(&entry.id),
            "Duplicate ID inserted into BaselineTree: {}",
            entry.id
        );
        debug_assert!(
            !self.path_index.contains_key(&entry.path),
            "Duplicate path inserted into BaselineTree: {}",
            entry.path
        );
        let position = self.entries.len();
        Arc::make_mut(&mut self.index).insert(entry.id, position);
        Arc::make_mut(&mut self.path_index).insert(entry.path.clone(), position);
        Arc::make_mut(&mut self.entries).push(entry);
    }

    /// エントリと表示用メタデータを同時に追加する。
    pub fn insert_with_meta(&mut self, entry: BaselineEntry, meta: crate::fileinfo::EntryMeta) {
        let id = entry.id;
        self.insert(entry);
        Arc::make_mut(&mut self.meta).insert(id, meta);
    }

    /// 現在のツリー構造を共有し、指定IDの表示用メタデータだけを更新した複製を返す。
    ///
    /// エントリ構造が変わらない差分再スキャンで、全パスと検索インデックスの
    /// 再構築を避けるために使う。未知のIDは通常のメタデータ挿入として扱う。
    pub fn clone_with_meta_updates(
        &self,
        updates: impl IntoIterator<Item = (EntryId, crate::fileinfo::EntryMeta)>,
    ) -> Self {
        let mut cloned = self.clone();
        Arc::make_mut(&mut cloned.meta).extend(updates);
        cloned
    }

    /// 列挙が不完全だったディレクトリを記録する。
    pub fn mark_incomplete(&mut self, path: TreePath, kind: ScanErrorKind) {
        Arc::make_mut(&mut self.incomplete_dirs).insert(path, kind);
    }

    /// 列挙が不完全だったディレクトリを決定的なパス順で返す。
    pub fn incomplete_dirs(&self) -> &BTreeMap<TreePath, ScanErrorKind> {
        &self.incomplete_dirs
    }

    /// このscanで収集した回復可能な警告を返す。
    pub fn scan_warnings(&self) -> &[ScanWarning] {
        &self.warnings
    }

    /// 回復可能なscan警告を追加する。
    pub fn push_warning(&mut self, warning: ScanWarning) {
        Arc::make_mut(&mut self.warnings).push(warning);
    }

    /// `path`自身または祖先が不完全ディレクトリかを返す。
    pub fn is_within_incomplete(&self, path: &TreePath) -> bool {
        self.incomplete_dirs
            .keys()
            .any(|dir| dir == path || dir.is_strict_ancestor_of(path))
    }

    /// `path`の部分木と不完全範囲が交差するかを返す。
    ///
    /// `path`が不完全範囲の内側にある場合と、`path`の子孫に不完全dirがある場合の
    /// 双方を含む。move/deleteのplan gatingに使う。
    pub fn subtree_intersects_incomplete(&self, path: &TreePath) -> bool {
        self.incomplete_dirs.keys().any(|dir| {
            dir == path || dir.is_strict_ancestor_of(path) || path.is_strict_ancestor_of(dir)
        })
    }

    /// IDに対応する表示用メタデータを返す。スキャン以外で構築したbaselineはNone。
    pub fn meta(&self, id: EntryId) -> Option<&crate::fileinfo::EntryMeta> {
        self.meta.get(&id)
    }

    /// IDに対応するエントリをO(1)で返す。
    pub fn get(&self, id: EntryId) -> Option<&BaselineEntry> {
        self.index
            .get(&id)
            .and_then(|position| self.entries.get(*position))
    }

    /// パスに対応する表示順インデックスをO(1)で返す。
    pub fn index_by_path(&self, path: &TreePath) -> Option<usize> {
        self.path_index.get(path).copied()
    }

    /// パスに対応するエントリをO(1)で返す。
    pub fn get_by_path(&self, path: &TreePath) -> Option<&BaselineEntry> {
        self.index_by_path(path)
            .and_then(|position| self.entries.get(position))
    }

    /// 指定ディレクトリ直下の子エントリのインデックスを表示順で返す。
    ///
    /// `parent`が`None`なら表示ルート直下を返す。baselineのDFS順契約により、
    /// 親の直後に連続する部分木だけを深さで走査するため、計算量はO(部分木)である。
    pub fn child_indices(&self, parent: Option<usize>) -> Vec<usize> {
        let (start, parent_depth) = match parent {
            Some(parent) => {
                let Some(entry) = self.entries.get(parent) else {
                    return Vec::new();
                };
                (parent + 1, entry.path.depth())
            }
            None => (0, 0),
        };

        let mut children = Vec::new();
        for (index, entry) in self.entries.iter().enumerate().skip(start) {
            let depth = entry.path.depth();
            if parent.is_some() && depth <= parent_depth {
                break;
            }
            if depth == parent_depth + 1 {
                children.push(index);
            }
        }
        children
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
        assert_eq!(tree.index_by_path(&TreePath::parse("first.txt")), Some(0));
        assert_eq!(tree.get_by_path(&TreePath::parse("second")), Some(&second));
        assert_eq!(tree.get_by_path(&TreePath::parse("missing")), None);
        assert_eq!(tree.entries(), &[first, second]);
    }

    #[test]
    fn child_indices_follow_dfs_order_for_root_and_nested_directory() {
        let mut tree = BaselineTree::new("C:/root");
        for (id, path, kind) in [
            (1, "a", EntryKind::Dir),
            (2, "a/first.txt", EntryKind::File),
            (3, "a/nested", EntryKind::Dir),
            (4, "a/nested/leaf.txt", EntryKind::File),
            (5, "a/second.txt", EntryKind::File),
            (6, "root.txt", EntryKind::File),
        ] {
            tree.insert(BaselineEntry {
                id: EntryId(id),
                path: TreePath::parse(path),
                kind,
            });
        }

        assert_eq!(tree.child_indices(None), [0, 5]);
        assert_eq!(tree.child_indices(Some(0)), [1, 2, 4]);
        assert_eq!(tree.child_indices(Some(2)), [3]);
        assert!(tree.child_indices(Some(99)).is_empty());
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

    #[test]
    fn access_sidecars_do_not_affect_equality_and_cover_both_intersection_directions() {
        let mut left = BaselineTree::new("C:/root");
        let right = BaselineTree::new("C:/root");
        left.mark_incomplete(
            TreePath::parse("parent/blocked"),
            ScanErrorKind::PermissionDenied,
        );
        left.push_warning(ScanWarning {
            path: PathBuf::from("C:/root/parent/blocked"),
            stage: crate::scanwarn::ScanStage::EnumerateDir,
            kind: ScanErrorKind::PermissionDenied,
        });

        assert_eq!(left, right);
        assert!(left.is_within_incomplete(&TreePath::parse("parent/blocked")));
        assert!(left.is_within_incomplete(&TreePath::parse("parent/blocked/child")));
        assert!(!left.is_within_incomplete(&TreePath::parse("parent")));
        assert!(left.subtree_intersects_incomplete(&TreePath::parse("parent")));
        assert!(left.subtree_intersects_incomplete(&TreePath::parse("parent/blocked/child")));
        assert!(!left.subtree_intersects_incomplete(&TreePath::parse("sibling")));
    }
}
