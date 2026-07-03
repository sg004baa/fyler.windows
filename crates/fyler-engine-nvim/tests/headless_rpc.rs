use std::path::PathBuf;
use std::time::Duration;

use fyler_core::editor::{
    EditorCommand, EditorEngine, EditorEvent, EditorLine, Key, KeyInput, Modifiers,
};
use fyler_engine_nvim::{NvimConfig, NvimEngine};
use tokio::sync::mpsc::UnboundedReceiver;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn spawn_attach_and_edit_updates_snapshot() -> anyhow::Result<()> {
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig { nvim_exe, root }).await?;

    assert!(engine.snapshot().revision >= 1);
    engine.set_initial_lines(vec![EditorLine::new("/001 alpha")])?;
    wait_for(&engine, |line| line == "/001 alpha").await?;

    engine.send(EditorCommand::Key(KeyInput {
        key: Key::Char('i'),
        mods: Modifiers::default(),
    }))?;
    engine.send(EditorCommand::Text("X".to_owned()))?;
    wait_for(&engine, |line| line == "/001 Xalpha").await?;
    engine.send(key_command(Key::Esc))?;

    engine.send(EditorCommand::RequestCommit)?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CommitRequested { .. })
    })
    .await?;

    engine.send(key_command(Key::Char(':')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineShow(_))
    })
    .await?;
    engine.send(key_command(Key::Esc))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineHide)
    })
    .await?;

    engine.send(key_command(Key::Char(':')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineShow(_))
    })
    .await?;
    engine.send(EditorCommand::Text("qa!".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::EngineCrashed { .. })
    })
    .await?;

    Ok(())
}

fn key_command(key: Key) -> EditorCommand {
    EditorCommand::Key(KeyInput {
        key,
        mods: Modifiers::default(),
    })
}

async fn wait_for(engine: &NvimEngine, predicate: impl Fn(&str) -> bool) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = engine.snapshot();
            if snapshot
                .lines
                .first()
                .is_some_and(|line| predicate(&line.text))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("snapshot update timed out"))
}

async fn wait_for_event(
    events: &mut UnboundedReceiver<EditorEvent>,
    predicate: impl Fn(&EditorEvent) -> bool,
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if predicate(&event) {
                return Ok(());
            }
        }
        anyhow::bail!("editor event channel closed")
    })
    .await
    .map_err(|_| anyhow::anyhow!("editor event timed out"))?
}
