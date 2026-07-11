//! 外部変更検知(notifyクレート)。M5。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

const DEBOUNCE_WINDOW: Duration = Duration::from_millis(200);

/// 外部プロセスによるファイルシステム変更の通知。
///
/// 1回の通知には固定debounceウィンドウ内で検知した全パスを重複なく保持する。
/// app層は部分再スキャンの対象ディレクトリ判定に使う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalChange {
    /// 同一のdebounceウィンドウ内で変更を検知したパスの集合。
    pub paths: BTreeSet<PathBuf>,
}

/// ルート以下の監視ハンドル。
///
/// drop時はnotify監視を先に停止して内部チャネルを切断し、保留中の変更を送信して
/// debounceスレッドが終了するまで待つ。これにより監視終了後にスレッドを残さない。
pub struct FsWatcher {
    watcher: Option<RecommendedWatcher>,
    debounce_thread: Option<JoinHandle<()>>,
}

impl Drop for FsWatcher {
    fn drop(&mut self) {
        drop(self.watcher.take());
        if let Some(debounce_thread) = self.debounce_thread.take() {
            let _ = debounce_thread.join();
        }
    }
}

#[derive(Default)]
struct PendingPaths {
    paths: BTreeSet<PathBuf>,
}

impl PendingPaths {
    fn extend(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.paths.extend(paths);
    }

    fn into_change(self) -> Option<ExternalChange> {
        if self.paths.is_empty() {
            None
        } else {
            Some(ExternalChange { paths: self.paths })
        }
    }
}

/// 監視を開始する。
///
/// 実装契約(DESIGN.md「その他の対応事項」):
/// - 最初の変更検知から200msの固定ウィンドウ内に届いたパスを集合へまとめ、
///   ウィンドウ終了時に `tx` へ1回通知する。イベント到着で期限は延長しない
/// - **編集中バッファがdirtyの場合は上書きしない**。通知のみ表示し、
///   ユーザーの保存/破棄の判断を待つ(この判定はapp層の責務。watchは通知に徹する)
/// - 自分自身のapply中の変更でイベントが再帰しないよう、apply中は抑制するか
///   イベントを間引く仕組みをapp層と取り決めること
pub fn watch(root: &Path, tx: Sender<ExternalChange>) -> anyhow::Result<FsWatcher> {
    let (notify_tx, notify_rx) = mpsc::channel::<Vec<PathBuf>>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        if !is_tree_change(&event.kind) || event.paths.is_empty() {
            return;
        }

        let _ = notify_tx.send(event.paths);
    })
    .context("ファイルシステム監視を作成できません")?;

    watcher
        .watch(root, RecursiveMode::Recursive)
        .with_context(|| format!("ファイルシステム監視を開始できません: {}", root.display()))?;

    let debounce_thread = thread::Builder::new()
        .name("fyler-fs-watch-debounce".to_owned())
        .spawn(move || run_debounce(notify_rx, tx))
        .context("ファイルシステム監視のdebounceスレッドを開始できません")?;

    Ok(FsWatcher {
        watcher: Some(watcher),
        debounce_thread: Some(debounce_thread),
    })
}

fn run_debounce(rx: Receiver<Vec<PathBuf>>, tx: Sender<ExternalChange>) {
    while let Ok(first_paths) = rx.recv() {
        let deadline = Instant::now() + DEBOUNCE_WINDOW;
        let mut pending = PendingPaths::default();
        pending.extend(first_paths);
        let mut source_disconnected = false;

        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if remaining.is_zero() {
                break;
            }

            match rx.recv_timeout(remaining) {
                Ok(paths) => pending.extend(paths),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    source_disconnected = true;
                    break;
                }
            }
        }

        if let Some(change) = pending.into_change() {
            if tx.send(change).is_err() {
                break;
            }
        }
        if source_disconnected {
            break;
        }
    }
}

/// Access通知はツリー・metadata・git状態を変えないため除外し、それ以外は安全側で受理する。
/// Modify(Data/Metadata)を落とすと外部編集後の表示やsort更新を見逃すため、未知種別も含める。
fn is_tree_change(kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Access(_))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    use tempfile::tempdir;

    use notify::event::{AccessKind, DataChange, MetadataKind, ModifyKind};

    use super::*;

    #[test]
    fn reports_a_created_file() {
        let root = tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let _watcher = watch(root.path(), tx).unwrap();

        let created = root.path().join("created.txt");
        fs::write(&created, b"content").unwrap();

        let change = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("ファイル作成の通知が届きませんでした");
        assert!(change.paths.contains(&created));
    }

    #[test]
    fn reports_overwritten_file_contents() {
        let root = tempdir().unwrap();
        let changed = root.path().join("changed.txt");
        fs::write(&changed, b"before").unwrap();
        let (tx, rx) = mpsc::channel();
        let _watcher = watch(root.path(), tx).unwrap();

        fs::write(&changed, b"after with different length").unwrap();

        let change = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("ファイル内容上書きの通知が届きませんでした");
        assert!(change.paths.contains(&changed));
    }

    #[test]
    fn tree_change_accepts_all_non_access_event_kinds() {
        for kind in [
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Any,
            EventKind::Other,
        ] {
            assert!(is_tree_change(&kind), "受理されませんでした: {kind:?}");
        }
        assert!(!is_tree_change(&EventKind::Access(AccessKind::Read)));
    }

    #[test]
    fn coalesces_rapid_file_creations() {
        const CREATED_FILES: usize = 8;

        let root = tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let _watcher = watch(root.path(), tx).unwrap();
        let created_paths = (0..CREATED_FILES)
            .map(|index| root.path().join(format!("created-{index}.txt")))
            .collect::<Vec<_>>();

        for path in &created_paths {
            fs::write(path, b"content").unwrap();
        }

        let first_change = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("ファイル作成の通知が届きませんでした");
        let mut changes = vec![first_change];
        while let Ok(change) = rx.recv_timeout(Duration::from_millis(500)) {
            changes.push(change);
        }

        let received_paths = changes
            .iter()
            .flat_map(|change| change.paths.iter().cloned())
            .collect::<BTreeSet<_>>();
        assert!(
            created_paths
                .iter()
                .all(|path| received_paths.contains(path)),
            "作成した全パスが通知に含まれていません: {received_paths:?}"
        );
        assert!(
            changes.iter().any(|change| change.paths.len() > 1) || changes.len() < CREATED_FILES,
            "作成イベントがdebounceされていません: {}件",
            changes.len()
        );
    }

    #[test]
    fn pending_paths_merges_batches_and_removes_duplicates() {
        let mut pending = PendingPaths::default();
        pending.extend([
            PathBuf::from("b.txt"),
            PathBuf::from("a.txt"),
            PathBuf::from("b.txt"),
        ]);
        pending.extend([PathBuf::from("c.txt"), PathBuf::from("a.txt")]);

        let change = pending.into_change().unwrap();
        assert_eq!(
            change.paths,
            BTreeSet::from([
                PathBuf::from("a.txt"),
                PathBuf::from("b.txt"),
                PathBuf::from("c.txt"),
            ])
        );
    }
}
