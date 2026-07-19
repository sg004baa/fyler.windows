//! NvimEngine本体: snapshot + command channel型の `EditorEngine` 実装。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwap;
use fyler_core::editor::{
    CmdlineState, Cursor, EditorCommand, EditorEngine, EditorEvent, EditorLine, EditorMessage,
    EditorSnapshot, FoldOp, MessageKind, Mode, PopupmenuItem, PopupmenuState, SearchHighlight,
};
use fyler_core::pane::PaneAction;
use fyler_core::transfer::TransferKind;
use nvim_rs::compat::tokio::Compat;
use nvim_rs::create::tokio::new_child_cmd;
use nvim_rs::{Buffer, Handler, Neovim, UiAttachOptions};
use rmpv::Value;
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;

use crate::guard;
use crate::spawn::{NVIM_ARGS, NvimConfig};
use crate::translate;

type NvimWriter = Compat<ChildStdin>;
type Nvim = Neovim<NvimWriter>;

#[derive(Debug)]
struct RpcNotification {
    name: String,
    args: Vec<Value>,
}

#[derive(Clone)]
struct NvimHandler {
    notification_tx: mpsc::UnboundedSender<RpcNotification>,
}

#[async_trait::async_trait]
impl Handler for NvimHandler {
    type Writer = NvimWriter;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _nvim: Nvim) {
        let _ = self.notification_tx.send(RpcNotification { name, args });
    }
}

enum EngineCommand {
    Editor(EditorCommand),
    SetInitialLines(Vec<EditorLine>),
}

/// `EditorEngine` のNeovim実装。
///
/// - GUIスレッドから見えるのは「snapshotの読み取り」と「コマンド送信」だけ。
///   RPCの往復はすべてバックグラウンドのtokioタスク(エンジンタスク)が行う
/// - snapshotは [`ArcSwap`] で原子的に差し替える(GUIはロックなしで読む)
pub struct NvimEngine {
    snapshot: Arc<ArcSwap<EditorSnapshot>>,
    snapshot_notice: Arc<AtomicBool>,
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
}

impl EditorEngine for NvimEngine {
    fn send(&self, cmd: EditorCommand) -> anyhow::Result<()> {
        self.cmd_tx
            .send(EngineCommand::Editor(cmd))
            .map_err(|_| anyhow::anyhow!("NvimEngine task has stopped"))
    }

    fn snapshot(&self) -> Arc<EditorSnapshot> {
        self.snapshot.load_full()
    }

    fn acknowledge_snapshot_update(&self) {
        self.snapshot_notice.store(false, Ordering::Release);
    }
}

impl NvimEngine {
    /// 初回スキャンで生成した行をバッファへ設定する。
    ///
    /// 初期投入は通常のPasteと異なり、投入後に`dirty=false`へ戻す。RPC自体は
    /// GUI入力と同じエンジンタスクで直列化され、呼び出し元をブロックしない。
    pub fn set_initial_lines(&self, lines: Vec<EditorLine>) -> anyhow::Result<()> {
        self.cmd_tx
            .send(EngineCommand::SetInitialLines(lines))
            .map_err(|_| anyhow::anyhow!("NvimEngine task has stopped"))
    }

