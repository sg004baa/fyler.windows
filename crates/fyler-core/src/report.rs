//! CommitReport: apply層の実行結果(操作単位の成功/失敗)。

use crate::plan::FsOperation;

/// apply実行中の進捗(操作単位)。workerスレッドからGUIへ通知する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyProgress {
    /// 完了済み操作数(現在実行を開始する操作は含まない)。
    pub completed: usize,
    /// plan全体の操作数。
    pub total: usize,
    /// これから実行する操作。全操作完了直後の最終通知では`None`。
    pub current: Option<FsOperation>,
}

/// planの実行結果。**全体ロールバックはしない**。部分成功を明示し、
/// 成功した操作のみbaselineへ反映する(DESIGN.md「保存処理の状態機械」)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitReport {
    /// planの `ops` と同順・同数。
    pub results: Vec<OpResult>,
}

impl CommitReport {
    pub fn all_succeeded(&self) -> bool {
        self.results
            .iter()
            .all(|r| matches!(r.outcome, OpOutcome::Success))
    }

    pub fn all_failed(&self) -> bool {
        !self.results.is_empty()
            && self
                .results
                .iter()
                .all(|r| !matches!(r.outcome, OpOutcome::Success))
    }

    pub fn any_failed(&self) -> bool {
        self.results
            .iter()
            .any(|r| !matches!(r.outcome, OpOutcome::Success))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpResult {
    pub op: FsOperation,
    pub outcome: OpOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpOutcome {
    Success,
    /// 失敗。ロック中ファイル等もここ(操作単位で報告、全体ロールバックなし)。
    Failed {
        error: String,
        /// 非原子的操作(クロスボリューム移動)の途中失敗時、
        /// 「どこまで完了したか」をユーザー向けに記述する(DESIGN.md「操作種別の内部分類」)。
        progress: Option<String>,
    },
    /// 先行操作の失敗により実行しなかった(例: 親ディレクトリのCreate失敗)。
    Skipped {
        reason: String,
    },
}
