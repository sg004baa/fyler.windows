//! NvimEngine本体: snapshot + command channel型の `EditorEngine` 実装。

use std::sync::Arc;

use arc_swap::ArcSwap;
use fyler_core::editor::{
    CmdlineState, Cursor, EditorCommand, EditorEngine, EditorEvent, EditorLine, EditorMessage,
    EditorSnapshot, MessageKind, Mode, SearchHighlight,
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
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
}

impl EditorEngine for NvimEngine {
    fn send(&self, cmd: EditorCommand) -> anyhow::Result<()> {
        self.cmd_tx
            .send(EngineCommand::Editor(cmd))
            .map_err(|_| anyhow::anyhow!("NvimEngineタスクが終了しています"))
    }

    fn snapshot(&self) -> Arc<EditorSnapshot> {
        self.snapshot.load_full()
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
            .map_err(|_| anyhow::anyhow!("NvimEngineタスクが終了しています"))
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
    ///    有効化するextは `ext_cmdline` と `ext_messages` の2つだけ
    ///    (ext_messagesがないと `E486` 等がgridに描かれて見えない)。
    ///    grid系描画イベントはすべて無視。`ext_popupmenu` は補完UI実装時に追加
    /// 5. **事故防止**: [`crate::guard`] のautocmd・remapを導入
    /// 6. **コマンド処理**: `cmd_rx` から受けた [`EditorCommand`] を処理する。
    ///    `Key` は [`crate::translate::to_nvim_keycodes`] → `nvim_input`、
    ///    `Text` は `nvim_input` でなくバッファ挿入APIまたは `nvim_paste`(M0で検証)、
    ///    `Paste` は `nvim_paste`、`RequestCommit` は `:w` 相当、
    ///    `Undo`/`Redo` は `u`/`<C-r>` 相当
    /// 7. **イベント**: BufWriteCmd等のrpcnotify → `EditorEvent::CommitRequested`、
    ///    行アクション → `ActivateLine` / `YankPath` / `NavigateInto` / `NavigateParent` /
    ///    `ToggleHidden`、ルート選択 → `ChangeDirectory` / `JumpBookmark`、
    ///    ext_cmdline → `CmdlineShow/CmdlineHide`、
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
                        "Neovimを起動できません ({}): {error}",
                        config.nvim_exe.display()
                    )
                })?;

        let api_info = nvim
            .get_api_info()
            .await
            .map_err(|error| anyhow::anyhow!("Neovim RPC疎通に失敗しました: {error}"))?;
        let channel_id = api_info
            .first()
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("Neovim channel IDを取得できません"))?;

        let buffer = nvim
            .get_current_buf()
            .await
            .map_err(|error| anyhow::anyhow!("初期バッファを取得できません: {error}"))?;
        // 名前設定時に架空URIのswapfileを作ろうとしないよう、先に無効化する。
        buffer
            .set_option("swapfile", Value::Boolean(false))
            .await
            .map_err(|error| anyhow::anyhow!("swapfileを無効化できません: {error}"))?;
        buffer
            .set_option("buftype", Value::from(guard::BUFTYPE))
            .await
            .map_err(|error| anyhow::anyhow!("buftypeを設定できません: {error}"))?;
        let buffer_uri = format!(
            "{}{}",
            guard::BUFFER_URI_SCHEME,
            config.root.to_string_lossy().replace('\\', "/")
        );
        buffer
            .set_name(&buffer_uri)
            .await
            .map_err(|error| anyhow::anyhow!("バッファ名を設定できません: {error}"))?;

        let mut ui_options = UiAttachOptions::new();
        ui_options.set_cmdline_external(true);
        // nvim-rs 0.9.2ではこのメソッド名が`messages_externa`になっている。
        ui_options.set_messages_externa(true);
        nvim.ui_attach(80, 24, &ui_options)
            .await
            .map_err(|error| anyhow::anyhow!("Neovim UI attachに失敗しました: {error}"))?;

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
                .map_err(|error| anyhow::anyhow!("{name}オプションを設定できません: {error}"))?;
        }

        guard::install_guards(&nvim, &buffer, channel_id).await?;

        let attached = buffer
            .attach(true, Vec::new())
            .await
            .map_err(|error| anyhow::anyhow!("バッファ通知をattachできません: {error}"))?;
        if !attached {
            anyhow::bail!("Neovimがバッファ通知のattachを拒否しました");
        }

        let mut lines = buffer
            .get_lines(0, -1, false)
            .await
            .map_err(|error| anyhow::anyhow!("初期バッファ行を取得できません: {error}"))?;
        let mut lines_arc = lines_to_arc(&lines);
        let status = query_status(&nvim).await?;
        let mut revision = 1;
        let initial_snapshot = build_snapshot(revision, &lines_arc, status, None);
        let shared_snapshot = Arc::new(ArcSwap::from_pointee(initial_snapshot));

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let task_snapshot = Arc::clone(&shared_snapshot);

        tokio::spawn(async move {
            let mut cmdline_state = None;
            let mut snapshot_pending = false;
            let mut commit_pending = false;
            let crash_reason = loop {
                tokio::select! {
                    child_status = child.wait() => {
                        break match child_status {
                            Ok(status) => format!("Neovimプロセスが終了しました: {status}"),
                            Err(error) => format!("Neovimプロセスの終了監視に失敗しました: {error}"),
                        };
                    }
                    io_result = &mut io_task => {
                        break match io_result {
                            Ok(Ok(())) => "Neovim RPC接続が終了しました".to_owned(),
                            Ok(Err(error)) => format!("Neovim RPC接続が異常終了しました: {error}"),
                            Err(error) => format!("Neovim RPC監視タスクが異常終了しました: {error}"),
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
                                format!("エディタ入力に失敗しました: {error}"),
                            );
                        }
                        snapshot_pending = true;
                        match publish_pending_snapshot(
                            &nvim,
                            &lines_arc,
                            &mut revision,
                            &task_snapshot,
                            &event_tx,
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
                                    format!("エディタ状態を更新できません: {error}"),
                                );
                            }
                        }
                    }
                    notification = notification_rx.recv() => {
                        let Some(notification) = notification else {
                            break "Neovim通知チャネルが終了しました".to_owned();
                        };

                        match notification.name.as_str() {
                            "nvim_buf_lines_event" => {
                                if apply_lines_notification(&notification.args, &mut lines) {
                                    lines_arc = lines_to_arc(&lines);
                                } else {
                                    match buffer.get_lines(0, -1, false).await {
                                        Ok(current_lines) => {
                                            lines = current_lines;
                                            lines_arc = lines_to_arc(&lines);
                                        }
                                        Err(error) => {
                                            send_message(
                                                &event_tx,
                                                MessageKind::Error,
                                                format!("バッファ行の再同期に失敗しました: {error}"),
                                            );
                                            continue;
                                        }
                                    }
                                }
                                snapshot_pending = true;
                            }
                            "nvim_buf_detach_event" => {
                                break "fylerバッファがNeovimからdetachされました".to_owned();
                            }
                            "redraw" => {
                                // cmdline系イベントが来たら検索パターンが変わりうるので
                                // snapshotを再発行する(incsearchプレビュー追従)。
                                if handle_redraw(
                                    &notification.args,
                                    &event_tx,
                                    &mut cmdline_state,
                                ) {
                                    snapshot_pending = true;
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
                                        "開く対象の行番号を取得できません".to_owned(),
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
                                        "移動対象の行番号を取得できません".to_owned(),
                                    ),
                                }
                            }
                            "fyler_toggle_hidden" => {
                                let _ = event_tx.send(EditorEvent::ToggleHidden);
                            }
                            "fyler_open_picker" => {
                                let _ = event_tx.send(EditorEvent::OpenFilePicker);
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
                                        "このpane操作は無効です".to_owned(),
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
                                        "transfer対象の行範囲を取得できません".to_owned(),
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
                                        "コピー対象の行番号を取得できません".to_owned(),
                                    ),
                                }
                            }
                            "fyler_action_blocked" => {
                                send_message(
                                    &event_tx,
                                    MessageKind::Info,
                                    "このキーは無効です".to_owned(),
                                );
                            }
                            "fyler_write_blocked" => {
                                let event = notification.args.first()
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                send_message(
                                    &event_tx,
                                    MessageKind::Error,
                                    format!("未対応の保存経路を拒否しました: {event}"),
                                );
                            }
                            "fyler_unexpected_buffer" => {
                                send_message(
                                    &event_tx,
                                    MessageKind::Warn,
                                    "想定外のバッファを閉じ、fylerへ戻りました".to_owned(),
                                );
                            }
                            _ => {}
                        }

                        match publish_pending_snapshot(
                            &nvim,
                            &lines_arc,
                            &mut revision,
                            &task_snapshot,
                            &event_tx,
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
                                    format!("エディタ状態を更新できません: {error}"),
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
                .map_err(|error| anyhow::anyhow!("キー入力RPCに失敗しました: {error}"))?;
        }
        EngineCommand::Editor(EditorCommand::Text(text))
        | EngineCommand::Editor(EditorCommand::Paste(text)) => {
            nvim.paste(&text, false, -1)
                .await
                .map_err(|error| anyhow::anyhow!("テキスト入力RPCに失敗しました: {error}"))?;
        }
        EngineCommand::Editor(EditorCommand::SetLines { lines, cursor_line }) => {
            replace_buffer_lines(nvim, buffer, lines, cursor_line, "reconcile").await?;
        }
        EngineCommand::Editor(EditorCommand::SetCursorLine(line)) => {
            set_cursor_line(nvim, buffer, line).await?;
        }
        EngineCommand::Editor(EditorCommand::SetModifiable(value)) => {
            set_buffer_modifiable(nvim, buffer, value, "保存フロー").await?;
        }
        EngineCommand::Editor(EditorCommand::RequestCommit) => {
            nvim.command("write")
                .await
                .map_err(|error| anyhow::anyhow!("保存要求RPCに失敗しました: {error}"))?;
        }
        EngineCommand::Editor(EditorCommand::Undo) => {
            nvim.input("u")
                .await
                .map_err(|error| anyhow::anyhow!("undo RPCに失敗しました: {error}"))?;
        }
        EngineCommand::Editor(EditorCommand::Redo) => {
            nvim.input("<C-r>")
                .await
                .map_err(|error| anyhow::anyhow!("redo RPCに失敗しました: {error}"))?;
        }
        EngineCommand::SetInitialLines(editor_lines) => {
            replace_buffer_lines(nvim, buffer, editor_lines, None, "初期化").await?;
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
    let line_count = buffer
        .line_count()
        .await
        .map_err(|error| anyhow::anyhow!("カーソル移動対象の行数を取得できません: {error}"))?;
    let target_line = clamp_cursor_line(requested_line, line_count);
    let line = buffer
        .get_lines(target_line, target_line + 1, false)
        .await
        .map_err(|error| anyhow::anyhow!("カーソル移動対象の行を取得できません: {error}"))?
        .into_iter()
        .next()
        .unwrap_or_default();
    let target_column =
        i64::try_from(fyler_core::grammar::id_prefix_len(&line)).unwrap_or(i64::MAX);
    let window = nvim.get_current_win().await.map_err(|error| {
        anyhow::anyhow!("カーソル移動対象のウィンドウを取得できません: {error}")
    })?;
    window
        .set_cursor((target_line + 1, target_column))
        .await
        .map_err(|error| anyhow::anyhow!("カーソルを設定できません: {error}"))?;
    Ok(())
}

