//! baselineから独立したSearchCatalogと、GUI外でmatchingする単一worker。
//!
//! 照合はnucleo-matcher(低レベルcrate)で行う。overlayが削除を表現するため
//! 高レベル`nucleo`のinjector runtimeは使えない。従来のchunk+overlay+latest-wins
//! worker構成を維持し、スコアリングだけを差し替える。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use fyler_core::pane::PaneId;
use fyler_core::path::TreePath;
use fyler_core::search::SearchCandidate;
use fyler_core::tree::EntryKind;
use fyler_gui::app::{GuiEvent, PickerHit};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};

use super::AppEvent;
use crate::queue_stats::CountingSender;

pub(super) const PICKER_RESULT_LIMIT: usize = 100;

#[derive(Default)]
struct CatalogOverlay {
    entries: HashMap<Box<str>, Option<SearchCandidate>>,
    order: Vec<Box<str>>,
    removed_directories: HashSet<Box<str>>,
}

/// sealed chunk。候補と、nucleoのhaystack(`Utf32String`)を事前変換して保持する。
/// haystackはchunkと同じ添字で並ぶ。
struct SealedChunk {
    candidates: Arc<[SearchCandidate]>,
    haystacks: Vec<Utf32String>,
}

/// 完了済みchunkは不変なArcとして共有し、検索中のappend lock競合を避ける。
pub(super) struct SearchCatalog {
    chunks: Mutex<Vec<Arc<SealedChunk>>>,
    // directoryは全entryの一部なので、display重複保持と引き換えにwatch処理をO(1)化する。
    dir_index: Mutex<HashSet<Box<str>>>,
    overlay: Mutex<CatalogOverlay>,
    indexed_count: AtomicUsize,
    complete: AtomicBool,
    cancel: AtomicBool,
}

impl SearchCatalog {
    fn new() -> Self {
        Self {
            chunks: Mutex::new(Vec::new()),
            dir_index: Mutex::new(HashSet::new()),
            overlay: Mutex::new(CatalogOverlay::default()),
            indexed_count: AtomicUsize::new(0),
            complete: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        }
    }

    fn chunks(&self) -> Vec<Arc<SealedChunk>> {
        self.chunks
            .lock()
            .map_or_else(|_| Vec::new(), |chunks| chunks.clone())
    }

    fn append_build_batch(&self, batch: Vec<SearchCandidate>) {
        let count = batch.len();
        if let Ok(mut dir_index) = self.dir_index.lock() {
            dir_index.extend(
                batch
                    .iter()
                    .filter(|candidate| candidate.kind == EntryKind::Dir)
                    .map(|candidate| candidate.display.clone()),
            );
        }
        let haystacks = batch
            .iter()
            .map(|candidate| Utf32String::from(candidate.display.as_ref()))
            .collect();
        let chunk = Arc::new(SealedChunk {
            candidates: Arc::from(batch),
            haystacks,
        });
        if let Ok(mut chunks) = self.chunks.lock() {
            chunks.push(chunk);
            self.indexed_count.fetch_add(count, Ordering::Relaxed);
        }
    }

    fn update_overlay(&self, root: &Path, paths: &BTreeSet<PathBuf>) {
        for path in paths {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let Some(display) = relative
                .components()
                .map(|component| component.as_os_str().to_str())
                .collect::<Option<Vec<_>>>()
                .map(|components| components.join("/"))
            else {
                continue;
            };
            if display.is_empty() {
                continue;
            }
            let candidate = match fyler_fsops::catalog::candidate_for_path(root, path) {
                Ok(candidate) => candidate,
                Err(_) => continue,
            };
            let Ok(mut dir_index) = self.dir_index.lock() else {
                continue;
            };
            let removed_directory = candidate.is_none() && dir_index.contains(display.as_str());
            match &candidate {
                Some(candidate) if candidate.kind == EntryKind::Dir => {
                    dir_index.insert(display.clone().into_boxed_str());
                }
                Some(_) => {
                    dir_index.remove(display.as_str());
                }
                None => {}
            }
            drop(dir_index);
            let Ok(mut overlay) = self.overlay.lock() else {
                continue;
            };
            if !overlay.entries.contains_key(display.as_str()) {
                overlay.order.push(display.clone().into_boxed_str());
            }
            if removed_directory {
                overlay
                    .removed_directories
                    .insert(display.clone().into_boxed_str());
                let prefix = format!("{display}/");
                for (path, candidate) in &mut overlay.entries {
                    if path.starts_with(&prefix) {
                        *candidate = None;
                    }
                }
            } else if candidate.is_some() {
                overlay.removed_directories.remove(display.as_str());
            }
            overlay.entries.insert(display.into_boxed_str(), candidate);
        }
    }
}

