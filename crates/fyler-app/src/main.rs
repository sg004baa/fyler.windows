//! fyler — エントリポイント。各レイヤーの配線だけを行う(ロジックを書かない)。
//!
//! 各レイヤーの役割はAGENTS.mdの依存境界表を参照。ここに書いてよいのは
//! 「起動」「イベントの受け渡し」「保存状態機械の副作用(SaveEffect)の実行」のみ。

mod save_flow;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;

use fyler_core::editor::{EditorEngine, EditorEvent, EditorMessage, MessageKind};
use fyler_core::id::IdAllocator;
use fyler_engine_nvim::{NvimConfig, NvimEngine};
use fyler_fsops::watch::ExternalChange;
use fyler_gui::app::GuiEvent;
use fyler_gui::confirm::ConfirmChoice;

use crate::save_flow::{SaveController, SaveFlowResult, baseline_to_lines};

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
    let watcher = fyler_fsops::watch::watch(&root, watch_tx)?;

    let mut ids = IdAllocator::new();
    let baseline = fyler_fsops::scan::scan_baseline(&root, &mut ids)?;
    let initial_lines = baseline_to_lines(&baseline);
    engine.set_initial_lines(initial_lines)?;

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
    let save_root = root.clone();
    let save_engine: Arc<dyn EditorEngine> = engine.clone();
    let event_bridge = thread::Builder::new()
        .name("fyler-app-events".to_owned())
        .spawn(move || {
            let mut save_controller = SaveController::new(save_root, ids, baseline, save_engine);
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
    drop(watcher);
    let _ = watch_bridge.join();
    let _ = event_bridge.join();
    let _ = confirm_bridge.join();
    let _ = editor_bridge.join();
    gui_result
}

fn send_save_result(
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    result: SaveFlowResult,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    match result {
        SaveFlowResult::ShowPlan(plan) => gui_event_tx.send(GuiEvent::ShowPlan(plan)),
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