    /// nvimを起動し、エンジンタスクを開始する(M1)。
    ///
    /// 実装契約(DESIGN.md「nvim統合の詳細」):
    ///
    /// 1. **プロセス起動**: `config.nvim_exe` を [`crate::spawn::NVIM_ARGS`] で起動。
    ///    Windowsでは必ず `creation_flags(CREATE_NO_WINDOW)` を付ける
    /// 2. **バッファ設定**: バッファ名は `filer://C:/Users/...` 形式の架空URI
    ///    ([`crate::guard::BUFFER_URI_SCHEME`])、`buftype=acwrite`
    /// 3. **同期経路**:
    ///    - 行内容: `nvim_buf_attach` のプッシュ通知(`nvim_buf_lines_event`)
    ///    - mode / cursor / changedtick: キー送信ごとに一括取得し、
    ///      [`EditorSnapshot`] として原子的に更新(revisionはRust側で単調増加)。
    ///      エンジンが追加入力を同期待ちしている間は状態取得を保留し、入力完結後に再試行する
    /// 4. **UI attach**: 起動後に `nvim_ui_attach` を最小グリッドサイズで後付け実行。
    ///    有効化するextは `ext_cmdline` / `ext_messages` / `ext_popupmenu`。
    ///    `ext_messages`がないと `E486` 等がgridに描かれて見えない。
    ///    grid系描画イベントはすべて無視する
    /// 5. **事故防止**: [`crate::guard`] のautocmd・remapを導入
    /// 6. **コマンド処理**: `cmd_rx` から受けた [`EditorCommand`] を処理する。
    ///    `Key` は [`crate::translate::to_nvim_keycodes`] → `nvim_input`、
    ///    `Text` は `nvim_input` でなくバッファ挿入APIまたは `nvim_paste`(M0で検証)、
    ///    `Paste` は `nvim_paste`、`RequestCommit` は `:w` 相当、
    ///    `Undo`/`Redo` は `u`/`<C-r>` 相当
    /// 7. **イベント**: BufWriteCmd等のrpcnotify → `EditorEvent::CommitRequested`、
    ///    行アクション → `ActivateLine` / `OpenWith` / `YankPath` / `NavigateInto` / `NavigateParent` /
    ///    `ToggleHidden` / `Fold`、ルート選択 → `ChangeDirectory` / `JumpBookmark`、
    ///    ext_cmdline → `CmdlineShow/CmdlineHide`、補完UI → `Popupmenu*`、
    ///    ext_messages → `Message`、プロセス終了検知 →
    ///    `EngineCrashed` として `event_tx` へ流す
    ///
    /// 戻り値: エンジン本体と、GUI/app層が受けるイベントストリーム。
    pub async fn start(
        config: NvimConfig,
    ) -> anyhow::Result<(Arc<Self>, mpsc::UnboundedReceiver<EditorEvent>)> {
        let (notification_tx, mut notification_rx) = mpsc::unbounded_channel();
        let handler = NvimHandler { notification_tx };

        let mut command = Command::new(&config.nvim_exe);
        command.args(NVIM_ARGS).kill_on_drop(true);
        #[cfg(windows)]
        command.env("NVIM_LOG_FILE", "NUL");
        #[cfg(not(windows))]
        command.env("NVIM_LOG_FILE", "/dev/null");
        #[cfg(windows)]
        command.creation_flags(crate::spawn::CREATE_NO_WINDOW);

        let (nvim, mut io_task, mut child) =
            new_child_cmd(&mut command, handler)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "Failed to start Neovim ({}): {error}",
                        config.nvim_exe.display()
                    )
                })?;

        let api_info = nvim
            .get_api_info()
            .await
            .map_err(|error| anyhow::anyhow!("Neovim RPC handshake failed: {error}"))?;
        let channel_id = api_info
            .first()
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("Failed to get Neovim channel ID"))?;

        let buffer = nvim
            .get_current_buf()
            .await
            .map_err(|error| anyhow::anyhow!("Failed to get initial buffer: {error}"))?;
        // 名前設定時に架空URIのswapfileを作ろうとしないよう、先に無効化する。
        buffer
            .set_option("swapfile", Value::Boolean(false))
            .await
            .map_err(|error| anyhow::anyhow!("Failed to disable swapfile: {error}"))?;
        buffer
            .set_option("buftype", Value::from(guard::BUFTYPE))
            .await
            .map_err(|error| anyhow::anyhow!("Failed to set buftype: {error}"))?;
        let buffer_uri = format!(
            "{}{}",
            guard::BUFFER_URI_SCHEME,
            config.root.to_string_lossy().replace('\\', "/")
        );
        buffer
            .set_name(&buffer_uri)
            .await
            .map_err(|error| anyhow::anyhow!("Failed to set buffer name: {error}"))?;

        let mut ui_options = UiAttachOptions::new();
        ui_options.set_cmdline_external(true);
        // nvim-rs 0.9.2ではこのメソッド名が`messages_externa`になっている。
        ui_options.set_messages_externa(true);
        ui_options.set_popupmenu_external(true);
        nvim.ui_attach(80, 24, &ui_options)
            .await
            .map_err(|error| anyhow::anyhow!("Failed to attach Neovim UI: {error}"))?;

        // `/` 検索の大文字小文字挙動(smartcase)と、検索状態の露出を有効化する。
        // ハイライトはGUIがsnapshotの `search` から自前描画するが、`v:hlsearch` が
        // ハイライトのON/OFFゲート(`:noh` 後に0)になるため hlsearch も有効化する。
        for (name, value) in [
            ("ignorecase", true),
            ("smartcase", true),
            ("hlsearch", true),
            ("incsearch", true),
        ] {
            nvim.set_option_value(name, Value::Boolean(value), Vec::new())
                .await
                .map_err(|error| anyhow::anyhow!("Failed to set {name} option: {error}"))?;
        }

        guard::install_guards(&nvim, &buffer, channel_id, &config.bindings).await?;

        let attached = buffer
            .attach(true, Vec::new())
            .await
            .map_err(|error| anyhow::anyhow!("Failed to attach buffer notifications: {error}"))?;
        if !attached {
            anyhow::bail!("Neovim rejected the buffer notification attachment");
        }

        let mut lines = buffer
            .get_lines(0, -1, false)
            .await
            .map_err(|error| anyhow::anyhow!("Failed to get initial buffer lines: {error}"))?
            .into_iter()
            .map(EditorLine::new)
            .collect::<Vec<_>>();
        let mut lines_arc = lines_to_arc(&lines);
        let status = query_status(&nvim).await?;
        let mut revision = 1;
        let initial_snapshot = build_snapshot(revision, &lines_arc, status, None);
        let shared_snapshot = Arc::new(ArcSwap::from_pointee(initial_snapshot));

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let task_snapshot = Arc::clone(&shared_snapshot);
        let snapshot_notice = Arc::new(AtomicBool::new(false));
        let task_snapshot_notice = Arc::clone(&snapshot_notice);

        tokio::spawn(async move {
            let mut cmdline_state = None;
            let mut snapshot_pending = false;
            // ミラーが反映済みのbuffer changedtick。全再同期(get_lines)が
            // 処理中に進んだ後続編集を先取りした場合、キューに残った古い
            // buf_lines_eventを二重適用しないためのゲート(0 = 未確定)。
            let mut mirror_tick: u64 = 0;
            let mut commit_pending = false;
            let crash_reason = loop {
                tokio::select! {
                    child_status = child.wait() => {
                        break match child_status {
                            Ok(status) => format!("Neovim process exited: {status}"),
                            Err(error) => format!("Failed to monitor Neovim process exit: {error}"),
                        };
                    }
                    io_result = &mut io_task => {
                        break match io_result {
                            Ok(Ok(())) => "Neovim RPC connection closed".to_owned(),
                            Ok(Err(error)) => format!("Neovim RPC connection failed: {error}"),
                            Err(error) => format!("Neovim RPC monitor task failed: {error}"),
                        };
                    }
                    command = cmd_rx.recv() => {
                        let Some(command) = command else {
                            let _ = nvim.command("qa!").await;
                            let _ = child.wait().await;
                            return;
                        };

                        if let Err(error) = handle_command(&nvim, &buffer, command).await {
                            send_message(
                                &event_tx,
                                MessageKind::Error,
                                format!("Editor input failed: {error}"),
                            );
                        }
                        snapshot_pending = true;
                        match publish_pending_snapshot(
                            &nvim,
                            &lines_arc,
                            &mut revision,
                            &task_snapshot,
                            (&event_tx, &task_snapshot_notice),
                            &mut snapshot_pending,
                            cmdline_state.as_ref(),
                        ).await {
                            Ok(Some(snapshot)) => {
                                if commit_pending {
                                    let _ = event_tx.send(EditorEvent::CommitRequested {
                                        changedtick: snapshot.changedtick,
                                        lines: Arc::clone(&snapshot.lines),
                                    });
                                    commit_pending = false;
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                send_message(
                                    &event_tx,
                                    MessageKind::Error,
                                    format!("Failed to update editor state: {error}"),
                                );
                            }
                        }
                    }
                    notification = notification_rx.recv() => {
                        let Some(notification) = notification else {
                            break "Neovim notification channel closed".to_owned();
                        };

                        match notification.name.as_str() {
                            "nvim_buf_lines_event" => {
                                match apply_lines_notification(
                                    &notification.args,
                                    &mut lines,
                                    &mut mirror_tick,
                                ) {
                                    ApplyLinesResult::Applied => {
                                        lines_arc = lines_to_arc(&lines);
                                    }
                                    ApplyLinesResult::Stale => {
                                        // 全再同期が既に反映済みの編集。二重適用しない。
                                        continue;
                                    }
                                    ApplyLinesResult::Resync => {
                                        // changedtickと行内容を同一メインループステップで
                                        // 原子取得する。get_lines単独では後続編集を先取りし、
                                        // キュー内の古いイベントが二重適用される。
                                        match fetch_buffer_lines_with_tick(&nvim).await {
                                            Ok((tick, current_lines)) => {
                                                lines = current_lines;
                                                mirror_tick = tick;
                                                lines_arc = lines_to_arc(&lines);
                                            }
                                            Err(error) => {
                                                send_message(
                                                    &event_tx,
                                                    MessageKind::Error,
                                                    format!("Failed to resynchronize buffer lines: {error}"),
                                                );
                                                continue;
                                            }
                                        }
                                    }
                                }
                                snapshot_pending = true;
                            }
                            "nvim_buf_detach_event" => {
                                break "fyler buffer was detached from Neovim".to_owned();
                            }
                            "redraw" => {
                                // cmdline系イベントが来たら検索パターンが変わりうるので
                                // snapshotを再発行する(incsearchプレビュー追従)。
                                let outcome = handle_redraw(
                                    &notification.args,
                                    &event_tx,
                                    &mut cmdline_state,
                                );
                                if outcome.cmdline_changed {
                                    snapshot_pending = true;
                                }
                                // 検索失敗(E486)等でエコー行 + エラーが積み上がると
                                // nvimがhit-enter待ちに入りfylerがフリーズして見える。
                                // ext_messagesのreturn_promptは外部UIが表示を持つため、
                                // 即座に<CR>で解除し、プロンプト自体は表示しない。
                                // ただしユーザー入力が先にプロンプトを解除していた場合、
                                // この<CR>はNormalモードでActivateLine(ファイルを開く)に
                                // 化けるため、実際にblocking中のときだけ送る。
                                if outcome.hit_enter_prompt
                                    && input_is_blocking(&nvim).await.unwrap_or(false)
                                {
                                    let _ = nvim.input("<CR>").await;
                                }
                            }
                            "fyler_commit_requested" => {
                                commit_pending = true;
                                snapshot_pending = true;
                            }
                            "fyler_open" => {
                                let line = notification.args.first()
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match line {
                                    Some(line) => {
                                        let _ = event_tx.send(EditorEvent::ActivateLine { line });
                                    }
                                    None => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line number to open".to_owned(),
                                    ),
                                }
                            }
                            "fyler_open_with" => {
                                let line = notification.args.first()
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match line {
                                    Some(line) => {
                                        let _ = event_tx.send(EditorEvent::OpenWith { line });
                                    }
                                    None => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line number for open-with".to_owned(),
                                    ),
                                }
                            }
                            "fyler_parent" => {
                                let _ = event_tx.send(EditorEvent::NavigateParent);
                            }
                            "fyler_navigate_into" => {
                                let line = notification.args.first()
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match line {
                                    Some(line) => {
                                        let _ = event_tx.send(EditorEvent::NavigateInto { line });
                                    }
                                    None => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line number to navigate to".to_owned(),
                                    ),
                                }
                            }
                            "fyler_terminal" => {
                                let line = notification
                                    .args
                                    .first()
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                let args = notification
                                    .args
                                    .get(1)
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                match line {
                                    Some(_) if !args.trim().is_empty() => send_message(
                                        &event_tx,
                                        MessageKind::Warn,
                                        ":terminal arguments are not supported; run it without arguments"
                                            .to_owned(),
                                    ),
                                    Some(line) => {
                                        let _ = event_tx.send(EditorEvent::OpenTerminal { line });
                                    }
                                    None => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line number for terminal".to_owned(),
                                    ),
                                }
                            }
                            "fyler_admin" => {
                                let line = notification.args.first()
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match line {
                                    Some(line) => {
                                        let _ = event_tx.send(EditorEvent::OpenAsAdmin { line });
                                    }
                                    None => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line number for open-as-administrator"
                                            .to_owned(),
                                    ),
                                }
                            }
                            "fyler_shortcut" => {
                                let line = notification.args.first()
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match line {
                                    Some(line) => {
                                        let _ = event_tx.send(EditorEvent::CreateShortcut { line });
                                    }
                                    None => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line number for create-shortcut"
                                            .to_owned(),
                                    ),
                                }
                            }
                            "fyler_extract" => {
                                let line = notification.args.first()
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match line {
                                    Some(line) => {
                                        let _ = event_tx.send(EditorEvent::ExtractArchive { line });
                                    }
                                    None => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line number for extract".to_owned(),
                                    ),
                                }
                            }
                            "fyler_toggle_hidden" => {
                                let _ = event_tx.send(EditorEvent::ToggleHidden);
                            }
                            "fyler_fold" => {
                                let op = notification
                                    .args
                                    .first()
                                    .and_then(Value::as_str)
                                    .and_then(fold_op_from_str);
                                let line = notification
                                    .args
                                    .get(1)
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match (op, line) {
                                    (Some(op), Some(line)) => {
                                        let _ = event_tx.send(EditorEvent::Fold { op, line });
                                    }
                                    (None, _) => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get fold operation".to_owned(),
                                    ),
                                    (_, None) => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line number for folding".to_owned(),
                                    ),
                                }
                            }
                            "fyler_open_picker" => {
                                let _ = event_tx.send(EditorEvent::OpenFilePicker);
                            }
                            "fyler_dock_focus" => {
                                let _ = event_tx.send(EditorEvent::ToggleDockFocus);
                            }
                            "fyler_help" => {
                                let _ = event_tx.send(EditorEvent::ShowHelp);
                            }
                            "fyler_pane" => {
                                match notification.args.first()
                                    .and_then(Value::as_str)
                                    .and_then(parse_pane_action)
                                {
                                    Some(action) => {
                                        let _ = event_tx.send(EditorEvent::PaneAction(action));
                                    }
                                    None => send_message(
                                        &event_tx,
                                        MessageKind::Info,
                                        "This pane action is invalid".to_owned(),
                                    ),
                                }
                            }
                            "fyler_history" => {
                                match notification.args.first().and_then(Value::as_str) {
                                    Some("back") => {
                                        let _ = event_tx.send(EditorEvent::HistoryBack);
                                    }
                                    Some("forward") => {
                                        let _ = event_tx.send(EditorEvent::HistoryForward);
                                    }
                                    _ => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get history direction".to_owned(),
                                    ),
                                }
                            }
                            "fyler_refresh" => {
                                let _ = event_tx.send(EditorEvent::RefreshRequested);
                            }
                            "fyler_dir_size" => {
                                let line = notification.args.first()
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match line {
                                    Some(line) => {
                                        let _ = event_tx.send(EditorEvent::DirSizeRequested { line });
                                    }
                                    None => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line number for directory size".to_owned(),
                                    ),
                                }
                            }
                            "fyler_transfer" => {
                                let kind = notification.args.first()
                                    .and_then(Value::as_str)
                                    .and_then(parse_transfer_kind);
                                let first = notification.args.get(1)
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                let last = notification.args.get(2)
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match (kind, first, last) {
                                    (Some(kind), Some(first), Some(last)) if first <= last => {
                                        let _ = event_tx.send(EditorEvent::TransferRequested {
                                            kind,
                                            lines: (first..=last).collect(),
                                        });
                                    }
                                    _ => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line range for transfer".to_owned(),
                                    ),
                                }
                            }
                            "fyler_clipboard" => {
                                let kind = notification.args.first().and_then(Value::as_str);
                                let first = notification.args.get(1)
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                let last = notification.args.get(2)
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match (kind, first, last) {
                                    (Some("copy"), Some(first), Some(last)) if first <= last => {
                                        let _ = event_tx.send(EditorEvent::ClipboardCopyRequested {
                                            lines: (first..=last).collect(),
                                        });
                                    }
                                    (Some("cut"), Some(first), Some(last)) if first <= last => {
                                        let _ = event_tx.send(EditorEvent::ClipboardCutRequested {
                                            lines: (first..=last).collect(),
                                        });
                                    }
                                    _ => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line range for clipboard".to_owned(),
                                    ),
                                }
                            }
                            "fyler_clipboard_paste" => {
                                let line = notification.args.first()
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match line {
                                    Some(line) => {
                                        let _ = event_tx.send(EditorEvent::ClipboardPasteRequested { line });
                                    }
                                    None => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the cursor line for paste".to_owned(),
                                    ),
                                }
                            }
                            "fyler_cd" => {
                                let query = notification
                                    .args
                                    .first()
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|query| !query.is_empty())
                                    .map(str::to_owned);
                                let _ = event_tx.send(EditorEvent::ChangeDirectory { query });
                            }
                            "fyler_sort" => {
                                let query = notification
                                    .args
                                    .first()
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|query| !query.is_empty())
                                    .map(str::to_owned);
                                let _ = event_tx.send(EditorEvent::ChangeSort { query });
                            }
                            "fyler_bookmark" => {
                                let query = notification
                                    .args
                                    .first()
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|query| !query.is_empty())
                                    .map(str::to_owned);
                                let _ = event_tx.send(EditorEvent::JumpBookmark { query });
                            }
                            "fyler_undo" => {
                                let _ = event_tx.send(EditorEvent::UndoRequested);
                            }
                            "fyler_feedback" => {
                                let _ = event_tx.send(EditorEvent::FeedbackRequested);
                            }
                            "fyler_yank_path" => {
                                let line = notification.args.first()
                                    .and_then(value_as_u64)
                                    .and_then(|line| usize::try_from(line).ok());
                                match line {
                                    Some(line) => {
                                        let _ = event_tx.send(EditorEvent::YankPath { line });
                                    }
                                    None => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        "Failed to get the line number to copy".to_owned(),
                                    ),
                                }
                            }
                            "fyler_action_blocked" => {
                                send_message(
                                    &event_tx,
                                    MessageKind::Info,
                                    "This key is not available".to_owned(),
                                );
                            }
                            "fyler_write_blocked" => {
                                let event = notification.args.first()
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                send_message(
                                    &event_tx,
                                    MessageKind::Error,
                                    format!("Rejected unsupported save path: {event}"),
                                );
                            }
                            "fyler_unexpected_buffer" => {
                                send_message(
                                    &event_tx,
                                    MessageKind::Warn,
                                    "Closed an unexpected buffer and returned to fyler".to_owned(),
                                );
                            }
                            _ => {}
                        }

                        match publish_pending_snapshot(
                            &nvim,
                            &lines_arc,
                            &mut revision,
                            &task_snapshot,
                            (&event_tx, &task_snapshot_notice),
                            &mut snapshot_pending,
                            cmdline_state.as_ref(),
                        ).await {
                            Ok(Some(snapshot)) => {
                                if commit_pending {
                                    let _ = event_tx.send(EditorEvent::CommitRequested {
                                        changedtick: snapshot.changedtick,
                                        lines: Arc::clone(&snapshot.lines),
                                    });
                                    commit_pending = false;
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                send_message(
                                    &event_tx,
                                    MessageKind::Error,
                                    format!("Failed to update editor state: {error}"),
                                );
                            }
                        }
                    }
                }
            };

            let _ = child.start_kill();
            let _ = event_tx.send(EditorEvent::EngineCrashed {
                reason: crash_reason,
            });
        });

        let engine = Arc::new(Self {
            snapshot: shared_snapshot,
            snapshot_notice,
            cmd_tx,
        });
        Ok((engine, event_rx))
    }
}