fn clamp_cursor_line(requested_line: usize, line_count: i64) -> i64 {
    let requested_line = i64::try_from(requested_line).unwrap_or(i64::MAX);
    requested_line.min(line_count.saturating_sub(1)).max(0)
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
        editor_lines.into_iter().map(|line| line.text).collect()
    };
    buffer
        .set_lines(0, -1, false, new_lines.clone())
        .await
        .map_err(|error| anyhow::anyhow!("{purpose}のバッファ行を設定できません: {error}"))?;
    buffer
        .set_option("modified", Value::Boolean(false))
        .await
        .map_err(|error| anyhow::anyhow!("{purpose}後にバッファをcleanにできません: {error}"))?;

    // 折りたたみトグル等では操作した行へカーソルを戻す。行数を超える指定は
    // 最終行へクランプする(nvimのset_cursorは範囲外でエラーになるため)。
    let target_line = cursor_line
        .unwrap_or(0)
        .min(new_lines.len().saturating_sub(1));
    let target_column = new_lines
        .get(target_line)
        .map(|line| fyler_core::grammar::id_prefix_len(line))
        .unwrap_or_default();
    let window = nvim
        .get_current_win()
        .await
        .map_err(|error| anyhow::anyhow!("{purpose}対象のウィンドウを取得できません: {error}"))?;
    window
        .set_cursor((target_line as i64 + 1, target_column as i64))
        .await
        .map_err(|error| anyhow::anyhow!("{purpose}後のカーソルを設定できません: {error}"))?;

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
    .map_err(|error| anyhow::anyhow!("{purpose}のバッファmodifiable設定を変更できません: {error}"))
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
        .map_err(|error| anyhow::anyhow!("nvim_call_atomicに失敗しました: {error}"))?;

    let results = response
        .first()
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("nvim_call_atomicのresults形式が不正です"))?;
    if let Some(error) = response.get(1).filter(|value| !value.is_nil()) {
        anyhow::bail!("nvim_call_atomic内の呼び出しに失敗しました: {error:?}");
    }
    if results.len() != 6 {
        anyhow::bail!(
            "nvim_call_atomicのresults件数が不正です: expected 6, got {}",
            results.len()
        );
    }

    let mode_name = map_string(&results[0], "mode")
        .ok_or_else(|| anyhow::anyhow!("mode結果の形式が不正です"))?;
    let cursor_values = results[1]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("cursor結果の形式が不正です"))?;
    let row = cursor_values
        .first()
        .and_then(value_as_u64)
        .ok_or_else(|| anyhow::anyhow!("cursor rowの形式が不正です"))?;
    let col = cursor_values
        .get(1)
        .and_then(value_as_u64)
        .ok_or_else(|| anyhow::anyhow!("cursor colの形式が不正です"))?;
    let changedtick = value_as_u64(&results[2])
        .ok_or_else(|| anyhow::anyhow!("changedtick結果の形式が不正です"))?;
    let dirty = results[3]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("modified結果の形式が不正です"))?;
    let mode = normalize_mode(mode_name);
    let visual_start = if matches!(&mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
        let position = results[4]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("visual start結果の形式が不正です"))?;
        let line = position
            .get(1)
            .and_then(value_as_u64)
            .ok_or_else(|| anyhow::anyhow!("visual start lineの形式が不正です"))?;
        let col = position
            .get(2)
            .and_then(value_as_u64)
            .ok_or_else(|| anyhow::anyhow!("visual start colの形式が不正です"))?;
        Some(Cursor {
            line: line.saturating_sub(1) as usize,
            col: col.saturating_sub(1) as usize,
        })
    } else {
        None
    };

    let search_values = results[5]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("検索状態結果の形式が不正です"))?;
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

