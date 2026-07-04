//! 外部変更検知(notifyクレート)。M5。

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::Context;
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// 外部プロセスによるファイルシステム変更の通知。
#[derive(Debug, Clone)]
pub struct ExternalChange {
    pub path: PathBuf,
}

/// ルート以下の監視ハンドル。dropで監視停止。
pub struct FsWatcher {
    _watcher: RecommendedWatcher,
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
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        if !is_tree_change(&event.kind) {
            return;
        }

        for path in event.paths {
            if tx.send(ExternalChange { path }).is_err() {
                return;
            }
        }
    })
    .context("ファイルシステム監視を作成できません")?;

    watcher
        .watch(root, RecursiveMode::Recursive)
        .with_context(|| format!("ファイルシステム監視を開始できません: {}", root.display()))?;

    Ok(FsWatcher { _watcher: watcher })
}

fn is_tree_change(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Name(_) | ModifyKind::Any)
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn reports_a_created_file() {
        let root = tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let _watcher = watch(root.path(), tx).unwrap();

        fs::write(root.path().join("created.txt"), b"content").unwrap();

        let change = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("ファイル作成の通知が届きませんでした");
        assert!(change.path.starts_with(root.path()));
    }
}
