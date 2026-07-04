//! fyler — エントリポイント。各レイヤーの配線だけを行う(ロジックを書かない)。
//!
//! 各レイヤーの役割はAGENTS.mdの依存境界表を参照。ここに書いてよいのは
//! 「起動」「イベントの受け渡し」「ユーザー操作の各レイヤーへの配線」
//! 「保存状態機械の副作用(SaveEffect)の実行」のみ。

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
use fyler_fsops::watch::ExternalChange;
use fyler_gui::app::GuiEvent;
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
    let baseline = fyler_fsops::scan::scan_baseline(&root, &mut ids)?;
    let save_engine: Arc<dyn EditorEngine> = engine.clone();
    let mut save_controller =
        SaveController::new(root.clone(), ids, baseline, Arc::clone(&save_engine));
    save_controller.collapse_all_top_level();
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
    let app_engine = Arc::clone(&save_engine);
    let event_bridge = thread::Builder::new()
        .name("fyler-app-events".to_owned())
        .spawn(move || {
            let mut root = root;
            let mut _watcher = watcher;
            let mut pending_events = VecDeque::new();
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
                    AppEvent::Editor(EditorEvent::NavigateParent) => {
                        if app_engine.snapshot().dirty {
                            if send_gui_message(
                                &gui_event_tx,
                                MessageKind::Info,
                                "編集中です。保存または破棄してからディレクトリを移動してください",
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

                        let mut new_ids = IdAllocator::new();
                        let scan_options = save_controller.scan_options();
                        let new_baseline = match fyler_fsops::scan::scan_baseline_with(
                            &new_root,
                            &mut new_ids,
                            &scan_options,
                        ) {
                                Ok(baseline) => baseline,
                                Err(error) => {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        MessageKind::Error,
                                        format!("上のディレクトリを読み込めません: {error:#}"),
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }
                            };

                        // 新しい監視の作成に失敗した場合、現在のroot/baseline/watcherを
                        // そのまま維持できるよう、状態差し替え前に準備だけ済ませる。
                        let new_watcher =
                            match fyler_fsops::watch::watch(&new_root, watch_tx.clone()) {
                                Ok(watcher) => watcher,
                                Err(error) => {
                                    if send_gui_message(
                                        &gui_event_tx,
                                        MessageKind::Error,
                                        format!("上のディレクトリを監視できません: {error:#}"),
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }
                            };

                        if let Err(error) =
                            save_controller.change_root(new_root.clone(), new_ids, new_baseline)
                        {
                            if send_gui_message(
                                &gui_event_tx,
                                MessageKind::Error,
                                format!("表示ルートを変更できません: {error:#}"),
                            )
                            .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                        save_controller.collapse_all_top_level();
                        let new_lines = save_controller.visible_lines();

                        root = new_root;
                        _watcher = new_watcher;
                        if let Err(error) = app_engine.send(EditorCommand::SetLines {
                            lines: new_lines,
                            cursor_line: None,
                        }) {
                            if send_gui_message(
                                &gui_event_tx,
                                MessageKind::Error,
                                format!("上のディレクトリを表示できません: {error:#}"),
                            )
                            .is_err()
                            {
                                return;
                            }
                        }
                        if gui_event_tx
                            .send(GuiEvent::RootChanged(root.clone()))
                            .is_err()
                        {
                            return;
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
                    }
                    AppEvent::Editor(event) => {
                        if gui_event_tx.send(GuiEvent::Editor(event)).is_err() {
                            return;
                        }
                    }
                    AppEvent::Confirm(choice) => {
                        let result = save_controller.on_choice(choice);
                        if send_save_result(&gui_event_tx, result).is_err() {
                            return;
                        }
                    }
                    AppEvent::ExternalChange(change) => {
                        let _changed_path = change.path;
                        while let Ok(queued_event) = app_event_rx.try_recv() {
                            match queued_event {
                                AppEvent::ExternalChange(change) => {
                                    let _changed_path = change.path;
                                }
                                event => pending_events.push_back(event),
                            }
                        }

                        let result = save_controller.on_external_change();
                        if !matches!(&result, SaveFlowResult::NoChanges)
                            && send_save_result(&gui_event_tx, result).is_err()
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
    let gui_result = fyler_gui::app::run(gui_engine, gui_event_rx, confirm_tx);
    let _ = app_event_tx.send(AppEvent::Shutdown);
    let _ = watch_bridge.join();
    let _ = event_bridge.join();
    let _ = confirm_bridge.join();
    let _ = editor_bridge.join();
    gui_result
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