async fn handle_command(
    nvim: &Nvim,
    buffer: &Buffer<NvimWriter>,
    command: EngineCommand,
) -> anyhow::Result<()> {
    match command {
        EngineCommand::Editor(EditorCommand::Key(key)) => {
            let keycodes = translate::to_nvim_keycodes(&key);
            nvim.input(&keycodes)
                .await
                .map_err(|error| anyhow::anyhow!("Key input RPC failed: {error}"))?;
        }
        EngineCommand::Editor(EditorCommand::Text(text))
        | EngineCommand::Editor(EditorCommand::Paste(text)) => {
            nvim.paste(&text, false, -1)
                .await
                .map_err(|error| anyhow::anyhow!("Text input RPC failed: {error}"))?;
        }
        EngineCommand::Editor(EditorCommand::SetLines { lines, cursor_line }) => {
            replace_buffer_lines(nvim, buffer, lines, cursor_line, "reconcile").await?;
        }
        EngineCommand::Editor(EditorCommand::SetCursorLine(line)) => {
            set_cursor_line(nvim, buffer, line).await?;
        }
        EngineCommand::Editor(EditorCommand::SetModifiable(value)) => {
            set_buffer_modifiable(nvim, buffer, value, "save flow").await?;
        }
        EngineCommand::Editor(EditorCommand::RequestCommit) => {
            nvim.command("write")
                .await
                .map_err(|error| anyhow::anyhow!("Save request RPC failed: {error}"))?;
        }
        EngineCommand::Editor(EditorCommand::Undo) => {
            nvim.input("u")
                .await
                .map_err(|error| anyhow::anyhow!("Undo RPC failed: {error}"))?;
        }
        EngineCommand::Editor(EditorCommand::Redo) => {
            nvim.input("<C-r>")
                .await
                .map_err(|error| anyhow::anyhow!("Redo RPC failed: {error}"))?;
        }
        EngineCommand::Editor(EditorCommand::SelectLines { anchor, head }) => {
            select_lines(nvim, buffer, anchor, head).await?;
        }
        EngineCommand::Editor(EditorCommand::BeginNameEdit { line }) => {
            begin_name_edit(nvim, buffer, line).await?;
        }
        EngineCommand::Editor(EditorCommand::DeleteLine { line }) => {
            delete_line(nvim, buffer, line).await?;
        }
        EngineCommand::SetInitialLines(editor_lines) => {
            replace_buffer_lines(nvim, buffer, editor_lines, None, "initialization").await?;
        }
    }

    Ok(())
}

