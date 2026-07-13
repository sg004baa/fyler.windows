//! 複数paneのセッション所有とイベント配線。

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use fyler_core::WindowGeometry;
use fyler_core::editor::{EditorCommand, EditorEngine, EditorEvent, FoldOp, MessageKind};
use fyler_core::feedback::{FeedbackPayload, validate_body};
use fyler_core::gitstatus::GitBadge;
use fyler_core::grammar::PrefixParse;
use fyler_core::id::{EntryId, IdAllocator};
use fyler_core::keymap::{EditorAction, KeyBinding};
use fyler_core::pane::{FocusDirection, PaneAction, PaneId, PaneLayout, SplitDirection};
use fyler_core::path::TreePath;
use fyler_core::report::{ApplyProgress, CommitReport, OpOutcome, OpResult};
use fyler_core::transfer::TransferKind;
use fyler_core::tree::{BaselineTree, EntryKind};
use fyler_core::undo::UndoTransaction;
use fyler_engine_nvim::{NvimConfig, NvimEngine};
use fyler_fsops::scan::ScanOptions;
use fyler_fsops::watch::{FsWatcher, WatchEvent};
use fyler_gui::app::{FeedbackResultKind, GuiAction, GuiEvent, GuiOptions, PickerAction};
use fyler_gui::confirm::ConfirmChoice;

use super::feedback::{FeedbackOutcome, resolve_endpoint, send_feedback};
use super::nvim_locate;
use super::picker::{ActivePicker, CatalogService, PickerSearchWorker};
use super::save_flow::{FoldResult, SaveController, SaveFlowResult};
use super::session::{self, SessionPane, SessionState};
use super::transfer_flow::{
    TransferController, TransferFlowResult, TransferPaneState, build_plan, destination_directory,
    resolve_selection, resolve_target, start_rejection,
};
use super::{
    AppEvent, BookmarkResolution, GitRefresher, after_root_change, bookmark_list_message,
    default_root, format_drive_paths, handle_activate_line, handle_external_change,
    handle_open_file_picker, handle_open_terminal, handle_open_with, handle_picker_select,
    handle_yank_path, normalize_root, parse_sort_query, resolve_bookmark_query, resolve_cd_target,
    send_gui_message, send_save_result, send_view_state, sort_state_message,
};
use super::{undo_format, undo_journal};
use crate::queue_stats::{CountingSender, QueueGauge};

const MAX_PANES: usize = 4;

/// 1 paneが独立所有する実行状態。
struct PaneSession {
    id: PaneId,
    root: PathBuf,
    engine: Arc<dyn EditorEngine>,
    save_controller: SaveController,
    watcher: FsWatcher,
    watch_tx: mpsc::Sender<WatchEvent>,
    watch_degraded: bool,
    git_badges: HashMap<EntryId, GitBadge>,
    deferred_changes: BTreeSet<PathBuf>,
    undo_slot: Option<UndoTransaction>,
    crashed: bool,
    restoration_warnings: Vec<String>,
}

/// app全体で同時に1本だけ実行する列挙workerの所有状態。
struct LoaderOwner {
    pane_id: PaneId,
    root: PathBuf,
    kind: LoaderKind,
    cancel: Arc<AtomicBool>,
}

enum LoaderKind {
    Root {
        cursor_target: Option<OsString>,
    },
    Directory {
        dir: TreePath,
        line: usize,
    },
    Recursive {
        dirs: Vec<TreePath>,
        line: usize,
        op: FoldOp,
    },
    PickerReveal {
        target: TreePath,
        action: fyler_gui::app::PickerAction,
        dir: TreePath,
    },
}

impl LoaderOwner {
    fn loads_directory(&self) -> bool {
        !matches!(self.kind, LoaderKind::Root { .. })
    }

