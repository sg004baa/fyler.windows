//! fyler — エントリポイント。各レイヤーの配線だけを行う(ロジックを書かない)。
//!
//! 各レイヤーの役割はAGENTS.mdの依存境界表を参照。ここに書いてよいのは
//! 「起動」「イベントの受け渡し」「ユーザー操作の各レイヤーへの配線」
//! 「保存状態機械の副作用(SaveEffect)の実行」のみ。

mod config;
mod save_flow;

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;

use fyler_core::editor::{EditorCommand, EditorEngine, EditorEvent, EditorMessage, MessageKind};
use fyler_core::gitstatus::GitBadge;
use fyler_core::grammar::PrefixParse;
use fyler_core::id::IdAllocator;
use fyler_core::tree::EntryKind;
use fyler_engine_nvim::{NvimConfig, NvimEngine};
use fyler_fsops::scan::ScanOptions;
use fyler_fsops::watch::{ExternalChange, FsWatcher};
use fyler_gui::app::{GuiEvent, GuiOptions};
use fyler_gui::confirm::ConfirmChoice;

use crate::save_flow::{SaveController, SaveFlowResult, ToggleCollapseResult};

enum AppEvent {
    Editor(EditorEvent),
    Confirm(ConfirmChoice),
    ExternalChange(ExternalChange),
    GitStatus {
        root: PathBuf,
        statuses: HashMap<PathBuf, GitBadge>,
    },
    Shutdown,
}

/// git statusサブプロセスをappイベントスレッド外で実行し、結果をAppEventで返す。
///
/// 常に同時実行1本まで。実行中に再要求されたら完了後に1回だけ再実行する。
struct GitRefresher {
    event_tx: mpsc::Sender<AppEvent>,
    inflight: bool,
    queued: Option<PathBuf>,
}

impl GitRefresher {
    fn new(event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self {
            event_tx,
            inflight: false,
            queued: None,
        }
    }

    fn request(&mut self, root: PathBuf) {
        let Some(root) = self.prepare_request(root) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        drop(thread::spawn(move || {
            let statuses = fyler_fsops::gitstatus::status_badges(&root).unwrap_or_default();
            let _ = event_tx.send(AppEvent::GitStatus { root, statuses });
        }));
    }

    fn prepare_request(&mut self, root: PathBuf) -> Option<PathBuf> {
        if self.inflight {
            self.queued = Some(root);
            return None;
        }
        self.inflight = true;
        Some(root)
    }

    fn on_finished(&mut self) -> Option<PathBuf> {
        self.inflight = false;
        self.queued.take()
    }
}