/// バッファを変更せず、指定行へカーソルだけを移動する。
async fn set_cursor_line(
    nvim: &Nvim,
    buffer: &Buffer<NvimWriter>,
    requested_line: usize,
) -> anyhow::Result<()> {
    let line_count = buffer.line_count().await.map_err(|error| {
        anyhow::anyhow!("Failed to get line count for cursor movement: {error}")
    })?;
    let target_line = clamp_cursor_line(requested_line, line_count);
    move_cursor_to_line(nvim, buffer, target_line, "cursor movement").await
}

/// 指定行の名前部分の先頭(表示上のカーソル位置)へカーソルを移動する共通処理。
/// `set_cursor_line` / `select_lines` が共有する。
async fn move_cursor_to_line(
    nvim: &Nvim,
    buffer: &Buffer<NvimWriter>,
    target_line: i64,
    purpose: &str,
) -> anyhow::Result<()> {
    let line = buffer
        .get_lines(target_line, target_line + 1, false)
        .await
        .map_err(|error| anyhow::anyhow!("Failed to get line for {purpose}: {error}"))?
        .into_iter()
        .next()
        .unwrap_or_default();
    let target_column =
        i64::try_from(fyler_core::grammar::id_prefix_len(&line)).unwrap_or(i64::MAX);
    let window = nvim
        .get_current_win()
        .await
        .map_err(|error| anyhow::anyhow!("Failed to get window for {purpose}: {error}"))?;
    window
        .set_cursor((target_line + 1, target_column))
        .await
        .map_err(|error| anyhow::anyhow!("Failed to set cursor for {purpose}: {error}"))?;
    Ok(())
}

fn clamp_cursor_line(requested_line: usize, line_count: i64) -> i64 {
    let requested_line = i64::try_from(requested_line).unwrap_or(i64::MAX);
    requested_line.min(line_count.saturating_sub(1)).max(0)
}

/// `anchor`行から`head`行までのlinewise Visual選択を行う(Shift+click契約)。
///
/// 現在のモードは`<Esc>`で正規化してから選択を開始する(Insert/Visual等の
/// 途中状態から呼ばれても安全なように)。`anchor`・`head`は個別にクランプする
/// (どちらかが範囲外でも他方は有効な選択として成立させる)。
async fn select_lines(
    nvim: &Nvim,
    buffer: &Buffer<NvimWriter>,
    anchor: usize,
    head: usize,
) -> anyhow::Result<()> {
    let line_count = buffer
        .line_count()
        .await
        .map_err(|error| anyhow::anyhow!("Failed to get line count for line selection: {error}"))?;
    let anchor_line = clamp_cursor_line(anchor, line_count);
    let head_line = clamp_cursor_line(head, line_count);
    nvim.input("<Esc>")
        .await
        .map_err(|error| anyhow::anyhow!("Failed to reset mode before line selection: {error}"))?;
    move_cursor_to_line(nvim, buffer, anchor_line, "line selection anchor").await?;
    nvim.command("normal! V")
        .await
        .map_err(|error| anyhow::anyhow!("Failed to start line-wise selection: {error}"))?;
    move_cursor_to_line(nvim, buffer, head_line, "line selection head").await?;
    Ok(())
}

/// 対象行の名前部分の先頭へカーソルを移動し、Insert modeで編集を開始する
/// (Rename契約)。IDプレフィックス・インデントはそのまま(削除しない)。
async fn begin_name_edit(
    nvim: &Nvim,
    buffer: &Buffer<NvimWriter>,
    requested_line: usize,
) -> anyhow::Result<()> {
    let line_count = buffer
        .line_count()
        .await
        .map_err(|error| anyhow::anyhow!("Failed to get line count for name edit: {error}"))?;
    let target_line = clamp_cursor_line(requested_line, line_count);
    let line = buffer
        .get_lines(target_line, target_line + 1, false)
        .await
        .map_err(|error| anyhow::anyhow!("Failed to get line for name edit: {error}"))?
        .into_iter()
        .next()
        .unwrap_or_default();
    let target_column = name_start_column(&line);
    let window = nvim
        .get_current_win()
        .await
        .map_err(|error| anyhow::anyhow!("Failed to get window for name edit: {error}"))?;
    window
        .set_cursor((target_line + 1, target_column))
        .await
        .map_err(|error| anyhow::anyhow!("Failed to move cursor for name edit: {error}"))?;
    nvim.input("i")
        .await
        .map_err(|error| anyhow::anyhow!("Failed to start insert mode for name edit: {error}"))?;
    Ok(())
}

/// 行の名前部分(IDプレフィックス+インデントの直後)が始まるバイトオフセットを返す。
/// `fyler-gui::conceal::conceal_line`の`concealed_bytes`と同じ計算(GUIの表示
/// カーソル補正と一致させる。nvim語彙には触れない純粋計算)。
fn name_start_column(line: &str) -> i64 {
    let prefix_bytes = fyler_core::grammar::id_prefix_len(line);
    let rest = &line[prefix_bytes..];
    let (_, display) = fyler_core::grammar::split_indent(rest);
    let indent_bytes = rest.len() - display.len();
    i64::try_from(prefix_bytes + indent_bytes).unwrap_or(i64::MAX)
}

/// 該当表示行をバッファから1行除去する(Mark for deletion契約)。実FSへは触れない。
async fn delete_line(
    nvim: &Nvim,
    buffer: &Buffer<NvimWriter>,
    requested_line: usize,
) -> anyhow::Result<()> {
    let _ = nvim;
    let line_count = buffer
        .line_count()
        .await
        .map_err(|error| anyhow::anyhow!("Failed to get line count for line deletion: {error}"))?;
    let target_line = clamp_cursor_line(requested_line, line_count);
    buffer
        .set_lines(target_line, target_line + 1, false, Vec::new())
        .await
        .map_err(|error| anyhow::anyhow!("Failed to delete line: {error}"))?;
    Ok(())
}

/// Rust側のプログラム的な全行差し替えを実行する。
///
/// `modifiable`はユーザー入力を止めるゲートであり、reconcileや初期化まで拒否する
/// ものではない。保存フロー中の`SetLines`も成功するよう、差し替え前に必ず
/// `modifiable=true`へ戻す。状態機械がreconcile完了後に改めて有効化するため、
/// この関数内では元の値へ復元しない。
async fn replace_buffer_lines(
    nvim: &Nvim,
    buffer: &Buffer<NvimWriter>,
    editor_lines: Vec<EditorLine>,
    cursor_line: Option<usize>,
    purpose: &str,
) -> anyhow::Result<()> {
    set_buffer_modifiable(nvim, buffer, true, purpose).await?;

    let new_lines: Vec<String> = if editor_lines.is_empty() {
        vec![String::new()]
    } else {
        editor_lines
            .into_iter()
            .map(|line| line.text.to_string())
            .collect()
    };
    let target_line = cursor_line
        .unwrap_or(0)
        .min(new_lines.len().saturating_sub(1));
    let target_column = new_lines
        .get(target_line)
        .map(|line| fyler_core::grammar::id_prefix_len(line))
        .unwrap_or_default();
    buffer
        .set_lines(0, -1, false, new_lines)
        .await
        .map_err(|error| anyhow::anyhow!("Failed to set buffer lines for {purpose}: {error}"))?;
    buffer
        .set_option("modified", Value::Boolean(false))
        .await
        .map_err(|error| anyhow::anyhow!("Failed to mark buffer clean after {purpose}: {error}"))?;

    // 折りたたみトグル等では操作した行へカーソルを戻す。行数を超える指定は
    // 最終行へクランプする(nvimのset_cursorは範囲外でエラーになるため)。
    let window = nvim
        .get_current_win()
        .await
        .map_err(|error| anyhow::anyhow!("Failed to get target window for {purpose}: {error}"))?;
    window
        .set_cursor((target_line as i64 + 1, target_column as i64))
        .await
        .map_err(|error| anyhow::anyhow!("Failed to set cursor after {purpose}: {error}"))?;

    Ok(())
}

/// 対象バッファの`modifiable`オプションを、バッファスコープで設定する。
async fn set_buffer_modifiable(
    nvim: &Nvim,
    buffer: &Buffer<NvimWriter>,
    value: bool,
    purpose: &str,
) -> anyhow::Result<()> {
    nvim.set_option_value(
        "modifiable",
        Value::Boolean(value),
        vec![(Value::from("buf"), buffer.get_value().clone())],
    )
    .await
    .map_err(|error| {
        anyhow::anyhow!("Failed to change buffer modifiable setting for {purpose}: {error}")
    })
}

