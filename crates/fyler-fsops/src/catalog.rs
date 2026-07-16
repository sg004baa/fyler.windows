//! fuzzy finder専用SearchCatalogの読み取り専用walker。
//!
//! `ignore::WalkBuilder`の並列walkで`root`部分木を列挙する。並列walkは到達順が
//! 非決定的なので、候補の順序に意味は無い(同点scoreの安定順はmatching側で
//! path昇順として決める。`fyler-app`のpicker参照)。
//!
//! フィルタは全て無効化する(`.standard_filters(false)`+個別無効化)。file
//! managerはgitignore対象・hidden・`.git`配下も含めて全て索引する必要があるため、
//! 候補カバレッジは従来の単一スレッドwalkerと完全に一致する。symlink/junction/
//! reparse pointには潜らない(`.follow_links(false)`)。
//!
//! `\\?\`変換は`long_path::to_fs`の呼び出し1か所に閉じ込める(絶対ルール3)。
//! WalkBuilderへ渡すrootだけを拡張形式へ変換し、各entryの論理パスは
//! `entry.path()`から拡張形式rootをstrip_prefixした相対成分で組み立てる
//! (`\\?\`文字列自体はこのモジュールに現れない)。
//!
//! 効果hiddenの伝播: entryは、自身の名前が`.`始まり/Windows hidden属性/root配下の
//! いずれかの祖先がhiddenなら hidden とする。並列walkでは、`ignore`が親ディレクトリ
//! entryを子より先にvisitorへ渡す保証を使い、visit時にdir→累積hiddenを共有mapへ
//! 記録して子から参照する。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use anyhow::Context;
use fyler_core::search::SearchCandidate;
use fyler_core::tree::EntryKind;
use ignore::{WalkBuilder, WalkState};

