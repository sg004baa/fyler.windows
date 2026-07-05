use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use fyler_core::editor::{
    EditorCommand, EditorEngine, EditorEvent, EditorLine, Key, KeyInput, Mode, Modifiers,
};
use fyler_engine_nvim::{NvimConfig, NvimEngine};
use tokio::sync::mpsc::UnboundedReceiver;

// 実nvimを起動する統合テストは重い共有リソースなので直列化する
// (並列だと2つのnvim起動が競合しpoll timeoutでフラキーになる)。
static NVIM_TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn spawn_attach_and_edit_updates_snapshot() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig { nvim_exe, root }).await?;

    assert!(engine.snapshot().revision >= 1);
    engine.set_initial_lines(vec![EditorLine::new("/001 alpha")])?;
    wait_for(&engine, |line| line == "/001 alpha").await?;

    engine.send(key_command(Key::Char('v')))?;
    wait_for_mode(&engine, Mode::Visual).await?;
    let visual_snapshot = engine.snapshot();
    assert_eq!(visual_snapshot.visual_start, Some(visual_snapshot.cursor));
    engine.send(key_command(Key::Esc))?;
    wait_for_mode(&engine, Mode::Normal).await?;
    assert_eq!(engine.snapshot().visual_start, None);

    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::ActivateLine { line: 0 })
    })
    .await?;
    engine.send(key_command(Key::Char('^')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::NavigateParent)
    })
    .await?;
    engine.send(key_command(Key::Char('g')))?;
    engine.send(key_command(Key::Char('.')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::ToggleHidden)
    })
    .await
    .context("g. did not emit ToggleHidden")?;
    engine.send(key_command(Key::Char('g')))?;
    engine.send(key_command(Key::Char('y')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::YankPath { line: 0 })
    })
    .await
    .context("gy did not emit YankPath")?;

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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn character_waiting_commands_do_not_block_following_input() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig { nvim_exe, root }).await?;

    engine.set_initial_lines(vec![EditorLine::new("/001 alpha.txt")])?;
    wait_for(&engine, |line| line == "/001 alpha.txt").await?;

    engine.send(key_command(Key::Char('c')))?;
    engine.send(key_command(Key::Char('i')))?;
    engine.send(key_command(Key::Char('w')))?;
    wait_for_mode(&engine, Mode::Insert)
        .await
        .context("ciw did not reach Insert mode")?;

    engine.send(key_command(Key::Esc))?;
    wait_for_mode(&engine, Mode::Normal).await?;

    let revision = engine.snapshot().revision;
    engine.send(key_command(Key::Char('f')))?;
    engine.send(key_command(Key::Char('x')))?;
    wait_for_revision_after(&engine, revision)
        .await
        .context("fx did not complete after character wait")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn set_initial_lines_with_multiple_lines_has_no_duplication() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    // イベント受信端はエンジンのイベント送信が閉じないよう関数終了まで保持する。
    let (engine, _events) = NvimEngine::start(NvimConfig { nvim_exe, root }).await?;

    // 3つの異なる行を投入する。旧バグでは末尾2行が重複して5行に化けた。
    engine.set_initial_lines(vec![
        EditorLine::new("/001 a.txt"),
        EditorLine::new("/002 hoge.csv"),
        EditorLine::new("/003 test.txt"),
    ])?;

    // 件数と全行一致で安定するまで待つ(既存 wait_for は先頭行しか見ないため不十分)。
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = engine.snapshot();
            let lines = &snapshot.lines;
            if lines.len() == 3
                && lines[0].text == "/001 a.txt"
                && lines[1].text == "/002 hoge.csv"
                && lines[2].text == "/003 test.txt"
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("initial lines did not stabilize to 3 unique lines"))?;

    let snapshot = engine.snapshot();
    // 重複しないことの核心: 5行ではなく厳密に3行。
    assert_eq!(
        snapshot.lines.len(),
        3,
        "expected exactly 3 lines with no duplication, got {}: {:?}",
        snapshot.lines.len(),
        snapshot.lines
    );
    // 各行が投入順どおりの内容であること。
    assert_eq!(snapshot.lines[0].text, "/001 a.txt");
    assert_eq!(snapshot.lines[1].text, "/002 hoge.csv");
    assert_eq!(snapshot.lines[2].text, "/003 test.txt");

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn modifiable_blocks_normal_mode_edits_until_reenabled() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig { nvim_exe, root }).await?;

    engine.send(EditorCommand::SetLines {
        lines: vec![EditorLine::new("/001 alpha")],
        cursor_line: None,
    })?;
    wait_for(&engine, |line| line == "/001 alpha").await?;

    let revision = engine.snapshot().revision;
    engine.send(EditorCommand::SetModifiable(false))?;
    wait_for_revision_after(&engine, revision).await?;
    let revision = engine.snapshot().revision;
    engine.send(key_command(Key::Char('x')))?;
    wait_for_revision_after(&engine, revision).await?;
    assert_eq!(engine.snapshot().lines[0].text, "/001 alpha");

    let revision = engine.snapshot().revision;
    engine.send(EditorCommand::SetModifiable(true))?;
    wait_for_revision_after(&engine, revision).await?;
    engine.send(key_command(Key::Char('x')))?;
    wait_for(&engine, |line| line == "/001 lpha").await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn bookmark_command_emits_jump_request() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig { nvim_exe, root }).await?;

    engine.send(key_command(Key::Char(':')))?;
    engine.send(EditorCommand::Text("FylerBookmark x".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(
            event,
            EditorEvent::JumpBookmark {
                query: Some(query)
            } if query == "x"
        )
    })
    .await
    .context(":FylerBookmark x did not emit JumpBookmark")?;

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

async fn wait_for_revision_after(engine: &NvimEngine, revision: u64) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if engine.snapshot().revision > revision {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("snapshot revision update timed out"))
}

async fn wait_for_mode(engine: &NvimEngine, expected: Mode) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if engine.snapshot().mode == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("snapshot mode did not become {expected:?}"))
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