#[derive(Debug)]
struct Status {
    changedtick: u64,
    cursor: Cursor,
    mode: Mode,
    visual_start: Option<Cursor>,
    dirty: bool,
    /// 検索状態の生値。パターンの実効解決は [`resolve_search`] で行う。
    search: SearchRaw,
}

/// nvimから読んだ検索状態の生値(未解決)。
#[derive(Debug)]
struct SearchRaw {
    /// `@/` レジスタ(直近の検索パターン)。
    register: String,
    /// `v:hlsearch`(ハイライトが有効か。`:noh` 後は偽)。
    hlsearch: bool,
    /// `&ignorecase`。
    ignorecase: bool,
    /// `&smartcase`。
    smartcase: bool,
}

async fn query_status(nvim: &Nvim) -> anyhow::Result<Status> {
    let calls = vec![
        atomic_call("nvim_get_mode", Vec::new()),
        atomic_call("nvim_win_get_cursor", vec![Value::from(0)]),
        atomic_call("nvim_buf_get_changedtick", vec![Value::from(0)]),
        atomic_call(
            "nvim_get_option_value",
            vec![
                Value::from("modified"),
                Value::Map(vec![(Value::from("buf"), Value::from(0))]),
            ],
        ),
        atomic_call(
            "nvim_call_function",
            vec![Value::from("getpos"), Value::Array(vec![Value::from("v")])],
        ),
        atomic_call(
            "nvim_eval",
            vec![Value::from(
                "[getreg('/'), v:hlsearch, &ignorecase, &smartcase]",
            )],
        ),
    ];
    let response = nvim
        .call_atomic(calls)
        .await
        .map_err(|error| anyhow::anyhow!("nvim_call_atomic failed: {error}"))?;

    let results = response
        .first()
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Invalid results format from nvim_call_atomic"))?;
    if let Some(error) = response.get(1).filter(|value| !value.is_nil()) {
        anyhow::bail!("A call inside nvim_call_atomic failed: {error:?}");
    }
    if results.len() != 6 {
        anyhow::bail!(
            "Invalid result count from nvim_call_atomic: expected 6, got {}",
            results.len()
        );
    }

    let mode_name = map_string(&results[0], "mode")
        .ok_or_else(|| anyhow::anyhow!("Invalid mode result format"))?;
    let cursor_values = results[1]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Invalid cursor result format"))?;
    let row = cursor_values
        .first()
        .and_then(value_as_u64)
        .ok_or_else(|| anyhow::anyhow!("Invalid cursor row format"))?;
    let col = cursor_values
        .get(1)
        .and_then(value_as_u64)
        .ok_or_else(|| anyhow::anyhow!("Invalid cursor column format"))?;
    let changedtick = value_as_u64(&results[2])
        .ok_or_else(|| anyhow::anyhow!("Invalid changedtick result format"))?;
    let dirty = results[3]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("Invalid modified result format"))?;
    let mode = normalize_mode(mode_name);
    let visual_start = if matches!(&mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
        let position = results[4]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Invalid visual start result format"))?;
        let line = position
            .get(1)
            .and_then(value_as_u64)
            .ok_or_else(|| anyhow::anyhow!("Invalid visual start line format"))?;
        let col = position
            .get(2)
            .and_then(value_as_u64)
            .ok_or_else(|| anyhow::anyhow!("Invalid visual start column format"))?;
        Some(Cursor {
            line: line.saturating_sub(1) as usize,
            col: col.saturating_sub(1) as usize,
        })
    } else {
        None
    };

    let search_values = results[5]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Invalid search state result format"))?;
    let search = SearchRaw {
        register: search_values
            .first()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        // v:hlsearch / &ignorecase / &smartcase は数値(0/1)で返る。
        hlsearch: search_values.get(1).and_then(value_as_u64).unwrap_or(0) != 0,
        ignorecase: search_values.get(2).and_then(value_as_u64).unwrap_or(0) != 0,
        smartcase: search_values.get(3).and_then(value_as_u64).unwrap_or(0) != 0,
    };

    Ok(Status {
        changedtick,
        cursor: Cursor {
            line: row.saturating_sub(1) as usize,
            col: col as usize,
        },
        mode,
        visual_start,
        dirty,
        search,
    })
}

fn atomic_call(name: &str, args: Vec<Value>) -> Value {
    Value::Array(vec![Value::from(name), Value::Array(args)])
}

fn lines_to_arc(lines: &[EditorLine]) -> Arc<[EditorLine]> {
    Arc::from(lines.to_vec())
}

fn build_snapshot(
    revision: u64,
    lines: &Arc<[EditorLine]>,
    status: Status,
    cmdline: Option<&CmdlineState>,
) -> EditorSnapshot {
    let search = resolve_search(&status.search, cmdline);
    EditorSnapshot {
        revision,
        changedtick: status.changedtick,
        lines: Arc::clone(lines),
        cursor: status.cursor,
        mode: status.mode,
        visual_start: status.visual_start,
        dirty: status.dirty,
        search,
    }
}

/// 検索ハイライトの実効値を解決する。
///
/// - `/` または `?` のcmdline入力中(incsearch): その内容を生パターンにする。
///   `v:hlsearch` に関係なくハイライトする(vimのincsearchプレビュー相当)
/// - それ以外: `v:hlsearch` が真のときだけ `@/` レジスタをハイライトする
///   (`:noh` 後・検索なしは `None`)
fn resolve_search(raw: &SearchRaw, cmdline: Option<&CmdlineState>) -> Option<SearchHighlight> {
    let live_pattern = cmdline
        .filter(|state| matches!(state.prompt, '/' | '?'))
        .map(|state| state.content.as_str());
    let pattern = match live_pattern {
        Some(pattern) => pattern,
        None if raw.hlsearch => raw.register.as_str(),
        None => return None,
    };
    SearchHighlight::resolve(pattern, raw.ignorecase, raw.smartcase)
}

/// 保留中のsnapshot更新を試みる。
///
/// 追加入力待ちならfast APIの判定だけで戻り、保留フラグを維持する。通常状態へ
/// 戻って一括取得に成功したときだけフラグを落とす。
async fn publish_pending_snapshot(
    nvim: &Nvim,
    lines: &Arc<[EditorLine]>,
    revision: &mut u64,
    shared_snapshot: &ArcSwap<EditorSnapshot>,
    snapshot_notice: (&mpsc::UnboundedSender<EditorEvent>, &AtomicBool),
    snapshot_pending: &mut bool,
    cmdline: Option<&CmdlineState>,
) -> anyhow::Result<Option<Arc<EditorSnapshot>>> {
    if !*snapshot_pending {
        return Ok(None);
    }

    let snapshot = publish_snapshot(
        nvim,
        lines,
        revision,
        shared_snapshot,
        snapshot_notice,
        cmdline,
    )
    .await?;
    if snapshot.is_some() {
        *snapshot_pending = false;
    }
    Ok(snapshot)
}

async fn publish_snapshot(
    nvim: &Nvim,
    lines: &Arc<[EditorLine]>,
    revision: &mut u64,
    shared_snapshot: &ArcSwap<EditorSnapshot>,
    snapshot_notice: (&mpsc::UnboundedSender<EditorEvent>, &AtomicBool),
    cmdline: Option<&CmdlineState>,
) -> anyhow::Result<Option<Arc<EditorSnapshot>>> {
    if input_is_blocking(nvim).await? {
        return Ok(None);
    }

    let status = query_status(nvim).await?;
    *revision = revision.saturating_add(1);
    let snapshot = Arc::new(build_snapshot(*revision, lines, status, cmdline));
    shared_snapshot.store(Arc::clone(&snapshot));
    send_snapshot_notice(snapshot_notice.0, snapshot_notice.1);
    Ok(Some(snapshot))
}

fn send_snapshot_notice(
    event_tx: &mpsc::UnboundedSender<EditorEvent>,
    snapshot_notice: &AtomicBool,
) {
    // snapshotのstoreを先に完了してからゲートを立てる。表示側は通知をackした後の
    // snapshot読み出しで、通知が表す世代以上の最新状態を取得できる。
    if !snapshot_notice.swap(true, Ordering::AcqRel) {
        let _ = event_tx.send(EditorEvent::SnapshotUpdated);
    }
}