const BATCH_SIZE: usize = 4096;
const MAX_THREADS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogProgress {
    pub indexed_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogSummary {
    pub indexed_count: usize,
    pub skipped_directories: usize,
}

/// watcherが報告した論理パスを1回だけ再statし、overlay用候補へ変換する。
///
/// hiddenは自身の名前/属性だけを判定する。祖先hiddenの再計算は行わない。
pub fn candidate_for_path(root: &Path, path: &Path) -> anyhow::Result<Option<SearchCandidate>> {
    let relative = match path.strip_prefix(root) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative,
        _ => return Ok(None),
    };
    let Some(display) = join_relative(relative) else {
        return Ok(None);
    };
    let metadata = match std::fs::symlink_metadata(crate::long_path::to_fs(path)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(file_name) = path.file_name() else {
        return Ok(None);
    };
    Ok(Some(SearchCandidate::new(
        display,
        super::scan::kind_from_metadata(&metadata),
        super::scan::is_hidden(file_name, &metadata),
    )))
}

/// `root`以下を並列列挙し、候補をbatch単位で返す。
///
/// walkは専用スレッド上で行い、呼び出し側スレッドはchannelから受けたbatchを
/// `on_batch`/`on_progress`へ単一consumerとして流す(callbackはworkerスレッドへ
/// 移動しない)。`cancel`はvisitorが確認し、立っていれば`WalkState::Quit`で巻き取る。
/// キャンセルが確定していれば`Ok(None)`を返す。
pub fn build_catalog(
    root: &Path,
    cancel: &AtomicBool,
    mut on_batch: impl FnMut(Vec<SearchCandidate>),
    mut on_progress: impl FnMut(CatalogProgress),
) -> anyhow::Result<Option<CatalogSummary>> {
    let fs_root = crate::long_path::to_fs(root);
    // rootの列挙失敗はfatal(従来契約)。子dirの失敗はskipped扱いで継続する。
    std::fs::read_dir(&fs_root)
        .with_context(|| format!("failed to enumerate catalog root: {}", root.display()))?;

    let hidden_by_dir: Mutex<HashMap<String, bool>> = Mutex::new(HashMap::new());
    let skipped_directories = AtomicUsize::new(0);
    let (batch_tx, batch_rx) = mpsc::channel::<Vec<SearchCandidate>>();

    let threads = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .clamp(1, MAX_THREADS);

    let mut indexed_count = 0usize;

    std::thread::scope(|scope| {
        let fs_root = &fs_root;
        let hidden_by_dir = &hidden_by_dir;
        let skipped_directories = &skipped_directories;
        scope.spawn(move || {
            let walker = WalkBuilder::new(fs_root)
                .standard_filters(false)
                .hidden(false)
                .parents(false)
                .ignore(false)
                .git_ignore(false)
                .git_global(false)
                .git_exclude(false)
                .require_git(false)
                .follow_links(false)
                .threads(threads)
                .build_parallel();
            walker.run(|| {
                let mut collector = BatchCollector::new(batch_tx.clone());
                Box::new(move |result| {
                    if cancel.load(Ordering::Relaxed) {
                        return WalkState::Quit;
                    }
                    let entry = match result {
                        Ok(entry) => entry,
                        Err(_) => {
                            skipped_directories.fetch_add(1, Ordering::Relaxed);
                            return WalkState::Continue;
                        }
                    };
                    // depth 0 はroot自身。候補にしない。
                    if entry.depth() == 0 {
                        return WalkState::Continue;
                    }
                    if let Some(candidate) = make_candidate(fs_root, &entry, hidden_by_dir) {
                        collector.push(candidate);
                    }
                    WalkState::Continue
                })
            });
            // batch_tx(と全collectorの複製)はこのクロージャのdropで閉じる。
        });

        while let Ok(batch) = batch_rx.recv() {
            indexed_count += batch.len();
            on_batch(batch);
            on_progress(CatalogProgress { indexed_count });
        }
    });

    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    Ok(Some(CatalogSummary {
        indexed_count,
        skipped_directories: skipped_directories.load(Ordering::Relaxed),
    }))
}

/// 相対パスを`/`区切りへ結合する。非UTF-8成分があれば`None`。
fn join_relative(relative: &Path) -> Option<String> {
    relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .map(|components| components.join("/"))
}

/// entryを候補へ変換し、dirなら累積hiddenを共有mapへ記録する。
fn make_candidate(
    fs_root: &Path,
    entry: &ignore::DirEntry,
    hidden_by_dir: &Mutex<HashMap<String, bool>>,
) -> Option<SearchCandidate> {
    let relative = entry.path().strip_prefix(fs_root).ok()?;
    let display = join_relative(relative)?;
    let file_name = entry.file_name();
    let parent_hidden = {
        let parent = display.rsplit_once('/').map_or("", |(parent, _)| parent);
        if parent.is_empty() {
            false
        } else {
            hidden_by_dir
                .lock()
                .ok()
                .and_then(|map| map.get(parent).copied())
                .unwrap_or(false)
        }
    };
    let dot_name = file_name.as_encoded_bytes().first() == Some(&b'.');
    let file_type = entry.file_type();

    // Unixのsymlinkはlink先を追わずbroken linkでも候補として残すため、metadataを
    // 呼ばずdot名だけでhiddenを判定する。
    #[cfg(not(windows))]
    if file_type.is_some_and(|file_type| file_type.is_symlink()) {
        let hidden = parent_hidden || dot_name;
        return Some(SearchCandidate::new(display, EntryKind::Symlink, hidden));
    }

    let metadata = entry.metadata().ok()?;
    let own_hidden = dot_name || super::scan::is_hidden(file_name, &metadata);
    let hidden = parent_hidden || own_hidden;
    let kind = if file_type.is_some_and(|file_type| file_type.is_symlink()) {
        EntryKind::Symlink
    } else {
        super::scan::kind_from_metadata(&metadata)
    };
    if kind == EntryKind::Dir
        && let Ok(mut map) = hidden_by_dir.lock()
    {
        map.insert(display.clone(), hidden);
    }
    Some(SearchCandidate::new(display, kind, hidden))
}

/// worker毎のbatch蓄積。満杯でchannelへ送り、drop時に残りをflushする。
struct BatchCollector {
    batch: Vec<SearchCandidate>,
    tx: mpsc::Sender<Vec<SearchCandidate>>,
}

impl BatchCollector {
    fn new(tx: mpsc::Sender<Vec<SearchCandidate>>) -> Self {
        Self {
            batch: Vec::with_capacity(BATCH_SIZE),
            tx,
        }
    }

    fn push(&mut self, candidate: SearchCandidate) {
        self.batch.push(candidate);
        if self.batch.len() >= BATCH_SIZE {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if !self.batch.is_empty() {
            let batch = std::mem::replace(&mut self.batch, Vec::with_capacity(BATCH_SIZE));
            let _ = self.tx.send(batch);
        }
    }
}

impl Drop for BatchCollector {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::AtomicBool;

    use tempfile::tempdir;

    use super::*;

    fn collect(root: &Path, cancel: &AtomicBool) -> anyhow::Result<Option<Vec<SearchCandidate>>> {
        let mut candidates = Vec::new();
        let result = build_catalog(root, cancel, |batch| candidates.extend(batch), |_| {})?;
        Ok(result.map(|_| candidates))
    }

    #[test]
    fn walker_lists_all_entries_and_propagates_hidden() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("visible")).unwrap();
        fs::write(root.path().join("visible/file.txt"), b"x").unwrap();
        fs::create_dir(root.path().join(".hidden")).unwrap();
        fs::write(root.path().join(".hidden/child.txt"), b"x").unwrap();

        let candidates = collect(root.path(), &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        let by_path = candidates
            .iter()
            .map(|candidate| (&*candidate.display, candidate.hidden))
            .collect::<HashMap<_, _>>();
        assert_eq!(by_path.len(), 4);
        assert!(!by_path["visible"]);
        assert!(!by_path["visible/file.txt"]);
        assert!(by_path[".hidden"]);
        assert!(by_path[".hidden/child.txt"]);
    }

    #[test]
    fn missing_root_is_a_fatal_error() {
        let root = tempdir().unwrap();
        let missing = root.path().join("does-not-exist");
        assert!(build_catalog(&missing, &AtomicBool::new(false), |_| {}, |_| {}).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn walker_does_not_descend_into_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("target")).unwrap();
        fs::write(root.path().join("target/file.txt"), b"x").unwrap();
        symlink(root.path().join("target"), root.path().join("link")).unwrap();

        let candidates = collect(root.path(), &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.display.as_ref() == "link")
        );
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.display.as_ref() == "link/file.txt")
        );
    }

    #[test]
    fn walker_cancels_after_a_partial_batch() {
        let root = tempdir().unwrap();
        for index in 0..(BATCH_SIZE + 10) {
            fs::write(root.path().join(format!("file-{index}")), b"x").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let result = build_catalog(
            root.path(),
            &cancel,
            |_| cancel.store(true, Ordering::Relaxed),
            |_| {},
        )
        .unwrap();
        assert_eq!(result, None);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directory_is_skipped_and_siblings_continue() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let blocked = root.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("hidden.txt"), b"x").unwrap();
        fs::write(root.path().join("sibling.txt"), b"x").unwrap();
        let original = fs::metadata(&blocked).unwrap().permissions();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        struct Restore<'a>(&'a Path, fs::Permissions);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                fs::set_permissions(self.0, self.1.clone()).unwrap();
            }
        }
        let _restore = Restore(&blocked, original);
        if fs::read_dir(&blocked).is_ok() {
            return;
        }

        let candidates = collect(root.path(), &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.display.as_ref() == "sibling.txt")
        );
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.display.as_ref() == "blocked/hidden.txt")
        );
    }

    /// 旧実装(単一スレッド再帰walker)。before計測のベースラインとして温存する。
    fn old_build_catalog(root: &Path) -> usize {
        fn walk(directory: &Path, relative: &str, out: &mut Vec<SearchCandidate>) {
            let Ok(entries) = fs::read_dir(crate::long_path::to_fs(directory)) else {
                return;
            };
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let Some(name) = file_name.to_str() else {
                    continue;
                };
                let logical_path = directory.join(&file_name);
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let display = if relative.is_empty() {
                    name.to_owned()
                } else {
                    format!("{relative}/{name}")
                };
                if file_type.is_symlink() {
                    out.push(SearchCandidate::new(display, EntryKind::Symlink, false));
                    continue;
                }
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                let kind = crate::scan::kind_from_metadata(&metadata);
                out.push(SearchCandidate::new(display.clone(), kind, false));
                if kind == EntryKind::Dir {
                    walk(&logical_path, &display, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, "", &mut out);
        out.len()
    }

    #[test]
    #[ignore = "environment-dependent performance measurement with a ~50k entry tree"]
    fn bench_build_catalog_on_50k_entries() {
        use std::time::Instant;

        const DIRECTORY_COUNT: usize = 200;
        const FILES_PER_DIRECTORY: usize = 250;
        const ITERATIONS: usize = 10;

        let root = tempdir().unwrap();
        for directory in 0..DIRECTORY_COUNT {
            let dir = root.path().join(format!("dir_{directory:04}"));
            fs::create_dir(&dir).unwrap();
            for file in 0..FILES_PER_DIRECTORY {
                fs::write(dir.join(format!("file_{file:04}.txt")), b"x").unwrap();
            }
        }

        let mut before_total = std::time::Duration::ZERO;
        let mut before_count = 0usize;
        for _ in 0..ITERATIONS {
            let started = Instant::now();
            before_count = old_build_catalog(root.path());
            before_total += started.elapsed();
        }

        let mut after_total = std::time::Duration::ZERO;
        let mut after_count = 0usize;
        for _ in 0..ITERATIONS {
            let started = Instant::now();
            let mut count = 0usize;
            let summary = build_catalog(
                root.path(),
                &AtomicBool::new(false),
                |batch| count += batch.len(),
                |_| {},
            )
            .unwrap()
            .unwrap();
            after_total += started.elapsed();
            after_count = count;
            assert_eq!(summary.indexed_count, count);
        }

        assert_eq!(before_count, after_count);
        eprintln!(
            "build_catalog bench: entries={after_count}, iterations={ITERATIONS}, threads={}, before(single-thread)={:?}, after(parallel)={:?}",
            std::thread::available_parallelism()
                .map(|value| value.get())
                .unwrap_or(1)
                .clamp(1, MAX_THREADS),
            before_total / ITERATIONS as u32,
            after_total / ITERATIONS as u32,
        );
    }
}
