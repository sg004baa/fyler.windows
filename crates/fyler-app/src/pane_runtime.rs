//! 複数paneのセッション所有とイベント配線。

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use fyler_core::editor::{EditorCommand, EditorEngine, EditorEvent, MessageKind};
use fyler_core::gitstatus::GitBadge;
use fyler_core::grammar::PrefixParse;
use fyler_core::id::{EntryId, IdAllocator};
use fyler_core::keymap::{EditorAction, KeyBinding};
use fyler_core::pane::{FocusDirection, PaneAction, PaneId, PaneLayout, SplitDirection};
use fyler_core::report::{ApplyProgress, CommitReport, OpOutcome, OpResult};
use fyler_core::transfer::TransferKind;
use fyler_core::tree::EntryKind;
use fyler_core::undo::UndoTransaction;
use fyler_engine_nvim::{NvimConfig, NvimEngine};
use fyler_fsops::scan::ScanOptions;
use fyler_fsops::watch::{ExternalChange, FsWatcher};
use fyler_gui::app::{GuiAction, GuiEvent, GuiOptions};
use fyler_gui::confirm::ConfirmChoice;

use super::save_flow::{FoldResult, SaveController, SaveFlowResult};
use super::transfer_flow::{
    TransferController, TransferFlowResult, TransferPaneState, build_plan, destination_directory,
    resolve_selection, resolve_target, start_rejection,
};
use super::{
    AppEvent, BookmarkResolution, GitRefresher, after_root_change, bookmark_list_message,
    change_root_to, default_root, format_drive_paths, handle_activate_line, handle_external_change,
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
    _watcher: FsWatcher,
    watch_tx: mpsc::Sender<ExternalChange>,
    git_badges: HashMap<EntryId, GitBadge>,
    deferred_changes: BTreeSet<PathBuf>,
    undo_slot: Option<UndoTransaction>,
    crashed: bool,
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
) -> anyhow::Result<PaneSession> {
    // nvim起動はflaky回避のため呼び出し元イベントループで必ず直列に行う。
    let (engine, mut engine_events) = runtime.block_on(NvimEngine::start(NvimConfig {
        nvim_exe: nvim_exe.to_path_buf(),
        root: root.clone(),
        bindings: bindings.to_vec(),
    }))?;
    let baseline = {
        let mut ids = shared_ids
            .lock()
            .map_err(|_| anyhow::anyhow!("ID採番器のロックが破損しています"))?;
        fyler_fsops::scan::scan_baseline_with(&root, &mut ids, &scan_options)?
    };
    let save_engine: Arc<dyn EditorEngine> = engine.clone();
    let mut save_controller = SaveController::new_shared_with_scan_options(
        root.clone(),
        shared_ids,
        baseline,
        Arc::clone(&save_engine),
        scan_options,
    );
    save_controller.collapse_all_dirs();
    engine.set_initial_lines(save_controller.visible_lines())?;

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
        .map_err(|error| anyhow::anyhow!("エディタイベント配線を開始できません: {error}"))?;

    let watch_event_tx = app_event_tx.clone();
    thread::Builder::new()
        .name(format!("fyler-watch-events-{id}"))
        // recvからapp channelへ転送するだけの非再帰ループ。
        .stack_size(256 * 1024)
        .spawn(move || {
            while let Ok(change) = watch_rx.recv() {
                if watch_event_tx
                    .send(AppEvent::ExternalChange(id, change))
                    .is_err()
                {
                    return;
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("ファイル監視イベントの配線を開始できません: {error}"))?;

    Ok(PaneSession {
        id,
        root,
        engine: save_engine,
        save_controller,
        _watcher: watcher,
        watch_tx,
        git_badges: HashMap::new(),
        deferred_changes: BTreeSet::new(),
        undo_slot: None,
        crashed: false,
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
        ":w  変更を保存".to_owned(),
        ":cd  ルートを移動".to_owned(),
        ":b  ブックマーク / 最近使ったルート".to_owned(),
        ":terminal  ここでterminalを開く".to_owned(),
    ]);
    lines
}

pub(super) fn run() -> anyhow::Result<()> {
    let root = match std::env::args_os().nth(1) {
        Some(root) => PathBuf::from(root),
        None => default_root()?,
    };
    let root = normalize_root(&root)?;
    let (config, config_warnings) = super::config::load();
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
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        // pane最大4個のnvim RPC非同期I/Oをホストする。CPU bound処理は載せない。
        .worker_threads(2)
        .enable_all()
        .build()?;

    // 一回性イベントを落とさないためunboundedのまま保ち、滞留は計測する。
    let app_event_gauge = Arc::new(QueueGauge::new());
    let (app_event_inner_tx, app_event_rx) = mpsc::channel();
    let app_event_tx = CountingSender::new(app_event_inner_tx, Arc::clone(&app_event_gauge));
    let shared_ids = Arc::new(Mutex::new(IdAllocator::new()));
    let initial_id = PaneId::new(1);
    let initial = create_pane(
        &runtime,
        initial_id,
        root,
        &nvim_exe,
        &bindings,
        scan_options,
        Arc::clone(&shared_ids),
        &app_event_tx,
    )?;
    let (journal, journal_warning) = match undo_journal::UndoJournal::open() {
        Ok(journal) => (Some(journal), None),
        Err(error) => (
            None,
            Some(format!(
                "undo journalを開けません。次のapplyはundo不可になります: {error:#}"
            )),
        ),
    };
    let (pending_recovery, recovery_warning) = match &journal {
        Some(journal) => match journal.scan_on_startup() {
            Ok(entries) => (entries, None),
            Err(error) => (
                Vec::new(),
                Some(format!("undo journalの起動時走査に失敗しました: {error:#}")),
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
                    GuiAction::PickerSelect {
                        pane_id,
                        entry_id,
                        action,
                    } => AppEvent::PickerSelect {
                        pane_id,
                        entry_id,
                        action,
                    },
                };
                if action_event_tx.send(event).is_err() {
                    return;
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("確認結果の配線を開始できません: {error}"))?;

    let gui_event_gauge = Arc::new(QueueGauge::new());
    let (gui_event_inner_tx, gui_event_rx) = mpsc::channel();
    let gui_event_tx = CountingSender::new(gui_event_inner_tx, Arc::clone(&gui_event_gauge));
    gui_event_tx.send(GuiEvent::AddPane {
        pane_id: initial.id,
        engine: Arc::clone(&initial.engine),
        root: initial.root.clone(),
    })?;
    let initial_layout = PaneLayout::leaf(initial_id);
    gui_event_tx.send(GuiEvent::LayoutChanged {
        layout: initial_layout.clone(),
        active: initial_id,
    })?;

    let event_tx = app_event_tx.clone();
    let event_loop_gauge = Arc::clone(&app_event_gauge);
    // rescanを含むapp event loopは再帰深度が読めないため既定stackを維持する。
    let event_bridge = thread::Builder::new()
        .name("fyler-app-events".to_owned())
        .spawn(move || {
            let mut panes = BTreeMap::from([(initial_id, initial)]);
            let mut layout = initial_layout;
            let mut active = initial_id;
            let mut last_active = initial_id;
            let mut next_pane_id = 2_u64;
            let mut pending_events = VecDeque::new();
            let mut git = GitRefresher::new(event_tx.clone());
            let mut dialog_owner = None;
            let mut apply_owner = None;
            let mut transfer = TransferController::new();
            let mut pending_recovery = pending_recovery;
            let mut pending_open_with: Option<(
                PathBuf,
                Vec<fyler_fsops::openwith::OpenWithHandler>,
            )> = None;

            if send_view_state(
                &gui_event_tx,
                initial_id,
                &panes[&initial_id].save_controller,
            )
            .is_err()
            {
                return;
            }
            git.request(initial_id, panes[&initial_id].root.clone());
            if !config_warnings.is_empty()
                && send_gui_message(
                    &gui_event_tx,
                    initial_id,
                    MessageKind::Warn,
                    format!("設定: {}", config_warnings.join(" / ")),
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
                    format!("最近使ったルートを記録できません: {error:#}"),
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
                            journal.as_ref(),
                            dialog_owner,
                            apply_owner.is_some()
                                || transfer.is_awaiting()
                                || transfer.is_running(),
                        )
                        .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::Editor(pane_id, EditorEvent::EngineCrashed { reason }) => {
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
                        session.crashed = true;
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
                                    "すべてのeditor engineが停止しました".to_owned(),
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
                        if session.crashed && !matches!(event, EditorEvent::OpenFilePicker) {
                            continue;
                        }
                        match event {
                            EditorEvent::OpenFilePicker => {
                                if handle_open_file_picker(
                                    pane_id,
                                    &session.save_controller,
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
                                        "別の保存処理が進行中です",
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
                                                "undo journalを破棄できません: {error:#}"
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
                                if handle_activate_line(
                                    pane_id,
                                    &mut session.save_controller,
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
                            EditorEvent::NavigateInto { line } => {
                                let snapshot = session.engine.snapshot();
                                let Some(editor_line) = snapshot.lines.get(line) else {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Error,
                                        "移動対象の行が見つかりません",
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
                                        "保存済みのディレクトリ行ではありません",
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
                                        "ディレクトリ行ではありません",
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                };
                                let new_root = path.to_fs_path(&session.root);
                                if change_session_root(
                                    pane_id,
                                    new_root,
                                    None,
                                    session,
                                    &shared_ids,
                                    &gui_event_tx,
                                    &mut git,
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
                                            "これ以上、上のディレクトリはありません | ドライブ: {} (:cd で移動)",
                                            format_drive_paths(&drives)
                                        )
                                    } else {
                                        "これ以上、上のディレクトリはありません".to_owned()
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
                                if change_session_root(
                                    pane_id,
                                    new_root,
                                    cursor_target.as_deref(),
                                    session,
                                    &shared_ids,
                                    &gui_event_tx,
                                    &mut git,
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
                                            "現在: {} | ドライブ: {}",
                                            session.root.display(),
                                            format_drive_paths(&drives)
                                        )
                                    } else {
                                        format!("現在: {}", session.root.display())
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
                                        format!("パスを解決できません: {query}"),
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                };
                                if change_session_root(
                                    pane_id,
                                    new_root,
                                    None,
                                    session,
                                    &shared_ids,
                                    &gui_event_tx,
                                    &mut git,
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
                                        if change_session_root(
                                            pane_id,
                                            new_root,
                                            None,
                                            session,
                                            &shared_ids,
                                            &gui_event_tx,
                                            &mut git,
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
                                                "ブックマーク名が曖昧です: {query} ({})",
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
                                                "ブックマークまたは最近使ったルートが見つかりません: {query}"
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
                                        "編集中は隠しファイル表示を切り替えできません。保存または破棄してください",
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
                                                "隠しファイル表示を切り替えできません: {error:#}"
                                            ),
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
                                    format!("隠しファイル表示を更新できません: {error:#}"),
                                )
                                .is_err()
                                {
                                    return;
                                }
                                if send_view_state(
                                    &gui_event_tx,
                                    pane_id,
                                    &session.save_controller,
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
                                        "編集中は折りたたみを変更できません(:w で確定するか元に戻してください)",
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
                                                    "折りたたみ表示を更新できません: {error:#}"
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
                                            &session.save_controller,
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
                                            "この行のエントリを解決できません",
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
                                        "編集中はソート条件を変更できません。保存または破棄してください",
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
                                                    "ソート条件を変更できません: {error:#}"
                                                ),
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
                                        format!("ソート表示を更新できません: {error:#}"),
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
                                    &session.save_controller,
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
                        entry_id,
                        action,
                    } => {
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
                        let engine = Arc::clone(&session.engine);
                        let root = session.root.clone();
                        if handle_picker_select(
                            pane_id,
                            entry_id,
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
                                    "open-with候補の選択位置が範囲外です: {index}"
                                ))
                            };
                            if let Err(error) = result
                                && send_gui_message(
                                    &gui_event_tx,
                                    active,
                                    MessageKind::Error,
                                    format!("指定アプリで開けません: {error:#}"),
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
                                            "undo復旧候補を破棄できません: {}: {error:#}",
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
                                            format!("transfer workerを開始できません: {error}");
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
                                                    "undo journalを開始できません。このapplyはundo不可です: {error:#}"
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
                                            "起動失敗したapplyのundo journalを破棄できません: {error:#}"
                                        );
                                    }
                                    let error = format!("apply workerを開始できません: {error}");
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
                                        "undo journalをUndoingへ更新できません: {error:#}"
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
                                    let error = format!("undo workerを開始できません: {error}");
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
                    AppEvent::ApplyProgress(pane_id, progress) => {
                        if apply_owner == Some(pane_id)
                            && gui_event_tx
                                .send(GuiEvent::ApplyProgress(progress))
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
                                            "空のundo transactionを破棄できません: {error:#}"
                                        );
                                    }
                                    if send_gui_message(
                                        &gui_event_tx,
                                        pane_id,
                                        MessageKind::Warn,
                                        "このapplyのundo記録は空です。undoできません",
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
                                            "undo journalをCommittedへ更新できません: {error:#}"
                                        );
                                    }
                                    session.undo_slot = Some(transaction);
                                }
                            } else if send_gui_message(
                                &gui_event_tx,
                                pane_id,
                                MessageKind::Warn,
                                "このapplyはundo記録がないためundoできません",
                            )
                            .is_err()
                            {
                                return;
                            }
                        } else if let Some(transaction) = transaction
                            && let Some(journal) = &journal
                            && let Err(error) = journal.discard(&transaction.id)
                        {
                            eprintln!("失敗したapplyのundo journalを破棄できません: {error:#}");
                        }
                        let result = session.save_controller.on_apply_finished(report);
                        if send_save_result(&gui_event_tx, pane_id, result).is_err()
                            || send_view_state(
                                &gui_event_tx,
                                pane_id,
                                &session.save_controller,
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
                                &session.save_controller,
                            )
                            .is_err()
                        {
                            return;
                        }
                        git.request(pane_id, session.root.clone());
                        if let (Some(journal), Some(transaction_id)) = (&journal, transaction_id)
                            && let Err(error) = journal.finish_undone(&transaction_id)
                        {
                            eprintln!("undo journalをUndoneへ更新できません: {error:#}");
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
                            let _ = session.engine.send(EditorCommand::SetModifiable(true));
                            if send_view_state(
                                &gui_event_tx,
                                pane_id,
                                &session.save_controller,
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
                                    "transfer後の再読込に失敗しました: {}",
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
                        for (changed_id, changed_paths) in by_pane {
                            let Some(session) = panes.get_mut(&changed_id) else {
                                continue;
                            };
                            if apply_owner.is_some() || transfer.is_running() {
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
                                        "外部でファイルが変更されたため、transferを中断しました。内容を確認して再度実行してください",
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
                    AppEvent::Shutdown => return,
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("GUIイベント配線を開始できません: {error}"))?;

    let gui_dequeue_gauge = Arc::clone(&gui_event_gauge);
    let gui_result = fyler_gui::app::run(
        gui_event_rx,
        action_tx,
        gui_options,
        Arc::new(move || gui_dequeue_gauge.dequeue()),
    );
    let _ = app_event_tx.send(AppEvent::Shutdown);
    let _ = event_bridge.join();
    let _ = action_bridge.join();
    if std::env::var_os("FYLER_QUEUE_STATS").as_deref() == Some(OsStr::new("1")) {
        eprintln!(
            "fyler queue high-water: app_event={} gui_event={}",
            app_event_gauge.high_water(),
            gui_event_gauge.high_water()
        );
    }
    gui_result
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
                    "applyまたはtransfer中はpaneを分割できません",
                );
            }
            if panes.len() >= MAX_PANES {
                return send_gui_message(
                    gui_event_tx,
                    source,
                    MessageKind::Info,
                    "paneは最大4個です",
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
            ) {
                Ok(session) => session,
                Err(error) => {
                    return send_gui_message(
                        gui_event_tx,
                        source,
                        MessageKind::Error,
                        format!("paneを追加できません: {error:#}"),
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
            send_view_state(gui_event_tx, new_session.id, &new_session.save_controller)?;
            git.request(new_session.id, new_session.root.clone());
            panes.insert(new_id, new_session);
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
                    "確認ダイアログを閉じてからpaneを閉じてください",
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
            "transfer先のpaneがありません。先にpaneを分割してください",
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
    };
    let target_state = TransferPaneState {
        dirty: target_snapshot.dirty,
        idle: target_session.save_controller.is_idle(),
        crashed: target_session.crashed,
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
                "未保存の新規行を含むためtransferできません。先に保存してください",
            );
        }
        Err(super::transfer_flow::SelectionError::Empty) => {
            return send_gui_message(
                gui_event_tx,
                source,
                MessageKind::Info,
                "transfer対象が選択されていません",
            );
        }
        Err(super::transfer_flow::SelectionError::MissingLine) => {
            return send_gui_message(
                gui_event_tx,
                source,
                MessageKind::Error,
                "transfer対象の行が見つかりません",
            );
        }
        Err(super::transfer_flow::SelectionError::UnknownId) => {
            return send_gui_message(
                gui_event_tx,
                source,
                MessageKind::Error,
                "transfer対象を現在のファイル一覧へ解決できません",
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
            "transfer先paneのカーソル位置を解決できません",
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
            "transfer対象がありません",
        );
    }
    let preflight = fyler_fsops::preflight_transfer(&plan);
    if !preflight.blocked.is_empty() {
        return send_gui_message(
            gui_event_tx,
            source,
            MessageKind::Error,
            format!(
                "transferできないパスがあります: {}",
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
        Some("最後のpaneは閉じられません")
    } else if applying {
        Some("apply中はpaneを閉じられません")
    } else if !crashed && dirty {
        Some("編集中のpaneは閉じられません")
    } else if !crashed && !idle {
        Some("保存処理中のpaneは閉じられません")
    } else {
        None
    }
}

fn undo_rejection(dirty: bool, slot_empty: bool, busy: bool) -> Option<&'static str> {
    if busy {
        Some("別の保存処理が進行中です")
    } else if dirty {
        Some("編集中はundoできません。保存または破棄してください")
    } else if slot_empty {
        Some("undoできる操作がありません")
    } else {
        None
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
        eprintln!("undo journalを破棄できません: {error:#}");
    }
}

fn change_session_root(
    pane_id: PaneId,
    new_root: PathBuf,
    cursor_target: Option<&OsStr>,
    session: &mut PaneSession,
    shared_ids: &Arc<Mutex<IdAllocator>>,
    gui_event_tx: &CountingSender<GuiEvent>,
    git: &mut GitRefresher,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    let changed = change_root_to(
        pane_id,
        new_root,
        cursor_target,
        &mut session.root,
        &mut session._watcher,
        &session.watch_tx,
        shared_ids,
        &mut session.save_controller,
        session.engine.as_ref(),
        gui_event_tx,
    )?;
    if changed {
        after_root_change(
            pane_id,
            gui_event_tx,
            &session.save_controller,
            git,
            &session.root,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Some("最後のpaneは閉じられません")
        );
        assert_eq!(
            close_rejection(true, true, false, false, false),
            Some("編集中のpaneは閉じられません")
        );
        assert_eq!(
            close_rejection(false, false, false, false, false),
            Some("保存処理中のpaneは閉じられません")
        );
        assert_eq!(
            close_rejection(false, true, true, false, false),
            Some("apply中はpaneを閉じられません")
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
            undo_rejection(false, false, true),
            Some("別の保存処理が進行中です")
        );
        assert_eq!(
            undo_rejection(true, false, false),
            Some("編集中はundoできません。保存または破棄してください")
        );
        assert_eq!(
            undo_rejection(false, true, false),
            Some("undoできる操作がありません")
        );
        assert_eq!(undo_rejection(false, false, false), None);
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