fn main() -> anyhow::Result<()> {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let root = normalize_root(&root)?;
    let (config, config_warnings) = config::load();
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

    // eframeはメインスレッドで動かすため、runtimeはGUI存続中も別スレッドを維持する。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (engine, mut engine_events) = runtime.block_on(NvimEngine::start(NvimConfig {
        nvim_exe,
        root: root.clone(),
    }))?;

    let (watch_tx, watch_rx) = mpsc::channel::<ExternalChange>();
    let watcher = fyler_fsops::watch::watch(&root, watch_tx.clone())?;

    let mut ids = IdAllocator::new();
    let baseline = fyler_fsops::scan::scan_baseline_with(&root, &mut ids, &scan_options)?;
    let save_engine: Arc<dyn EditorEngine> = engine.clone();
    let mut save_controller = SaveController::new_with_scan_options(
        root.clone(),
        ids,
        baseline,
        Arc::clone(&save_engine),
        scan_options,
    );
    save_controller.collapse_all_dirs();
    engine.set_initial_lines(save_controller.visible_lines())?;

    // tokioのengine channelとGUIのstd channelをapp内の1本へ集約する。
    let (app_event_tx, app_event_rx) = mpsc::channel();
    let editor_event_tx = app_event_tx.clone();
    let editor_bridge = thread::Builder::new()
        .name("fyler-engine-events".to_owned())
        .spawn(move || {
            while let Some(event) = engine_events.blocking_recv() {
                if editor_event_tx.send(AppEvent::Editor(event)).is_err() {
                    return;
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("エディタイベント配線を開始できません: {error}"))?;

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

    let watch_event_tx = app_event_tx.clone();
    let watch_bridge = thread::Builder::new()
        .name("fyler-watch-events".to_owned())
        .spawn(move || {
            while let Ok(change) = watch_rx.recv() {
                if watch_event_tx
                    .send(AppEvent::ExternalChange(change))
                    .is_err()
                {
                    return;
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("ファイル監視イベントの配線を開始できません: {error}"))?;

    // GUIクレートへtokio型を漏らさず、core型とConfirmChoiceだけを受け渡す。
    let (gui_event_tx, gui_event_rx) = mpsc::channel();
    gui_event_tx.send(GuiEvent::RootChanged(root.clone()))?;
    if !config_warnings.is_empty() {
        send_gui_message(
            &gui_event_tx,
            MessageKind::Warn,
            format!("設定: {}", config_warnings.join(" / ")),
        )?;
    }
    let app_engine = Arc::clone(&save_engine);
    let git_event_tx = app_event_tx.clone();
    let event_bridge = thread::Builder::new()
        .name("fyler-app-events".to_owned())
        .spawn(move || {
            let mut root = root;
            let mut _watcher = watcher;
            let mut pending_events = VecDeque::new();
            let mut git = GitRefresher::new(git_event_tx);
            if send_file_infos(&gui_event_tx, &save_controller).is_err() {
                return;
            }
            git.request(root.clone());
            if let Err(error) = config::record_recent_root(&root)
                && send_gui_message(
                    &gui_event_tx,
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
                    AppEvent::Editor(EditorEvent::CommitRequested { changedtick, lines }) => {
                        let result = save_controller.on_commit(changedtick, &lines);
                        if send_save_result(&gui_event_tx, result).is_err() {
                            return;
                        }
                    }
                    AppEvent::Editor(EditorEvent::ActivateLine { line }) => {
                        if handle_activate_line(
                            &mut save_controller,
                            app_engine.as_ref(),
                            &root,
                            line,
                            &gui_event_tx,
                        )
                        .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::Editor(EditorEvent::NavigateInto { line }) => {
                        let snapshot = app_engine.snapshot();
                        let Some(editor_line) = snapshot.lines.get(line) else {
                            if send_gui_message(
                                &gui_event_tx,
                                MessageKind::Error,
                                "移動対象の行が見つかりません",
                            )
                            .is_err()
                            {
                                return;
                            }
                            continue;
                        };

                        match fyler_core::grammar::split_id_prefix(&editor_line.text) {
                            PrefixParse::NoId { .. } => {
                                if send_gui_message(
                                    &gui_event_tx,
                                    MessageKind::Info,
                                    "保存されていない行です",
                                )
                                .is_err()
                                {
                                    return;
                                }
                                continue;
                            }
                            PrefixParse::Broken => {
                                if send_gui_message(
                                    &gui_event_tx,
                                    MessageKind::Error,
                                    "壊れたIDプレフィックスの行は移動できません",
                                )
                                .is_err()
                                {
                                    return;
                                }
                                continue;
                            }
                            PrefixParse::WithId { .. } => {}
                        }

                        let Some((path, kind)) =
                            save_controller.resolve_line(&snapshot.lines, line)
                        else {
                            if send_gui_message(
                                &gui_event_tx,
                                MessageKind::Error,
                                "行に対応するディレクトリが現在のツリーに見つかりません",
                            )
                            .is_err()
                            {
                                return;
                            }
                            continue;
                        };
                        if kind != EntryKind::Dir {
                            if send_gui_message(
                                &gui_event_tx,
                                MessageKind::Info,
                                "ディレクトリ行ではありません",
                            )
                            .is_err()
                            {
                                return;
                            }
                            continue;
                        }

                        let root_changed = match change_root_to(
                            path.to_fs_path(&root),
                            None,
                            &mut root,
                            &mut _watcher,
                            &watch_tx,
                            &mut save_controller,
                            app_engine.as_ref(),
                            &gui_event_tx,
                        ) {
                            Ok(root_changed) => root_changed,
                            Err(_) => return,
                        };
                        if root_changed
                            && after_root_change(
                                &gui_event_tx,
                                &save_controller,
                                &mut git,
                                &root,
                            )
                            .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::Editor(EditorEvent::YankPath { line }) => {
                        if handle_yank_path(
                            &save_controller,
                            app_engine.as_ref(),
                            &root,
                            line,
                            &gui_event_tx,
                        )
                        .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::Editor(EditorEvent::NavigateParent) => {
                        let cursor_target = root.file_name().map(OsStr::to_owned);
                        let Some(new_root) = root.parent().map(Path::to_path_buf) else {
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
                                MessageKind::Info,
                                message,
                            )
                            .is_err()
                            {
                                return;
                            }
                            continue;
                        };

                        let root_changed = match change_root_to(
                            new_root,
                            cursor_target.as_deref(),
                            &mut root,
                            &mut _watcher,
                            &watch_tx,
                            &mut save_controller,
                            app_engine.as_ref(),
                            &gui_event_tx,
                        ) {
                            Ok(root_changed) => root_changed,
                            Err(_) => return,
                        };
                        if root_changed
                            && after_root_change(
                                &gui_event_tx,
                                &save_controller,
                                &mut git,
                                &root,
                            )
                            .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::Editor(EditorEvent::ChangeDirectory { query }) => {
                        let Some(query) = query else {
                            let drives = fyler_fsops::drives::list_drives();
                            let message = if drives.len() >= 2 {
                                format!(
                                    "現在: {} | ドライブ: {}",
                                    root.display(),
                                    format_drive_paths(&drives)
                                )
                            } else {
                                format!("現在: {}", root.display())
                            };
                            if send_gui_message(&gui_event_tx, MessageKind::Info, message).is_err() {
                                return;
                            }
                            continue;
                        };

                        let home = std::env::home_dir();
                        let Some(new_root) = resolve_cd_target(&query, &root, home.as_deref()) else {
                            if send_gui_message(
                                &gui_event_tx,
                                MessageKind::Error,
                                format!("パスを解決できません: {query}"),
                            )
                            .is_err()
                            {
                                return;
                            }
                            continue;
                        };
                        let root_changed = match change_root_to(
                            new_root,
                            None,
                            &mut root,
                            &mut _watcher,
                            &watch_tx,
                            &mut save_controller,
                            app_engine.as_ref(),
                            &gui_event_tx,
                        ) {
                            Ok(root_changed) => root_changed,
                            Err(_) => return,
                        };
                        if root_changed
                            && after_root_change(
                                &gui_event_tx,
                                &save_controller,
                                &mut git,
                                &root,
                            )
                            .is_err()
                        {
                            return;
                        }
                    }
                    AppEvent::Editor(EditorEvent::JumpBookmark { query }) => {
                        let recent = config::load_recent_roots();
                        let Some(query) = query else {
                            if send_gui_message(
                                &gui_event_tx,
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
                                let root_changed = match change_root_to(
                                    new_root,
                                    None,
                                    &mut root,
                                    &mut _watcher,
                                    &watch_tx,
                                    &mut save_controller,
                                    app_engine.as_ref(),
                                    &gui_event_tx,
                                ) {
                                    Ok(root_changed) => root_changed,
                                    Err(_) => return,
                                };
                                if root_changed
                                    && after_root_change(
                                        &gui_event_tx,
                                        &save_controller,
                                        &mut git,
                                        &root,
                                    )
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            BookmarkResolution::Ambiguous(names) => {
                                if send_gui_message(
                                    &gui_event_tx,
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
                                    MessageKind::Error,
                                    format!("ブックマークまたは最近使ったルートが見つかりません: {query}"),
                                )
                                .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                    AppEvent::Editor(EditorEvent::ToggleHidden) => {
                        if app_engine.snapshot().dirty {
                            if send_gui_message(
                                &gui_event_tx,
                                MessageKind::Info,
                                "編集中は隠しファイル表示を切り替えできません。保存または破棄してください",
                            )
                            .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                        if !save_controller.is_idle() {
                            continue;
                        }

                        let lines = match save_controller.toggle_hidden() {
                            Ok(lines) => lines,
                            Err(error) => {
                                if send_gui_message(
                                    &gui_event_tx,
                                    MessageKind::Error,
                                    format!("隠しファイル表示を切り替えできません: {error:#}"),
                                )
                                .is_err()
                                {
                                    return;
                                }
                                continue;
                            }
                        };
                        if let Err(error) = app_engine.send(EditorCommand::SetLines {
                            lines,
                            cursor_line: None,
                        }) {
                            if send_gui_message(
                                &gui_event_tx,
                                MessageKind::Error,
                                format!("隠しファイル表示を更新できません: {error:#}"),
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                        if send_file_infos(&gui_event_tx, &save_controller).is_err() {
                            return;
                        }
                        git.request(root.clone());
                    }
                    AppEvent::Editor(event) => {
                        if gui_event_tx.send(GuiEvent::Editor(event)).is_err() {
                            return;
                        }
                    }
                    AppEvent::Confirm(choice) => {
                        let result = save_controller.on_choice(choice);
                        let refresh_decorations = matches!(
                            &result,
                            SaveFlowResult::ShowReport(_)
                                | SaveFlowResult::ReconcileFailed { .. }
                        );
                        if send_save_result(&gui_event_tx, result).is_err() {
                            return;
                        }
                        if refresh_decorations {
                            if send_file_infos(&gui_event_tx, &save_controller).is_err() {
                                return;
                            }
                            git.request(root.clone());
                        }
                    }
                    AppEvent::ExternalChange(change) => {
                        let mut changed_paths = change.paths;
                        while let Ok(queued_event) = app_event_rx.try_recv() {
                            match queued_event {
                                AppEvent::ExternalChange(change) => {
                                    changed_paths.extend(change.paths);
                                }
                                event => pending_events.push_back(event),
                            }
                        }
                        let result = save_controller.on_external_change(&changed_paths);
                        if !matches!(&result, SaveFlowResult::NoChanges)
                            && send_save_result(&gui_event_tx, result).is_err()
                        {
                            return;
                        }
                        if send_file_infos(&gui_event_tx, &save_controller).is_err() {
                            return;
                        }
                        git.request(root.clone());
                    }
                    AppEvent::GitStatus {
                        root: status_root,
                        statuses,
                    } => {
                        if let Some(next_root) = git.on_finished() {
                            git.request(next_root);
                        }
                        if status_root == root
                            && gui_event_tx
                                .send(GuiEvent::GitBadges(
                                    save_controller.map_git_badges(&statuses),
                                ))
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

    let gui_engine: Arc<dyn EditorEngine> = engine;
    let gui_result = fyler_gui::app::run(gui_engine, gui_event_rx, confirm_tx, gui_options);
    let _ = app_event_tx.send(AppEvent::Shutdown);
    let _ = watch_bridge.join();
    let _ = event_bridge.join();
    let _ = confirm_bridge.join();
    let _ = editor_bridge.join();
    gui_result
}

#[allow(clippy::too_many_arguments)] // ルート差し替えの全状態とカーソル復元対象を明示する。
fn change_root_to(
    new_root: PathBuf,
    cursor_target: Option<&OsStr>,
    root: &mut PathBuf,
    watcher: &mut FsWatcher,
    watch_tx: &mpsc::Sender<ExternalChange>,
    save_controller: &mut SaveController,
    engine: &dyn EditorEngine,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
) -> Result<bool, mpsc::SendError<GuiEvent>> {
    let new_root = match normalize_root(&new_root) {
        Ok(new_root) => new_root,
        Err(error) => {
            send_gui_message(
                gui_event_tx,
                MessageKind::Error,
                format!(
                    "表示ルートを正規化できません ({}): {error}",
                    new_root.display()
                ),
            )?;
            return Ok(false);
        }
    };

    if engine.snapshot().dirty {
        send_gui_message(
            gui_event_tx,
            MessageKind::Info,
            "編集中です。保存または破棄してからディレクトリを移動してください",
        )?;
        return Ok(false);
    }
    if !save_controller.is_idle() {
        return Ok(false);
    }

    let mut new_ids = IdAllocator::new();
    let scan_options = save_controller.scan_options();
    let new_baseline =
        match fyler_fsops::scan::scan_baseline_with(&new_root, &mut new_ids, &scan_options) {
            Ok(baseline) => baseline,
            Err(error) => {
                send_gui_message(
                    gui_event_tx,
                    MessageKind::Error,
                    format!(
                        "表示ルートを読み込めません ({}): {error:#}",
                        new_root.display()
                    ),
                )?;
                return Ok(false);
            }
        };

    // 新しい監視の作成に失敗した場合、現在のroot/baseline/watcherを
    // そのまま維持できるよう、状態差し替え前に準備だけ済ませる。
    let new_watcher = match fyler_fsops::watch::watch(&new_root, watch_tx.clone()) {
        Ok(watcher) => watcher,
        Err(error) => {
            send_gui_message(
                gui_event_tx,
                MessageKind::Error,
                format!(
                    "表示ルートを監視できません ({}): {error:#}",
                    new_root.display()
                ),
            )?;
            return Ok(false);
        }
    };

    if let Err(error) = save_controller.change_root(new_root.clone(), new_ids, new_baseline) {
        send_gui_message(
            gui_event_tx,
            MessageKind::Error,
            format!("表示ルートを変更できません: {error:#}"),
        )?;
        return Ok(false);
    }
    save_controller.collapse_all_dirs();
    let cursor_line = cursor_target.and_then(|name| save_controller.find_top_level_line(name));
    let new_lines = save_controller.visible_lines();

    *root = new_root;
    *watcher = new_watcher;
    if let Err(error) = engine.send(EditorCommand::SetLines {
        lines: new_lines,
        cursor_line,
    }) {
        send_gui_message(
            gui_event_tx,
            MessageKind::Error,
            format!("新しいディレクトリを表示できません: {error:#}"),
        )?;
    }
    gui_event_tx.send(GuiEvent::RootChanged(root.clone()))?;
    if let Err(error) = config::record_recent_root(root) {
        send_gui_message(
            gui_event_tx,
            MessageKind::Warn,
            format!("最近使ったルートを記録できません: {error:#}"),
        )?;
    }
    Ok(true)
}

fn after_root_change(
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    save_controller: &SaveController,
    git: &mut GitRefresher,
    root: &Path,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    gui_event_tx.send(GuiEvent::GitBadges(HashMap::new()))?;
    send_file_infos(gui_event_tx, save_controller)?;
    git.request(root.to_path_buf());
    Ok(())
}

fn normalize_root(root: &Path) -> std::io::Result<PathBuf> {
    std::path::absolute(root)
}

/// `:cd`の引数を移動先の絶対パスへ解決する。
///
/// 絶対パスはそのまま返し、`~`単独と`~/...`はホームディレクトリ基準に
/// 展開する。それ以外は現在ルートからの相対パスとして解決する。
/// `~user`形式と、ホームディレクトリ不明時の`~`は解決できない。
fn resolve_cd_target(query: &str, root: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if query == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(relative) = query.strip_prefix("~/") {
        return home.map(|home| home.join(relative));
    }
    if query.starts_with('~') {
        return None;
    }

    let path = Path::new(query);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    })
}

fn format_drive_paths(drives: &[PathBuf]) -> String {
    drives
        .iter()
        .map(|drive| drive.display().to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BookmarkResolution {
    Resolved(PathBuf),
    Ambiguous(Vec<String>),
    NotFound,
}

fn resolve_bookmark_query(
    query: &str,
    bookmarks: &[(String, PathBuf)],
    recent: &[PathBuf],
) -> BookmarkResolution {
    if let Some((_, path)) = bookmarks.iter().find(|(name, _)| name == query) {
        return BookmarkResolution::Resolved(path.clone());
    }

    let prefix_matches = bookmarks
        .iter()
        .filter(|(name, _)| name.starts_with(query))
        .collect::<Vec<_>>();
    if let [(_, path)] = prefix_matches.as_slice() {
        return BookmarkResolution::Resolved((*path).clone());
    }

    if let Ok(index) = query.parse::<usize>()
        && let Some(path) = index.checked_sub(1).and_then(|index| recent.get(index))
    {
        return BookmarkResolution::Resolved(path.clone());
    }

    if prefix_matches.len() > 1 {
        return BookmarkResolution::Ambiguous(
            prefix_matches
                .into_iter()
                .map(|(name, _)| name.clone())
                .collect(),
        );
    }
    BookmarkResolution::NotFound
}

fn bookmark_list_message(bookmarks: &[(String, PathBuf)], recent: &[PathBuf]) -> String {
    let mut entries = bookmarks
        .iter()
        .map(|(name, path)| format!("b:{name}={}", path.display()))
        .collect::<Vec<_>>();
    entries.extend(
        recent
            .iter()
            .enumerate()
            .map(|(index, path)| format!("{}:{}", index + 1, path.display())),
    );
    if entries.is_empty() {
        "ブックマークと最近使ったルートはありません".to_owned()
    } else {
        entries.join(" | ")
    }
}

fn handle_activate_line(
    save_controller: &mut SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    line: usize,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    let snapshot = engine.snapshot();
    let Some(editor_line) = snapshot.lines.get(line) else {
        return send_gui_message(
            gui_event_tx,
            MessageKind::Error,
            "開く対象の行が見つかりません",
        );
    };

    match fyler_core::grammar::split_id_prefix(&editor_line.text) {
        PrefixParse::NoId { .. } => {
            return send_gui_message(gui_event_tx, MessageKind::Info, "保存されていない行です");
        }
        PrefixParse::Broken => {
            return send_gui_message(
                gui_event_tx,
                MessageKind::Error,
                "壊れたIDプレフィックスの行は開けません",
            );
        }
        PrefixParse::WithId { .. } => {}
    }

    let Some((path, kind)) = save_controller.resolve_line(&snapshot.lines, line) else {
        return send_gui_message(
            gui_event_tx,
            MessageKind::Error,
            "行に対応するファイルが現在のツリーに見つかりません",
        );
    };

    match kind {
        EntryKind::File | EntryKind::Symlink => {
            let path = path.to_fs_path(root);
            if let Err(error) = fyler_fsops::open::open_with_default_app(&path) {
                send_gui_message(
                    gui_event_tx,
                    MessageKind::Error,
                    format!("ファイルを開けません: {error:#}"),
                )?;
            }
        }
        EntryKind::Dir => {
            if snapshot.dirty {
                return send_gui_message(
                    gui_event_tx,
                    MessageKind::Info,
                    "編集中は折りたたみできません。保存または破棄してください",
                );
            }

            match save_controller.toggle_collapse(&snapshot.lines, line) {
                ToggleCollapseResult::Toggled(lines) => {
                    // 折りたたみ/展開した行は差し替え後も同じindexに残るため、
                    // カーソルをその行へ戻す(先頭へ飛ばさない)。
                    if let Err(error) = engine.send(EditorCommand::SetLines {
                        lines,
                        cursor_line: Some(line),
                    }) {
                        send_gui_message(
                            gui_event_tx,
                            MessageKind::Error,
                            format!("折りたたみ表示を更新できません: {error:#}"),
                        )?;
                    }
                    gui_event_tx.send(GuiEvent::FileInfos(save_controller.visible_file_infos()))?;
                }
                ToggleCollapseResult::NotADirectory => {
                    send_gui_message(
                        gui_event_tx,
                        MessageKind::Error,
                        "対象行はディレクトリではありません",
                    )?;
                }
                ToggleCollapseResult::NotFound => {
                    send_gui_message(
                        gui_event_tx,
                        MessageKind::Error,
                        "行に対応するディレクトリが現在のツリーに見つかりません",
                    )?;
                }
                ToggleCollapseResult::Busy => {}
            }
        }
    }
    Ok(())
}

fn handle_yank_path(
    save_controller: &SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    line: usize,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    let snapshot = engine.snapshot();
    let Some(editor_line) = snapshot.lines.get(line) else {
        return send_gui_message(
            gui_event_tx,
            MessageKind::Error,
            "コピー対象の行が見つかりません",
        );
    };

    match fyler_core::grammar::split_id_prefix(&editor_line.text) {
        PrefixParse::NoId { .. } => {
            return send_gui_message(gui_event_tx, MessageKind::Info, "保存されていない行です");
        }
        PrefixParse::Broken => {
            return send_gui_message(
                gui_event_tx,
                MessageKind::Error,
                "壊れたIDプレフィックスの行はコピーできません",
            );
        }
        PrefixParse::WithId { .. } => {}
    }

    let Some((path, _)) = save_controller.resolve_line(&snapshot.lines, line) else {
        return send_gui_message(
            gui_event_tx,
            MessageKind::Error,
            "行に対応するファイルが現在のツリーに見つかりません",
        );
    };
    let path = path.to_fs_path(root);
    gui_event_tx.send(GuiEvent::CopyPath(path.to_string_lossy().into_owned()))
}

fn send_gui_message(
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    kind: MessageKind,
    text: impl Into<String>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    gui_event_tx.send(GuiEvent::Editor(EditorEvent::Message(EditorMessage {
        kind,
        text: text.into(),
    })))
}

/// 表示中エントリのインメモリなファイル情報をGUIへ送る。
fn send_file_infos(
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    save_controller: &SaveController,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    gui_event_tx.send(GuiEvent::FileInfos(save_controller.visible_file_infos()))
}

fn send_save_result(
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    result: SaveFlowResult,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    match result {
        SaveFlowResult::ShowPlan {
            plan,
            warnings,
            overwrites,
        } => gui_event_tx.send(GuiEvent::ShowPlan {
            plan,
            warnings,
            overwrites,
        }),
        SaveFlowResult::ShowValidationErrors(errors) => {
            gui_event_tx.send(GuiEvent::ShowValidationErrors(errors))
        }
        SaveFlowResult::ShowReport(report) => gui_event_tx.send(GuiEvent::ShowReport(report)),
        SaveFlowResult::ReconcileFailed { report, error } => {
            gui_event_tx.send(GuiEvent::ShowReport(report))?;
            gui_event_tx.send(GuiEvent::FatalError(format!(
                "実行後の再読込に失敗しました。安全のため編集を停止します: {error}"
            )))
        }
        SaveFlowResult::ExternalChanged => Ok(()),
        SaveFlowResult::ExternalChangeNotified(text) => {
            gui_event_tx.send(GuiEvent::Editor(EditorEvent::Message(EditorMessage {
                kind: MessageKind::Info,
                text,
            })))
        }
        SaveFlowResult::ExternalChangeFailed(text) => {
            gui_event_tx.send(GuiEvent::Editor(EditorEvent::Message(EditorMessage {
                kind: MessageKind::Error,
                text,
            })))
        }
        SaveFlowResult::PlanInvalidated(text) => {
            gui_event_tx.send(GuiEvent::CloseDialog)?;
            gui_event_tx.send(GuiEvent::Editor(EditorEvent::Message(EditorMessage {
                kind: MessageKind::Warn,
                text,
            })))
        }
        SaveFlowResult::NoChanges => {
            gui_event_tx.send(GuiEvent::Editor(EditorEvent::Message(EditorMessage {
                kind: MessageKind::Info,
                text: "変更はありません".to_owned(),
            })))
        }
        SaveFlowResult::Cancelled => gui_event_tx.send(GuiEvent::CloseDialog),
        SaveFlowResult::Ignored => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_root_removes_joined_current_directory_component() {
        let current_dir = std::env::current_dir().unwrap();

        assert_eq!(normalize_root(&current_dir.join(".")).unwrap(), current_dir);
    }

    #[test]
    fn resolve_cd_target_keeps_absolute_paths() {
        let absolute = std::env::current_dir().unwrap().join("absolute-target");

        assert_eq!(
            resolve_cd_target(&absolute.to_string_lossy(), Path::new("ignored"), None),
            Some(absolute)
        );
    }

    #[test]
    fn resolve_cd_target_expands_home_paths() {
        let home = Path::new("home");

        assert_eq!(
            resolve_cd_target("~", Path::new("root"), Some(home)),
            Some(PathBuf::from("home"))
        );
        assert_eq!(
            resolve_cd_target("~/sub", Path::new("root"), Some(home)),
            Some(PathBuf::from("home").join("sub"))
        );
        assert_eq!(resolve_cd_target("~", Path::new("root"), None), None);
        assert_eq!(
            resolve_cd_target("~user", Path::new("root"), Some(home)),
            None
        );
    }

    #[test]
    fn resolve_cd_target_joins_relative_paths_to_root_without_normalizing() {
        let root = Path::new("root").join("current");

        assert_eq!(resolve_cd_target("..", &root, None), Some(root.join("..")));
        assert_eq!(
            resolve_cd_target("sub/dir", &root, None),
            Some(root.join("sub").join("dir"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolve_cd_target_recognizes_windows_absolute_paths() {
        assert_eq!(
            resolve_cd_target(r"C:\x", Path::new(r"D:\root"), None),
            Some(PathBuf::from(r"C:\x"))
        );
    }

    #[test]
    fn git_refresher_coalesces_inflight_requests_to_latest_root() {
        let (event_tx, _event_rx) = mpsc::channel();
        let mut git = GitRefresher::new(event_tx);
        let first = PathBuf::from("first");
        let second = PathBuf::from("second");
        let latest = PathBuf::from("latest");

        assert_eq!(git.prepare_request(first.clone()), Some(first));
        assert!(git.inflight);
        assert_eq!(git.prepare_request(second), None);
        assert_eq!(git.prepare_request(latest.clone()), None);
        assert_eq!(git.queued, Some(latest.clone()));

        assert_eq!(git.on_finished(), Some(latest.clone()));
        assert!(!git.inflight);
        assert_eq!(git.prepare_request(latest.clone()), Some(latest));
        assert!(git.inflight);
        assert_eq!(git.on_finished(), None);
        assert!(!git.inflight);
    }

    #[test]
    fn bookmark_resolution_prefers_exact_then_unique_prefix_then_recent_index() {
        let bookmarks = vec![
            ("project".to_owned(), PathBuf::from("/bookmark/project")),
            ("profile".to_owned(), PathBuf::from("/bookmark/profile")),
            ("docs".to_owned(), PathBuf::from("/bookmark/docs")),
            ("1".to_owned(), PathBuf::from("/bookmark/numeric")),
        ];
        let recent = vec![PathBuf::from("/recent/one"), PathBuf::from("/recent/two")];

        assert_eq!(
            resolve_bookmark_query("1", &bookmarks, &recent),
            BookmarkResolution::Resolved(PathBuf::from("/bookmark/numeric"))
        );
        assert_eq!(
            resolve_bookmark_query("doc", &bookmarks, &recent),
            BookmarkResolution::Resolved(PathBuf::from("/bookmark/docs"))
        );
        assert_eq!(
            resolve_bookmark_query("2", &bookmarks, &recent),
            BookmarkResolution::Resolved(PathBuf::from("/recent/two"))
        );
        assert_eq!(
            resolve_bookmark_query("pro", &bookmarks, &recent),
            BookmarkResolution::Ambiguous(vec!["project".to_owned(), "profile".to_owned()])
        );
        assert_eq!(
            resolve_bookmark_query("missing", &bookmarks, &recent),
            BookmarkResolution::NotFound
        );
    }
}
