//! Move操作の内部3分類(DESIGN.md「操作種別の内部分類」)。
//!
//! `std::fs::rename` は別ボリューム間で失敗する。MoveFileExWもディレクトリは
//! 同一ドライブが必要。そのため実行前に必ず分類する。

use std::path::Path;

use fyler_core::tree::EntryKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveClass {
    /// 同一ボリューム内のrename。**原子的**。
    SameVolumeRename,
    /// 別ボリュームへのファイル移動 = copy + delete。**非原子的**。
    CrossVolumeFileMove,
    /// 別ボリュームへのディレクトリ移動 = 再帰copy + delete。**非原子的**で
    /// 途中失敗時の挙動が異なる(どこまでコピー/削除できたかをprogressで報告する)。
    CrossVolumeDirectoryMove,
}

/// 移動元と移動先のボリュームを比較して分類する。
///
/// 実装契約(Windows): ボリューム判定は `GetVolumePathNameW` 等で
/// 実際のマウントポイントを比較する(ドライブレターの文字比較だけでは
/// junction・マウントされたボリュームで誤判定する)。
pub fn classify_move(_from: &Path, _to: &Path, _kind: EntryKind) -> anyhow::Result<MoveClass> {
    todo!("M4: ボリューム判定(同一ボリューム限定のM3では常にSameVolumeRenameを前提としてよい)")
}