pub(super) struct CatalogService {
    catalogs: HashMap<PathBuf, Arc<SearchCatalog>>,
    pane_roots: HashMap<PaneId, PathBuf>,
    event_tx: CountingSender<AppEvent>,
}

impl CatalogService {
    pub(super) fn new(event_tx: CountingSender<AppEvent>) -> Self {
        Self {
            catalogs: HashMap::new(),
            pane_roots: HashMap::new(),
            event_tx,
        }
    }

    /// paneのrootが判明した時点で(startup登録・split)背景indexを先行起動する。
    pub(super) fn register_pane(&mut self, pane_id: PaneId, root: PathBuf) {
        self.pane_roots.insert(pane_id, root.clone());
        self.ensure(&root);
    }

    /// root変更でも新rootの背景indexを先行起動し、参照されなくなったrootを解放する。
    pub(super) fn change_root(&mut self, pane_id: PaneId, root: PathBuf) {
        self.pane_roots.insert(pane_id, root.clone());
        self.drop_unreferenced();
        self.ensure(&root);
    }

    pub(super) fn remove_pane(&mut self, pane_id: PaneId) {
        self.pane_roots.remove(&pane_id);
        self.drop_unreferenced();
    }

    fn drop_unreferenced(&mut self) {
        self.catalogs.retain(|root, catalog| {
            let keep = self.pane_roots.values().any(|pane_root| pane_root == root);
            if !keep {
                catalog.cancel.store(true, Ordering::Relaxed);
            }
            keep
        });
    }

    pub(super) fn ensure(&mut self, root: &Path) -> Arc<SearchCatalog> {
        if let Some(catalog) = self.catalogs.get(root) {
            return Arc::clone(catalog);
        }
        let root = root.to_path_buf();
        let catalog = Arc::new(SearchCatalog::new());
        self.catalogs.insert(root.clone(), Arc::clone(&catalog));
        self.spawn_build(root, Arc::clone(&catalog));
        catalog
    }

    pub(super) fn get(&self, root: &Path) -> Option<Arc<SearchCatalog>> {
        self.catalogs.get(root).map(Arc::clone)
    }

    fn spawn_build(&self, root: PathBuf, catalog: Arc<SearchCatalog>) {
        let event_tx = self.event_tx.clone();
        let worker_catalog = Arc::clone(&catalog);
        let worker_root = root.clone();
        let spawn = thread::Builder::new()
            .name("fyler-catalog-scan".to_owned())
            // filesystem再帰の深さが読めないため既定stackを維持する。
            .spawn(move || {
                let result = fyler_fsops::catalog::build_catalog(
                    &worker_root,
                    &worker_catalog.cancel,
                    |batch| {
                        worker_catalog.append_build_batch(batch);
                        let _ = event_tx.send(AppEvent::CatalogChanged {
                            root: worker_root.clone(),
                            error: None,
                        });
                    },
                    |_| {},
                );
                match result {
                    Ok(Some(_)) => {
                        worker_catalog.complete.store(true, Ordering::Release);
                        let _ = event_tx.send(AppEvent::CatalogChanged {
                            root: worker_root,
                            error: None,
                        });
                    }
                    Ok(None) => {}
                    Err(error) => {
                        worker_catalog.complete.store(true, Ordering::Release);
                        let _ = event_tx.send(AppEvent::CatalogChanged {
                            root: worker_root,
                            error: Some(format!("Failed to index files: {error:#}")),
                        });
                    }
                }
            });
        if spawn.is_err() {
            catalog.complete.store(true, Ordering::Release);
            let _ = self.event_tx.send(AppEvent::CatalogChanged {
                root,
                error: Some("Failed to start file indexing worker".to_owned()),
            });
        }
    }