fn lines_to_arc(lines: &[String]) -> Arc<[EditorLine]> {
    Arc::from(
        lines
            .iter()
            .cloned()
            .map(EditorLine::new)
            .collect::<Vec<_>>(),
    )
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
    event_tx: &mpsc::UnboundedSender<EditorEvent>,
    snapshot_pending: &mut bool,
    cmdline: Option<&CmdlineState>,
) -> anyhow::Result<Option<Arc<EditorSnapshot>>> {
    if !*snapshot_pending {
        return Ok(None);
    }

    let snapshot =
        publish_snapshot(nvim, lines, revision, shared_snapshot, event_tx, cmdline).await?;
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
    event_tx: &mpsc::UnboundedSender<EditorEvent>,
    cmdline: Option<&CmdlineState>,
) -> anyhow::Result<Option<Arc<EditorSnapshot>>> {
    if input_is_blocking(nvim).await? {
        return Ok(None);
    }

    let status = query_status(nvim).await?;
    *revision = revision.saturating_add(1);
    let snapshot = Arc::new(build_snapshot(*revision, lines, status, cmdline));
    shared_snapshot.store(Arc::clone(&snapshot));
    let _ = event_tx.send(EditorEvent::SnapshotUpdated);
    Ok(Some(snapshot))
}

