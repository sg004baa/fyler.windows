//! OperationPlan: diff層の出力 = 承認待ちのファイル操作列。

use crate::id::EntryId;
use crate::path::TreePath;
use crate::tree::EntryKind;

/// 1件のファイル操作。パスはすべて表示ルート相対の [`TreePath`]
/// (実FSパス・OS固有形式への変換はfsops層の責務)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsOperation {
    /// IDのない行 → 新規作成。
    Create { path: TreePath, kind: EntryKind },
    /// ID一致で名前/親ディレクトリが変化 → rename / move(同一操作として扱う)。
    /// collapsedなディレクトリのMoveは子孫ごと動く(planには親1件のみ入れる)。
    Move {
        id: EntryId,
        from: TreePath,
        to: TreePath,
    },
    /// 同一IDが複数行に出現(yy→p)→ 元位置以外はCOPY。
    /// `src`はコピー元エントリのID(コピー先は新規エントリになる)。
    Copy {
        src: EntryId,
        from: TreePath,
        to: TreePath,
    },
    /// baselineに存在したIDがバッファから消滅 → 削除。**必ずごみ箱経由**(fsops::recycle)。
    Delete { id: EntryId, path: TreePath },
}

/// 承認待ちの操作列。
///
/// **順序の契約**: `ops` は上から順にそのまま実行できる順序で並べる。
/// 順序付けはdiff層(fyler-pipeline::diff)の責務であり、apply層(fyler-fsops)は
/// 並べ替えずに実行する。少なくとも以下を満たすこと:
///
/// - 親ディレクトリのCreateは子のCreate/Move先より前
/// - Move/Copyの読み取り元が先行操作で消えない(Deleteは最後)
/// - Move同士の玉突き(a→b, b→c)は依存順に並べる
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OperationPlan {
    pub ops: Vec<FsOperation>,
}

impl OperationPlan {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}
