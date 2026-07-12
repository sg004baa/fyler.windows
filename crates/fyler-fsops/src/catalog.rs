//! fuzzy finder専用SearchCatalogの読み取り専用walker。
//!
//! 候補はソートせずDFS到達順で追加する。同点scoreの安定順は、この挿入順を正典とする。

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use fyler_core::search::SearchCandidate;
use fyler_core::tree::EntryKind;

const BATCH_SIZE: usize = 4096;

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
    let Some(display) = relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .map(|components| components.join("/"))
    else {
        return Ok(None);
    };
    let metadata = match fs::symlink_metadata(crate::long_path::to_fs(path)) {
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

/// `root`以下を読み取り専用で再帰列挙し、候補をbatch単位で返す。
///
/// `long_path::to_fs`はsyscall引数だけへ適用し、候補の論理パスは呼び出し側の
/// 素のパスと`file_name`から構築する。symlink/junction/reparse pointには潜らない。
/// キャンセルはディレクトリ開始時と各entry処理時に確認し、`Ok(None)`を返す。
pub fn build_catalog(
    root: &Path,
    cancel: &AtomicBool,
    mut on_batch: impl FnMut(Vec<SearchCandidate>),
    mut on_progress: impl FnMut(CatalogProgress),
) -> anyhow::Result<Option<CatalogSummary>> {
    let mut state = WalkState {
        cancel,
        batch: Vec::with_capacity(BATCH_SIZE),
        indexed_count: 0,
        skipped_directories: 0,
        on_batch: &mut on_batch,
        on_progress: &mut on_progress,
    };
    walk_directory(root, "", false, true, &mut state)?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    state.flush();
    Ok(Some(CatalogSummary {
        indexed_count: state.indexed_count,
        skipped_directories: state.skipped_directories,
    }))
}

struct WalkState<'a, B, P> {
    cancel: &'a AtomicBool,
    batch: Vec<SearchCandidate>,
    indexed_count: usize,
    skipped_directories: usize,
    on_batch: &'a mut B,
    on_progress: &'a mut P,
}

impl<B, P> WalkState<'_, B, P>
where
    B: FnMut(Vec<SearchCandidate>),
    P: FnMut(CatalogProgress),
{
    fn push(&mut self, candidate: SearchCandidate) {
        self.batch.push(candidate);
        self.indexed_count += 1;
        if self.batch.len() >= BATCH_SIZE {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if !self.batch.is_empty() {
            (self.on_batch)(std::mem::replace(
                &mut self.batch,
                Vec::with_capacity(BATCH_SIZE),
            ));
        }
        (self.on_progress)(CatalogProgress {
            indexed_count: self.indexed_count,
        });
    }
}

fn walk_directory<B, P>(
    directory: &Path,
    relative: &str,
    ancestor_hidden: bool,
    root: bool,
    state: &mut WalkState<'_, B, P>,
) -> anyhow::Result<()>
where
    B: FnMut(Vec<SearchCandidate>),
    P: FnMut(CatalogProgress),
{
    if state.cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    let entries = match fs::read_dir(crate::long_path::to_fs(directory)) {
        Ok(entries) => entries,
        Err(_error) if !root => {
            state.skipped_directories += 1;
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to enumerate catalog root: {}", directory.display())
            });
        }
    };

    for entry in entries {
        if state.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Ok(entry) = entry else {
            continue;
        };
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        // DirEntry::path()はread_dirへ渡した拡張形式を引き継ぐため使用しない。
        let logical_path = directory.join(&file_name);
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        #[cfg(not(windows))]
        if file_type.is_symlink() {
            // UnixではDirEntry::metadataがlink先を追うため、broken linkでも候補として
            // 残せるようmetadataを呼ばない。hidden判定はdot名だけで完結する。
            let hidden = ancestor_hidden || file_name.as_encoded_bytes().first() == Some(&b'.');
            let display = if relative.is_empty() {
                name.to_owned()
            } else {
                format!("{relative}/{name}")
            };
            state.push(SearchCandidate::new(display, EntryKind::Symlink, hidden));
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let hidden = ancestor_hidden || super::scan::is_hidden(&file_name, &metadata);
        let display = if relative.is_empty() {
            name.to_owned()
        } else {
            format!("{relative}/{name}")
        };
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else {
            super::scan::kind_from_metadata(&metadata)
        };
        state.push(SearchCandidate::new(display.clone(), kind, hidden));
        if kind == EntryKind::Dir {
            walk_directory(&logical_path, &display, hidden, false, state)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
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
        assert!(!by_path["visible/file.txt"]);
        assert!(by_path[".hidden/child.txt"]);
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
}
