use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use fyler_core::editor::{
    EditorCommand, EditorEngine, EditorEvent, EditorLine, EditorMessage, FoldOp, Key, KeyInput,
    MessageKind, Mode, Modifiers, SearchHighlight,
};
use fyler_core::keymap::{default_leader, resolve_bindings};
use fyler_core::pane::PaneAction;
use fyler_core::transfer::TransferKind;
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
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

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
    engine.send(key_command(Key::Backspace))?;
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
    engine.send(key_command(Key::Char('g')))?;
    engine.send(key_command(Key::Char('o')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::OpenWith { line: 0 })
    })
    .await
    .context("go did not emit OpenWith")?;

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
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

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
async fn file_picker_keymap_emits_open_request() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.send(key_command(Key::Char('g')))?;
    engine.send(key_command(Key::Char('/')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::OpenFilePicker)
    })
    .await
    .context("g/ did not emit OpenFilePicker")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn dock_focus_keymap_emits_toggle_request() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.send(key_command(Key::Char(' ')))?;
    engine.send(key_command(Key::Char('e')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::ToggleDockFocus)
    })
    .await
    .context("<leader>e did not emit ToggleDockFocus")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn custom_leader_binding_fires_once_and_unmap_removes_default() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let space = KeyInput {
        key: Key::Char(' '),
        mods: Modifiers::default(),
    };
    let (bindings, warnings) = resolve_bindings(
        space,
        &[
            ("Leader f".into(), "file_picker".into()),
            ("g .".into(), "none".into()),
        ],
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut config = NvimConfig::new(nvim_exe, root);
    config.bindings = bindings;
    let (engine, mut events) = NvimEngine::start(config).await?;

    engine.send(key_command(Key::Char(' ')))?;
    engine.send(key_command(Key::Char('f')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::OpenFilePicker)
    })
    .await?;
    assert_no_event(&mut events, |event| {
        matches!(event, EditorEvent::OpenFilePicker)
    })
    .await?;

    engine.send(key_command(Key::Char('g')))?;
    engine.send(key_command(Key::Char('.')))?;
    assert_no_event(&mut events, |event| {
        matches!(event, EditorEvent::ToggleHidden)
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn custom_ctrl_w_trie_dispatches_and_blocks_unknown_keys() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (bindings, warnings) = resolve_bindings(
        default_leader(),
        &[("Ctrl+W x".into(), "pane_focus_next".into())],
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut config = NvimConfig::new(nvim_exe, root);
    config.bindings = bindings;
    let (engine, mut events) = NvimEngine::start(config).await?;

    let ctrl_w = EditorCommand::Key(KeyInput {
        key: Key::Char('w'),
        mods: Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    });
    engine.send(ctrl_w.clone())?;
    engine.send(key_command(Key::Char('x')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::PaneAction(PaneAction::FocusNext))
    })
    .await?;

    engine.send(ctrl_w)?;
    engine.send(key_command(Key::Char('z')))?;
    wait_for_event(&mut events, |event| {
        matches!(
            event,
            EditorEvent::Message(message)
                if message.kind == MessageKind::Info && message.text == "This key is not available"
        )
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn transfer_keymaps_emit_normal_and_visual_requests() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;
    engine.set_initial_lines(vec![
        EditorLine::new("/001 a.txt"),
        EditorLine::new("/002 b.txt"),
        EditorLine::new("/003 c.txt"),
    ])?;
    wait_for(&engine, |line| line == "/001 a.txt").await?;

    engine.send(key_command(Key::Char('g')))?;
    engine.send(key_command(Key::Char('m')))?;
    wait_for_event(&mut events, |event| {
        matches!(
            event,
            EditorEvent::TransferRequested {
                kind: TransferKind::Move,
                lines
            } if lines == &[0]
        )
    })
    .await
    .context("gm did not emit a move TransferRequested")?;

    engine.send(key_command(Key::Char('V')))?;
    engine.send(key_command(Key::Down))?;
    engine.send(key_command(Key::Char('g')))?;
    engine.send(key_command(Key::Char('c')))?;
    wait_for_event(&mut events, |event| {
        matches!(
            event,
            EditorEvent::TransferRequested {
                kind: TransferKind::Copy,
                lines
            } if lines == &[0, 1]
        )
    })
    .await
    .context("visual gc did not emit a ranged copy TransferRequested")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn clipboard_copy_and_cut_keymaps_emit_normal_and_visual_requests() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;
    engine.set_initial_lines(vec![
        EditorLine::new("/001 a.txt"),
        EditorLine::new("/002 b.txt"),
        EditorLine::new("/003 c.txt"),
    ])?;
    wait_for(&engine, |line| line == "/001 a.txt").await?;

    engine.send(EditorCommand::Key(KeyInput {
        key: Key::Char('c'),
        mods: Modifiers {
            ctrl: true,
            alt: false,
            shift: false,
        },
    }))?;
    wait_for_event(
        &mut events,
        |event| matches!(event, EditorEvent::ClipboardCopyRequested { lines } if lines == &[0]),
    )
    .await
    .context("Ctrl+C did not emit ClipboardCopyRequested")?;

    engine.send(key_command(Key::Char('V')))?;
    engine.send(key_command(Key::Down))?;
    engine.send(EditorCommand::Key(KeyInput {
        key: Key::Char('x'),
        mods: Modifiers {
            ctrl: true,
            alt: false,
            shift: false,
        },
    }))?;
    wait_for_event(
        &mut events,
        |event| matches!(event, EditorEvent::ClipboardCutRequested { lines } if lines == &[0, 1]),
    )
    .await
    .context("visual Ctrl+X did not emit a ranged ClipboardCutRequested")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn clipboard_paste_keymap_emits_request_at_cursor_line() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;
    engine.set_initial_lines(vec![
        EditorLine::new("/001 a.txt"),
        EditorLine::new("/002 b.txt"),
    ])?;
    wait_for(&engine, |line| line == "/001 a.txt").await?;

    engine.send(key_command(Key::Down))?;
    engine.send(EditorCommand::Key(KeyInput {
        key: Key::Char('v'),
        mods: Modifiers {
            ctrl: true,
            alt: false,
            shift: false,
        },
    }))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::ClipboardPasteRequested { line: 1 })
    })
    .await
    .context("Ctrl+V did not emit ClipboardPasteRequested at the cursor line")?;

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
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

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
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

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
                && lines[0].text.as_ref() == "/001 a.txt"
                && lines[1].text.as_ref() == "/002 hoge.csv"
                && lines[2].text.as_ref() == "/003 test.txt"
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
    assert_eq!(snapshot.lines[0].text.as_ref(), "/001 a.txt");
    assert_eq!(snapshot.lines[1].text.as_ref(), "/002 hoge.csv");
    assert_eq!(snapshot.lines[2].text.as_ref(), "/003 test.txt");

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
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

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
    assert_eq!(engine.snapshot().lines[0].text.as_ref(), "/001 alpha");

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
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

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
async fn undo_command_emits_undo_request() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.send(key_command(Key::Char(':')))?;
    engine.send(EditorCommand::Text("FylerUndo".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::UndoRequested)
    })
    .await
    .context(":FylerUndo did not emit UndoRequested")?;

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
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

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
async fn history_and_refresh_keymaps_and_commands_emit_expected_events() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    let ctrl_key = |character: char| {
        EditorCommand::Key(KeyInput {
            key: Key::Char(character),
            mods: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        })
    };

    engine.send(ctrl_key('p'))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::HistoryBack)
    })
    .await
    .context("Ctrl+P did not emit HistoryBack")?;

    engine.send(ctrl_key('n'))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::HistoryForward)
    })
    .await
    .context("Ctrl+N did not emit HistoryForward")?;

    engine.send(ctrl_key('r'))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::RefreshRequested)
    })
    .await
    .context("Ctrl+R did not emit RefreshRequested")?;

    engine.send(key_command(Key::Char(':')))?;
    engine.send(EditorCommand::Text("FylerBack".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::HistoryBack)
    })
    .await
    .context(":FylerBack did not emit HistoryBack")?;

    engine.send(key_command(Key::Char(':')))?;
    engine.send(EditorCommand::Text("FylerForward".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::HistoryForward)
    })
    .await
    .context(":FylerForward did not emit HistoryForward")?;

    engine.send(key_command(Key::Char(':')))?;
    engine.send(EditorCommand::Text("FylerReload".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::RefreshRequested)
    })
    .await
    .context(":FylerReload did not emit RefreshRequested")?;

    // command_aliases: :back / :forward / :reload の書き換え(<CR>直前のrewrite_command_alias)。
    engine.send(key_command(Key::Char(':')))?;
    engine.send(EditorCommand::Text("back".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::HistoryBack)
    })
    .await
    .context(":back alias did not emit HistoryBack")?;

    engine.send(key_command(Key::Char(':')))?;
    engine.send(EditorCommand::Text("forward".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::HistoryForward)
    })
    .await
    .context(":forward alias did not emit HistoryForward")?;

    engine.send(key_command(Key::Char(':')))?;
    engine.send(EditorCommand::Text("reload".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::RefreshRequested)
    })
    .await
    .context(":reload alias did not emit RefreshRequested")?;

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
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

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

