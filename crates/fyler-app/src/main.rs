//! fyler — エントリポイント。各レイヤーの配線だけを行う(ロジックを書かない)。
//!
//! 各レイヤーの役割はAGENTS.mdの依存境界表を参照。ここに書いてよいのは
//! 「起動」「イベントの受け渡し」「ユーザー操作の各レイヤーへの配線」
//! 「保存状態機械の副作用(SaveEffect)の実行」のみ。

mod config;
mod save_flow;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;

use fyler_core::editor::{EditorCommand, EditorEngine, EditorEvent, EditorMessage, MessageKind};
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
    Shutdown,
}

fn main() -> anyhow::Result<()> {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let root = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()?.join(root)
    };
    let (config, config_warnings) = config::load();
    let scan_options = ScanOptions {
        show_hidden: config.show_hidden,
        sort: config.sort,
    };
    let gui_options = GuiOptions {
        confirm_detail: config.confirm_detail,
        font_path: config.font,
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
    send_decorations(&gui_event_tx, &save_controller)?;
    if !config_warnings.is_empty() {
        send_gui_message(
            &gui_event_tx,
            MessageKind::Warn,
            format!("設定: {}", config_warnings.join(" / ")),
        )?;
    }
    let app_engine = Arc::clone(&save_engine);
    let event_bridge = thread::Builder::new()
        .name("fyler-app-events".to_owned())
        .spawn(move || {
            let mut root = root;
            let mut _watcher = watcher;
            let mut pending_events = VecDeque::new();
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
                        let Some(new_root) = root.parent().map(Path::to_path_buf) else {
                            if send_gui_message(
                                &gui_event_tx,
                                MessageKind::Info,
                                "これ以上、上のディレクトリはありません",
                            )
                            .is_err()
                            {
                                return;
                            }
                            continue;
                        };

                        if change_root_to(
                            new_root,
                            &mut root,
                            &mut _watcher,
                            &watch_tx,
                            &mut save_controller,
                            app_engine.as_ref(),
                            &gui_event_tx,
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
                                if change_root_to(
                                    new_root,
                                    &mut root,
                                    &mut _watcher,
                                    &watch_tx,
                                    &mut save_controller,
                                    app_engine.as_ref(),
                                    &gui_event_tx,
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
                        if send_decorations(&gui_event_tx, &save_controller).is_err() {
                            return;
                        }
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
                        if refresh_decorations
                            && send_decorations(&gui_event_tx, &save_controller).is_err()
                        {
                            return;
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
                        // M7では全再スキャンするため、集合は将来の部分再スキャンまで使わない。
                        drop(changed_paths);

                        let result = save_controller.on_external_change();
                        if !matches!(&result, SaveFlowResult::NoChanges)
                            && send_save_result(&gui_event_tx, result).is_err()
                        {
                            return;
                        }
                        if send_decorations(&gui_event_tx, &save_controller).is_err() {
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

fn change_root_to(
    new_root: PathBuf,
    root: &mut PathBuf,
    watcher: &mut FsWatcher,
    watch_tx: &mpsc::Sender<ExternalChange>,
    save_controller: &mut SaveController,
    engine: &dyn EditorEngine,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    if engine.snapshot().dirty {
        return send_gui_message(
            gui_event_tx,
            MessageKind::Info,
            "編集中です。保存または破棄してからディレクトリを移動してください",
        );
    }
    if !save_controller.is_idle() {
        return Ok(());
    }

    let mut new_ids = IdAllocator::new();
    let scan_options = save_controller.scan_options();
    let new_baseline =
        match fyler_fsops::scan::scan_baseline_with(&new_root, &mut new_ids, &scan_options) {
            Ok(baseline) => baseline,
            Err(error) => {
                return send_gui_message(
                    gui_event_tx,
                    MessageKind::Error,
                    format!(
                        "表示ルートを読み込めません ({}): {error:#}",
                        new_root.display()
                    ),
                );
            }
        };

    // 新しい監視の作成に失敗した場合、現在のroot/baseline/watcherを
    // そのまま維持できるよう、状態差し替え前に準備だけ済ませる。
    let new_watcher = match fyler_fsops::watch::watch(&new_root, watch_tx.clone()) {
        Ok(watcher) => watcher,
        Err(error) => {
            return send_gui_message(
                gui_event_tx,
                MessageKind::Error,
                format!(
                    "表示ルートを監視できません ({}): {error:#}",
                    new_root.display()
                ),
            );
        }
    };

    if let Err(error) = save_controller.change_root(new_root.clone(), new_ids, new_baseline) {
        return send_gui_message(
            gui_event_tx,
            MessageKind::Error,
            format!("表示ルートを変更できません: {error:#}"),
        );
    }
    save_controller.collapse_all_dirs();
    let new_lines = save_controller.visible_lines();

    *root = new_root;
    *watcher = new_watcher;
    if let Err(error) = engine.send(EditorCommand::SetLines {
        lines: new_lines,
        cursor_line: None,
    }) {
        send_gui_message(
            gui_event_tx,
            MessageKind::Error,
            format!("新しいディレクトリを表示できません: {error:#}"),
        )?;
    }
    gui_event_tx.send(GuiEvent::RootChanged(root.clone()))?;
    send_decorations(gui_event_tx, save_controller)?;
    if let Err(error) = config::record_recent_root(root) {
        send_gui_message(
            gui_event_tx,
            MessageKind::Warn,
            format!("最近使ったルートを記録できません: {error:#}"),
        )?;
    }
    Ok(())
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

/// 現在のbaselineに対応するGit装飾と、表示中エントリのファイル情報を再計算して送る。
///
/// Gitが利用できない場合とリポジトリ外では、空のmapを送って既存装飾を消す。
/// ファイル情報の取得に失敗したエントリは送信するmapに含めない。
fn send_decorations(
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    save_controller: &SaveController,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    gui_event_tx.send(GuiEvent::GitBadges(save_controller.git_badges()))?;
    gui_event_tx.send(GuiEvent::FileInfos(save_controller.visible_file_infos()))
}

fn send_save_result(
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    result: SaveFlowResult,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    match result {
        SaveFlowResult::ShowPlan { plan, warnings } => {
            gui_event_tx.send(GuiEvent::ShowPlan { plan, warnings })
        }
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
