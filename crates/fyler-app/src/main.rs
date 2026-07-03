//! fyler — エントリポイント。各レイヤーの配線だけを行う(ロジックを書かない)。
//!
//! 各レイヤーの役割はAGENTS.mdの依存境界表を参照。ここに書いてよいのは
//! 「起動」「イベントの受け渡し」「保存状態機械の副作用(SaveEffect)の実行」のみ。

use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;

use fyler_core::editor::{EditorEngine, EditorLine};
use fyler_core::id::IdAllocator;
use fyler_core::tree::EntryKind;
use fyler_engine_nvim::{NvimConfig, NvimEngine};

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

    // GUIクレートへtokio型を漏らさず、coreのEditorEventだけを受け渡す。
    let (gui_event_tx, gui_event_rx) = mpsc::channel();
    let event_bridge = thread::Builder::new()
        .name("fyler-app-events".to_owned())
        .spawn(move || {
            while let Some(event) = engine_events.blocking_recv() {
                if gui_event_tx.send(event).is_err() {
                    return;
                }
            }
        })
        .map_err(|error| anyhow::anyhow!("GUIイベント配線を開始できません: {error}"))?;

    let gui_engine: Arc<dyn EditorEngine> = engine;
    let gui_result = fyler_gui::app::run(gui_engine, gui_event_rx);
    let _ = event_bridge.join();
    gui_result

    // M2で追加する配線:
    // - EditorEvent::CommitRequested → fyler_core::save::transition →
    //   SaveEffectの実行(RunPipeline = fyler_pipeline::{parse,validate,diff}、
    //   ExecutePlan = fyler_fsops::apply::apply_plan ※M3から。M2はdry-runのみ)
    //
    // M5で追加する配線:
    // - fyler_fsops::watch → 再スキャン・再描画(dirty中は通知のみ)
    // - アプリmanifestに longPathAware を入れる(ビルド設定)
}