    pub(super) fn update(&self, root: &Path, paths: &BTreeSet<PathBuf>) -> bool {
        let Some(catalog) = self.catalogs.get(root) else {
            return false;
        };
        catalog.update_overlay(root, paths);
        true
    }

    pub(super) fn invalidate(&mut self, root: &Path) {
        if let Some(catalog) = self.catalogs.remove(root) {
            catalog.cancel.store(true, Ordering::Relaxed);
        }
    }
}

pub(super) struct PickerSearchWorker {
    request_tx: mpsc::Sender<SearchRequest>,
    generation: Arc<AtomicU64>,
}

struct SearchRequest {
    generation: u64,
    pane_id: PaneId,
    query: String,
    include_hidden: bool,
    catalog: Arc<SearchCatalog>,
}

impl PickerSearchWorker {
    pub(super) fn new(gui_event_tx: CountingSender<GuiEvent>) -> anyhow::Result<Self> {
        let (request_tx, request_rx) = mpsc::channel::<SearchRequest>();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&generation);
        thread::Builder::new()
            .name("fyler-picker-search".to_owned())
            .stack_size(256 * 1024)
            .spawn(move || {
                // Matcherはstatefulなscratchメモリなのでworkerで1つを再利用する。
                let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
                while let Ok(mut request) = request_rx.recv() {
                    // 連投時は開始前に古いqueryを捨て、最新だけを処理する。
                    while let Ok(latest) = request_rx.try_recv() {
                        request = latest;
                    }
                    let results = run_search(&request, &mut matcher);
                    if worker_generation.load(Ordering::Acquire) != request.generation {
                        continue;
                    }
                    let _ = gui_event_tx.send(GuiEvent::PickerResults {
                        pane_id: request.pane_id,
                        query: request.query,
                        results,
                        indexed_count: request.catalog.indexed_count.load(Ordering::Relaxed),
                        indexing: !request.catalog.complete.load(Ordering::Acquire),
                    });
                }
            })
            .map_err(|error| anyhow::anyhow!("Failed to start picker search worker: {error}"))?;
        Ok(Self {
            request_tx,
            generation,
        })
    }

    pub(super) fn request(
        &self,
        pane_id: PaneId,
        query: String,
        include_hidden: bool,
        catalog: Arc<SearchCatalog>,
    ) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.request_tx.send(SearchRequest {
            generation,
            pane_id,
            query,
            include_hidden,
            catalog,
        });
    }

    pub(super) fn invalidate_pending(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// sealed chunk + overlayをnucleoで照合し、上位`PICKER_RESULT_LIMIT`件を返す。
///
/// scoreは降順、同点はpath昇順で安定させる(並列scan下でも決定的)。空queryは
/// 走査順の先頭N件をscore 0で返す。matchのみをsortし、候補全体はsortしない。
fn run_search(request: &SearchRequest, matcher: &mut Matcher) -> Vec<PickerHit> {
    let chunks = request.catalog.chunks();
    let (overlay_entries, overlay_order, removed_directories) = request
        .catalog
        .overlay
        .lock()
        .map(|overlay| {
            (
                overlay.entries.clone(),
                overlay.order.clone(),
                overlay.removed_directories.clone(),
            )
        })
        .unwrap_or_default();

    // 未sealed batchのdirがdir_indexへ入る前にdelete通知が先行した場合は、一時的な
    // ghostを許容する。選択時のbaseline再解決でstale Warnとなり、次のwatch更新または
    // catalog再構築で収束するため、検索毎の全件走査はしない。
    let shadowed = |display: &str| {
        overlay_entries.contains_key(display)
            || removed_directories.iter().any(|directory| {
                display.len() > directory.len()
                    && display.starts_with(directory.as_ref())
                    && display.as_bytes().get(directory.len()) == Some(&b'/')
            })
    };
    let include = |candidate: &SearchCandidate| {
        (request.include_hidden || !candidate.hidden) && !shadowed(candidate.display.as_ref())
    };

    let pattern = Pattern::parse(&request.query, CaseMatching::Ignore, Normalization::Smart);

    // 空query: 走査順の先頭N件。sortしない。
    if pattern.atoms.is_empty() {
        let mut hits = Vec::with_capacity(PICKER_RESULT_LIMIT);
        for chunk in &chunks {
            for candidate in chunk.candidates.iter() {
                if include(candidate) {
                    hits.push(picker_hit(candidate));
                    if hits.len() >= PICKER_RESULT_LIMIT {
                        return hits;
                    }
                }
            }
        }
        for path in &overlay_order {
            if let Some(Some(candidate)) = overlay_entries.get(path.as_ref())
                && include(candidate)
            {
                hits.push(picker_hit(candidate));
                if hits.len() >= PICKER_RESULT_LIMIT {
                    return hits;
                }
            }
        }
        return hits;
    }

    let mut matches: Vec<(&SearchCandidate, u32)> = Vec::new();
    for chunk in &chunks {
        for (index, candidate) in chunk.candidates.iter().enumerate() {
            if !include(candidate) {
                continue;
            }
            if let Some(score) = pattern.score(chunk.haystacks[index].slice(..), matcher) {
                matches.push((candidate, score));
            }
        }
    }
    for path in &overlay_order {
        if let Some(Some(candidate)) = overlay_entries.get(path.as_ref())
            && include(candidate)
        {
            let haystack = Utf32String::from(candidate.display.as_ref());
            if let Some(score) = pattern.score(haystack.slice(..), matcher) {
                matches.push((candidate, score));
            }
        }
    }

    matches.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.0.display.cmp(&right.0.display))
    });
    matches.truncate(PICKER_RESULT_LIMIT);
    matches
        .into_iter()
        .map(|(candidate, _)| picker_hit(candidate))
        .collect()
}