/// 追加入力待ちかをfast APIだけで判定する。
///
/// ここでdeferred APIを呼ぶと、text objectや1文字引数の入力待ち中にコマンドループ
/// 自身が停止するため、状態一括取得は必ずこのゲートの後で行う。
async fn input_is_blocking(nvim: &Nvim) -> anyhow::Result<bool> {
    let mode = nvim
        .get_mode()
        .await
        .map_err(|error| anyhow::anyhow!("Failed to get pending input state: {error}"))?;
    mode.iter()
        .find_map(|(key, value)| {
            (key.as_str() == Some("blocking"))
                .then(|| value.as_bool())
                .flatten()
        })
        .ok_or_else(|| anyhow::anyhow!("Invalid pending input state format"))
}

/// [`apply_lines_notification`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyLinesResult {
    /// イベントをミラーへ適用した。
    Applied,
    /// ミラーが既に(全再同期で)反映済みの古いイベント。捨てる。
    Stale,
    /// 解釈不能または境界不整合。全再同期が必要。
    Resync,
}

/// `nvim_buf_lines_event` をミラーへ適用する。
///
/// - `changedtick`(args[1])が `mirror_tick` 以下なら全再同期で反映済みのStale。
/// - `lastline == -1` は全バッファ差し替え(attach時のsend_buffer等)。
/// - 適用に成功したら `mirror_tick` をイベントのtickへ進める(tick不明時は据え置き)。
fn apply_lines_notification(
    args: &[Value],
    lines: &mut Vec<EditorLine>,
    mirror_tick: &mut u64,
) -> ApplyLinesResult {
    let tick = args.get(1).and_then(value_as_u64);
    if let Some(tick) = tick
        && tick <= *mirror_tick
    {
        return ApplyLinesResult::Stale;
    }
    let Some(replacement_values) = args.get(4).and_then(Value::as_array) else {
        return ApplyLinesResult::Resync;
    };
    let mut replacement = Vec::with_capacity(replacement_values.len());
    for value in replacement_values {
        let Some(line) = value.as_str() else {
            return ApplyLinesResult::Resync;
        };
        replacement.push(EditorLine::new(line));
    }

    let whole_buffer = args.get(3).and_then(Value::as_i64) == Some(-1);
    if whole_buffer {
        *lines = replacement;
    } else {
        let Some(first) = args
            .get(2)
            .and_then(value_as_u64)
            .map(|value| value as usize)
        else {
            return ApplyLinesResult::Resync;
        };
        let Some(last) = args
            .get(3)
            .and_then(value_as_u64)
            .map(|value| value as usize)
        else {
            return ApplyLinesResult::Resync;
        };
        if first > last || last > lines.len() {
            return ApplyLinesResult::Resync;
        }
        lines.splice(first..last, replacement);
    }
    if let Some(tick) = tick {
        *mirror_tick = tick;
    }
    ApplyLinesResult::Applied
}

/// バッファのchangedtickと全行を、nvimの同一メインループステップで原子取得する。
///
/// `get_lines` 単独の再同期は、要求処理時点までに進んだ後続編集を先取りして返すため、
/// 通知キューに残った古い `nvim_buf_lines_event` がその上へ二重適用される
/// (行の重複・欠落としてsnapshotへ現れる)。tickを同時に取り、以後 `tick` 以下の
/// イベントをStaleとして捨てることで整合を保つ。
async fn fetch_buffer_lines_with_tick(nvim: &Nvim) -> anyhow::Result<(u64, Vec<EditorLine>)> {
    let calls = vec![
        atomic_call("nvim_buf_get_changedtick", vec![Value::from(0)]),
        atomic_call(
            "nvim_buf_get_lines",
            vec![
                Value::from(0),
                Value::from(0),
                Value::from(-1),
                Value::from(false),
            ],
        ),
    ];
    let response = nvim
        .call_atomic(calls)
        .await
        .map_err(|error| anyhow::anyhow!("nvim_call_atomic failed: {error}"))?;
    let results = response
        .first()
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Invalid results format from nvim_call_atomic"))?;
    if let Some(error) = response.get(1).filter(|value| !value.is_nil()) {
        anyhow::bail!("A call inside nvim_call_atomic failed: {error:?}");
    }
    let tick = results
        .first()
        .and_then(value_as_u64)
        .ok_or_else(|| anyhow::anyhow!("Invalid changedtick result format"))?;
    let lines = results
        .get(1)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Invalid lines result format"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(EditorLine::new)
                .ok_or_else(|| anyhow::anyhow!("Invalid line format"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((tick, lines))
}

/// [`handle_redraw`] の結果。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RedrawOutcome {
    /// cmdline系イベントでsnapshot再発行が必要か。
    cmdline_changed: bool,
    /// hit-enter(press-enter)プロンプトを受け取ったか。
    hit_enter_prompt: bool,
}

fn handle_redraw(
    args: &[Value],
    event_tx: &mpsc::UnboundedSender<EditorEvent>,
    cmdline_state: &mut Option<CmdlineState>,
) -> RedrawOutcome {
    let mut outcome = RedrawOutcome::default();
    for batch in args {
        let Some(batch) = batch.as_array() else {
            continue;
        };
        let Some(name) = batch.first().and_then(Value::as_str) else {
            continue;
        };

        match name {
            "cmdline_show" => {
                for update in &batch[1..] {
                    if let Some(state) = parse_cmdline(update) {
                        *cmdline_state = Some(state.clone());
                        let _ = event_tx.send(EditorEvent::CmdlineShow(state));
                        outcome.cmdline_changed = true;
                    }
                }
            }
            "cmdline_pos" => {
                for update in &batch[1..] {
                    let Some(cursor) = update
                        .as_array()
                        .and_then(|fields| fields.first())
                        .and_then(value_as_u64)
                        .map(|cursor| cursor as usize)
                    else {
                        continue;
                    };
                    if let Some(state) = cmdline_state {
                        state.cursor = cursor;
                        let _ = event_tx.send(EditorEvent::CmdlineShow(state.clone()));
                        outcome.cmdline_changed = true;
                    }
                }
            }
            "cmdline_hide" => {
                *cmdline_state = None;
                let _ = event_tx.send(EditorEvent::CmdlineHide);
                outcome.cmdline_changed = true;
            }
            "popupmenu_show" => {
                for update in &batch[1..] {
                    if let Some(fields) = update.as_array()
                        && let Some(state) = parse_popupmenu_show(fields)
                    {
                        let _ = event_tx.send(EditorEvent::PopupmenuShow(state));
                    }
                }
            }
            "popupmenu_select" => {
                for update in &batch[1..] {
                    if let Some(fields) = update.as_array()
                        && let Some(selected) = parse_popupmenu_select(fields)
                    {
                        let _ = event_tx.send(EditorEvent::PopupmenuSelect { selected });
                    }
                }
            }
            "popupmenu_hide" => {
                let _ = event_tx.send(EditorEvent::PopupmenuHide);
            }
            "msg_show" => {
                for update in &batch[1..] {
                    // hit-enter(press-enter)プロンプトはfylerが表示を持つため
                    // 表示せず、呼び出し側が<CR>で即解除する(検索失敗のフリーズ対策)。
                    if is_return_prompt(update) {
                        outcome.hit_enter_prompt = true;
                        continue;
                    }
                    // undo/redoの状態報告("N changes; before #M ...")はファイラーには
                    // ノイズなので表示しない。
                    if is_muted_kind(update) {
                        continue;
                    }
                    if let Some(message) = parse_message(update) {
                        let _ = event_tx.send(EditorEvent::Message(message));
                    }
                }
            }
            _ => {
                // grid系を含む、明示的に有効化していないUIイベントは描画に使わない。
            }
        }
    }
    outcome
}

fn parse_popupmenu_show(args: &[Value]) -> Option<PopupmenuState> {
    let item_values = args.first()?.as_array()?;
    let mut items = Vec::with_capacity(item_values.len());
    for item in item_values {
        let fields = item.as_array()?;
        items.push(PopupmenuItem {
            word: fields
                .first()
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            kind: fields
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            menu: fields
                .get(2)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
    }
    let selected = args
        .get(1)
        .and_then(parse_popupmenu_selected)
        .unwrap_or(None);
    Some(PopupmenuState { items, selected })
}

fn parse_popupmenu_select(args: &[Value]) -> Option<Option<usize>> {
    args.first().and_then(parse_popupmenu_selected)
}

fn parse_popupmenu_selected(value: &Value) -> Option<Option<usize>> {
    let selected = value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))?;
    Some(if selected < 0 {
        None
    } else {
        usize::try_from(selected).ok()
    })
}

/// `msg_show` の更新が hit-enter(press-enter)プロンプトかを返す。
fn is_return_prompt(value: &Value) -> bool {
    value
        .as_array()
        .and_then(|fields| fields.first())
        .and_then(Value::as_str)
        == Some("return_prompt")
}

/// `msg_show` の更新が、ファイラーで表示不要なノイズ種別かを返す。
///
/// 現状は undo/redo の状態報告(`kind == "undo"`)を抑制する。
fn is_muted_kind(value: &Value) -> bool {
    matches!(
        value
            .as_array()
            .and_then(|fields| fields.first())
            .and_then(Value::as_str),
        Some("undo")
    )
}

fn parse_cmdline(value: &Value) -> Option<CmdlineState> {
    let fields = value.as_array()?;
    let content = chunks_text(fields.first()?)?;
    let cursor = fields.get(1).and_then(value_as_u64)? as usize;
    let first_char = fields.get(2).and_then(Value::as_str).unwrap_or_default();
    let prompt_text = fields.get(3).and_then(Value::as_str).unwrap_or_default();
    let prompt = first_char
        .chars()
        .next()
        .or_else(|| prompt_text.chars().next())
        .unwrap_or(':');

    Some(CmdlineState {
        prompt,
        content,
        cursor,
    })
}

fn parse_message(value: &Value) -> Option<EditorMessage> {
    let fields = value.as_array()?;
    let kind_name = fields.first()?.as_str().unwrap_or_default();
    let mut text = chunks_text(fields.get(1)?)?;
    if text.is_empty() {
        return None;
    }
    if kind_name == "search_count" {
        remove_search_wrap_marker(&mut text);
        return Some(EditorMessage {
            kind: MessageKind::Search,
            text,
        });
    }

    let kind = match kind_name {
        "echoerr" | "emsg" | "lua_error" | "rpc_error" => MessageKind::Error,
        "wmsg" | "warningmsg" => MessageKind::Warn,
        _ => MessageKind::Info,
    };
    Some(EditorMessage { kind, text })
}

fn remove_search_wrap_marker(text: &mut String) {
    let Some(count_start) = text.rfind('[') else {
        return;
    };
    let prefix = text[..count_start].trim_end();
    let Some(without_marker) = prefix.strip_suffix('W') else {
        return;
    };
    if without_marker.ends_with(char::is_whitespace) {
        text.replace_range(without_marker.len()..count_start, "");
    }
}

fn chunks_text(value: &Value) -> Option<String> {
    let chunks = value.as_array()?;
    let mut text = String::new();
    for chunk in chunks {
        let fields = chunk.as_array()?;
        text.push_str(fields.get(1)?.as_str()?);
    }
    Some(text)
}

fn map_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.as_map()?.iter().find_map(|(map_key, map_value)| {
        (map_key.as_str() == Some(key))
            .then(|| map_value.as_str())
            .flatten()
    })
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| number.try_into().ok()))
}

