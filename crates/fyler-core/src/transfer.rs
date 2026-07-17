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

/// clipboard・drag&dropの取り込み効果(コピーか移動か)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropEffect {
    Copy,
    Move,
}
/// OLE drag-out(fylerのentryを外部Shellターゲットへdragする)の結果。
///
/// fsops層がDoDragDropの戻り値とtargetが書き込む"Performed DropEffect"から
/// 判定して返す、エンジン・OS非依存の要約。app層はこれだけを見て
/// 「何もしない」か「source側の後始末(確認付きごみ箱退避)」かを決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragOutcome {
    /// Escキャンセル、またはtargetがdropしないままdragが終了した。
    Cancelled,
    /// targetがdropを受理した。
    Dropped {
        /// 判定済みの取り込み効果。moveが報告されなかった場合は[`DropEffect::Copy`]
        /// (link等、source側の後始末が不要な効果もCopyへ畳む)。
        effect: DropEffect,
        /// DoDragDropの戻り値または"Performed DropEffect"でmoveが報告されたか。
        /// trueでもsourceの削除はtarget(Explorer等)が済ませている場合がある
        /// (optimized move)ため、app層はsourceの存在を確認してから後始末する。
        move_reported: bool,
    },
}

/// 外部source(Explorer clipboard・inbound drop)由来の1件の取り込み操作。
///
/// `source`は取り込み元の絶対パス、`target`は[`ImportPlan::destination`]配下に
/// `source`のbasenameを保って解決した絶対パス。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOp {
    pub source: PathBuf,
    pub target: PathBuf,
}

/// clipboard・inbound dropで外部sourceを現在paneのdestinationへ取り込む、
/// 承認待ちの実行計画。
///
/// [`TransferPlan`]と異なり、外部sourceは単一rootに収まる保証がない
/// (ExplorerのCF_HDROP・inbound dropは複数ドライブ・複数フォルダにまたがり
/// 得るため)。`ops`内のsource/targetは常に絶対パスで持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlan {
    pub destination: PathBuf,
    pub effect: DropEffect,
    pub ops: Vec<ImportOp>,
}

impl ImportPlan {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// 絶対sourceパス列とdestinationからplanを構築する。basenameを取れない
    /// source(ボリュームルート等)と、同一絶対パスの重複selectionは除く。
    pub fn build(
        sources: impl IntoIterator<Item = PathBuf>,
        destination: PathBuf,
        effect: DropEffect,
    ) -> Self {
        let mut seen = std::collections::HashSet::new();
        let ops = sources
            .into_iter()
            .filter(|source| seen.insert(source.clone()))
            .filter_map(|source| {
                let name = source.file_name()?.to_owned();
                Some(ImportOp {
                    target: destination.join(name),
                    source,
                })
            })
            .collect();
        Self {
            destination,
            effect,
            ops,
        }
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

    #[test]
    fn import_plan_build_joins_destination_and_dedupes_sources() {
        let plan = ImportPlan::build(
            vec![
                PathBuf::from("C:/src/a.txt"),
                PathBuf::from("C:/src/a.txt"),
                PathBuf::from("D:/other/b"),
            ],
            PathBuf::from("C:/dest"),
            DropEffect::Move,
        );
        assert_eq!(
            plan.ops,
            vec![
                ImportOp {
                    source: PathBuf::from("C:/src/a.txt"),
                    target: PathBuf::from("C:/dest").join("a.txt"),
                },
                ImportOp {
                    source: PathBuf::from("D:/other/b"),
                    target: PathBuf::from("C:/dest").join("b"),
                },
            ]
        );
        assert!(!plan.is_empty());
        assert_eq!(plan.effect, DropEffect::Move);
    }

    #[test]
    fn import_plan_build_empty_sources_is_empty() {
        let plan = ImportPlan::build(Vec::new(), PathBuf::from("C:/dest"), DropEffect::Copy);
        assert!(plan.is_empty());
    }

    #[test]
    fn import_plan_build_skips_sources_without_a_filename() {
        let plan = ImportPlan::build(
            vec![PathBuf::from("."), PathBuf::from("C:/src/a.txt")],
            PathBuf::from("C:/dest"),
            DropEffect::Copy,
        );
        assert_eq!(plan.ops.len(), 1);
        assert_eq!(plan.ops[0].source, PathBuf::from("C:/src/a.txt"));
    }
}