fn picker_hit(candidate: &SearchCandidate) -> PickerHit {
    PickerHit {
        path: TreePath::parse(&candidate.display),
        display: candidate.display.to_string(),
        kind: candidate.kind,
    }
}

#[derive(Debug, Clone)]
pub(super) struct ActivePicker {
    pub pane_id: PaneId,
    pub root: PathBuf,
    pub query: String,
    pub include_hidden: bool,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::queue_stats::QueueGauge;

    use super::*;

    fn sealed_chunk(candidates: Vec<SearchCandidate>) -> Arc<SealedChunk> {
        let haystacks = candidates
            .iter()
            .map(|candidate| Utf32String::from(candidate.display.as_ref()))
            .collect();
        Arc::new(SealedChunk {
            candidates: Arc::from(candidates),
            haystacks,
        })
    }

    fn catalog(candidates: Vec<SearchCandidate>) -> Arc<SearchCatalog> {
        let dir_index = candidates
            .iter()
            .filter(|candidate| candidate.kind == EntryKind::Dir)
            .map(|candidate| candidate.display.clone())
            .collect();
        Arc::new(SearchCatalog {
            indexed_count: AtomicUsize::new(candidates.len()),
            complete: AtomicBool::new(true),
            chunks: Mutex::new(vec![sealed_chunk(candidates)]),
            dir_index: Mutex::new(dir_index),
            overlay: Mutex::new(CatalogOverlay::default()),
            cancel: AtomicBool::new(false),
        })
    }

    fn file(path: &str) -> SearchCandidate {
        SearchCandidate::new(path.to_owned(), EntryKind::File, false)
    }

    fn matched(candidates: Vec<SearchCandidate>, query: &str, include_hidden: bool) -> Vec<String> {
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let request = SearchRequest {
            generation: 0,
            pane_id: PaneId::new(1),
            query: query.to_owned(),
            include_hidden,
            catalog: catalog(candidates),
        };
        run_search(&request, &mut matcher)
            .into_iter()
            .map(|hit| hit.display)
            .collect()
    }