fn fold_op_from_str(op: &str) -> Option<FoldOp> {
    match op {
        "close" => Some(FoldOp::Close),
        "open" => Some(FoldOp::Open),
        "toggle" => Some(FoldOp::Toggle),
        "close_rec" => Some(FoldOp::CloseRecursive),
        "open_rec" => Some(FoldOp::OpenRecursive),
        "close_all" => Some(FoldOp::CloseAll),
        "open_all" => Some(FoldOp::OpenAll),
        _ => None,
    }
}

fn parse_pane_action(action: &str) -> Option<PaneAction> {
    match action {
        "split_horizontal" => Some(PaneAction::SplitHorizontal),
        "split_vertical" => Some(PaneAction::SplitVertical),
        "focus_left" => Some(PaneAction::FocusLeft),
        "focus_right" => Some(PaneAction::FocusRight),
        "focus_up" => Some(PaneAction::FocusUp),
        "focus_down" => Some(PaneAction::FocusDown),
        "focus_next" => Some(PaneAction::FocusNext),
        "focus_previous" => Some(PaneAction::FocusPrevious),
        "close" => Some(PaneAction::Close),
        _ => None,
    }
}

fn parse_transfer_kind(kind: &str) -> Option<TransferKind> {
    match kind {
        "move" => Some(TransferKind::Move),
        "copy" => Some(TransferKind::Copy),
        _ => None,
    }
}

fn normalize_mode(mode: &str) -> Mode {
    if mode.starts_with("no") {
        Mode::OperatorPending
    } else if mode.starts_with('n') {
        Mode::Normal
    } else if mode.starts_with('i') {
        Mode::Insert
    } else if mode.starts_with('R') {
        Mode::Replace
    } else if mode.starts_with('V') {
        Mode::VisualLine
    } else if mode.starts_with('\u{16}') {
        Mode::VisualBlock
    } else if mode.starts_with('v') || mode.starts_with('s') {
        Mode::Visual
    } else if mode.starts_with('c') {
        Mode::Cmdline
    } else {
        Mode::Other(mode.to_owned())
    }
}

