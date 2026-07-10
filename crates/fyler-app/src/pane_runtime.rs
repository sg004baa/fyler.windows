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
use fyler_core::pane::{FocusDirection, PaneAction, PaneId, PaneLayout, SplitDirection};
use fyler_core::report::{CommitReport, OpOutcome, OpResult};
use fyler_core::tree::EntryKind;
use fyler_engine_nvim::{NvimConfig, NvimEngine};
use fyler_fsops::scan::ScanOptions;
use fyler_fsops::watch::{ExternalChange, FsWatcher};
use fyler_gui::app::{GuiEvent, GuiOptions};
use fyler_gui::confirm::ConfirmChoice;

use super::save_flow::{SaveController, SaveFlowResult};
use super::{
    AppEvent, BookmarkResolution, GitRefresher, after_root_change, bookmark_list_message,
    change_root_to, default_root, format_drive_paths, handle_activate_line, handle_external_change,
    handle_yank_path, normalize_root, resolve_bookmark_query, resolve_cd_target, send_gui_message,
    send_save_result, send_view_state,
};

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
    crashed: bool,
}

#[allow(clippy::too_many_arguments)]
fn create_pane(
    runtime: &tokio::runtime::Runtime,
    id: PaneId,
    root: PathBuf,
    nvim_exe: &Path,
    scan_options: ScanOptions,
    shared_ids: Arc<Mutex<IdAllocator>>,
    app_event_tx: &mpsc::Sender<AppEvent>,
) -> anyhow::Result<PaneSession> {
    // nvim起動はflaky回避のため呼び出し元イベントループで必ず直列に行う。
    let (engine, mut engine_events) = runtime.block_on(NvimEngine::start(NvimConfig {
        nvim_exe: nvim_exe.to_path_buf(),
        root: root.clone(),
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
        crashed: false,
    })
}

pub(super) fn run() -> anyhow::Result<()> {
    let root = match std::env::args_os().nth(1) {
        Some(root) => PathBuf::from(root),
        None => default_root()?,
    };
    let root = normalize_root(&root)?;
    let (config, config_warnings) = super::config::load();
    let scan_options = ScanOptions {
        show_hidden: config.show_hidden,
        sort: config.sort,
    };
    let gui_options = GuiOptions {
        confirm_detail: config.confirm_detail,
        font_path: config.font,
        font_y_offset_factor: config.font_y_offset_factor,
        icon_style: config.icons,
    };
    let bookmarks = config.bookmarks;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let (app_event_tx, app_event_rx) = mpsc::channel();
    let shared_ids = Arc::new(Mutex::new(IdAllocator::new()));
    let initial_id = PaneId::new(1);
    let initial = create_pane(
        &runtime,
        initial_id,
        root,
        &nvim_exe,
        scan_options,
        Arc::clone(&shared_ids),
        &app_event_tx,
    )?;

    let (confirm_tx, confirm_rx) = mpsc::channel();
    let confirm_event_tx = app_event_tx.clone();
    let confirm_bridge = thread::Builder::new()
        .name("fyler-confirm-events".to_owned())
        .spawn(move || {
            while let Ok(choice) = confirm_rx.recv() {
                if confirm_event_tx.send(AppEvent::Confirm(choice)).is_err() {
                    return;
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("確認結果の配線を開始できません: {error}"))?;

    let (gui_event_tx, gui_event_rx) = mpsc::channel();
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

            loop {
                let event = match pending_events.pop_front() {
                    Some(event) => event,
                    None => match app_event_rx.recv() {
                        Ok(event) => event,
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
                            dialog_owner,
                            apply_owner,
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
                            let _ = session.save_controller.on_choice(ConfirmChoice::Cancel);
                            dialog_owner = None;
                            if gui_event_tx.send(GuiEvent::CloseDialog).is_err() {
                                return;
                            }
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
                    AppEvent::Editor(pane_id, event) => {
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
                        if session.crashed {
                            continue;
                        }
                        match event {
                            EditorEvent::CommitRequested { changedtick, lines } => {
                                if apply_owner.is_some() || dialog_owner.is_some() {
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
                    AppEvent::Confirm(choice) => {
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
                                let spawn_result = thread::Builder::new()
                                    .name("fyler-apply".to_owned())
                                    .spawn(move || {
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
                                            );
                                        let _ = worker_event_tx
                                            .send(AppEvent::ApplyFinished(pane_id, report));
                                    });
                                if let Err(error) = spawn_result {
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
                                        .send(AppEvent::ApplyFinished(pane_id, report))
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
                    AppEvent::ApplyFinished(pane_id, report) => {
                        if apply_owner != Some(pane_id) {
                            continue;
                        }
                        let Some(session) = panes.get_mut(&pane_id) else {
                            continue;
                        };
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
                    AppEvent::ExternalChange(pane_id, change) => {
                        let mut by_pane = BTreeMap::<PaneId, BTreeSet<PathBuf>>::new();
                        by_pane.entry(pane_id).or_default().extend(change.paths);
                        while let Ok(queued_event) = app_event_rx.try_recv() {
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
                            if apply_owner.is_some() {
                                session.deferred_changes.extend(changed_paths);
                            } else {
                                let invalidated = match handle_external_change(
                                    changed_id,
                                    &changed_paths,
                                    &mut session.save_controller,
                                    &gui_event_tx,
                                    &mut git,
                                    &session.root,
                                ) {
                                    Ok(invalidated) => invalidated,
                                    Err(_) => return,
                                };
                                if invalidated && dialog_owner == Some(changed_id) {
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

    let gui_result = fyler_gui::app::run(gui_event_rx, confirm_tx, gui_options);
    let _ = app_event_tx.send(AppEvent::Shutdown);
    let _ = event_bridge.join();
    let _ = confirm_bridge.join();
    gui_result
}

#[allow(clippy::too_many_arguments)]
fn handle_pane_action(
    action: PaneAction,
    source: PaneId,
    runtime: &tokio::runtime::Runtime,
    nvim_exe: &Path,
    scan_options: ScanOptions,
    shared_ids: &Arc<Mutex<IdAllocator>>,
    app_event_tx: &mpsc::Sender<AppEvent>,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    panes: &mut BTreeMap<PaneId, PaneSession>,
    layout: &mut PaneLayout,
    active: &mut PaneId,
    last_active: &mut PaneId,
    next_pane_id: &mut u64,
    git: &mut GitRefresher,
    dialog_owner: Option<PaneId>,
    apply_owner: Option<PaneId>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    if source != *active {
        return Ok(());
    }
    match action {
        PaneAction::SplitHorizontal | PaneAction::SplitVertical => {
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
                apply_owner.is_some(),
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

fn change_session_root(
    pane_id: PaneId,
    new_root: PathBuf,
    cursor_target: Option<&OsStr>,
    session: &mut PaneSession,
    shared_ids: &Arc<Mutex<IdAllocator>>,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
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