    fn visible_overlay_paths(catalog: &SearchCatalog) -> Vec<String> {
        let chunks = catalog.chunks();
        let overlay = catalog.overlay.lock().unwrap();
        chunks
            .iter()
            .flat_map(|chunk| chunk.candidates.iter())
            .filter(|candidate| {
                !overlay.entries.contains_key(candidate.display.as_ref())
                    && !overlay.removed_directories.iter().any(|directory| {
                        candidate.display.starts_with(directory.as_ref())
                            && candidate.display.as_bytes().get(directory.len()) == Some(&b'/')
                    })
            })
            .chain(
                overlay
                    .order
                    .iter()
                    .filter_map(|path| overlay.entries.get(path.as_ref()).and_then(Option::as_ref)),
            )
            .map(|candidate| candidate.display.to_string())
            .collect()
    }

    #[test]
    fn basename_match_ranks_above_dir_component_match() {
        let results = matched(
            vec![
                file("foo/other.txt"),
                file("docs/foo.txt"),
                file("bar/baz.txt"),
            ],
            "foo",
            true,
        );
        // basename一致(docs/foo.txt)がdir成分一致(foo/other.txt)より上位。
        assert_eq!(results[0], "docs/foo.txt");
        assert!(results.contains(&"foo/other.txt".to_owned()));
        assert!(!results.contains(&"bar/baz.txt".to_owned()));
    }

    #[test]
    fn whitespace_tokens_all_must_match() {
        let results = matched(
            vec![
                file("src/main.rs"),
                file("src/lib.rs"),
                file("tests/main.rs"),
            ],
            "src main",
            true,
        );
        assert_eq!(results, ["src/main.rs"]);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let results = matched(vec![file("Foo.txt"), file("bar.txt")], "foo", true);
        assert_eq!(results, ["Foo.txt"]);
    }

    #[test]
    fn hidden_candidates_are_filtered_unless_requested() {
        let candidates = vec![
            SearchCandidate::new("visible.txt".to_owned(), EntryKind::File, false),
            SearchCandidate::new(".hidden.txt".to_owned(), EntryKind::File, true),
        ];
        assert_eq!(matched(candidates.clone(), "", false).len(), 1);
        assert_eq!(matched(candidates.clone(), "", true).len(), 2);
        assert!(matched(candidates, "hidden", false).is_empty());
    }

    #[test]
    fn equal_scores_break_ties_by_path_ascending() {
        // 同一basename・同一構造→同点。path昇順で決定的に並ぶ。
        let results = matched(
            vec![file("z/item.txt"), file("a/item.txt"), file("m/item.txt")],
            "item.txt",
            true,
        );
        assert_eq!(results, ["a/item.txt", "m/item.txt", "z/item.txt"]);
    }

    #[test]
    fn empty_query_returns_first_n_in_iteration_order() {
        let candidates = (0..(PICKER_RESULT_LIMIT + 50))
            .map(|index| file(&format!("item-{index:04}.txt")))
            .collect::<Vec<_>>();
        let results = matched(candidates, "", true);
        assert_eq!(results.len(), PICKER_RESULT_LIMIT);
        assert_eq!(results[0], "item-0000.txt");
        assert_eq!(results[1], "item-0001.txt");
    }

