//! fyler — エントリポイント。各レイヤーの配線だけを行う(ロジックを書かない)。
//!
//! 各レイヤーの役割はAGENTS.mdの依存境界表を参照。ここに書いてよいのは
//! 「起動」「イベントの受け渡し」「保存状態機械の副作用(SaveEffect)の実行」のみ。

mod save_flow;

use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;

use fyler_core::editor::{EditorEngine, EditorEvent, EditorLine, EditorMessage, MessageKind};
use fyler_core::id::IdAllocator;
use fyler_core::tree::EntryKind;
use fyler_engine_nvim::{NvimConfig, NvimEngine};
use fyler_gui::app::GuiEvent;
use fyler_gui::confirm::ConfirmChoice;

use crate::save_flow::{SaveController, SaveFlowResult};

enum AppEvent {
    Editor(EditorEvent),
    Confirm(ConfirmChoice),
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

    let mut ids = IdAllocator::new();
    let baseline = fyler_fsops::scan::scan_baseline(&root, &mut ids)?;
    let initial_lines = baseline
        .entries
        .iter()
        .map(|entry| {
            let indent = " "
                .repeat(entry.path.depth().saturating_sub(1) * fyler_core::grammar::INDENT_WIDTH);
            let directory_suffix = if entry.kind == EntryKind::Dir {
                fyler_core::grammar::DIR_SUFFIX.to_string()
            } else {
                String::new()
            };
            EditorLine::new(format!(
                "{}{}{}{}",
                fyler_core::grammar::format_id_prefix(entry.id),
                indent,
                entry.path.name().unwrap_or_default(),
                directory_suffix,
            ))
        })
        .collect();
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

    // GUIクレートへtokio型を漏らさず、core型とConfirmChoiceだけを受け渡す。
    let (gui_event_tx, gui_event_rx) = mpsc::channel();
    let controller_engine: Arc<dyn EditorEngine> = engine.clone();
    let event_bridge = thread::Builder::new()
        .name("fyler-app-events".to_owned())
        .spawn(move || {
            let mut save_controller = SaveController::new(baseline);
            while let Ok(event) = app_event_rx.recv() {
                match event {
                    AppEvent::Editor(EditorEvent::CommitRequested { changedtick }) => {
                        let snapshot = controller_engine.snapshot();
                        let result = save_controller.on_commit(changedtick, &snapshot.lines);
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
                    AppEvent::Shutdown => return,
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("GUIイベント配線を開始できません: {error}"))?;

    let gui_engine: Arc<dyn EditorEngine> = engine;
    let gui_result = fyler_gui::app::run(gui_engine, gui_event_rx, confirm_tx);
    let _ = app_event_tx.send(AppEvent::Shutdown);
    let _ = event_bridge.join();
    let _ = confirm_bridge.join();
    let _ = editor_bridge.join();
    gui_result

    // M5で追加する配線:
    // - fyler_fsops::watch → 再スキャン・再描画(dirty中は通知のみ)
    // - アプリmanifestに longPathAware を入れる(ビルド設定)
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
        SaveFlowResult::NoChanges => {
            gui_event_tx.send(GuiEvent::Editor(EditorEvent::Message(EditorMessage {
                kind: MessageKind::Info,
                text: "変更はありません".to_owned(),
            })))
        }
        SaveFlowResult::Cancelled => gui_event_tx.send(GuiEvent::CloseDialog),
        SaveFlowResult::ApprovedDryRun => {
            gui_event_tx.send(GuiEvent::CloseDialog)?;
            gui_event_tx.send(GuiEvent::Editor(EditorEvent::Message(EditorMessage {
                kind: MessageKind::Info,
                text: "承認されました (実行はM3で実装)".to_owned(),
            })))
        }
        SaveFlowResult::Ignored => Ok(()),
    }
}
