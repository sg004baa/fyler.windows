//! 外部変更検知(notifyクレート)。M5。

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

/// 外部プロセスによるファイルシステム変更の通知。
#[derive(Debug, Clone)]
pub struct ExternalChange {
    pub path: PathBuf,
}

/// ルート以下の監視ハンドル。dropで監視停止。
pub struct FsWatcher {
    // 実装時: notify::RecommendedWatcher を保持する(notifyの型は公開しない)
    _private: (),
}

/// 監視を開始する。
///
/// 実装契約(DESIGN.md「その他の対応事項」):
/// - 変更検知 → `tx` へ通知 → app層がツリー再描画
/// - **編集中バッファがdirtyの場合は上書きしない**。通知のみ表示し、
///   ユーザーの保存/破棄の判断を待つ(この判定はapp層の責務。watchは通知に徹する)
/// - 自分自身のapply中の変更でイベントが再帰しないよう、apply中は抑制するか
///   イベントを間引く仕組みをapp層と取り決めること
pub fn watch(root: &Path, tx: Sender<ExternalChange>) -> anyhow::Result<FsWatcher> {
    todo!("M5: notifyによる監視")
}
