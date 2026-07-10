use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use fyler_core::editor::{
    EditorCommand, EditorEngine, EditorEvent, EditorLine, Key, KeyInput, Mode, Modifiers,
    SearchHighlight,
};
use fyler_core::pane::PaneAction;
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
async fn pane_keymap_emits_split_action() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig { nvim_exe, root }).await?;

    engine.send(EditorCommand::Key(KeyInput {
        key: Key::Char('w'),
        mods: Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    }))?;
    engine.send(key_command(Key::Char('s')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::PaneAction(PaneAction::SplitHorizontal))
    })
    .await
    .context("pane split key did not emit PaneAction")?;

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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn navigate_into_and_cd_commands_emit_root_change_requests() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig { nvim_exe, root }).await?;

    engine.set_initial_lines(vec![EditorLine::new("/001 directory/")])?;
    wait_for(&engine, |line| line == "/001 directory/").await?;
    engine.send(key_command(Key::Char('g')))?;
    engine.send(key_command(Key::Char('d')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::NavigateInto { line: 0 })
    })
    .await
    .context("gd did not emit NavigateInto")?;

    engine.send(key_command(Key::Char(':')))?;
    engine.send(EditorCommand::Text("FylerCd ../target".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(
            event,
            EditorEvent::ChangeDirectory {
                query: Some(query)
            } if query == "../target"
        )
    })
    .await
    .context(":FylerCd ../target did not emit ChangeDirectory")?;

    engine.send(key_command(Key::Char(':')))?;
    engine.send(EditorCommand::Text("FylerCd".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::ChangeDirectory { query: None })
    })
    .await
    .context(":FylerCd did not emit ChangeDirectory without a query")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn search_state_surfaces_smartcase_and_hlsearch() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig { nvim_exe, root }).await?;

    engine.set_initial_lines(vec![
        EditorLine::new("/001 alpha"),
        EditorLine::new("/002 Alpha"),
    ])?;
    wait_for(&engine, |line| line == "/001 alpha").await?;

    // 1. all-lowercase pattern: smartcase + ignorecase => case-insensitive.
    engine.send(key_command(Key::Char('/')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineShow(_))
    })
    .await
    .context("/ did not open the search cmdline")?;
    engine.send(EditorCommand::Text("alpha".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_search(&engine, |search| {
        search.is_some_and(|s| s.pattern == "alpha" && !s.case_sensitive)
    })
    .await
    .context("lowercase search did not surface as case-insensitive")?;
    let lower_snapshot = engine.snapshot();
    let lower = lower_snapshot
        .search
        .as_ref()
        .context("search should be Some after /alpha")?;
    assert_eq!(lower.pattern, "alpha");
    assert!(
        !lower.case_sensitive,
        "all-lowercase pattern with smartcase+ignorecase must be case-insensitive"
    );

    // 2. pattern with an uppercase char: smartcase => case-sensitive.
    engine.send(key_command(Key::Char('/')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineShow(_))
    })
    .await
    .context("/ did not open the search cmdline for uppercase search")?;
    engine.send(EditorCommand::Text("Alpha".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_search(&engine, |search| {
        search.is_some_and(|s| s.pattern == "Alpha" && s.case_sensitive)
    })
    .await
    .context("uppercase search did not surface as case-sensitive")?;
    let upper_snapshot = engine.snapshot();
    let upper = upper_snapshot
        .search
        .as_ref()
        .context("search should be Some after /Alpha")?;
    assert_eq!(upper.pattern, "Alpha");
    assert!(
        upper.case_sensitive,
        "pattern containing an uppercase char with smartcase must be case-sensitive"
    );

    // 3. `:noh` clears v:hlsearch, so the snapshot exposes no highlight.
    engine.send(key_command(Key::Char(':')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineShow(_))
    })
    .await
    .context(": did not open the command cmdline for :noh")?;
    engine.send(EditorCommand::Text("noh".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_search(&engine, |search| search.is_none())
        .await
        .context(":noh did not clear the surfaced search highlight")?;
    assert!(
        engine.snapshot().search.is_none(),
        "after :noh (v:hlsearch cleared) search must be None"
    );

    // 4. Teardown.
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

async fn wait_for_search(
    engine: &NvimEngine,
    predicate: impl Fn(Option<&SearchHighlight>) -> bool,
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if predicate(engine.snapshot().search.as_ref()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("snapshot search state did not match predicate"))
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