async fn assert_no_event(
    events: &mut UnboundedReceiver<EditorEvent>,
    predicate: impl Fn(&EditorEvent) -> bool,
) -> anyhow::Result<()> {
    let result = tokio::time::timeout(Duration::from_millis(300), async {
        while let Some(event) = events.recv().await {
            if predicate(&event) {
                anyhow::bail!("unexpected editor event: {event:?}");
            }
        }
        Ok(())
    })
    .await;
    match result {
        Err(_) => Ok(()),
        Ok(result) => result,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn zc_emits_fold_close_without_native_fold_error() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.set_initial_lines(vec![
        EditorLine::new("/001 directory/"),
        EditorLine::new("/002   child.txt"),
    ])?;
    wait_for(&engine, |line| line == "/001 directory/").await?;

    engine.send(key_command(Key::Char('z')))?;
    engine.send(key_command(Key::Char('c')))?;
    wait_for_event(&mut events, |event| {
        matches!(
            event,
            EditorEvent::Fold {
                op: FoldOp::Close,
                line: 0
            }
        )
    })
    .await
    .context("zc did not emit Fold::Close")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn shift_indents_after_id_prefix() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.set_initial_lines(vec![
        EditorLine::new("/001 dir/"),
        EditorLine::new("/002 \tchild.txt"),
        EditorLine::new("/003 top.txt"),
    ])?;
    wait_for_lines(&engine, |lines| {
        lines.len() == 3 && lines[2].text.as_ref() == "/003 top.txt"
    })
    .await?;

    engine.send(key_command(Key::Down))?;
    engine.send(key_command(Key::Down))?;
    wait_for_cursor(&engine, |line, _| line == 2).await?;

    engine.send(key_command(Key::Char('>')))?;
    engine.send(key_command(Key::Char('>')))?;
    wait_for_lines(&engine, |lines| {
        lines.len() == 3 && lines[2].text.as_ref() == "/003 \ttop.txt"
    })
    .await?;

    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[2].text.as_ref(), "/003 \ttop.txt");
    assert!(
        !snapshot.lines[2].text.starts_with('\t'),
        "the tab must be inserted after the id prefix, not before it"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn shift_dedent_at_depth_zero_is_noop() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.set_initial_lines(vec![EditorLine::new("/001 top.txt")])?;
    wait_for_lines(&engine, |lines| {
        lines.len() == 1 && lines[0].text.as_ref() == "/001 top.txt"
    })
    .await?;

    let revision = engine.snapshot().revision;
    engine.send(key_command(Key::Char('<')))?;
    engine.send(key_command(Key::Char('<')))?;
    wait_for_revision_after(&engine, revision).await?;

    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines.len(), 1);
    assert_eq!(snapshot.lines[0].text.as_ref(), "/001 top.txt");

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn cursor_clamps_to_name_start() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.set_initial_lines(vec![EditorLine::new("/001 \tname.txt")])?;
    wait_for_lines(&engine, |lines| {
        lines.len() == 1 && lines[0].text.as_ref() == "/001 \tname.txt"
    })
    .await?;

    engine.send(key_command(Key::End))?;
    wait_for_cursor(&engine, |_, col| col > 6).await?;
    engine.send(key_command(Key::Char('0')))?;
    wait_for_cursor(&engine, |line, col| line == 0 && col == 6).await?;

    let snapshot = engine.snapshot();
    assert_eq!(snapshot.cursor.line, 0);
    assert_eq!(snapshot.cursor.col, 6);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn sort_command_completion_emits_popupmenu() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.send(key_command(Key::Char(':')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineShow(_))
    })
    .await
    .context(": did not open the command line")?;
    engine.send(EditorCommand::Text("FylerSort ".to_owned()))?;
    engine.send(key_command(Key::Tab))?;
    wait_for_event(&mut events, |event| {
        matches!(
            event,
            EditorEvent::PopupmenuShow(state)
                if state.items.iter().any(|item| item.word == "date")
        )
    })
    .await
    .context(":FylerSort <Tab> did not emit popupmenu with date")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn sort_alias_with_argument_reaches_fyler_sort() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.send(key_command(Key::Char(':')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineShow(_))
    })
    .await
    .context(": did not open the command line")?;
    engine.send(EditorCommand::Text("sort date".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(
            event,
            EditorEvent::ChangeSort {
                query: Some(query)
            } if query == "date"
        )
    })
    .await
    .context(":sort date did not emit ChangeSort(\"date\")")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn terminal_alias_fires_open_terminal_event() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.set_initial_lines(vec![EditorLine::new("/001 alpha")])?;
    wait_for(&engine, |line| line == "/001 alpha").await?;
    engine.send(key_command(Key::Char(':')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineShow(_))
    })
    .await
    .context(": did not open the command line")?;
    engine.send(EditorCommand::Text("terminal".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::OpenTerminal { line: 0 })
    })
    .await
    .context(":terminal did not emit OpenTerminal")?;
    wait_for(&engine, |line| line == "/001 alpha").await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn feedback_alias_fires_feedback_requested_event() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.set_initial_lines(vec![EditorLine::new("/001 alpha")])?;
    wait_for(&engine, |line| line == "/001 alpha").await?;
    engine.send(key_command(Key::Char(':')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineShow(_))
    })
    .await
    .context(": did not open the command line")?;
    engine.send(EditorCommand::Text("feedback".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::FeedbackRequested)
    })
    .await
    .context(":feedback did not emit FeedbackRequested")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn terminal_alias_with_argument_warns() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.set_initial_lines(vec![EditorLine::new("/001 alpha")])?;
    wait_for(&engine, |line| line == "/001 alpha").await?;
    engine.send(key_command(Key::Char(':')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineShow(_))
    })
    .await
    .context(": did not open the command line")?;
    engine.send(EditorCommand::Text("terminal git log".to_owned()))?;
    engine.send(key_command(Key::Enter))?;
    let opened_before_warning = tokio::time::timeout(Duration::from_secs(5), async {
        let mut opened = false;
        while let Some(event) = events.recv().await {
            match event {
                EditorEvent::OpenTerminal { .. } => opened = true,
                EditorEvent::Message(message)
                    if message.kind == MessageKind::Warn
                        && message.text.contains("arguments are not supported") =>
                {
                    return Some(opened);
                }
                _ => {}
            }
        }
        None
    })
    .await
    .context(":terminal with arguments did not emit a warning")?
    .context("editor event channel closed before the warning")?;
    assert!(
        !opened_before_warning,
        "argument form must not emit OpenTerminal"
    );
    let opened = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            match events.recv().await {
                Some(EditorEvent::OpenTerminal { .. }) => return true,
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(!opened, "argument form must not emit OpenTerminal");
    wait_for(&engine, |line| line == "/001 alpha").await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn sort_alias_tab_completes_and_executes() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.send(key_command(Key::Char(':')))?;
    wait_for_event(&mut events, |event| {
        matches!(event, EditorEvent::CmdlineShow(_))
    })
    .await
    .context(": did not open the command line")?;
    engine.send(EditorCommand::Text("sort da".to_owned()))?;
    engine.send(key_command(Key::Tab))?;
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(
            event,
            EditorEvent::ChangeSort {
                query: Some(query)
            } if query == "date"
        )
    })
    .await
    .context(":sort da<Tab><CR> did not emit ChangeSort(\"date\")")?;

    Ok(())
}
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn failed_search_keeps_engine_responsive() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;
    engine.set_initial_lines(vec![
        EditorLine::new("/001 alpha"),
        EditorLine::new("/002 beta"),
    ])?;
    wait_for_lines(&engine, |lines| lines.len() == 2).await?;

    engine.send(key_command(Key::Char('/')))?;
    for ch in ['z', 'z', 'q', 'x'] {
        engine.send(key_command(Key::Char(ch)))?;
    }
    engine.send(key_command(Key::Enter))?;
    wait_for_event(&mut events, |event| {
        matches!(
            event,
            EditorEvent::Message(EditorMessage {
                kind: MessageKind::Error,
                ..
            })
        )
    })
    .await
    .context("failed search did not report E486")?;

    // フリーズしていなければ後続のカーソル移動がそのまま反映される。
    engine.send(key_command(Key::Char('j')))?;
    wait_for_cursor(&engine, |line, _| line == 1).await?;
    assert_eq!(engine.snapshot().mode, Mode::Normal);

    Ok(())
}
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn buffer_undo_report_is_not_surfaced_as_a_message() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, mut events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;
    engine.set_initial_lines(vec![EditorLine::new("/001 alpha")])?;
    wait_for_lines(&engine, |lines| lines.len() == 1).await?;

    // 行を追加してから undo すると nvim は "N changes; before #M ..." を返す。
    engine.send(key_command(Key::Char('o')))?;
    engine.send(EditorCommand::Text("x".to_owned()))?;
    engine.send(key_command(Key::Esc))?;
    engine.send(key_command(Key::Char('u')))?;

    // undo報告(kind "undo")はメッセージとしてGUIへ送られない。
    let surfaced = tokio::time::timeout(Duration::from_millis(1200), async {
        while let Some(event) = events.recv().await {
            if let EditorEvent::Message(message) = event
                && message.text.contains("changes")
            {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(!surfaced, "undo report should be muted");

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn opening_a_line_preserves_the_current_indent() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;
    // 深さ2相当(先頭タブ2つ)の行。
    engine.set_initial_lines(vec![EditorLine::new("\t\tchild")])?;
    wait_for_lines(&engine, |lines| lines.len() == 1).await?;

    // `o` で改行し、内容を入力してからNormalへ戻す(空行だと autoindent は破棄される)。
    engine.send(key_command(Key::Char('o')))?;
    engine.send(EditorCommand::Text("x".to_owned()))?;
    engine.send(key_command(Key::Esc))?;

    // 新しい行は現在行の先頭タブを引き継ぐ(二重インデントしない)。
    wait_for_lines(&engine, |lines| {
        lines.len() == 2 && lines[1].text.as_ref() == "\t\tx"
    })
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn opening_a_line_preserves_id_prefixed_depth() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;
    engine.set_initial_lines(vec![EditorLine::new("/002 \tchild")])?;
    wait_for_lines(&engine, |lines| lines.len() == 1).await?;

    engine.send(key_command(Key::Char('o')))?;
    engine.send(EditorCommand::Text("x".to_owned()))?;
    engine.send(key_command(Key::Esc))?;

    wait_for_lines(&engine, |lines| {
        lines.len() == 2
            && lines[1].text.starts_with('\t')
            && !lines[1].text.starts_with("\t\t")
            && !lines[1].text.starts_with('/')
    })
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn insert_enter_preserves_id_prefixed_depth() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;
    engine.set_initial_lines(vec![EditorLine::new("/002 \tchild")])?;
    wait_for_lines(&engine, |lines| lines.len() == 1).await?;

    engine.send(key_command(Key::End))?;
    engine.send(key_command(Key::Char('a')))?;
    engine.send(key_command(Key::Enter))?;
    engine.send(EditorCommand::Text("x".to_owned()))?;
    engine.send(key_command(Key::Esc))?;

    wait_for_lines(&engine, |lines| {
        lines.len() == 2 && lines[1].text.as_ref() == "\tx" && !lines[1].text.starts_with('/')
    })
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn cursor_snaps_past_the_id_prefix_and_indent() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;
    // `/002 ` (5) + タブ1つ = name_start_col 6。
    engine.set_initial_lines(vec![EditorLine::new("/002 \tchild")])?;
    wait_for_lines(&engine, |lines| lines.len() == 1).await?;

    // 行頭(プレフィックス/インデント領域)へ移動すると name_start_col まで戻される。
    // name_start_col が例外を投げるとスナップが働かず col=0 のまま止まる。
    engine.send(key_command(Key::Char('0')))?;
    wait_for_cursor(&engine, |_line, col| col == 6).await?;

    Ok(())
}

async fn wait_for_lines(
    engine: &NvimEngine,
    predicate: impl Fn(&[EditorLine]) -> bool,
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = engine.snapshot();
            if predicate(&snapshot.lines) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| {
        let snapshot = engine.snapshot();
        let lines: Vec<&str> = snapshot
            .lines
            .iter()
            .map(|line| line.text.as_ref())
            .collect();
        anyhow::anyhow!("snapshot lines did not match predicate; last snapshot: {lines:?}")
    })
}

async fn wait_for_cursor(
    engine: &NvimEngine,
    predicate: impl Fn(usize, usize) -> bool,
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let cursor = engine.snapshot().cursor;
            if predicate(cursor.line, cursor.col) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("snapshot cursor did not match predicate"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn select_lines_enters_linewise_visual_selection_both_directions() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.set_initial_lines(vec![
        EditorLine::new("/001 alpha"),
        EditorLine::new("/002 beta"),
        EditorLine::new("/003 gamma"),
    ])?;
    wait_for_lines(&engine, |lines| lines.len() == 3).await?;

    // Shift+click契約: anchor=click前のカーソル行、head=click対象行。前方選択。
    engine.send(EditorCommand::SelectLines { anchor: 0, head: 2 })?;
    wait_for_mode(&engine, Mode::VisualLine).await?;
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.visual_start.map(|cursor| cursor.line), Some(0));
    assert_eq!(snapshot.cursor.line, 2);

    // 後方選択(anchor > head)も動くこと。
    engine.send(key_command(Key::Esc))?;
    wait_for_mode(&engine, Mode::Normal).await?;
    engine.send(EditorCommand::SelectLines { anchor: 2, head: 0 })?;
    wait_for_mode(&engine, Mode::VisualLine).await?;
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.visual_start.map(|cursor| cursor.line), Some(2));
    assert_eq!(snapshot.cursor.line, 0);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn begin_name_edit_starts_insert_at_name_start_and_is_undoable() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    // `/001 ` (5バイト) + タブ1つ = name_start_col 6。
    engine.set_initial_lines(vec![EditorLine::new("/001 \tname.txt")])?;
    wait_for_lines(&engine, |lines| lines.len() == 1).await?;

    engine.send(EditorCommand::BeginNameEdit { line: 0 })?;
    wait_for_mode(&engine, Mode::Insert).await?;
    wait_for_cursor(&engine, |line, col| line == 0 && col == 6).await?;

    // IDプレフィックス・インデントは変更されない。
    engine.send(EditorCommand::Text("X".to_owned()))?;
    wait_for(&engine, |line| line == "/001 \tXname.txt").await?;

    // 1回のuで取り消せる(実FSは一切触れない)。
    engine.send(key_command(Key::Esc))?;
    wait_for_mode(&engine, Mode::Normal).await?;
    engine.send(key_command(Key::Char('u')))?;
    wait_for(&engine, |line| line == "/001 \tname.txt").await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a compatible nvim executable"]
async fn delete_line_removes_only_the_target_line() -> anyhow::Result<()> {
    let _serial = NVIM_TEST_SERIAL.lock().await;
    let nvim_exe = std::env::var_os("FYLER_NVIM_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("nvim"));
    let root = std::env::current_dir()?;
    let (engine, _events) = NvimEngine::start(NvimConfig::new(nvim_exe, root)).await?;

    engine.set_initial_lines(vec![
        EditorLine::new("/001 alpha"),
        EditorLine::new("/002 beta"),
    ])?;
    wait_for_lines(&engine, |lines| lines.len() == 2).await?;

    engine.send(EditorCommand::DeleteLine { line: 0 })?;
    wait_for_lines(&engine, |lines| {
        lines.len() == 1 && lines[0].text.as_ref() == "/002 beta"
    })
    .await?;

    Ok(())
}