/// 追加入力待ちかをfast APIだけで判定する。
///
/// ここでdeferred APIを呼ぶと、text objectや1文字引数の入力待ち中にコマンドループ
/// 自身が停止するため、状態一括取得は必ずこのゲートの後で行う。
async fn input_is_blocking(nvim: &Nvim) -> anyhow::Result<bool> {
    let mode = nvim
        .get_mode()
        .await
        .map_err(|error| anyhow::anyhow!("入力待ち状態を取得できません: {error}"))?;
    mode.iter()
        .find_map(|(key, value)| {
            (key.as_str() == Some("blocking"))
                .then(|| value.as_bool())
                .flatten()
        })
        .ok_or_else(|| anyhow::anyhow!("入力待ち状態の形式が不正です"))
}

fn apply_lines_notification(args: &[Value], lines: &mut Vec<String>) -> bool {
    let Some(first) = args
        .get(2)
        .and_then(value_as_u64)
        .map(|value| value as usize)
    else {
        return false;
    };
    let Some(last) = args
        .get(3)
        .and_then(value_as_u64)
        .map(|value| value as usize)
    else {
        return false;
    };
    let Some(replacement_values) = args.get(4).and_then(Value::as_array) else {
        return false;
    };
    if first > last || last > lines.len() {
        return false;
    }

    let mut replacement = Vec::with_capacity(replacement_values.len());
    for value in replacement_values {
        let Some(line) = value.as_str() else {
            return false;
        };
        replacement.push(line.to_owned());
    }
    lines.splice(first..last, replacement);
    true
}

