//! apply: 承認済みOperationPlanの実行。**実FSに書き込むのはここだけ**(絶対ルール1)。

use std::path::Path;

use fyler_core::plan::OperationPlan;
use fyler_core::report::CommitReport;

/// planを実行し、操作単位の結果を返す。
///
/// 呼び出し契約: 保存状態機械(`fyler_core::save`)の `Applying` 状態
/// (= 確認ダイアログで承認済み)からのみ呼ぶこと。M2まではdry-runのみで、
/// この関数は呼ばれない。
///
/// 実装契約:
/// - `plan.ops` を**並べ替えずに**上から順に実行する(順序はdiff層が保証済み)
/// - `CommitReport.results` はopsと同順・同数で返す
/// - エラーは操作単位で報告する。**全体ロールバックはしない**。部分成功を明示する
///   (ロック中ファイル等。DESIGN.md「その他の対応事項」)
/// - 先行操作の失敗で実行不能になった操作は `OpOutcome::Skipped`
///   (例: 親ディレクトリのCreate失敗 → 子のCreate)
/// - Move実行前に [`crate::classify::classify_move`] で3分類し、
///   非原子的操作(クロスボリューム)の途中失敗は `OpOutcome::Failed.progress` に
///   「どこまで完了したか」を記述する
/// - Deleteは必ず [`crate::recycle`] 経由(ごみ箱)。直接削除しない
/// - case-onlyリネームは [`crate::case`] のtemp名経由2段renameを使う
/// - パスは `TreePath::to_fs_path(root)` → 必要時のみ [`crate::long_path`] で変換
pub fn apply_plan(root: &Path, plan: &OperationPlan) -> CommitReport {
    todo!("M3: create/delete/rename(同一ボリューム) → M4: move/copy/クロスボリューム")
}