    #[test]
    fn catalog_is_shared_until_the_last_pane_leaves_the_root() {
        let gauge = Arc::new(QueueGauge::new());
        let (tx, _rx) = mpsc::channel();
        let mut service = CatalogService::new(CountingSender::new(tx, gauge));
        let root = PathBuf::from("shared-root");
        let catalog = catalog(Vec::new());
        service.catalogs.insert(root.clone(), Arc::clone(&catalog));
        // register_paneはensureで背景buildを起動しうるが、既存catalogがあれば再利用する。
        service.pane_roots.insert(PaneId::new(1), root.clone());
        service.pane_roots.insert(PaneId::new(2), root.clone());

        service.remove_pane(PaneId::new(1));
        assert!(service.get(&root).is_some());
        assert!(!catalog.cancel.load(Ordering::Relaxed));

        service.remove_pane(PaneId::new(2));
        assert!(service.get(&root).is_none());
        assert!(catalog.cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn build_batch_directory_is_tombstoned_through_dir_index() {
        let root = tempfile::tempdir().unwrap();
        let catalog = SearchCatalog::new();
        catalog.append_build_batch(vec![
            SearchCandidate::new("built-dir".to_owned(), EntryKind::Dir, false),
            SearchCandidate::new("built-dir/child".to_owned(), EntryKind::File, false),
        ]);

        catalog.update_overlay(
            root.path(),
            &[root.path().join("built-dir")].into_iter().collect(),
        );

        assert!(visible_overlay_paths(&catalog).is_empty());
        assert!(
            catalog
                .overlay
                .lock()
                .unwrap()
                .removed_directories
                .contains("built-dir")
        );
    }

    #[test]
    fn overlay_create_delete_rename_and_directory_delete_affect_search() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("old.txt"), b"x").unwrap();
        std::fs::rename(root.path().join("old.txt"), root.path().join("renamed.txt")).unwrap();
        std::fs::write(root.path().join("created.txt"), b"x").unwrap();
        let catalog = catalog(vec![
            SearchCandidate::new("old.txt".to_owned(), EntryKind::File, false),
            SearchCandidate::new("deleted.txt".to_owned(), EntryKind::File, false),
            SearchCandidate::new("dir".to_owned(), EntryKind::Dir, false),
            SearchCandidate::new("dir/child.txt".to_owned(), EntryKind::File, false),
        ]);
        catalog.update_overlay(
            root.path(),
            &[
                root.path().join("old.txt"),
                root.path().join("renamed.txt"),
                root.path().join("created.txt"),
                root.path().join("deleted.txt"),
                root.path().join("dir"),
            ]
            .into_iter()
            .collect(),
        );

        let mut visible = visible_overlay_paths(&catalog);
        visible.sort();
        assert_eq!(visible, ["created.txt", "renamed.txt"]);
    }

    #[test]
    fn deleting_directory_tombstones_overlay_added_descendants() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("X")).unwrap();
        std::fs::write(root.path().join("X/a"), b"x").unwrap();
        let catalog = catalog(Vec::new());
        catalog.update_overlay(
            root.path(),
            &[root.path().join("X"), root.path().join("X/a")]
                .into_iter()
                .collect(),
        );
        let mut visible = visible_overlay_paths(&catalog);
        visible.sort();
        assert_eq!(visible, ["X", "X/a"]);

        std::fs::remove_dir_all(root.path().join("X")).unwrap();
        catalog.update_overlay(root.path(), &[root.path().join("X")].into_iter().collect());

        assert!(visible_overlay_paths(&catalog).is_empty());
        let overlay = catalog.overlay.lock().unwrap();
        assert!(overlay.entries["X"].is_none());
        assert!(overlay.entries["X/a"].is_none());
    }

    #[test]
    fn search_worker_latest_query_wins() {
        let gauge = Arc::new(QueueGauge::new());
        let (tx, rx) = mpsc::channel();
        let worker = PickerSearchWorker::new(CountingSender::new(tx, gauge)).unwrap();
        let catalog = catalog(
            (0..50_000)
                .map(|index| {
                    SearchCandidate::new(format!("item-{index:05}.txt"), EntryKind::File, false)
                })
                .collect(),
        );
        worker.request(
            PaneId::new(1),
            "item".to_owned(),
            false,
            Arc::clone(&catalog),
        );
        worker.request(PaneId::new(1), "49999".to_owned(), false, catalog);
        let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let GuiEvent::PickerResults { query, results, .. } = event else {
            panic!("unexpected event");
        };
        assert_eq!(query, "49999");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].display, "item-49999.txt");
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }
}