fn handle_redraw(
    args: &[Value],
    event_tx: &mpsc::UnboundedSender<EditorEvent>,
    cmdline_state: &mut Option<CmdlineState>,
) -> bool {
    let mut cmdline_changed = false;
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
                        cmdline_changed = true;
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
                        cmdline_changed = true;
                    }
                }
            }
            "cmdline_hide" => {
                *cmdline_state = None;
                let _ = event_tx.send(EditorEvent::CmdlineHide);
                cmdline_changed = true;
            }
            "msg_show" => {
                for update in &batch[1..] {
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
    cmdline_changed
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
    let text = chunks_text(fields.get(1)?)?;
    if text.is_empty() {
        return None;
    }

    let kind = match kind_name {
        "echoerr" | "emsg" | "lua_error" | "rpc_error" => MessageKind::Error,
        "wmsg" | "warningmsg" => MessageKind::Warn,
        _ => MessageKind::Info,
    };
    Some(EditorMessage { kind, text })
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
        let mut lines = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let args = vec![
            Value::Nil,
            Value::from(2),
            Value::from(1),
            Value::from(2),
            Value::Array(vec![Value::from("B"), Value::from("B2")]),
            Value::from(false),
        ];
        assert!(apply_lines_notification(&args, &mut lines));
        assert_eq!(lines, ["a", "B", "B2", "c"]);
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
