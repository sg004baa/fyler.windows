//! pane間transferの、エンジン・FS非依存な実行計画型。

use std::path::PathBuf;

use crate::path::TreePath;
use crate::tree::EntryKind;

/// pane間で選択エントリを移動するか、コピーするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    Move,
    Copy,
}

/// 1件のpane間transfer操作。
///
/// `from`は[`TransferPlan::from_root`]相対、`to`は
/// [`TransferPlan::to_root`]相対であり、OS固有パスへの変換はfsops層の責務。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOp {
    pub kind: TransferKind,
    pub from: TreePath,
    pub to: TreePath,
    pub entry_kind: EntryKind,
}

/// 1 source paneから1 target paneへの、承認待ちtransfer操作列。
///
/// v1の`ops`は相互依存を持たない平坦な列であり、並べ替えずに実行する。
/// 親子孫が重複する選択は計画構築時に最上位祖先だけへ畳み込み、残った
/// from/to間の干渉はfsopsのpreflightで拒否する。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TransferPlan {
    pub from_root: PathBuf,
    pub to_root: PathBuf,
    pub ops: Vec<TransferOp>,
}

impl TransferPlan {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_keeps_roots_once_and_paths_relative() {
        let plan = TransferPlan {
            from_root: PathBuf::from("C:/source"),
            to_root: PathBuf::from("D:/target"),
            ops: vec![TransferOp {
                kind: TransferKind::Copy,
                from: TreePath::parse("directory/file.txt"),
                to: TreePath::parse("file.txt"),
                entry_kind: EntryKind::File,
            }],
        };

        assert_eq!(
            plan.ops[0].from.to_fs_path(&plan.from_root),
            PathBuf::from("C:/source/directory/file.txt")
        );
        assert_eq!(
            plan.ops[0].to.to_fs_path(&plan.to_root),
            PathBuf::from("D:/target/file.txt")
        );
        assert!(!plan.is_empty());
    }
}