fn send_message(event_tx: &mpsc::UnboundedSender<EditorEvent>, kind: MessageKind, text: String) {
    let _ = event_tx.send(EditorEvent::Message(EditorMessage { kind, text }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_notice_coalesces_until_acknowledged() {
        let notice = AtomicBool::new(false);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        send_snapshot_notice(&event_tx, &notice);
        send_snapshot_notice(&event_tx, &notice);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(EditorEvent::SnapshotUpdated)
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        notice.store(false, Ordering::Release);
        send_snapshot_notice(&event_tx, &notice);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(EditorEvent::SnapshotUpdated)
        ));
    }

    #[test]
    fn name_start_column_skips_id_prefix_and_indent() {
        // IDプレフィックスのみ(インデント無し)。
        assert_eq!(name_start_column("/012 src/"), 5);
        // IDプレフィックス + インデント(タブ2つ)。
        assert_eq!(name_start_column("/013 \t\tmain.rs"), 7);
        // 未保存行(IDプレフィックス無し)はインデントだけ飛ばす。
        assert_eq!(name_start_column("\tchild.txt"), 1);
        assert_eq!(name_start_column("root.txt"), 0);
    }

    #[test]
    fn arc_swap_releases_the_old_snapshot_generation() {
        let first = Arc::new(EditorSnapshot::empty());
        let weak = Arc::downgrade(&first);
        let shared = ArcSwap::from(Arc::clone(&first));
        drop(first);

        shared.store(Arc::new(EditorSnapshot::empty()));

        assert!(
            weak.upgrade().is_none(),
            "engine must not retain an old snapshot after publishing its replacement"
        );
    }

    #[test]
    fn mode_names_are_normalized_without_leaking_engine_terms() {
        assert_eq!(normalize_mode("n"), Mode::Normal);
        assert_eq!(normalize_mode("no"), Mode::OperatorPending);
        assert_eq!(normalize_mode("i"), Mode::Insert);
        assert_eq!(normalize_mode("R"), Mode::Replace);
        assert_eq!(normalize_mode("v"), Mode::Visual);
        assert_eq!(normalize_mode("V"), Mode::VisualLine);
        assert_eq!(normalize_mode("\u{16}"), Mode::VisualBlock);
        assert_eq!(normalize_mode("c"), Mode::Cmdline);
        assert_eq!(normalize_mode("mystery"), Mode::Other("mystery".to_owned()));
    }

    #[test]
    fn cursor_line_is_clamped_to_the_last_buffer_line() {
        assert_eq!(clamp_cursor_line(0, 3), 0);
        assert_eq!(clamp_cursor_line(1, 3), 1);
        assert_eq!(clamp_cursor_line(99, 3), 2);
        assert_eq!(clamp_cursor_line(99, 0), 0);
    }

    #[test]
    fn transfer_kinds_are_normalized_without_leaking_rpc_strings() {
        assert_eq!(parse_transfer_kind("move"), Some(TransferKind::Move));
        assert_eq!(parse_transfer_kind("copy"), Some(TransferKind::Copy));
        assert_eq!(parse_transfer_kind("unknown"), None);
    }

    #[test]
    fn line_notification_applies_incremental_replacement() {
        let mut lines = vec![
            EditorLine::new("a"),
            EditorLine::new("b"),
            EditorLine::new("c"),
        ];
        let args = vec![
            Value::Nil,
            Value::from(2),
            Value::from(1),
            Value::from(2),
            Value::Array(vec![Value::from("B"), Value::from("B2")]),
            Value::from(false),
        ];
        let mut mirror_tick = 0;
        assert_eq!(
            apply_lines_notification(&args, &mut lines, &mut mirror_tick),
            ApplyLinesResult::Applied
        );
        assert_eq!(mirror_tick, 2);
        assert_eq!(
            lines,
            [
                EditorLine::new("a"),
                EditorLine::new("B"),
                EditorLine::new("B2"),
                EditorLine::new("c"),
            ]
        );
    }

    #[test]
    fn line_notification_at_or_below_mirror_tick_is_stale() {
        // 全再同期がtick=5まで反映済みのミラーに、キュー滞留していた
        // tick=5以下のイベントが届いても二重適用しない(行重複の回帰テスト)。
        let mut lines = vec![EditorLine::new("a"), EditorLine::new("b")];
        let args = vec![
            Value::Nil,
            Value::from(5),
            Value::from(1),
            Value::from(1),
            Value::Array(vec![Value::from("b")]),
            Value::from(false),
        ];
        let mut mirror_tick = 5;
        assert_eq!(
            apply_lines_notification(&args, &mut lines, &mut mirror_tick),
            ApplyLinesResult::Stale
        );
        assert_eq!(lines, [EditorLine::new("a"), EditorLine::new("b")]);
        assert_eq!(mirror_tick, 5);
    }

    #[test]
    fn line_notification_with_negative_lastline_replaces_the_whole_buffer() {
        // attach(send_buffer=true)等の lastline == -1 は全バッファ差し替え。
        // 以前はu64パース失敗でget_lines再同期へ落ち、後続編集の先取りと
        // キュー内イベントの二重適用を引き起こしていた。
        let mut lines = vec![EditorLine::new("stale")];
        let args = vec![
            Value::Nil,
            Value::from(2),
            Value::from(0),
            Value::from(-1),
            Value::Array(vec![Value::from("a"), Value::from("b")]),
            Value::from(false),
        ];
        let mut mirror_tick = 0;
        assert_eq!(
            apply_lines_notification(&args, &mut lines, &mut mirror_tick),
            ApplyLinesResult::Applied
        );
        assert_eq!(lines, [EditorLine::new("a"), EditorLine::new("b")]);
        assert_eq!(mirror_tick, 2);
    }

    #[test]
    fn malformed_line_notification_requests_resync() {
        let mut lines = vec![EditorLine::new("a")];
        // 範囲がミラー長を超える(ミラーが既に不整合)。
        let args = vec![
            Value::Nil,
            Value::from(3),
            Value::from(5),
            Value::from(6),
            Value::Array(vec![Value::from("x")]),
            Value::from(false),
        ];
        let mut mirror_tick = 0;
        assert_eq!(
            apply_lines_notification(&args, &mut lines, &mut mirror_tick),
            ApplyLinesResult::Resync
        );
        // 適用失敗時はtickを進めない(再同期側で確定する)。
        assert_eq!(mirror_tick, 0);
    }

    #[test]
    fn single_line_edit_shares_unchanged_line_payloads() {
        let mut lines = (1..=50)
            .map(|index| EditorLine::new(format!("/{index:05} name_{index:05}.txt")))
            .collect::<Vec<_>>();
        let before = lines_to_arc(&lines);
        let args = vec![
            Value::Nil,
            Value::from(2),
            Value::from(24),
            Value::from(25),
            Value::Array(vec![Value::from("/00025 renamed_00025.txt")]),
            Value::from(false),
        ];

        let mut mirror_tick = 0;
        assert_eq!(
            apply_lines_notification(&args, &mut lines, &mut mirror_tick),
            ApplyLinesResult::Applied
        );
        let after = lines_to_arc(&lines);

        for index in 0..50 {
            assert_eq!(
                Arc::ptr_eq(&before[index].text, &after[index].text),
                index != 24,
                "payload sharing mismatch at line {index}"
            );
        }
    }

    #[test]
    #[ignore = "environment-dependent performance measurement with 50k lines"]
    fn bench_single_line_edit_snapshot_rebuild_on_50k_lines() {
        let mut lines = (1..=50_000)
            .map(|index| EditorLine::new(format!("/{index:05} name_{index:05}.txt")))
            .collect::<Vec<_>>();
        let args = vec![
            Value::Nil,
            Value::from(2),
            Value::from(25_000),
            Value::from(25_001),
            Value::Array(vec![Value::from("/25001 renamed_25001.txt")]),
            Value::from(false),
        ];
        let started = std::time::Instant::now();

        for _ in 0..100 {
            let mut mirror_tick = 0;
            assert_eq!(
                apply_lines_notification(&args, &mut lines, &mut mirror_tick),
                ApplyLinesResult::Applied
            );
            std::hint::black_box(lines_to_arc(&lines));
        }

        let elapsed = started.elapsed();
        eprintln!(
            "50k single-line edit + snapshot rebuild x100: {elapsed:?} ({:?} per iteration)",
            elapsed / 100
        );
    }

    #[test]
    fn popupmenu_show_parser_extracts_items_and_selection() {
        let args = vec![
            Value::Array(vec![
                Value::Array(vec![
                    Value::from("name"),
                    Value::from("sort"),
                    Value::from("default"),
                    Value::from("ignored"),
                ]),
                Value::Array(vec![
                    Value::from("date"),
                    Value::from("sort"),
                    Value::from("mtime"),
                    Value::from("ignored"),
                ]),
            ]),
            Value::from(1),
            Value::from(0),
            Value::from(0),
            Value::from(1),
        ];

        let state = parse_popupmenu_show(&args).unwrap();

        assert_eq!(state.selected, Some(1));
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[1].word, "date");
        assert_eq!(state.items[1].kind, "sort");
        assert_eq!(state.items[1].menu, "mtime");
    }

    #[test]
    fn popupmenu_show_parser_maps_minus_one_to_no_selection() {
        let args = vec![
            Value::Array(vec![Value::Array(vec![Value::from("date")])]),
            Value::from(-1),
        ];

        let state = parse_popupmenu_show(&args).unwrap();

        assert_eq!(state.selected, None);
        assert_eq!(state.items[0].word, "date");
        assert_eq!(state.items[0].kind, "");
        assert_eq!(state.items[0].menu, "");
    }

    #[test]
    fn popupmenu_show_parser_rejects_missing_items_array() {
        assert_eq!(parse_popupmenu_show(&[]), None);
        assert_eq!(parse_popupmenu_show(&[Value::from("not-items")]), None);
    }

    #[test]
    fn popupmenu_select_parser_maps_selection() {
        assert_eq!(parse_popupmenu_select(&[Value::from(2)]), Some(Some(2)));
        assert_eq!(parse_popupmenu_select(&[Value::from(-1)]), Some(None));
        assert_eq!(parse_popupmenu_select(&[]), None);
    }

    #[test]
    fn search_count_message_drops_gui_icon_kind_and_wrap_marker() {
        let value = Value::Array(vec![
            Value::from("search_count"),
            Value::Array(vec![Value::Array(vec![
                Value::from(0),
                Value::from("/test W [1/3]"),
            ])]),
        ]);

        assert_eq!(
            parse_message(&value),
            Some(EditorMessage {
                kind: MessageKind::Search,
                text: "/test [1/3]".to_owned(),
            })
        );
    }

    #[test]
    fn hit_enter_prompt_is_detected_and_dismissed_not_shown() {
        let update = Value::Array(vec![
            Value::from("return_prompt"),
            Value::Array(vec![Value::Array(vec![
                Value::from(0),
                Value::from("Press ENTER or type command to continue"),
            ])]),
        ]);
        assert!(is_return_prompt(&update));

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let mut cmdline_state = None;
        let args = vec![Value::Array(vec![Value::from("msg_show"), update])];
        let outcome = handle_redraw(&args, &event_tx, &mut cmdline_state);
        assert!(outcome.hit_enter_prompt);
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn ordinary_error_message_is_shown_without_hit_enter() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let mut cmdline_state = None;
        let args = vec![Value::Array(vec![
            Value::from("msg_show"),
            Value::Array(vec![
                Value::from("emsg"),
                Value::Array(vec![Value::Array(vec![
                    Value::from(0),
                    Value::from("E486: Pattern not found: zzz"),
                ])]),
            ]),
        ])];
        let outcome = handle_redraw(&args, &event_tx, &mut cmdline_state);
        assert!(!outcome.hit_enter_prompt);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(EditorEvent::Message(EditorMessage {
                kind: MessageKind::Error,
                ..
            }))
        ));
    }

    #[test]
    fn undo_report_message_is_muted() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let mut cmdline_state = None;
        let args = vec![Value::Array(vec![
            Value::from("msg_show"),
            Value::Array(vec![
                Value::from("undo"),
                Value::Array(vec![Value::Array(vec![
                    Value::from(0),
                    Value::from("3 changes; before #4  69 seconds ago"),
                ])]),
            ]),
        ])];
        let outcome = handle_redraw(&args, &event_tx, &mut cmdline_state);
        assert!(!outcome.hit_enter_prompt);
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn pane_notifications_map_to_every_core_action() {
        for (raw, expected) in [
            ("split_horizontal", PaneAction::SplitHorizontal),
            ("split_vertical", PaneAction::SplitVertical),
            ("focus_left", PaneAction::FocusLeft),
            ("focus_right", PaneAction::FocusRight),
            ("focus_up", PaneAction::FocusUp),
            ("focus_down", PaneAction::FocusDown),
            ("focus_next", PaneAction::FocusNext),
            ("focus_previous", PaneAction::FocusPrevious),
            ("close", PaneAction::Close),
        ] {
            assert_eq!(parse_pane_action(raw), Some(expected));
        }
        assert_eq!(parse_pane_action("unknown"), None);
    }
}