    fn shows_dialog(&self) -> bool {
        !matches!(
            self.kind,
            LoaderKind::Directory { .. } | LoaderKind::PickerReveal { .. }
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn create_pane(
    runtime: &tokio::runtime::Runtime,
    id: PaneId,
    root: PathBuf,
    nvim_exe: &Path,
    bindings: &[KeyBinding],
    scan_options: ScanOptions,
    shared_ids: Arc<Mutex<IdAllocator>>,
    app_event_tx: &CountingSender<AppEvent>,
    nvim_diagnostics: Option<&[String]>,
    restored: Option<&SessionPane>,
) -> anyhow::Result<PaneSession> {
    // nvim起動はflaky回避のため呼び出し元イベントループで必ず直列に行う。
    let (engine, mut engine_events) = runtime
        .block_on(NvimEngine::start(NvimConfig {
            nvim_exe: nvim_exe.to_path_buf(),
            root: root.clone(),
            bindings: bindings.to_vec(),
        }))
        .map_err(|error| {
            let Some(diagnostics) = nvim_diagnostics else {
                return error;
            };
            error.context(format!(
                "Failed to start Neovim.\nSearch results:\n{}\n\nReinstall fyler, install Neovim and add it to PATH, or specify the executable with FYLER_NVIM_EXE.",
                diagnostics.join("\n")
            ))
        })?;
    let mut restoration_warnings = Vec::new();
    let baseline = {
        let mut ids = shared_ids
            .lock()
            .map_err(|_| anyhow::anyhow!("ID allocator lock is poisoned"))?;
        let mut baseline =
            fyler_fsops::scan::scan_baseline_shallow_with(&root, &mut ids, &scan_options)?;
        if let Some(restored) = restored {
            let mut expanded = restored.expanded.clone();
            expanded.sort_by_key(TreePath::depth);
            for directory in expanded {
                if !baseline.is_unloaded(&directory) {
                    continue;
                }
                match fyler_fsops::scan::load_directory(
                    &root,
                    &directory,
                    &mut ids,
                    &baseline,
                    &scan_options,
                ) {
                    Ok(loaded) => baseline = loaded,
                    Err(error) => restoration_warnings.push(format!(
                        "Could not restore expanded folder {}: {error:#}",
                        directory
                    )),
                }
            }
        }
        baseline
    };
    let save_engine: Arc<dyn EditorEngine> = engine.clone();
    let mut save_controller = SaveController::new_shared_with_scan_options(
        root.clone(),
        shared_ids,
        baseline,
        Arc::clone(&save_engine),
        scan_options,
    );
    if let Some(restored) = restored {
        save_controller.restore_collapsed_paths(&restored.collapsed);
    } else {
        save_controller.collapse_all_dirs();
    }
    engine.set_initial_lines(save_controller.visible_lines())?;
    if let Some(cursor_line) = restored
        .and_then(|restored| restored.cursor.as_ref())
        .and_then(|cursor| save_controller.visible_line_for_path(cursor))
    {
        engine.send(EditorCommand::SetCursorLine(cursor_line))?;
    }

    // scanと初期行投入が成功した後にwatcherを作る。ここまでのどこかが失敗すれば
    // sessionを返さず、生成済みengineもdropされる。
    let (watch_tx, watch_rx) = mpsc::channel();
    let watcher = fyler_fsops::watch::watch(&root, watch_tx.clone())?;

    let editor_event_tx = app_event_tx.clone();
    thread::Builder::new()
        .name(format!("fyler-engine-events-{id}"))
        // blocking_recvからapp channelへ転送するだけの非再帰ループ。
        .stack_size(256 * 1024)
        .spawn(move || {
            while let Some(event) = engine_events.blocking_recv() {
                if editor_event_tx.send(AppEvent::Editor(id, event)).is_err() {
                    return;
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("Failed to start editor event forwarding: {error}"))?;

    let watch_event_tx = app_event_tx.clone();
    thread::Builder::new()
        .name(format!("fyler-watch-events-{id}"))
        // recvからapp channelへ転送するだけの非再帰ループ。
        .stack_size(256 * 1024)
        .spawn(move || {
            while let Ok(event) = watch_rx.recv() {
                let event = match event {
                    WatchEvent::Changed(change) => AppEvent::ExternalChange(id, change),
                    WatchEvent::Degraded(error) => AppEvent::WatchDegraded(id, error),
                };
                if watch_event_tx.send(event).is_err() {
                    return;
                }
            }
        })
        .map_err(|error| {
            anyhow::anyhow!("Failed to start file watcher event forwarding: {error}")
        })?;

    Ok(PaneSession {
        id,
        root,
        engine: save_engine,
        save_controller,
        watcher,
        watch_tx,
        watch_degraded: false,
        git_badges: HashMap::new(),
        deferred_changes: BTreeSet::new(),
        undo_slot: None,
        crashed: false,
        restoration_warnings,
    })
}

fn help_lines(bindings: &[KeyBinding]) -> Vec<String> {
    let mut actions = Vec::<(EditorAction, Vec<String>)>::new();
    for binding in bindings {
        if let Some((_, sequences)) = actions
            .iter_mut()
            .find(|(action, _)| *action == binding.action)
        {
            sequences.push(binding.sequence.to_string());
        } else {
            actions.push((binding.action, vec![binding.sequence.to_string()]));
        }
    }
    let mut lines = actions
        .into_iter()
        .map(|(action, sequences)| format!("{}  {}", sequences.join(", "), action.description()))
        .collect::<Vec<_>>();
    lines.extend([
        ":w  Save changes".to_owned(),
        ":cd  Change root".to_owned(),
        ":b  Bookmarks / Recent roots".to_owned(),
        ":terminal  Open terminal here".to_owned(),
        ":feedback  Send anonymous feedback".to_owned(),
    ]);
    lines
}

fn should_restore_session(explicit_root: bool, restore_session: bool) -> bool {
    !explicit_root && restore_session
}

fn existing_root_or_ancestor(requested: &Path, fallback: &Path) -> PathBuf {
    requested
        .ancestors()
        .find(|candidate| candidate.is_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| fallback.to_path_buf())
}

fn capture_session(
    panes: &BTreeMap<PaneId, PaneSession>,
    layout: PaneLayout,
    active: PaneId,
    window: Option<WindowGeometry>,
) -> SessionState {
    let panes = panes
        .iter()
        .map(|(id, pane)| {
            let snapshot = pane.engine.snapshot();
            let cursor = pane
                .save_controller
                .resolve_line(&snapshot.lines, snapshot.cursor.line)
                .map(|(path, _)| path);
            let (collapsed, expanded) = pane.save_controller.session_directory_state();
            (
                *id,
                SessionPane {
                    root: pane.root.clone(),
                    cursor,
                    collapsed,
                    expanded,
                    scan_options: pane.save_controller.scan_options(),
                },
            )
        })
        .collect();
    SessionState {
        layout,
        active,
        panes,
        window,
    }
}

pub(super) fn run() -> anyhow::Result<()> {
    let explicit_root = std::env::args_os().nth(1).map(PathBuf::from);
    let fallback_root = normalize_root(&default_root()?)?;
    let (config, config_warnings) = super::config::load();
    let mut startup_warnings = Vec::new();
    let restored_session =
        if should_restore_session(explicit_root.is_some(), config.restore_session) {
            match session::load() {
                Ok(session) => session,
                Err(error) => {
                    startup_warnings.push(format!(
                        "Could not restore the previous session; starting with one pane: {error:#}"
                    ));
                    None
                }
            }
        } else {
            None
        };
    let initial_window = restored_session.as_ref().and_then(|session| session.window);
    let window_geometry = Arc::new(Mutex::new(initial_window));
    let root = explicit_root
        .as_deref()
        .map(normalize_root)
        .transpose()?
        .unwrap_or_else(|| fallback_root.clone());
    let feedback_endpoint = resolve_endpoint(
        config.feedback_url.as_deref(),
        option_env!("FYLER_FEEDBACK_URL"),
    );
    let terminal_kind = config.terminal;
    let scan_options = ScanOptions {
        show_hidden: config.show_hidden,
        sort: config.sort,
        key: config.sort_key,
        reverse: config.sort_reverse,
    };
    let bindings = Arc::new(config.bindings);
    let gui_options = GuiOptions {
        confirm_detail: config.confirm_detail,
        font_path: config.font,
        font_y_offset_factor: config.font_y_offset_factor,
        icon_style: config.icons,
        help_lines: help_lines(&bindings),
    };
    let bookmarks = config.bookmarks;
    let resolved_nvim = nvim_locate::resolve();
    let nvim_exe = resolved_nvim.path.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        // pane最大4個のnvim RPC非同期I/Oをホストする。CPU bound処理は載せない。
        .worker_threads(2)
        .enable_all()
        .build()?;

    // 一回性イベントを落とさないためunboundedのまま保ち、滞留は計測する。
    let app_event_gauge = Arc::new(QueueGauge::new());
    let (app_event_inner_tx, app_event_rx) = mpsc::channel();
    let app_event_tx = CountingSender::new(app_event_inner_tx, Arc::clone(&app_event_gauge));
    let retry_event_tx = app_event_tx.clone();
    thread::Builder::new()
        .name("fyler-offline-retry".to_owned())
        // 5秒待機してtickを送るだけの非再帰ループ。
        .stack_size(256 * 1024)
        .spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(5));
                if retry_event_tx.send(AppEvent::OfflineRetryTick).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("Failed to start offline retry ticker: {error}"))?;
    let shared_ids = Arc::new(Mutex::new(IdAllocator::new()));
    let mut startup_panes = BTreeMap::new();
    let (requested_layout, requested_active) = match &restored_session {
        Some(restored) => (restored.layout.clone(), restored.active),
        None => {
            let id = PaneId::new(1);
            (PaneLayout::leaf(id), id)
        }
    };
    for id in requested_layout.leaves() {
        let restored = restored_session
            .as_ref()
            .and_then(|session| session.panes.get(&id));
        let requested_root = restored.map_or(root.as_path(), |pane| pane.root.as_path());
        let candidate = existing_root_or_ancestor(requested_root, &fallback_root);
        if candidate != requested_root {
            startup_warnings.push(format!(
                "Pane {id} root is unavailable; using {}",
                candidate.display()
            ));
        }
        let restored_hint = restored.filter(|pane| pane.root == candidate);
        let pane_options = restored.map_or(scan_options, |pane| pane.scan_options);
        let created = create_pane(
            &runtime,
            id,
            candidate.clone(),
            &nvim_exe,
            &bindings,
            pane_options,
            Arc::clone(&shared_ids),
            &app_event_tx,
            Some(&resolved_nvim.diagnostics),
            restored_hint,
        )
        .or_else(|error| {
            if candidate == fallback_root {
                return Err(error);
            }
            startup_warnings.push(format!(
                "Pane {id} could not open {} ({error:#}); using {}",
                candidate.display(),
                fallback_root.display()
            ));
            create_pane(
                &runtime,
                id,
                fallback_root.clone(),
                &nvim_exe,
                &bindings,
                pane_options,
                Arc::clone(&shared_ids),
                &app_event_tx,
                Some(&resolved_nvim.diagnostics),
                None,
            )
        });
        match created {
            Ok(pane) => {
                startup_panes.insert(id, pane);
            }
            Err(error) if restored_session.is_some() => startup_warnings.push(format!(
                "Pane {id} could not be restored and was removed: {error:#}"
            )),
            Err(error) => return Err(error),
        }
    }
    if startup_panes.is_empty() {
        let id = PaneId::new(1);
        startup_panes.insert(
            id,
            create_pane(
                &runtime,
                id,
                fallback_root.clone(),
                &nvim_exe,
                &bindings,
                scan_options,
                Arc::clone(&shared_ids),
                &app_event_tx,
                Some(&resolved_nvim.diagnostics),
                None,
            )?,
        );
    }
    let available = startup_panes.keys().copied().collect::<BTreeSet<_>>();
    let initial_layout = session::retain_available_layout(&requested_layout, &available)
        .unwrap_or_else(|| PaneLayout::leaf(*available.first().expect("pane exists")));
    let initial_active = if available.contains(&requested_active) {
        requested_active
    } else {
        *initial_layout
            .leaves()
            .first()
            .expect("layout contains pane")
    };
    let next_pane_id = available
        .iter()
        .map(|id| id.get())
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let (journal, journal_warning) = match undo_journal::UndoJournal::open() {
        Ok(journal) => (Some(journal), None),
        Err(error) => (
            None,
            Some(format!(
                "Failed to open undo journal. The next apply cannot be undone: {error:#}"
            )),
        ),
    };
    let (pending_recovery, recovery_warning) = match &journal {
        Some(journal) => match journal.scan_on_startup() {
            Ok(entries) => (entries, None),
            Err(error) => (
                Vec::new(),
                Some(format!("Failed to scan undo journal at startup: {error:#}")),
            ),
        },
        None => (Vec::new(), None),
    };

    let (action_tx, action_rx) = mpsc::channel();
    let action_event_tx = app_event_tx.clone();
    let action_bridge = thread::Builder::new()
        .name("fyler-gui-actions".to_owned())
        // GUI actionをapp eventへ変換するだけの非再帰ループ。
        .stack_size(256 * 1024)
        .spawn(move || {
            while let Ok(action) = action_rx.recv() {
                let event = match action {
                    GuiAction::Confirm(choice) => AppEvent::Confirm(choice),
                    GuiAction::Editor { pane_id, event } => AppEvent::Editor(pane_id, event),
                    GuiAction::LoaderCancel => AppEvent::LoaderCancel,
                    GuiAction::PickerSelect {
                        pane_id,
                        path,
                        action,
                    } => AppEvent::PickerSelect {
                        pane_id,
                        path,
                        action,
                    },
                    GuiAction::PickerQuery { pane_id, query } => {
                        AppEvent::PickerQuery { pane_id, query }
                    }
                    GuiAction::PickerClosed { pane_id } => AppEvent::PickerClosed(pane_id),
                    GuiAction::FeedbackSubmit { kind, body } => {
                        AppEvent::FeedbackSubmit { kind, body }
                    }
                    GuiAction::FeedbackClosed => AppEvent::FeedbackClosed,
                };
                if action_event_tx.send(event).is_err() {
                    return;
                }
            }
        })
        .map_err(|error| {
            anyhow::anyhow!("Failed to start confirmation result forwarding: {error}")
        })?;

    let gui_event_gauge = Arc::new(QueueGauge::new());
    let (gui_event_inner_tx, gui_event_rx) = mpsc::channel();
    let gui_event_tx = CountingSender::new(gui_event_inner_tx, Arc::clone(&gui_event_gauge));
    let picker_search = PickerSearchWorker::new(gui_event_tx.clone())?;
    for pane in startup_panes.values_mut() {
        gui_event_tx.send(GuiEvent::AddPane {
            pane_id: pane.id,
            engine: Arc::clone(&pane.engine),
            root: pane.root.clone(),
        })?;
        startup_warnings.append(&mut pane.restoration_warnings);
    }
    gui_event_tx.send(GuiEvent::LayoutChanged {
        layout: initial_layout.clone(),
        active: initial_active,
    })?;

    let event_tx = app_event_tx.clone();
    let event_loop_gauge = Arc::clone(&app_event_gauge);
    let (shutdown_result_tx, shutdown_result_rx) = mpsc::sync_channel(1);
    // rescanを含むapp event loopは再帰深度が読めないため既定stackを維持する。
    let event_bridge = thread::Builder::new()
        .name("fyler-app-events".to_owned())
        .spawn(move || {
            let mut panes = startup_panes;
            let mut layout = initial_layout;
            let mut active = initial_active;
            let mut last_active = initial_active;
            let mut next_pane_id = next_pane_id;
            let mut pending_events = VecDeque::new();
            let mut git = GitRefresher::new(event_tx.clone());
            let mut catalogs = CatalogService::new(event_tx.clone());
            for (pane_id, pane) in &panes {
                catalogs.register_pane(*pane_id, pane.root.clone());
            }
            let mut active_picker: Option<ActivePicker> = None;
            let mut dialog_owner = None;
            let mut feedback_open = false;
            let mut apply_owner = None;
            let mut loader_owner: Option<LoaderOwner> = None;
            let mut transfer = TransferController::new();
            let mut pending_recovery = pending_recovery;
            let mut pending_open_with: Option<(
                PathBuf,
                Vec<fyler_fsops::openwith::OpenWithHandler>,
            )> = None;

            for (pane_id, pane) in &mut panes {
                if send_view_state(&gui_event_tx, *pane_id, &mut pane.save_controller).is_err() {
                    return;
                }
                git.request(*pane_id, pane.root.clone());
            }
            let initial_id = active;
            if !config_warnings.is_empty()
                && send_gui_message(
                    &gui_event_tx,
                    initial_id,
                    MessageKind::Warn,
                    format!("Configuration: {}", config_warnings.join(" / ")),
                )
                .is_err()
            {
                return;
            }
            if !startup_warnings.is_empty()
                && send_gui_message(
                    &gui_event_tx,
                    initial_id,
                    MessageKind::Warn,
                    format!("Session: {}", startup_warnings.join(" / ")),
                )
                .is_err()
            {
                return;
            }
            if let Some(warning) = &journal_warning
                && send_gui_message(&gui_event_tx, initial_id, MessageKind::Warn, warning)
                    .is_err()
            {
                return;
            }
            if let Some(warning) = &recovery_warning
                && send_gui_message(&gui_event_tx, initial_id, MessageKind::Warn, warning)
                    .is_err()
            {
                return;
            }
            if let Err(error) = super::config::record_recent_root(&panes[&initial_id].root)
                && send_gui_message(
                    &gui_event_tx,
                    initial_id,
                    MessageKind::Warn,
                    format!("Failed to record recent root: {error:#}"),
                )
                .is_err()
            {
                return;
            }
            if undo_format::should_show_undo_recovery(&pending_recovery)
                && gui_event_tx
                    .send(GuiEvent::ShowUndoRecovery {
                        descriptions: undo_format::recovery_descriptions(&pending_recovery),
                    })
                    .is_err()
            {
                return;
            }

            loop {
                let event = match pending_events.pop_front() {
                    Some(event) => event,
                    None => match app_event_rx.recv() {
                        Ok(event) => {
                            event_loop_gauge.dequeue();
                            event
                        }
                        Err(_) => return,
                    },
                };
                match event {
                    AppEvent::Editor(pane_id, EditorEvent::PaneAction(action)) => {
                        if handle_pane_action(
                            action,
                            pane_id,
                            &runtime,
                            &nvim_exe,
                            &bindings,
                            scan_options,
                            &shared_ids,
                            &event_tx,
                            &gui_event_tx,
                            &mut panes,
                            &mut layout,
                            &mut active,
                            &mut last_active,
                            &mut next_pane_id,
                            &mut git,
                            &mut catalogs,
                            journal.as_ref(),
                            dialog_owner,
                            apply_owner.is_some()
                                || transfer.is_awaiting()
                                || transfer.is_running()
                                || loader_owner.is_some(),
                        )
                        .is_err()
                        {
                            return;
                        }
                        if active_picker
                            .as_ref()
                            .is_some_and(|picker| !panes.contains_key(&picker.pane_id))
                        {
                            active_picker = None;
                            picker_search.invalidate_pending();
                        }
                    }
                    AppEvent::Editor(pane_id, EditorEvent::EngineCrashed { reason }) => {
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
                        session.crashed = true;
                        if loader_owner
                            .as_ref()
                            .is_some_and(|owner| owner.pane_id == pane_id)
                        {
                            let owner = loader_owner.as_ref().expect("checked loader owner");
                            owner.cancel.store(true, Ordering::Relaxed);
                            if owner.shows_dialog() && dialog_owner == Some(pane_id) {
                                dialog_owner = None;
                                if gui_event_tx.send(GuiEvent::CloseDialog).is_err() {
                                    return;
                                }
                            }
                        }
                        if active_picker
                            .as_ref()
                            .is_some_and(|picker| picker.pane_id == pane_id)
                        {
                            active_picker = None;
                            picker_search.invalidate_pending();
                        }
                        if dialog_owner == Some(pane_id) {
                            if let SaveFlowResult::UndoCancelled { transaction } =
                                session.save_controller.on_choice(ConfirmChoice::Cancel)
                            {
                                session.undo_slot = Some(transaction);
                            }
                            dialog_owner = None;
                            if gui_event_tx.send(GuiEvent::CloseDialog).is_err() {
                                return;
                            }
                        }
                        if transfer.invalidate_if_involves(pane_id)
                            && gui_event_tx.send(GuiEvent::CloseDialog).is_err()
                        {
                            return;
                        }
                        if gui_event_tx
                            .send(GuiEvent::Editor {
                                pane_id,
                                event: EditorEvent::EngineCrashed { reason },
                            })
                            .is_err()
                        {
                            return;
                        }
                        if panes.values().all(|pane| pane.crashed)
                            && gui_event_tx
                                .send(GuiEvent::FatalError(
                                    "All editor engines have stopped".to_owned(),
                                ))
                                .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::Editor(
                        pane_id,
                        EditorEvent::TransferRequested { kind, lines },
                    ) => {
                        if pane_id != active {
                            continue;
                        }
                        if handle_transfer_request(
                            pane_id,
                            kind,
                            &lines,
                            last_active,
                            &panes,
                            apply_owner.is_some()
                                || dialog_owner.is_some()
                                || transfer.is_awaiting()
                                || transfer.is_running(),
                            &mut transfer,
                            &gui_event_tx,
                        )
                        .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::Editor(pane_id, event) => {
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
                        if session.crashed
                            && !matches!(
                                event,
                                EditorEvent::OpenFilePicker | EditorEvent::FeedbackRequested
                            )
                        {
                            continue;
                        }
                        match event {
                            EditorEvent::FeedbackRequested => {
                                if let Some(reason) = feedback_start_rejection(
                                    dialog_owner.is_some() || feedback_open,
                                ) {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        reason,
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }
                                if feedback_endpoint.is_none() {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Warn,
                                        "Feedback is currently unavailable. Details: https://github.com/sg004baa/fyler.windows/blob/main/docs/PRIVACY.md / GitHub Issues: https://github.com/sg004baa/fyler.windows/issues",
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }
                                feedback_open = true;
                                if gui_event_tx.send(GuiEvent::ShowFeedback).is_err() {
                                    return;
                                }
                            }
                            EditorEvent::OpenFilePicker => {
                                let opened = match handle_open_file_picker(
                                    pane_id,
                                    &session.save_controller,
                                    session.crashed,
                                    dialog_owner.is_some(),
                                    apply_owner.is_some(),
                                    transfer.is_awaiting(),
                                    transfer.is_running(),
                                    &gui_event_tx,
                                ) {
                                    Ok(opened) => opened,
                                    Err(_) => return,
                                };
                                if opened {
                                    let root = session.root.clone();
                                    let include_hidden =
                                        session.save_controller.scan_options().show_hidden;
                                    let catalog = catalogs.ensure(&root);
                                    active_picker = Some(ActivePicker {
                                        pane_id,
                                        root,
                                        query: String::new(),
                                        include_hidden,
                                    });
                                    picker_search.request(
                                        pane_id,
                                        String::new(),
                                        include_hidden,
                                        catalog,
                                    );
                                }
                            }
                            EditorEvent::CommitRequested { changedtick, lines } => {
                                if apply_owner.is_some()
                                    || dialog_owner.is_some()
                                    || transfer.is_awaiting()
                                    || transfer.is_running()
                                {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        "Another save is in progress",
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }
                                let result = session.save_controller.on_commit(changedtick, &lines);
                                if matches!(result, SaveFlowResult::ShowPlan { .. }) {
                                    dialog_owner = Some(pane_id);
                                }
                                if send_save_result(&gui_event_tx, pane_id, result).is_err() {
                                    return;
                                }
                            }
                            EditorEvent::UndoRequested => {
                                if let Some(reason) = undo_rejection(
                                    session.engine.snapshot().dirty,
                                    session.undo_slot.is_none(),
                                    session.save_controller.is_offline(),
                                    apply_owner.is_some()
                                        || dialog_owner.is_some()
                                        || transfer.is_awaiting()
                                        || transfer.is_running(),
                                ) {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        reason,
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }
                                let Some(transaction) = session.undo_slot.take() else {
                                    continue;
                                };
                                let restore_transaction = transaction.clone();
                                let result = session.save_controller.request_undo(transaction);
                                match &result {
                                    SaveFlowResult::ShowUndoPlan { .. } => {
                                        dialog_owner = Some(pane_id);
                                    }
                                    SaveFlowResult::UndoNothingLeft { .. } => {
                                        if let Some(journal) = &journal
                                            && let Err(error) =
                                                journal.discard(&restore_transaction.id)
                                        {
                                            eprintln!(
                                                "Failed to discard undo journal: {error:#}"
                                            );
                                        }
                                    }
                                    SaveFlowResult::Ignored => {
                                        session.undo_slot = Some(restore_transaction);
                                    }
                                    _ => {
                                        session.undo_slot = Some(restore_transaction);
                                    }
                                }
                                if send_save_result(&gui_event_tx, pane_id, result).is_err() {
                                    return;
                                }
                            }
                            EditorEvent::ActivateLine { line } => {
                                let load = match handle_activate_line(
                                    pane_id,
                                    &mut session.save_controller,
                                    session.engine.as_ref(),
                                    &session.root,
                                    line,
                                    &gui_event_tx,
                                ) {
                                    Ok(load) => load,
                                    Err(_) => return,
                                };
                                if let Some(dir) = load
                                    && request_directory_load(
                                        pane_id,
                                        dir,
                                        line,
                                        session,
                                        &shared_ids,
                                        &event_tx,
                                        &mut loader_owner,
                                    )
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            EditorEvent::NavigateInto { line } => {
                                let snapshot = session.engine.snapshot();
                                let Some(editor_line) = snapshot.lines.get(line) else {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Error,
                                        "Line to navigate to was not found",
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                };
                                if !matches!(
                                    fyler_core::grammar::split_id_prefix(&editor_line.text),
                                    PrefixParse::WithId { .. }
                                ) {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        "This is not a saved directory line",
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }
                                let Some((path, EntryKind::Dir)) =
                                    session.save_controller.resolve_line(&snapshot.lines, line)
                                else {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        "This is not a directory line",
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                };
                                let new_root = path.to_fs_path(&session.root);
                                if request_session_root_change(
                                    pane_id,
                                    new_root,
                                    None,
                                    session,
                                    &shared_ids,
                                    &event_tx,
                                    &gui_event_tx,
                                    &mut loader_owner,
                                    &mut dialog_owner,
                                    feedback_open
                                        || apply_owner.is_some()
                                        || transfer.is_awaiting()
                                        || transfer.is_running(),
                                )
                                .is_err()
                                {
                                    return;
                                }
                            }
                            EditorEvent::OpenTerminal { line } => {
                                let snapshot = session.engine.snapshot();
                                if handle_open_terminal(
                                    pane_id,
                                    &session.save_controller,
                                    &snapshot.lines,
                                    &session.root,
                                    line,
                                    terminal_kind,
                                    session.crashed,
                                    dialog_owner.is_some(),
                                    apply_owner.is_some(),
                                    transfer.is_awaiting(),
                                    transfer.is_running(),
                                    &gui_event_tx,
                                )
                                .is_err()
                                {
                                    return;
                                }
                            }
                            EditorEvent::YankPath { line } => {
                                if handle_yank_path(
                                    pane_id,
                                    &session.save_controller,
                                    session.engine.as_ref(),
                                    &session.root,
                                    line,
                                    &gui_event_tx,
                                )
                                .is_err()
                                {
                                    return;
                                }
                            }
                            EditorEvent::NavigateParent => {
                                let cursor_target = session.root.file_name().map(OsStr::to_owned);
                                let Some(new_root) = session.root.parent().map(Path::to_path_buf)
                                else {
                                    let drives = fyler_fsops::drives::list_drives();
                                    let message = if drives.len() >= 2 {
                                        format!(
                                            "No parent directory | Drives: {} (use :cd to navigate)",
                                            format_drive_paths(&drives)
                                        )
                                    } else {
                                        "No parent directory".to_owned()
                                    };
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        message,
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                };
                                if request_session_root_change(
                                    pane_id,
                                    new_root,
                                    cursor_target,
                                    session,
                                    &shared_ids,
                                    &event_tx,
                                    &gui_event_tx,
                                    &mut loader_owner,
                                    &mut dialog_owner,
                                    feedback_open
                                        || apply_owner.is_some()
                                        || transfer.is_awaiting()
                                        || transfer.is_running(),
                                )
                                .is_err()
                                {
                                    return;
                                }
                            }
                            EditorEvent::ChangeDirectory { query } => {
                                let Some(query) = query else {
                                    let drives = fyler_fsops::drives::list_drives();
                                    let message = if drives.len() >= 2 {
                                        format!(
                                            "Current: {} | Drives: {}",
                                            session.root.display(),
                                            format_drive_paths(&drives)
                                        )
                                    } else {
                                        format!("Current: {}", session.root.display())
                                    };
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        message,
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                };
                                let home = std::env::home_dir();
                                let Some(new_root) =
                                    resolve_cd_target(&query, &session.root, home.as_deref())
                                else {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Error,
                                        format!("Failed to resolve path: {query}"),
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                };
                                if request_session_root_change(
                                    pane_id,
                                    new_root,
                                    None,
                                    session,
                                    &shared_ids,
                                    &event_tx,
                                    &gui_event_tx,
                                    &mut loader_owner,
                                    &mut dialog_owner,
                                    feedback_open
                                        || apply_owner.is_some()
                                        || transfer.is_awaiting()
                                        || transfer.is_running(),
                                )
                                .is_err()
                                {
                                    return;
                                }
                            }
                            EditorEvent::JumpBookmark { query } => {
                                let recent = super::config::load_recent_roots();
                                let Some(query) = query else {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        bookmark_list_message(&bookmarks, &recent),
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                };
                                match resolve_bookmark_query(&query, &bookmarks, &recent) {
                                    BookmarkResolution::Resolved(new_root) => {
                                        if request_session_root_change(
                                            pane_id,
                                            new_root,
                                            None,
                                            session,
                                            &shared_ids,
                                            &event_tx,
                                            &gui_event_tx,
                                            &mut loader_owner,
                                            &mut dialog_owner,
                                            feedback_open
                                                || apply_owner.is_some()
                                                || transfer.is_awaiting()
                                                || transfer.is_running(),
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                    BookmarkResolution::Ambiguous(names) => {
                                        if send_gui_message(
                                            &gui_event_tx,
                                            pane_id,
                                            MessageKind::Error,
                                            format!(
                                                "Bookmark name is ambiguous: {query} ({})",
                                                names.join(", ")
                                            ),
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                    BookmarkResolution::NotFound => {
                                        if send_gui_message(
                                            &gui_event_tx,
                                            pane_id,
                                            MessageKind::Error,
                                            format!(
                                                "Bookmark or recent root was not found: {query}"
                                            ),
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                }
                            }
                            EditorEvent::ToggleHidden => {
                                if session.engine.snapshot().dirty {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        "Cannot toggle hidden files while editing. Save or discard changes.",
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }
                                let lines = match session.save_controller.toggle_hidden() {
                                    Ok(lines) => lines,
                                    Err(error) => {
                                        if send_gui_message(
                                            &gui_event_tx,
                                            pane_id,
                                            MessageKind::Error,
                                            format!(
                                                "Failed to toggle hidden files: {error:#}"
                                            ),
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                        if session.save_controller.is_offline()
                                            && send_view_state(
                                                &gui_event_tx,
                                                pane_id,
                                                &mut session.save_controller,
                                            )
                                            .is_err()
                                        {
                                            return;
                                        }
                                        continue;
                                    }
                                };
                                if let Err(error) = session.engine.send(EditorCommand::SetLines {
                                    lines,
                                    cursor_line: None,
                                }) && send_gui_message(
                                    &gui_event_tx,
                                    pane_id,
                                    MessageKind::Error,
                                    format!("Failed to update hidden-file view: {error:#}"),
                                )
                                .is_err()
                                {
                                    return;
                                }
                                if send_view_state(
                                    &gui_event_tx,
                                    pane_id,
                                    &mut session.save_controller,
                                )
                                .is_err()
                                {
                                    return;
                                }
                                git.request(pane_id, session.root.clone());
                            }
                            EditorEvent::Fold { op, line } => {
                                let snapshot = session.engine.snapshot();
                                if snapshot.dirty {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        "Cannot change folds while editing. Save with :w or undo the changes.",
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }

                                match session.save_controller.fold(&snapshot.lines, line, op) {
                                    FoldResult::Applied { lines, cursor_line } => {
                                        if let Err(error) =
                                            session.engine.send(EditorCommand::SetLines {
                                                lines,
                                                cursor_line,
                                            })
                                        {
                                            if send_gui_message(
                                                &gui_event_tx,
                                                pane_id,
                                                MessageKind::Error,
                                                format!(
                                                    "Failed to update folded view: {error:#}"
                                                ),
                                            )
                                            .is_err()
                                            {
                                                return;
                                            }
                                            continue;
                                        }
                                        if send_view_state(
                                            &gui_event_tx,
                                            pane_id,
                                            &mut session.save_controller,
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                    FoldResult::NotFound => {
                                        if send_gui_message(
                                            &gui_event_tx,
                                            pane_id,
                                            MessageKind::Info,
                                            "Failed to resolve the entry on this line",
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                    FoldResult::CannotExpandIncomplete => {
                                        if send_gui_message(
                                            &gui_event_tx,
                                            pane_id,
                                            MessageKind::Info,
                                            "Cannot expand: directory could not be read (access denied or unavailable)",
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                    FoldResult::NeedsLoad { dir } => {
                                        if request_directory_load(
                                            pane_id,
                                            dir,
                                            line,
                                            session,
                                            &shared_ids,
                                            &event_tx,
                                            &mut loader_owner,
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                    FoldResult::NeedsLoadRecursive { dirs } => {
                                        if request_recursive_load(
                                            pane_id,
                                            dirs,
                                            line,
                                            op,
                                            session,
                                            &shared_ids,
                                            &event_tx,
                                            &gui_event_tx,
                                            &mut loader_owner,
                                            &mut dialog_owner,
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                    FoldResult::NoOp | FoldResult::Busy => {}
                                }
                            }
                            EditorEvent::ChangeSort { query } => {
                                let Some(query) = query else {
                                    let (key, reverse) =
                                        session.save_controller.sort_state();
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        sort_state_message(key, reverse),
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                };

                                let (key, reverse) = match parse_sort_query(&query) {
                                    Ok(sort) => sort,
                                    Err(error) => {
                                        if send_gui_message(
                                            &gui_event_tx,
                                            pane_id,
                                            MessageKind::Error,
                                            error,
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                        continue;
                                    }
                                };
                                if session.engine.snapshot().dirty {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        "Cannot change sorting while editing. Save or discard changes.",
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }
                                if !session.save_controller.is_idle() {
                                    continue;
                                }

                                let lines =
                                    match session.save_controller.change_sort(key, reverse) {
                                        Ok(lines) => lines,
                                        Err(error) => {
                                            if send_gui_message(
                                                &gui_event_tx,
                                                pane_id,
                                                MessageKind::Error,
                                                format!(
                                                    "Failed to change sorting: {error:#}"
                                                ),
                                            )
                                            .is_err()
                                            {
                                                return;
                                            }
                                            if session.save_controller.is_offline()
                                                && send_view_state(
                                                    &gui_event_tx,
                                                    pane_id,
                                                    &mut session.save_controller,
                                                )
                                                .is_err()
                                            {
                                                return;
                                            }
                                            continue;
                                        }
                                    };
                                if let Err(error) = session.engine.send(EditorCommand::SetLines {
                                    lines,
                                    cursor_line: None,
                                }) {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Error,
                                        format!("Failed to update sorted view: {error:#}"),
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }
                                if send_view_state(
                                    &gui_event_tx,
                                    pane_id,
                                    &mut session.save_controller,
                                )
                                .is_err()
                                    || send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Info,
                                        sort_state_message(key, reverse),
                                    )
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            EditorEvent::OpenWith { line } => {
                                if !session.save_controller.is_idle()
                                    || pending_open_with.is_some()
                                    || dialog_owner.is_some()
                                    || apply_owner.is_some()
                                    || transfer.is_awaiting()
                                    || transfer.is_running()
                                {
                                    continue;
                                }
                                match handle_open_with(
                                    pane_id,
                                    &session.save_controller,
                                    session.engine.as_ref(),
                                    &session.root,
                                    line,
                                    &gui_event_tx,
                                ) {
                                    Ok(Some(pending)) => pending_open_with = Some(pending),
                                    Ok(None) => {}
                                    Err(_) => return,
                                }
                            }
                            event => {
                                if gui_event_tx
                                    .send(GuiEvent::Editor { pane_id, event })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                    AppEvent::PickerSelect {
                        pane_id,
                        path,
                        action,
                    } => {
                        if active_picker.as_ref().is_some_and(|picker| picker.pane_id == pane_id) {
                            active_picker = None;
                            picker_search.invalidate_pending();
                        }
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
                        if action == PickerAction::Jump
                            && session.save_controller.baseline().get_by_path(&path).is_none()
                            && let Some(dir) = super::next_picker_reveal_directory(
                                session.save_controller.baseline(),
                                &path,
                            )
                        {
                            if let Some(message) = picker_reveal_start_rejection(
                                session.engine.snapshot().dirty,
                                loader_owner.is_some()
                                    || dialog_owner.is_some()
                                    || apply_owner.is_some()
                                    || transfer.is_awaiting()
                                    || transfer.is_running(),
                                session.crashed,
                                session.save_controller.is_idle(),
                            ) {
                                if send_gui_message(
                                    &gui_event_tx,
                                    pane_id,
                                    MessageKind::Info,
                                    message,
                                )
                                .is_err()
                                {
                                    return;
                                }
                                continue;
                            }
                            request_picker_reveal_load(
                                pane_id,
                                path,
                                action,
                                dir,
                                session,
                                &shared_ids,
                                &event_tx,
                                &mut loader_owner,
                            );
                            continue;
                        }
                        let engine = Arc::clone(&session.engine);
                        let root = session.root.clone();
                        if handle_picker_select(
                            pane_id,
                            path,
                            action,
                            &mut session.save_controller,
                            engine.as_ref(),
                            &root,
                            &gui_event_tx,
                        )
                        .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::PickerQuery { pane_id, query } => {
                        let Some(picker) = active_picker.as_mut() else {
                            continue;
                        };
                        if picker.pane_id != pane_id {
                            continue;
                        }
                        picker.query = query.clone();
                        if let Some(catalog) = catalogs.get(&picker.root) {
                            picker_search.request(
                                pane_id,
                                query,
                                picker.include_hidden,
                                catalog,
                            );
                        }
                    }
                    AppEvent::PickerClosed(pane_id) => {
                        if active_picker.as_ref().is_some_and(|picker| picker.pane_id == pane_id) {
                            active_picker = None;
                            picker_search.invalidate_pending();
                        }
                    }
                    AppEvent::CatalogChanged { root, error } => {
                        if let Some(error) = error
                            && send_gui_message(
                                &gui_event_tx,
                                active,
                                MessageKind::Warn,
                                error,
                            )
                            .is_err()
                        {
                            return;
                        }
                        let Some(picker) = active_picker.as_ref() else {
                            continue;
                        };
                        if picker.root != root {
                            continue;
                        }
                        if let Some(catalog) = catalogs.get(&root) {
                            picker_search.request(
                                picker.pane_id,
                                picker.query.clone(),
                                picker.include_hidden,
                                catalog,
                            );
                        }
                    }
                    AppEvent::Confirm(choice) => {
                        if let ConfirmChoice::OpenWithSelected(index) = choice {
                            let Some((path, handlers)) = pending_open_with.take() else {
                                continue;
                            };
                            let result = if index < handlers.len() {
                                fyler_fsops::openwith::open_with_handler(
                                    &path,
                                    &handlers[index].key,
                                )
                            } else if index == handlers.len() {
                                fyler_fsops::openwith::open_with_system_dialog(&path)
                            } else {
                                Err(anyhow::anyhow!(
                                    "Open-with selection is out of range: {index}"
                                ))
                            };
                            if let Err(error) = result
                                && send_gui_message(
                                    &gui_event_tx,
                                    active,
                                    MessageKind::Error,
                                    format!("Failed to open with selected application: {error:#}"),
                                )
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                        if pending_open_with.is_some() {
                            // open-withダイアログのキャンセル。保存フローへ流さない。
                            pending_open_with = None;
                            continue;
                        }
                        if !pending_recovery.is_empty() {
                            if choice == ConfirmChoice::Approve
                                && let Some(journal) = &journal
                            {
                                for entry in &pending_recovery {
                                    if let Err(error) = journal.discard(&entry.id) {
                                        eprintln!(
                                            "Failed to discard undo recovery candidate: {}: {error:#}",
                                            entry.dir.display()
                                        );
                                    }
                                }
                            }
                            pending_recovery.clear();
                            if gui_event_tx.send(GuiEvent::CloseDialog).is_err() {
                                return;
                            }
                            continue;
                        }
                        if transfer.is_awaiting() || transfer.is_running() {
                            match transfer.on_choice(choice) {
                                TransferFlowResult::StartApply {
                                    source,
                                    target,
                                    plan,
                                    overwrites,
                                    cancel,
                                } => {
                                    discard_all_undo_slots(&mut panes, journal.as_ref());
                                    for pane_id in [source, target] {
                                        if let Some(session) = panes.get(&pane_id) {
                                            let _ = session
                                                .engine
                                                .send(EditorCommand::SetModifiable(false));
                                        }
                                    }
                                    if gui_event_tx
                                        .send(GuiEvent::ShowApplyProgress {
                                            total: plan.ops.len(),
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
                                    let worker_plan = plan.clone();
                                    let worker_event_tx = event_tx.clone();
                                    // copy/moveは再帰深度が読めないため既定stackを維持する。
                                    let spawn_result = thread::Builder::new()
                                        .name("fyler-transfer".to_owned())
                                        .spawn(move || {
                                            let report = fyler_fsops::apply::apply_transfer_plan_cancellable(
                                                &worker_plan,
                                                &overwrites,
                                                &cancel,
                                                &mut |progress| {
                                                    let _ = worker_event_tx.send(
                                                        AppEvent::TransferProgress(progress),
                                                    );
                                                },
                                            );
                                            let _ = worker_event_tx
                                                .send(AppEvent::TransferFinished(report));
                                        });
                                    if let Err(error) = spawn_result {
                                        let error =
                                            format!("Failed to start transfer worker: {error}");
                                        let report = CommitReport {
                                            results: plan
                                                .ops
                                                .into_iter()
                                                .map(|op| OpResult {
                                                    op,
                                                    outcome: OpOutcome::Failed {
                                                        error: error.clone(),
                                                        progress: None,
                                                    },
                                                })
                                                .collect(),
                                        };
                                        if event_tx
                                            .send(AppEvent::TransferFinished(report))
                                            .is_err()
                                        {
                                            return;
                                        }
                                    }
                                }
                                TransferFlowResult::Cancelled => {
                                    if gui_event_tx.send(GuiEvent::CloseDialog).is_err() {
                                        return;
                                    }
                                }
                                TransferFlowResult::CancelRequested => {
                                    if gui_event_tx
                                        .send(GuiEvent::ApplyCancelRequested)
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                TransferFlowResult::Finished { .. }
                                | TransferFlowResult::Ignored => {}
                            }
                            continue;
                        }
                        let Some(pane_id) = dialog_owner.or(apply_owner) else {
                            continue;
                        };
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
                        let result = session.save_controller.on_choice(choice);
                        match result {
                            SaveFlowResult::StartApply {
                                plan,
                                overwrites,
                                cancel,
                            } => {
                                dialog_owner = None;
                                apply_owner = Some(pane_id);
                                let mut recorder = None;
                                let mut recorder_id = None;
                                if let Some(journal) = &journal {
                                    let id = undo_journal::new_transaction_id();
                                    match journal.begin(&id, &session.root) {
                                        Ok(dir) => {
                                            recorder = Some(fyler_fsops::UndoRecorder::new(
                                                id.clone(),
                                                session.root.clone(),
                                                dir,
                                            ));
                                            recorder_id = Some(id);
                                        }
                                        Err(error) => {
                                            if send_gui_message(
                                                &gui_event_tx,
                                                pane_id,
                                                MessageKind::Warn,
                                                format!(
                                                    "Failed to start undo journal. This apply cannot be undone: {error:#}"
                                                ),
                                            )
                                            .is_err()
                                            {
                                                return;
                                            }
                                        }
                                    }
                                }
                                if gui_event_tx
                                    .send(GuiEvent::ShowApplyProgress {
                                        total: plan.ops.len(),
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                                let worker_root = session.root.clone();
                                let worker_plan = plan.clone();
                                let worker_event_tx = event_tx.clone();
                                // scan/applyは再帰深度が読めないため既定stackを維持する。
                                let spawn_result = thread::Builder::new()
                                    .name("fyler-apply".to_owned())
                                    .spawn(move || {
                                        let mut recorder = recorder;
                                        let report =
                                            fyler_fsops::apply::apply_plan_cancellable(
                                                &worker_root,
                                                &worker_plan,
                                                &overwrites,
                                                &cancel,
                                                &mut |progress| {
                                                    let _ = worker_event_tx.send(
                                                        AppEvent::ApplyProgress(pane_id, progress),
                                                    );
                                                },
                                                recorder.as_mut(),
                                            );
                                        let transaction =
                                            recorder.map(fyler_fsops::UndoRecorder::into_transaction);
                                        let _ = worker_event_tx.send(AppEvent::ApplyFinished(
                                            pane_id,
                                            report,
                                            transaction,
                                        ));
                                    });
                                if let Err(error) = spawn_result {
                                    if let (Some(journal), Some(id)) = (&journal, recorder_id)
                                        && let Err(error) = journal.discard(&id)
                                    {
                                        eprintln!(
                                            "Failed to discard undo journal for an apply that failed to start: {error:#}"
                                        );
                                    }
                                    let error = format!("Failed to start apply worker: {error}");
                                    let report = CommitReport {
                                        results: plan
                                            .ops
                                            .into_iter()
                                            .map(|op| OpResult {
                                                op,
                                                outcome: OpOutcome::Failed {
                                                    error: error.clone(),
                                                    progress: None,
                                                },
                                            })
                                            .collect(),
                                    };
                                    if event_tx
                                        .send(AppEvent::ApplyFinished(pane_id, report, None))
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            SaveFlowResult::StartUndo {
                                transaction,
                                cancel,
                            } => {
                                dialog_owner = None;
                                apply_owner = Some(pane_id);
                                if let Some(journal) = &journal
                                    && let Err(error) = journal.mark_undoing(&transaction.id)
                                {
                                    eprintln!(
                                        "Failed to update undo journal to Undoing: {error:#}"
                                    );
                                }
                                if gui_event_tx
                                    .send(GuiEvent::ShowApplyProgress {
                                        total: transaction.steps.len(),
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                                let worker_transaction = transaction.clone();
                                let worker_event_tx = event_tx.clone();
                                // undoの復元処理は再帰深度が読めないため既定stackを維持する。
                                let spawn_result = thread::Builder::new()
                                    .name("fyler-undo".to_owned())
                                    .spawn(move || {
                                        let report = fyler_fsops::apply_undo_cancellable(
                                            &worker_transaction,
                                            &cancel,
                                            &mut |progress| {
                                                let _ = worker_event_tx.send(
                                                    AppEvent::UndoProgress(pane_id, progress),
                                                );
                                            },
                                        );
                                        let _ = worker_event_tx
                                            .send(AppEvent::UndoFinished(pane_id, report));
                                    });
                                if let Err(error) = spawn_result {
                                    let error = format!("Failed to start undo worker: {error}");
                                    let report = CommitReport {
                                        results: transaction
                                            .steps
                                            .into_iter()
                                            .map(|op| OpResult {
                                                op,
                                                outcome: OpOutcome::Failed {
                                                    error: error.clone(),
                                                    progress: None,
                                                },
                                            })
                                            .collect(),
                                    };
                                    if event_tx
                                        .send(AppEvent::UndoFinished(pane_id, report))
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            SaveFlowResult::ApplyCancelRequested => {
                                if gui_event_tx
                                    .send(GuiEvent::ApplyCancelRequested)
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            SaveFlowResult::UndoCancelled { transaction } => {
                                session.undo_slot = Some(transaction.clone());
                                dialog_owner = None;
                                if send_save_result(
                                    &gui_event_tx,
                                    pane_id,
                                    SaveFlowResult::UndoCancelled { transaction },
                                )
                                .is_err()
                                {
                                    return;
                                }
                            }
                            SaveFlowResult::UndoInvalidated {
                                transaction,
                                message,
                            } => {
                                session.undo_slot = Some(transaction.clone());
                                dialog_owner = None;
                                if send_save_result(
                                    &gui_event_tx,
                                    pane_id,
                                    SaveFlowResult::UndoInvalidated {
                                        transaction,
                                        message,
                                    },
                                )
                                .is_err()
                                {
                                    return;
                                }
                            }
                            result => {
                                if !matches!(result, SaveFlowResult::Ignored) {
                                    dialog_owner = None;
                                }
                                if send_save_result(&gui_event_tx, pane_id, result).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    AppEvent::LoaderCancel => {
                        let Some(owner) = &loader_owner else {
                            continue;
                        };
                        if !owner.cancel.swap(true, Ordering::Relaxed)
                            && owner.shows_dialog()
                            && gui_event_tx
                                .send(GuiEvent::LoaderCancelRequested)
                                .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::LoaderProgress(pane_id, entries) => {
                        if loader_owner.as_ref().is_some_and(|owner| {
                            owner.pane_id == pane_id && owner.shows_dialog()
                        }) && gui_event_tx
                            .send(GuiEvent::LoaderProgress { entries })
                            .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::LoaderFinished {
                        pane_id,
                        root,
                        result,
                    } => {
                        let Some(owner) = loader_owner.take() else {
                            continue;
                        };
                        if owner.pane_id != pane_id || owner.root != root {
                            loader_owner = Some(owner);
                            continue;
                        }
                        let showed_dialog = owner.shows_dialog();
                        let loads_directory = owner.loads_directory();
                        let picker_reveal = matches!(owner.kind, LoaderKind::PickerReveal { .. });
                        // Root loadはswap前で session.root が旧rootのままなので、
                        // 新rootとの不一致を root_changed 扱いすると必ず破棄されてしまう。
                        // loader_owner排他により競合root変更は起き得ないため除外する。
                        let is_root_load = matches!(owner.kind, LoaderKind::Root { .. });
                        let load_target = match &owner.kind {
                            LoaderKind::Root { .. } => root.clone(),
                            LoaderKind::Directory { dir, .. } => dir.to_fs_path(&root),
                            LoaderKind::Recursive { dirs, .. } => dirs
                                .first()
                                .map(|dir| dir.to_fs_path(&root))
                                .unwrap_or_else(|| root.clone()),
                            LoaderKind::PickerReveal { dir, .. } => dir.to_fs_path(&root),
                        };
                        let pane_stale = panes.get(&pane_id).is_none_or(|session| {
                            loader_completion_is_stale(
                                false,
                                session.crashed,
                                loader_root_changed(is_root_load, &session.root, &root),
                                session.engine.snapshot().dirty,
                                session.save_controller.is_idle(),
                            )
                        });
                        let other_busy = apply_owner.is_some()
                            || transfer.is_awaiting()
                            || transfer.is_running()
                            || if showed_dialog {
                                dialog_owner != Some(pane_id)
                            } else {
                                dialog_owner.is_some()
                            };

                        if !pane_stale && !other_busy {
                            match classify_loader_result(result) {
                                LoaderResult::Ready(baseline) => {
                                    let session = panes.get_mut(&pane_id).expect("checked pane");
                                    let finish = match owner.kind {
                                        LoaderKind::Root { cursor_target } => {
                                            finish_session_root_change(
                                                pane_id,
                                                root,
                                                cursor_target.as_deref(),
                                                baseline,
                                                session,
                                                &gui_event_tx,
                                                &mut git,
                                                &mut catalogs,
                                            )
                                        }
                                        LoaderKind::PickerReveal {
                                            target, action, ..
                                        } => {
                                            let next = match finish_picker_reveal_load(
                                                pane_id,
                                                &target,
                                                action,
                                                baseline,
                                                session,
                                                &gui_event_tx,
                                            ) {
                                                Ok(next) => next,
                                                Err(_) => return,
                                            };
                                            if let Some(dir) = next {
                                                request_picker_reveal_load(
                                                    pane_id,
                                                    target,
                                                    action,
                                                    dir,
                                                    session,
                                                    &shared_ids,
                                                    &event_tx,
                                                    &mut loader_owner,
                                                );
                                            }
                                            Ok(())
                                        }
                                        kind => finish_directory_load(
                                            pane_id,
                                            kind,
                                            baseline,
                                            session,
                                            &gui_event_tx,
                                        ),
                                    };
                                    if finish.is_err() {
                                        return;
                                    }
                                }
                                LoaderResult::Cancelled => {
                                    if showed_dialog
                                        && send_gui_message(
                                            &gui_event_tx,
                                            pane_id,
                                            MessageKind::Info,
                                            "Loading cancelled",
                                        )
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                LoaderResult::Failed(error) => {
                                    if picker_reveal {
                                        if send_gui_message(
                                            &gui_event_tx,
                                            pane_id,
                                            MessageKind::Warn,
                                            "Search candidate was not found. It may have changed externally.",
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                        eprintln!(
                                            "Failed to load picker reveal directory {}: {error:#}",
                                            load_target.display()
                                        );
                                    } else {
                                        let kind = if loads_directory {
                                            MessageKind::Warn
                                        } else {
                                            MessageKind::Error
                                        };
                                        if send_gui_message(
                                            &gui_event_tx,
                                            pane_id,
                                            kind,
                                            format!(
                                                "Failed to load {}: {error:#}",
                                                load_target.display()
                                            ),
                                        )
                                        .is_err()
                                        {
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                        if showed_dialog && dialog_owner == Some(pane_id) {
                            dialog_owner = None;
                            if gui_event_tx.send(GuiEvent::CloseDialog).is_err() {
                                return;
                            }
                        }
                        if loads_directory
                            && loader_owner.is_none()
                            && let Some(session) = panes.get_mut(&pane_id)
                            && !session.deferred_changes.is_empty()
                            && apply_owner.is_none()
                            && !transfer.is_running()
                        {
                            let changed_paths = std::mem::take(&mut session.deferred_changes);
                            if handle_external_change(
                                pane_id,
                                &changed_paths,
                                &mut session.save_controller,
                                &gui_event_tx,
                                &mut git,
                                &session.root,
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                    }
                    AppEvent::ApplyProgress(pane_id, progress) => {
                        if apply_owner == Some(pane_id)
                            && gui_event_tx
                                .send(GuiEvent::ApplyProgress(progress))
                                .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::FeedbackSubmit { kind, body } => {
                        let Some(endpoint) = feedback_endpoint.clone() else {
                            continue;
                        };
                        if !feedback_open || validate_body(&body).is_err() {
                            if feedback_open
                                && gui_event_tx
                                    .send(GuiEvent::FeedbackResult {
                                        outcome: FeedbackResultKind::Invalid,
                                        message: FeedbackOutcome::Invalid.message(),
                                    })
                                    .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                        let payload = FeedbackPayload {
                            kind,
                            body,
                            app_version: env!("CARGO_PKG_VERSION").to_owned(),
                            os: std::env::consts::OS.to_owned(),
                            arch: std::env::consts::ARCH.to_owned(),
                        };
                        let json = payload.to_json();
                        let feedback_event_tx = event_tx.clone();
                        if thread::Builder::new()
                            .name("fyler-feedback".to_owned())
                            .spawn(move || {
                                let outcome = send_feedback(
                                    &endpoint,
                                    &json,
                                    std::time::Duration::from_secs(10),
                                );
                                let _ =
                                    feedback_event_tx.send(AppEvent::FeedbackFinished(outcome));
                            })
                            .is_err()
                            && event_tx
                                .send(AppEvent::FeedbackFinished(FeedbackOutcome::Network))
                                .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::FeedbackClosed => {
                        feedback_open = false;
                    }
                    AppEvent::FeedbackFinished(outcome) => {
                        if feedback_open
                            && gui_event_tx
                                .send(GuiEvent::FeedbackResult {
                                    outcome: feedback_result_kind(outcome),
                                    message: outcome.message(),
                                })
                                .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::ApplyFinished(pane_id, report, transaction) => {
                        if apply_owner != Some(pane_id) {
                            continue;
                        }
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
                        let any_success = report
                            .results
                            .iter()
                            .any(|result| matches!(result.outcome, OpOutcome::Success));
                        if any_success {
                            discard_undo_slot(session, journal.as_ref());
                            if let Some(transaction) = transaction {
                                if transaction.steps.is_empty() {
                                    if let Some(journal) = &journal
                                        && let Err(error) = journal.discard(&transaction.id)
                                    {
                                        eprintln!(
                                            "Failed to discard empty undo transaction: {error:#}"
                                        );
                                    }
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Warn,
                                        "Undo record for this apply is empty. It cannot be undone.",
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                } else {
                                    if let Some(journal) = &journal
                                        && let Err(error) = journal.commit(&transaction)
                                    {
                                        eprintln!(
                                            "Failed to update undo journal to Committed: {error:#}"
                                        );
                                    }
                                    session.undo_slot = Some(transaction);
                                }
                            } else if send_gui_message(
                                &gui_event_tx,
                                pane_id,
                                MessageKind::Warn,
                                "This apply has no undo record and cannot be undone",
                            )
                            .is_err()
                            {
                                return;
                            }
                        } else if let Some(transaction) = transaction
                            && let Some(journal) = &journal
                            && let Err(error) = journal.discard(&transaction.id)
                        {
                            eprintln!("Failed to discard undo journal for failed apply: {error:#}");
                        }
                        let result = session.save_controller.on_apply_finished(report);
                        if send_save_result(&gui_event_tx, pane_id, result).is_err()
                            || send_view_state(
                                &gui_event_tx,
                                pane_id,
                                &mut session.save_controller,
                            )
                            .is_err()
                        {
                            return;
                        }
                        git.request(pane_id, session.root.clone());
                        apply_owner = None;

                        let pane_ids = panes.keys().copied().collect::<Vec<_>>();
                        for deferred_id in pane_ids {
                            let Some(deferred) = panes.get_mut(&deferred_id) else {
                                continue;
                            };
                            if deferred.deferred_changes.is_empty() {
                                continue;
                            }
                            let changed_paths = std::mem::take(&mut deferred.deferred_changes);
                            if handle_external_change(
                                deferred_id,
                                &changed_paths,
                                &mut deferred.save_controller,
                                &gui_event_tx,
                                &mut git,
                                &deferred.root,
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                    }
                    AppEvent::UndoProgress(pane_id, progress) => {
                        if apply_owner == Some(pane_id) {
                            let current = progress
                                .current
                                .as_ref()
                                .map(undo_format::undo_step_label);
                            if gui_event_tx
                                .send(GuiEvent::UndoProgress(ApplyProgress {
                                    completed: progress.completed,
                                    total: progress.total,
                                    current,
                                }))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    AppEvent::UndoFinished(pane_id, report) => {
                        if apply_owner != Some(pane_id) {
                            continue;
                        }
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
                        let transaction_id = session
                            .save_controller
                            .applying_undo_transaction_id()
                            .map(str::to_owned);
                        let result = session.save_controller.on_undo_finished(report);
                        if send_save_result(&gui_event_tx, pane_id, result).is_err()
                            || send_view_state(
                                &gui_event_tx,
                                pane_id,
                                &mut session.save_controller,
                            )
                            .is_err()
                        {
                            return;
                        }
                        git.request(pane_id, session.root.clone());
                        if let (Some(journal), Some(transaction_id)) = (&journal, transaction_id)
                            && let Err(error) = journal.finish_undone(&transaction_id)
                        {
                            eprintln!("Failed to update undo journal to Undone: {error:#}");
                        }
                        apply_owner = None;

                        let pane_ids = panes.keys().copied().collect::<Vec<_>>();
                        for deferred_id in pane_ids {
                            let Some(deferred) = panes.get_mut(&deferred_id) else {
                                continue;
                            };
                            if deferred.deferred_changes.is_empty() {
                                continue;
                            }
                            let changed_paths = std::mem::take(&mut deferred.deferred_changes);
                            if handle_external_change(
                                deferred_id,
                                &changed_paths,
                                &mut deferred.save_controller,
                                &gui_event_tx,
                                &mut git,
                                &deferred.root,
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                    }
                    AppEvent::TransferProgress(progress) => {
                        if transfer.is_running()
                            && gui_event_tx
                                .send(GuiEvent::TransferProgress(progress))
                                .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::TransferFinished(report) => {
                        let TransferFlowResult::Finished {
                            source,
                            target,
                            report,
                        } = transfer.on_finished(report)
                        else {
                            continue;
                        };
                        let mut reconcile_errors = Vec::new();
                        for pane_id in [source, target] {
                            let Some(session) = panes.get_mut(&pane_id) else {
                                continue;
                            };
                            if let Err(error) = session.save_controller.reconcile_after_transfer() {
                                reconcile_errors.push(format!("pane {pane_id}: {error:#}"));
                            }
                            if !session.save_controller.is_offline() {
                                let _ = session.engine.send(EditorCommand::SetModifiable(true));
                            }
                            if send_view_state(
                                &gui_event_tx,
                                pane_id,
                                &mut session.save_controller,
                            )
                            .is_err()
                            {
                                return;
                            }
                            git.request(pane_id, session.root.clone());
                        }
                        if gui_event_tx
                            .send(GuiEvent::ShowTransferReport(report))
                            .is_err()
                        {
                            return;
                        }
                        if !reconcile_errors.is_empty()
                            && send_gui_message(
                                &gui_event_tx,
                                source,
                                MessageKind::Error,
                                format!(
                                    "A folder became offline or unreachable after transfer and will retry automatically: {}",
                                    reconcile_errors.join(" / ")
                                ),
                            )
                            .is_err()
                        {
                            return;
                        }
                        let pane_ids = panes.keys().copied().collect::<Vec<_>>();
                        for deferred_id in pane_ids {
                            let Some(deferred) = panes.get_mut(&deferred_id) else {
                                continue;
                            };
                            if deferred.deferred_changes.is_empty() {
                                continue;
                            }
                            let changed_paths = std::mem::take(&mut deferred.deferred_changes);
                            if handle_external_change(
                                deferred_id,
                                &changed_paths,
                                &mut deferred.save_controller,
                                &gui_event_tx,
                                &mut git,
                                &deferred.root,
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                    }
                    AppEvent::ExternalChange(pane_id, change) => {
                        let mut by_pane = BTreeMap::<PaneId, BTreeSet<PathBuf>>::new();
                        by_pane.entry(pane_id).or_default().extend(change.paths);
                        while let Ok(queued_event) = app_event_rx.try_recv() {
                            event_loop_gauge.dequeue();
                            match queued_event {
                                AppEvent::ExternalChange(queued_id, change) => {
                                    by_pane.entry(queued_id).or_default().extend(change.paths);
                                }
                                event => pending_events.push_back(event),
                            }
                        }
                        let mut catalog_changes = HashMap::<PathBuf, BTreeSet<PathBuf>>::new();
                        for (changed_id, paths) in &by_pane {
                            if let Some(session) = panes.get(changed_id) {
                                catalog_changes
                                    .entry(session.root.clone())
                                    .or_default()
                                    .extend(paths.iter().cloned());
                            }
                        }
                        for (root, paths) in catalog_changes {
                            if catalogs.update(&root, &paths)
                                && let Some(picker) = active_picker.as_ref()
                                && picker.root == root
                                && let Some(catalog) = catalogs.get(&root)
                            {
                                picker_search.request(
                                    picker.pane_id,
                                    picker.query.clone(),
                                    picker.include_hidden,
                                    catalog,
                                );
                            }
                        }
                        for (changed_id, changed_paths) in by_pane {
                            let Some(session) = panes.get_mut(&changed_id) else {
                                continue;
                            };
                            if should_defer_external_change(
                                apply_owner.is_some(),
                                transfer.is_running(),
                                loader_owner.as_ref().is_some_and(|owner| {
                                    owner.pane_id == changed_id && owner.loads_directory()
                                }),
                            )
                            {
                                session.deferred_changes.extend(changed_paths);
                            } else {
                                let transfer_invalidated =
                                    transfer.invalidate_if_involves(changed_id);
                                if transfer_invalidated
                                    && gui_event_tx.send(GuiEvent::CloseDialog).is_err()
                                {
                                    return;
                                }
                                if transfer_invalidated
                                    && send_gui_message(
                                        &gui_event_tx,
                                        changed_id,
                                        MessageKind::Warn,
                                        "Transfer was cancelled because files changed externally. Review the changes and try again.",
                                    )
                                    .is_err()
                                {
                                    return;
                                }
                                let outcome = match handle_external_change(
                                    changed_id,
                                    &changed_paths,
                                    &mut session.save_controller,
                                    &gui_event_tx,
                                    &mut git,
                                    &session.root,
                                ) {
                                    Ok(outcome) => outcome,
                                    Err(_) => return,
                                };
                                if let Some(transaction) = outcome.undo_transaction {
                                    session.undo_slot = Some(transaction);
                                }
                                if outcome.invalidated_dialog && dialog_owner == Some(changed_id) {
                                    dialog_owner = None;
                                }
                            }
                        }
                    }
                    AppEvent::WatchDegraded(pane_id, error) => {
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
                        let root = session.root.clone();
                        catalogs.invalidate(&root);
                        if let Some(picker) = active_picker.as_ref()
                            && picker.root == root
                        {
                            let catalog = catalogs.ensure(&root);
                            picker_search.request(
                                picker.pane_id,
                                picker.query.clone(),
                                picker.include_hidden,
                                catalog,
                            );
                        }
                        if !session.watch_degraded {
                            session.watch_degraded = true;
                            if send_gui_message(
                                &gui_event_tx,
                                pane_id,
                                MessageKind::Warn,
                                format!(
                                    "File watching stopped and will retry automatically: {error}"
                                ),
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                    }
                    AppEvent::OfflineRetryTick => {
                        if apply_owner.is_some() || transfer.is_running() {
                            continue;
                        }
                        let pane_ids = panes.keys().copied().collect::<Vec<_>>();
                        for pane_id in pane_ids {
                            let Some(session) = panes.get_mut(&pane_id) else {
                                continue;
                            };
                            if session.save_controller.is_offline() {
                                let result = session.save_controller.retry_offline();
                                if !matches!(result, SaveFlowResult::Reconnected(_)) {
                                    continue;
                                }
                                match fyler_fsops::watch::watch(
                                    &session.root,
                                    session.watch_tx.clone(),
                                ) {
                                    Ok(watcher) => {
                                        session.watcher = watcher;
                                        session.watch_degraded = false;
                                    }
                                    Err(_) => session.watch_degraded = true,
                                }
                                if send_save_result(&gui_event_tx, pane_id, result).is_err()
                                    || send_view_state(
                                        &gui_event_tx,
                                        pane_id,
                                        &mut session.save_controller,
                                    )
                                    .is_err()
                                {
                                    return;
                                }
                                git.request(pane_id, session.root.clone());
                            } else if session.watch_degraded {
                                let Ok(watcher) = fyler_fsops::watch::watch(
                                    &session.root,
                                    session.watch_tx.clone(),
                                ) else {
                                    continue;
                                };
                                session.watcher = watcher;
                                session.watch_degraded = false;
                                let outcome = handle_external_change(
                                    pane_id,
                                    &BTreeSet::new(),
                                    &mut session.save_controller,
                                    &gui_event_tx,
                                    &mut git,
                                    &session.root,
                                );
                                if outcome.is_err() {
                                    return;
                                }
                            } else if let Some(paths) = incomplete_probe_paths(
                                &session.save_controller,
                                dialog_owner.is_some(),
                                session.crashed,
                            ) {
                                // 恒久的に読めないdirだけを差分rescanへ流す。未回復なら
                                // carryされて無音、回復した範囲だけmarkerと表示を更新する。
                                let outcome = handle_external_change(
                                    pane_id,
                                    &paths,
                                    &mut session.save_controller,
                                    &gui_event_tx,
                                    &mut git,
                                    &session.root,
                                );
                                if outcome.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    AppEvent::GitStatus {
                        pane_id,
                        root,
                        statuses,
                    } => {
                        if let Some(next_root) = git.on_finished(pane_id) {
                            git.request(pane_id, next_root);
                        }
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
                        if session.root != root {
                            continue;
                        }
                        session.git_badges = session.save_controller.map_git_badges(&statuses);
                        if gui_event_tx
                            .send(GuiEvent::GitBadges {
                                pane_id,
                                badges: session.git_badges.clone(),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::Shutdown {
                        save_session,
                        window,
                    } => {
                        let result = if save_session {
                            session::save(&capture_session(&panes, layout, active, window))
                        } else {
                            Ok(())
                        };
                        let _ = shutdown_result_tx.send(result);
                        return;
                    }
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("Failed to start GUI event forwarding: {error}"))?;

    let gui_dequeue_gauge = Arc::clone(&gui_event_gauge);
    let gui_result = fyler_gui::app::run(
        gui_event_rx,
        action_tx,
        gui_options,
        Arc::new(move || gui_dequeue_gauge.dequeue()),
        initial_window,
        Arc::clone(&window_geometry),
    );
    let final_window = window_geometry.lock().ok().and_then(|window| *window);
    let _ = app_event_tx.send(AppEvent::Shutdown {
        save_session: gui_result.is_ok(),
        window: final_window,
    });
    let _ = event_bridge.join();
    let _ = action_bridge.join();
    if std::env::var_os("FYLER_QUEUE_STATS").as_deref() == Some(OsStr::new("1")) {
        eprintln!(
            "fyler queue high-water: app_event={} gui_event={}",
            app_event_gauge.high_water(),
            gui_event_gauge.high_water()
        );
    }
    gui_result?;
    if let Ok(result) = shutdown_result_rx.try_recv() {
        result?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_pane_action(
    action: PaneAction,
    source: PaneId,
    runtime: &tokio::runtime::Runtime,
    nvim_exe: &Path,
    bindings: &[KeyBinding],
    scan_options: ScanOptions,
    shared_ids: &Arc<Mutex<IdAllocator>>,
    app_event_tx: &CountingSender<AppEvent>,
    gui_event_tx: &CountingSender<GuiEvent>,
    panes: &mut BTreeMap<PaneId, PaneSession>,
    layout: &mut PaneLayout,
    active: &mut PaneId,
    last_active: &mut PaneId,
    next_pane_id: &mut u64,
    git: &mut GitRefresher,
    catalogs: &mut CatalogService,
    journal: Option<&undo_journal::UndoJournal>,
    dialog_owner: Option<PaneId>,
    workspace_applying: bool,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    if source != *active {
        return Ok(());
    }
    match action {
        PaneAction::SplitHorizontal | PaneAction::SplitVertical => {
            if workspace_applying {
                return send_gui_message(
                    gui_event_tx,
                    source,
                    MessageKind::Info,
                    "Cannot split a pane during apply or transfer",
                );
            }
            if panes.len() >= MAX_PANES {
                return send_gui_message(
                    gui_event_tx,
                    source,
                    MessageKind::Info,
                    "A maximum of four panes is allowed",
                );
            }
            let Some(root) = panes.get(&source).map(|pane| pane.root.clone()) else {
                return Ok(());
            };
            let new_id = PaneId::new(*next_pane_id);
            let new_session = match create_pane(
                runtime,
                new_id,
                root,
                nvim_exe,
                bindings,
                scan_options,
                Arc::clone(shared_ids),
                app_event_tx,
                None,
                None,
            ) {
                Ok(session) => session,
                Err(error) => {
                    return send_gui_message(
                        gui_event_tx,
                        source,
                        MessageKind::Error,
                        format!("Failed to add pane: {error:#}"),
                    );
                }
            };
            let direction = if action == PaneAction::SplitHorizontal {
                SplitDirection::Horizontal
            } else {
                SplitDirection::Vertical
            };
            let Some(new_layout) = layout.split(source, direction, new_id) else {
                return Ok(());
            };
            *next_pane_id += 1;
            gui_event_tx.send(GuiEvent::AddPane {
                pane_id: new_session.id,
                engine: Arc::clone(&new_session.engine),
                root: new_session.root.clone(),
            })?;
            let mut new_session = new_session;
            send_view_state(
                gui_event_tx,
                new_session.id,
                &mut new_session.save_controller,
            )?;
            git.request(new_session.id, new_session.root.clone());
            panes.insert(new_id, new_session);
            catalogs.register_pane(new_id, panes[&new_id].root.clone());
            *layout = new_layout;
            *last_active = *active;
            *active = new_id;
            gui_event_tx.send(GuiEvent::LayoutChanged {
                layout: layout.clone(),
                active: *active,
            })?;
        }
        PaneAction::Close => {
            let Some(session) = panes.get(&source) else {
                return Ok(());
            };
            let reason = close_rejection(
                session.engine.snapshot().dirty,
                session.save_controller.is_idle(),
                workspace_applying,
                panes.len() == 1,
                session.crashed,
            );
            if let Some(reason) = reason {
                return send_gui_message(gui_event_tx, source, MessageKind::Info, reason);
            }
            if dialog_owner == Some(source) {
                return send_gui_message(
                    gui_event_tx,
                    source,
                    MessageKind::Info,
                    "Close the confirmation dialog before closing the pane",
                );
            }
            let Some(target) = layout.sibling_leaf(source) else {
                return Ok(());
            };
            let Some(new_layout) = layout.close(source) else {
                return Ok(());
            };
            if let Some(session) = panes.get_mut(&source) {
                discard_undo_slot(session, journal);
            }
            panes.remove(&source);
            catalogs.remove_pane(source);
            git.remove(source);
            *layout = new_layout;
            *last_active = target;
            *active = target;
            gui_event_tx.send(GuiEvent::RemovePane(source))?;
            gui_event_tx.send(GuiEvent::LayoutChanged {
                layout: layout.clone(),
                active: *active,
            })?;
        }
        action => {
            let target = match action {
                PaneAction::FocusLeft => layout.focus_neighbor(source, FocusDirection::Left),
                PaneAction::FocusRight => layout.focus_neighbor(source, FocusDirection::Right),
                PaneAction::FocusUp => layout.focus_neighbor(source, FocusDirection::Up),
                PaneAction::FocusDown => layout.focus_neighbor(source, FocusDirection::Down),
                PaneAction::FocusNext => cycle_focus(layout, source, true),
                PaneAction::FocusPrevious => cycle_focus(layout, source, false),
                _ => None,
            };
            if let Some(target) = target {
                *last_active = *active;
                *active = target;
                gui_event_tx.send(GuiEvent::LayoutChanged {
                    layout: layout.clone(),
                    active: *active,
                })?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_transfer_request(
    source: PaneId,
    kind: TransferKind,
    selected_lines: &[usize],
    last_active: PaneId,
    panes: &BTreeMap<PaneId, PaneSession>,
    globally_busy: bool,
    transfer: &mut TransferController,
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    let Some(target) = resolve_target(source, last_active, panes.keys().copied()) else {
        return send_gui_message(
            gui_event_tx,
            source,
            MessageKind::Info,
            "There is no destination pane for transfer. Split a pane first.",
        );
    };
    let Some(source_session) = panes.get(&source) else {
        return Ok(());
    };
    let Some(target_session) = panes.get(&target) else {
        return Ok(());
    };
    let source_snapshot = source_session.engine.snapshot();
    let target_snapshot = target_session.engine.snapshot();
    let source_state = TransferPaneState {
        dirty: source_snapshot.dirty,
        idle: source_session.save_controller.is_idle(),
        crashed: source_session.crashed,
        offline: source_session.save_controller.is_offline(),
    };
    let target_state = TransferPaneState {
        dirty: target_snapshot.dirty,
        idle: target_session.save_controller.is_idle(),
        crashed: target_session.crashed,
        offline: target_session.save_controller.is_offline(),
    };
    if let Some(reason) = start_rejection(source_state, target_state, globally_busy) {
        return send_gui_message(gui_event_tx, source, MessageKind::Info, reason);
    }

    let selected = match resolve_selection(
        &source_session.save_controller,
        &source_snapshot.lines,
        selected_lines,
    ) {
        Ok(selected) => selected,
        Err(super::transfer_flow::SelectionError::UnsavedLine) => {
            return send_gui_message(
                gui_event_tx,
                source,
                MessageKind::Info,
                "Cannot transfer because the selection contains an unsaved new line. Save first.",
            );
        }
        Err(super::transfer_flow::SelectionError::Empty) => {
            return send_gui_message(
                gui_event_tx,
                source,
                MessageKind::Info,
                "No transfer target is selected",
            );
        }
        Err(super::transfer_flow::SelectionError::MissingLine) => {
            return send_gui_message(
                gui_event_tx,
                source,
                MessageKind::Error,
                "Transfer target line was not found",
            );
        }
        Err(super::transfer_flow::SelectionError::UnknownId) => {
            return send_gui_message(
                gui_event_tx,
                source,
                MessageKind::Error,
                "Failed to resolve transfer target in the current file list",
            );
        }
    };

    let target_empty = target_session.save_controller.visible_lines().is_empty();
    let resolved_target = target_session
        .save_controller
        .resolve_line(&target_snapshot.lines, target_snapshot.cursor.line);
    let Some(destination) = destination_directory(target_empty, resolved_target) else {
        return send_gui_message(
            gui_event_tx,
            source,
            MessageKind::Error,
            "Failed to resolve cursor position in destination pane",
        );
    };
    let plan = build_plan(
        kind,
        source_session.root.clone(),
        target_session.root.clone(),
        &destination,
        selected,
    );
    if plan.is_empty() {
        return send_gui_message(
            gui_event_tx,
            source,
            MessageKind::Info,
            "No transfer targets",
        );
    }
    let preflight = fyler_fsops::preflight_transfer(&plan);
    if !preflight.blocked.is_empty() {
        return send_gui_message(
            gui_event_tx,
            source,
            MessageKind::Error,
            format!(
                "Some paths cannot be transferred: {}",
                preflight
                    .blocked
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    transfer.begin(source, target, plan.clone(), preflight.overwritable.clone());
    gui_event_tx.send(GuiEvent::ShowTransferPlan {
        plan,
        target,
        overwrites: preflight.overwritable,
    })
}

fn cycle_focus(layout: &PaneLayout, active: PaneId, forward: bool) -> Option<PaneId> {
    let leaves = layout.leaves();
    if leaves.len() < 2 {
        return None;
    }
    let index = leaves.iter().position(|id| *id == active)?;
    let next = if forward {
        (index + 1) % leaves.len()
    } else {
        (index + leaves.len() - 1) % leaves.len()
    };
    Some(leaves[next])
}

fn close_rejection(
    dirty: bool,
    idle: bool,
    applying: bool,
    last_pane: bool,
    crashed: bool,
) -> Option<&'static str> {
    if last_pane {
        Some("The last pane cannot be closed")
    } else if applying {
        Some("A pane cannot be closed during apply")
    } else if !crashed && dirty {
        Some("A pane being edited cannot be closed")
    } else if !crashed && !idle {
        Some("A pane that is saving cannot be closed")
    } else {
        None
    }
}

fn undo_rejection(
    dirty: bool,
    slot_empty: bool,
    offline: bool,
    busy: bool,
) -> Option<&'static str> {
    if busy {
        Some("Another save is in progress")
    } else if offline {
        Some("Cannot undo while the folder is offline or unreachable")
    } else if dirty {
        Some("Cannot undo while editing. Save or discard changes.")
    } else if slot_empty {
        Some("No operations are available to undo")
    } else {
        None
    }
}

fn feedback_start_rejection(dialog_open: bool) -> Option<&'static str> {
    dialog_open.then_some("Close the other dialog before opening feedback")
}

fn incomplete_probe_paths(
    save_controller: &SaveController,
    dialog_open: bool,
    crashed: bool,
) -> Option<BTreeSet<PathBuf>> {
    if dialog_open || crashed {
        None
    } else {
        save_controller.incomplete_probe_paths()
    }
}

fn feedback_result_kind(outcome: FeedbackOutcome) -> FeedbackResultKind {
    match outcome {
        FeedbackOutcome::Accepted => FeedbackResultKind::Accepted,
        FeedbackOutcome::Invalid => FeedbackResultKind::Invalid,
        FeedbackOutcome::RateLimited => FeedbackResultKind::RateLimited,
        FeedbackOutcome::ServerError => FeedbackResultKind::ServerError,
        FeedbackOutcome::Network => FeedbackResultKind::Network,
        FeedbackOutcome::Timeout => FeedbackResultKind::Timeout,
    }
}

fn discard_all_undo_slots(
    panes: &mut BTreeMap<PaneId, PaneSession>,
    journal: Option<&undo_journal::UndoJournal>,
) {
    for session in panes.values_mut() {
        discard_undo_slot(session, journal);
    }
}

fn discard_undo_slot(session: &mut PaneSession, journal: Option<&undo_journal::UndoJournal>) {
    let Some(transaction) = session.undo_slot.take() else {
        return;
    };
    if let Some(journal) = journal
        && let Err(error) = journal.discard(&transaction.id)
    {
        eprintln!("Failed to discard undo journal: {error:#}");
    }
}

#[allow(clippy::too_many_arguments)]
fn request_session_root_change(
    pane_id: PaneId,
    new_root: PathBuf,
    cursor_target: Option<OsString>,
    session: &PaneSession,
    shared_ids: &Arc<Mutex<IdAllocator>>,
    app_event_tx: &CountingSender<AppEvent>,
    gui_event_tx: &CountingSender<GuiEvent>,
    loader_owner: &mut Option<LoaderOwner>,
    dialog_owner: &mut Option<PaneId>,
    globally_busy: bool,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    let new_root = match normalize_root(&new_root) {
        Ok(root) => root,
        Err(error) => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                format!("Failed to normalize root ({}): {error}", new_root.display()),
            )?;
            return Ok(());
        }
    };
    match loader_start_gate(
        session.engine.snapshot().dirty,
        session.save_controller.is_offline(),
        session.save_controller.is_idle(),
        globally_busy || dialog_owner.is_some(),
        loader_owner.is_some(),
    ) {
        LoaderStartGate::Allow => {}
        LoaderStartGate::RejectSilently => return Ok(()),
        LoaderStartGate::Reject(message) => {
            send_gui_message(gui_event_tx, pane_id, MessageKind::Info, message)?;
            return Ok(());
        }
    }

    let cancel = Arc::new(AtomicBool::new(false));
    gui_event_tx.send(GuiEvent::ShowLoaderProgress {
        title: "Loading folder".to_owned(),
        path: new_root.clone(),
    })?;
    *dialog_owner = Some(pane_id);
    *loader_owner = Some(LoaderOwner {
        pane_id,
        root: new_root.clone(),
        kind: LoaderKind::Root { cursor_target },
        cancel: Arc::clone(&cancel),
    });

    let scan_options = session.save_controller.scan_options();
    let worker_ids = Arc::clone(shared_ids);
    let worker_event_tx = app_event_tx.clone();
    let worker_root = new_root.clone();
    let spawn_result = thread::Builder::new()
        .name("fyler-loader".to_owned())
        // network share上の列挙は再帰し得るため既定stackを維持する。
        .spawn(move || {
            let progress_tx = worker_event_tx.clone();
            let result = fyler_fsops::scan::scan_baseline_shallow_cancellable_with(
                &worker_root,
                move |_| {
                    let mut ids = worker_ids
                        .lock()
                        .map_err(|_| anyhow::anyhow!("ID allocator lock is poisoned"))?;
                    Ok(ids.allocate())
                },
                &scan_options,
                move |entries| {
                    let _ = progress_tx.send(AppEvent::LoaderProgress(pane_id, entries));
                },
                &cancel,
            );
            let _ = worker_event_tx.send(AppEvent::LoaderFinished {
                pane_id,
                root: worker_root,
                result,
            });
        });
    if let Err(error) = spawn_result {
        let _ = app_event_tx.send(AppEvent::LoaderFinished {
            pane_id,
            root: new_root,
            result: Err(anyhow::anyhow!("Failed to start loader worker: {error}")),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn request_directory_load(
    pane_id: PaneId,
    dir: TreePath,
    line: usize,
    session: &PaneSession,
    shared_ids: &Arc<Mutex<IdAllocator>>,
    app_event_tx: &CountingSender<AppEvent>,
    loader_owner: &mut Option<LoaderOwner>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    if loader_owner.is_some() {
        return Ok(());
    }
    let root = session.root.clone();
    let previous = session.save_controller.baseline().clone();
    let options = session.save_controller.scan_options();
    let cancel = Arc::new(AtomicBool::new(false));
    *loader_owner = Some(LoaderOwner {
        pane_id,
        root: root.clone(),
        kind: LoaderKind::Directory {
            dir: dir.clone(),
            line,
        },
        cancel: Arc::clone(&cancel),
    });
    let worker_ids = Arc::clone(shared_ids);
    let worker_event_tx = app_event_tx.clone();
    let worker_root = root.clone();
    let spawn_result = thread::Builder::new()
        .name("fyler-loader".to_owned())
        .spawn(move || {
            let result = worker_ids
                .lock()
                .map_err(|_| anyhow::anyhow!("ID allocator lock is poisoned"))
                .and_then(|mut ids| {
                    fyler_fsops::scan::load_directory(
                        &worker_root,
                        &dir,
                        &mut ids,
                        &previous,
                        &options,
                    )
                    .map(Some)
                });
            let _ = worker_event_tx.send(AppEvent::LoaderFinished {
                pane_id,
                root: worker_root,
                result,
            });
        });
    if let Err(error) = spawn_result {
        let _ = app_event_tx.send(AppEvent::LoaderFinished {
            pane_id,
            root,
            result: Err(anyhow::anyhow!("Failed to start loader worker: {error}")),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn request_picker_reveal_load(
    pane_id: PaneId,
    target: TreePath,
    action: fyler_gui::app::PickerAction,
    dir: TreePath,
    session: &PaneSession,
    shared_ids: &Arc<Mutex<IdAllocator>>,
    app_event_tx: &CountingSender<AppEvent>,
    loader_owner: &mut Option<LoaderOwner>,
) {
    debug_assert!(loader_owner.is_none());
    let root = session.root.clone();
    let previous = session.save_controller.baseline().clone();
    let options = session.save_controller.scan_options();
    let cancel = Arc::new(AtomicBool::new(false));
    *loader_owner = Some(LoaderOwner {
        pane_id,
        root: root.clone(),
        kind: LoaderKind::PickerReveal {
            target,
            action,
            dir: dir.clone(),
        },
        cancel: Arc::clone(&cancel),
    });
    let worker_ids = Arc::clone(shared_ids);
    let worker_event_tx = app_event_tx.clone();
    let worker_root = root.clone();
    let spawn_result = thread::Builder::new()
        .name("fyler-loader".to_owned())
        .spawn(move || {
            let result = worker_ids
                .lock()
                .map_err(|_| anyhow::anyhow!("ID allocator lock is poisoned"))
                .and_then(|mut ids| {
                    fyler_fsops::scan::load_directory(
                        &worker_root,
                        &dir,
                        &mut ids,
                        &previous,
                        &options,
                    )
                    .map(Some)
                });
            let _ = worker_event_tx.send(AppEvent::LoaderFinished {
                pane_id,
                root: worker_root,
                result,
            });
        });
    if let Err(error) = spawn_result {
        let _ = app_event_tx.send(AppEvent::LoaderFinished {
            pane_id,
            root,
            result: Err(anyhow::anyhow!("Failed to start loader worker: {error}")),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn request_recursive_load(
    pane_id: PaneId,
    dirs: Vec<TreePath>,
    line: usize,
    op: FoldOp,
    session: &PaneSession,
    shared_ids: &Arc<Mutex<IdAllocator>>,
    app_event_tx: &CountingSender<AppEvent>,
    gui_event_tx: &CountingSender<GuiEvent>,
    loader_owner: &mut Option<LoaderOwner>,
    dialog_owner: &mut Option<PaneId>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    if loader_owner.is_some() || dialog_owner.is_some() {
        return Ok(());
    }
    let root = session.root.clone();
    let previous = session.save_controller.baseline().clone();
    let options = session.save_controller.scan_options();
    let cancel = Arc::new(AtomicBool::new(false));
    let display_path = dirs
        .first()
        .map(|dir| dir.to_fs_path(&root))
        .unwrap_or_else(|| root.clone());
    gui_event_tx.send(GuiEvent::ShowLoaderProgress {
        title: "Loading folders".to_owned(),
        path: display_path,
    })?;
    *dialog_owner = Some(pane_id);
    *loader_owner = Some(LoaderOwner {
        pane_id,
        root: root.clone(),
        kind: LoaderKind::Recursive {
            dirs: dirs.clone(),
            line,
            op,
        },
        cancel: Arc::clone(&cancel),
    });
    let worker_ids = Arc::clone(shared_ids);
    let worker_event_tx = app_event_tx.clone();
    let worker_root = root.clone();
    let spawn_result = thread::Builder::new()
        .name("fyler-loader".to_owned())
        // 再帰directory loadは深さが読めないため既定stackを維持する。
        .spawn(move || {
            let mut current = previous;
            let mut result = Ok(Some(current.clone()));
            let mut completed = 0_usize;
            for dir in &dirs {
                if cancel.load(Ordering::Relaxed) {
                    result = Ok(None);
                    break;
                }
                let mut local_progress = 0_usize;
                let progress_tx = worker_event_tx.clone();
                let loaded = fyler_fsops::scan::load_directory_recursive_cancellable(
                    &worker_root,
                    dir,
                    |_| {
                        let mut ids = worker_ids
                            .lock()
                            .map_err(|_| anyhow::anyhow!("ID allocator lock is poisoned"))?;
                        Ok(ids.allocate())
                    },
                    &current,
                    &options,
                    |entries| {
                        local_progress = entries;
                        let _ = progress_tx
                            .send(AppEvent::LoaderProgress(pane_id, completed + entries));
                    },
                    &cancel,
                );
                match loaded {
                    Ok(Some(baseline)) => {
                        current = baseline;
                        completed += local_progress;
                        result = Ok(Some(current.clone()));
                    }
                    Ok(None) => {
                        result = Ok(None);
                        break;
                    }
                    Err(error) => {
                        result = Err(error);
                        break;
                    }
                }
            }
            let _ = worker_event_tx.send(AppEvent::LoaderFinished {
                pane_id,
                root: worker_root,
                result,
            });
        });
    if let Err(error) = spawn_result {
        let _ = app_event_tx.send(AppEvent::LoaderFinished {
            pane_id,
            root,
            result: Err(anyhow::anyhow!("Failed to start loader worker: {error}")),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoaderStartGate {
    Allow,
    RejectSilently,
    Reject(&'static str),
}

fn loader_start_gate(
    dirty: bool,
    offline: bool,
    idle: bool,
    globally_busy: bool,
    loader_active: bool,
) -> LoaderStartGate {
    if dirty && !offline {
        return LoaderStartGate::Reject(
            "You are editing. Save or discard changes before changing directories.",
        );
    }
    if !idle {
        return LoaderStartGate::RejectSilently;
    }
    if globally_busy || loader_active {
        return LoaderStartGate::Reject("Another operation is in progress");
    }
    LoaderStartGate::Allow
}

enum LoaderResult<T, E> {
    Ready(T),
    Cancelled,
    Failed(E),
}

fn classify_loader_result<T, E>(result: Result<Option<T>, E>) -> LoaderResult<T, E> {
    match result {
        Ok(Some(value)) => LoaderResult::Ready(value),
        Ok(None) => LoaderResult::Cancelled,
        Err(error) => LoaderResult::Failed(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_session_root_change(
    pane_id: PaneId,
    new_root: PathBuf,
    cursor_target: Option<&OsStr>,
    new_baseline: BaselineTree,
    session: &mut PaneSession,
    gui_event_tx: &CountingSender<GuiEvent>,
    git: &mut GitRefresher,
    catalogs: &mut CatalogService,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    let new_watcher = match fyler_fsops::watch::watch(&new_root, session.watch_tx.clone()) {
        Ok(watcher) => watcher,
        Err(error) => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                format!("Failed to watch root ({}): {error:#}", new_root.display()),
            )?;
            return Ok(());
        }
    };
    if let Err(error) = session
        .save_controller
        .change_root_preserving_allocator(new_root.clone(), new_baseline)
    {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            format!("Failed to change root: {error:#}"),
        )?;
        return Ok(());
    }
    session.save_controller.collapse_all_dirs();
    let cursor_line =
        cursor_target.and_then(|name| session.save_controller.find_top_level_line(name));
    let new_lines = session.save_controller.visible_lines();
    session.root = new_root;
    session.watcher = new_watcher;
    session.watch_degraded = false;
    if let Err(error) = session.engine.send(EditorCommand::SetLines {
        lines: new_lines,
        cursor_line,
    }) {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            format!("Failed to display new directory: {error:#}"),
        )?;
    }
    catalogs.change_root(pane_id, session.root.clone());
    if let Err(error) = super::config::record_recent_root(&session.root) {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Warn,
            format!("Failed to record recent root: {error:#}"),
        )?;
    }
    after_root_change(
        pane_id,
        gui_event_tx,
        &mut session.save_controller,
        git,
        &session.root,
    )
}

fn finish_directory_load(
    pane_id: PaneId,
    kind: LoaderKind,
    baseline: BaselineTree,
    session: &mut PaneSession,
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    if let Err(error) = session
        .save_controller
        .replace_baseline_after_load(baseline)
    {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Warn,
            format!("Loaded directory result was stale: {error:#}"),
        );
    }
    let snapshot = session.engine.snapshot();
    let (lines, cursor_line) = match kind {
        LoaderKind::Directory { dir, line } => {
            match session
                .save_controller
                .toggle_collapse(&snapshot.lines, line)
            {
                super::save_flow::ToggleCollapseResult::Toggled(lines) => (lines, Some(line)),
                _ => {
                    return send_gui_message(
                        gui_event_tx,
                        pane_id,
                        MessageKind::Warn,
                        format!("Loaded directory is no longer expandable: {dir}"),
                    );
                }
            }
        }
        LoaderKind::Recursive { dirs, line, op } => {
            match session.save_controller.fold(&snapshot.lines, line, op) {
                FoldResult::Applied { lines, cursor_line } => (lines, cursor_line),
                FoldResult::NoOp => (session.save_controller.visible_lines(), Some(line)),
                _ => {
                    return send_gui_message(
                        gui_event_tx,
                        pane_id,
                        MessageKind::Warn,
                        format!(
                            "Loaded directories are no longer expandable: {}",
                            dirs.len()
                        ),
                    );
                }
            }
        }
        LoaderKind::PickerReveal { .. } => {
            unreachable!("picker reveal load has a separate completion path")
        }
        LoaderKind::Root { .. } => unreachable!("root load has a separate completion path"),
    };
    if let Err(error) = session
        .engine
        .send(EditorCommand::SetLines { lines, cursor_line })
    {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            format!("Failed to display loaded directory: {error:#}"),
        )?;
    }
    send_view_state(gui_event_tx, pane_id, &mut session.save_controller)
}

fn finish_picker_reveal_load(
    pane_id: PaneId,
    target: &TreePath,
    action: PickerAction,
    baseline: BaselineTree,
    session: &mut PaneSession,
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<Option<TreePath>, mpsc::SendError<GuiEvent>> {
    if let Err(error) = session
        .save_controller
        .replace_baseline_after_load(baseline)
    {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Warn,
            format!("Loaded search result was stale: {error:#}"),
        )?;
        return Ok(None);
    }

    if session
        .save_controller
        .baseline()
        .get_by_path(target)
        .is_some()
    {
        let engine = Arc::clone(&session.engine);
        let root = session.root.clone();
        handle_picker_select(
            pane_id,
            target.clone(),
            action,
            &mut session.save_controller,
            engine.as_ref(),
            &root,
            gui_event_tx,
        )?;
        return Ok(None);
    }

    if let Some(dir) =
        super::next_picker_reveal_directory(session.save_controller.baseline(), target)
    {
        return Ok(Some(dir));
    }

    send_gui_message(
        gui_event_tx,
        pane_id,
        MessageKind::Warn,
        "Search candidate was not found. It may have changed externally.",
    )?;
    Ok(None)
}

fn loader_completion_is_stale(
    pane_missing: bool,
    crashed: bool,
    root_changed: bool,
    dirty: bool,
    idle: bool,
) -> bool {
    pane_missing || crashed || root_changed || dirty || !idle
}

/// ロード完了時にpaneのrootが「別のroot」へ移ったかを判定する。
///
/// Root load自体はswapがこの判定より後に走るため session.root は旧rootのままで、
/// 新rootとの不一致を変更扱いすると必ずstaleになる。loader_owner排他により
/// Root load中に競合するroot変更は起き得ないので、Root loadでは常にfalse。
/// Directory/Recursive/PickerReveal loadはsession.rootを基準に列挙したため、
/// その間にrootが変わっていたらstaleとして破棄する。
fn loader_root_changed(is_root_load: bool, session_root: &Path, loaded_root: &Path) -> bool {
    !is_root_load && session_root != loaded_root
}

fn picker_reveal_start_rejection(
    dirty: bool,
    busy: bool,
    crashed: bool,
    idle: bool,
) -> Option<&'static str> {
    if dirty {
        Some("Cannot reveal a folded search candidate while editing. Save or discard changes.")
    } else if busy || crashed || !idle {
        Some("Another operation is in progress")
    } else {
        None
    }
}

fn should_defer_external_change(applying: bool, transferring: bool, loading: bool) -> bool {
    applying || transferring || loading
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ProbeEngine;

    impl EditorEngine for ProbeEngine {
        fn send(&self, _command: EditorCommand) -> anyhow::Result<()> {
            Ok(())
        }

        fn snapshot(&self) -> Arc<fyler_core::editor::EditorSnapshot> {
            Arc::new(fyler_core::editor::EditorSnapshot::empty())
        }
    }

    #[test]
    fn help_reflects_default_custom_and_unmapped_bindings() {
        let defaults = fyler_core::keymap::default_bindings();
        let default_lines = help_lines(&defaults);
        assert!(default_lines.iter().any(|line| {
            line.starts_with("Enter  ") && line.contains(EditorAction::Activate.description())
        }));

        let mut customized = defaults
            .into_iter()
            .filter(|binding| binding.action != EditorAction::ToggleHidden)
            .collect::<Vec<_>>();
        customized.push(KeyBinding {
            sequence: fyler_core::keymap::parse_key_sequence("x", None).unwrap(),
            action: EditorAction::FilePicker,
        });
        let custom_lines = help_lines(&customized);
        assert!(
            !custom_lines
                .iter()
                .any(|line| line.contains(EditorAction::ToggleHidden.description()))
        );
        assert!(custom_lines.iter().any(|line| {
            line.starts_with("g /, x  ") && line.contains(EditorAction::FilePicker.description())
        }));
    }

    #[test]
    fn close_rejects_last_dirty_busy_and_applying_panes() {
        assert_eq!(
            close_rejection(false, true, false, true, false),
            Some("The last pane cannot be closed")
        );
        assert_eq!(
            close_rejection(true, true, false, false, false),
            Some("A pane being edited cannot be closed")
        );
        assert_eq!(
            close_rejection(false, false, false, false, false),
            Some("A pane that is saving cannot be closed")
        );
        assert_eq!(
            close_rejection(false, true, true, false, false),
            Some("A pane cannot be closed during apply")
        );
        assert_eq!(close_rejection(false, true, false, false, false), None);
    }

    #[test]
    fn crashed_pane_can_close_even_if_snapshot_is_dirty_or_save_is_busy() {
        assert_eq!(close_rejection(true, false, false, false, true), None);
    }

    #[test]
    fn undo_rejects_busy_dirty_and_empty_slot() {
        assert_eq!(
            undo_rejection(false, false, false, true),
            Some("Another save is in progress")
        );
        assert_eq!(
            undo_rejection(true, false, false, false),
            Some("Cannot undo while editing. Save or discard changes.")
        );
        assert_eq!(
            undo_rejection(false, true, false, false),
            Some("No operations are available to undo")
        );
        assert_eq!(
            undo_rejection(false, false, true, false),
            Some("Cannot undo while the folder is offline or unreachable")
        );
        assert_eq!(undo_rejection(false, false, false, false), None);
    }

    #[test]
    fn feedback_start_gate_only_rejects_an_existing_dialog() {
        assert_eq!(feedback_start_rejection(false), None);
        assert_eq!(
            feedback_start_rejection(true),
            Some("Close the other dialog before opening feedback")
        );
    }

    #[test]
    fn incomplete_probe_pauses_while_dialog_is_open() {
        let root = PathBuf::from("C:/root");
        let mut baseline = fyler_core::tree::BaselineTree::new(&root);
        baseline.mark_incomplete(
            fyler_core::path::TreePath::parse("blocked"),
            fyler_core::scanwarn::ScanErrorKind::PermissionDenied,
        );
        let controller = SaveController::new(
            root.clone(),
            IdAllocator::new(),
            baseline,
            Arc::new(ProbeEngine),
        );

        assert_eq!(
            incomplete_probe_paths(&controller, false, false),
            Some(BTreeSet::from([root.join("blocked")]))
        );
        assert!(incomplete_probe_paths(&controller, true, false).is_none());
    }

    #[test]
    fn root_change_loader_classifies_success_and_cancel_without_committing_cancelled_data() {
        assert!(matches!(
            classify_loader_result::<_, ()>(Ok(Some("new-root"))),
            LoaderResult::Ready("new-root")
        ));
        assert!(matches!(
            classify_loader_result::<(), ()>(Ok(None)),
            LoaderResult::Cancelled
        ));
    }

    #[test]
    fn loader_completion_discards_missing_crashed_changed_dirty_and_busy_panes() {
        assert!(loader_completion_is_stale(true, false, false, false, true));
        assert!(loader_completion_is_stale(false, true, false, false, true));
        assert!(loader_completion_is_stale(false, false, true, false, true));
        assert!(loader_completion_is_stale(false, false, false, true, true));
        assert!(loader_completion_is_stale(
            false, false, false, false, false
        ));
        assert!(!loader_completion_is_stale(
            false, false, false, false, true
        ));
    }

    #[test]
    fn root_load_completion_is_not_stale_despite_pending_root_swap() {
        let old_root = Path::new("C:/Users");
        let new_root = Path::new("C:/");
        // Root loadはswap前で session.root(旧) != loaded_root(新)。ここでstale扱い
        // すると gd/^/:cd/bookmark/recent/drive の移動が全て黙って破棄される(regression)。
        assert!(!loader_root_changed(true, old_root, new_root));
        // Directory/PickerReveal loadはsession.rootを基準に列挙したため、rootが
        // 変わっていたら破棄する。同一rootなら破棄しない。
        assert!(loader_root_changed(false, old_root, new_root));
        assert!(!loader_root_changed(false, old_root, old_root));
    }

    #[test]
    fn picker_reveal_start_rejects_dirty_and_busy_loader_owner() {
        assert_eq!(
            picker_reveal_start_rejection(true, false, false, true),
            Some("Cannot reveal a folded search candidate while editing. Save or discard changes.")
        );
        assert_eq!(
            picker_reveal_start_rejection(false, true, false, true),
            Some("Another operation is in progress")
        );
        assert_eq!(
            picker_reveal_start_rejection(false, false, false, true),
            None
        );
    }

    #[test]
    fn root_change_start_gate_preserves_existing_dirty_and_busy_rules() {
        assert_eq!(
            loader_start_gate(true, false, true, false, false),
            LoaderStartGate::Reject(
                "You are editing. Save or discard changes before changing directories."
            )
        );
        assert_eq!(
            loader_start_gate(false, false, false, false, false),
            LoaderStartGate::RejectSilently
        );
        assert_eq!(
            loader_start_gate(false, false, true, false, true),
            LoaderStartGate::Reject("Another operation is in progress")
        );
        assert_eq!(
            loader_start_gate(false, false, true, false, false),
            LoaderStartGate::Allow
        );
    }

    #[test]
    fn directory_loader_participates_in_external_change_deferral() {
        assert!(should_defer_external_change(false, false, true));
        assert!(should_defer_external_change(true, false, false));
        assert!(should_defer_external_change(false, true, false));
        assert!(!should_defer_external_change(false, false, false));
    }

    #[test]
    fn parent_navigation_cursor_target_finds_the_previous_child_in_shallow_baseline() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("child")).unwrap();
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline_shallow_with(
            root.path(),
            &mut ids,
            &ScanOptions::default(),
        )
        .unwrap();
        let controller = SaveController::new(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::new(ProbeEngine),
        );

        assert_eq!(
            controller.find_top_level_line(std::ffi::OsStr::new("child")),
            Some(0)
        );
    }

    #[test]
    fn explicit_root_and_disabled_setting_bypass_session_restore() {
        assert!(!should_restore_session(true, true));
        assert!(!should_restore_session(false, false));
        assert!(should_restore_session(false, true));
    }

    #[test]
    fn unavailable_root_falls_back_to_nearest_existing_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let requested = root.path().join("missing").join("nested");
        let fallback = tempfile::tempdir().unwrap();
        assert_eq!(
            existing_root_or_ancestor(&requested, fallback.path()),
            root.path()
        );
    }

    #[test]
    fn cycle_focus_wraps_in_both_directions() {
        let first = PaneId::new(1);
        let second = PaneId::new(2);
        let layout = PaneLayout::leaf(first)
            .split(first, SplitDirection::Vertical, second)
            .unwrap();
        assert_eq!(cycle_focus(&layout, second, true), Some(first));
        assert_eq!(cycle_focus(&layout, first, false), Some(second));
    }
}
