//! NvimEngine本体: snapshot + command channel型の `EditorEngine` 実装。

use std::sync::Arc;

use arc_swap::ArcSwap;
use fyler_core::editor::{
    CmdlineState, Cursor, EditorCommand, EditorEngine, EditorEvent, EditorLine, EditorMessage,
    EditorSnapshot, MessageKind, Mode,
};
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
    ///    - mode / cursor / changedtick: キー送信ごとに `nvim_call_atomic` で
    ///      一括取得し、[`EditorSnapshot`] として原子的に更新(revisionはRust側で単調増加)
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
    ///    ext_cmdline → `CmdlineShow/CmdlineHide`、ext_messages → `Message`、
    ///    プロセス終了検知 → `EngineCrashed` として `event_tx` へ流す
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
        let status = query_status(&nvim).await?;
        let mut revision = 1;
        let initial_snapshot = build_snapshot(revision, &lines, status);
        let shared_snapshot = Arc::new(ArcSwap::from_pointee(initial_snapshot));

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let task_snapshot = Arc::clone(&shared_snapshot);

        tokio::spawn(async move {
            let mut cmdline_state = None;
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
                        if let Err(error) = publish_snapshot(
                            &nvim,
                            &lines,
                            &mut revision,
                            &task_snapshot,
                            &event_tx,
                        ).await {
                            send_message(
                                &event_tx,
                                MessageKind::Error,
                                format!("エディタ状態を更新できません: {error}"),
                            );
                        }
                    }
                    notification = notification_rx.recv() => {
                        let Some(notification) = notification else {
                            break "Neovim通知チャネルが終了しました".to_owned();
                        };

                        match notification.name.as_str() {
                            "nvim_buf_lines_event" => {
                                if !apply_lines_notification(&notification.args, &mut lines) {
                                    match buffer.get_lines(0, -1, false).await {
                                        Ok(current_lines) => lines = current_lines,
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
                                if let Err(error) = publish_snapshot(
                                    &nvim,
                                    &lines,
                                    &mut revision,
                                    &task_snapshot,
                                    &event_tx,
                                ).await {
                                    send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        format!("エディタ状態を更新できません: {error}"),
                                    );
                                }
                            }
                            "nvim_buf_detach_event" => {
                                break "fylerバッファがNeovimからdetachされました".to_owned();
                            }
                            "redraw" => {
                                handle_redraw(
                                    &notification.args,
                                    &event_tx,
                                    &mut cmdline_state,
                                );
                            }
                            "fyler_commit_requested" => {
                                match publish_snapshot(
                                    &nvim,
                                    &lines,
                                    &mut revision,
                                    &task_snapshot,
                                    &event_tx,
                                ).await {
                                    Ok(snapshot) => {
                                        let _ = event_tx.send(EditorEvent::CommitRequested {
                                            changedtick: snapshot.changedtick,
                                            lines: Arc::clone(&snapshot.lines),
                                        });
                                    }
                                    Err(error) => send_message(
                                        &event_tx,
                                        MessageKind::Error,
                                        format!("保存要求時の状態取得に失敗しました: {error}"),
                                    ),
                                }
                            }
                            "fyler_action_blocked" => {
                                send_message(
                                    &event_tx,
                                    MessageKind::Info,
                                    "M1ではファイルを開きません".to_owned(),
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
        EngineCommand::Editor(EditorCommand::SetLines(editor_lines)) => {
            replace_buffer_lines(nvim, buffer, editor_lines, "reconcile").await?;
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
            replace_buffer_lines(nvim, buffer, editor_lines, "初期化").await?;
        }
    }

    Ok(())
}

async fn replace_buffer_lines(
    nvim: &Nvim,
    buffer: &Buffer<NvimWriter>,
    editor_lines: Vec<EditorLine>,
    purpose: &str,
) -> anyhow::Result<()> {
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

    let first_column = new_lines
        .first()
        .map(|line| fyler_core::grammar::id_prefix_len(line))
        .unwrap_or_default();
    let window = nvim
        .get_current_win()
        .await
        .map_err(|error| anyhow::anyhow!("{purpose}対象のウィンドウを取得できません: {error}"))?;
    window
        .set_cursor((1, first_column as i64))
        .await
        .map_err(|error| anyhow::anyhow!("{purpose}後のカーソルを設定できません: {error}"))?;

    Ok(())
}

#[derive(Debug)]
struct Status {
    changedtick: u64,
    cursor: Cursor,
    mode: Mode,
    dirty: bool,
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
    if results.len() != 4 {
        anyhow::bail!(
            "nvim_call_atomicのresults件数が不正です: expected 4, got {}",
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

    Ok(Status {
        changedtick,
        cursor: Cursor {
            line: row.saturating_sub(1) as usize,
            col: col as usize,
        },
        mode: normalize_mode(mode_name),
        dirty,
    })
}

fn atomic_call(name: &str, args: Vec<Value>) -> Value {
    Value::Array(vec![Value::from(name), Value::Array(args)])
}

fn build_snapshot(revision: u64, lines: &[String], status: Status) -> EditorSnapshot {
    EditorSnapshot {
        revision,
        changedtick: status.changedtick,
        lines: Arc::from(
            lines
                .iter()
                .cloned()
                .map(EditorLine::new)
                .collect::<Vec<_>>(),
        ),
        cursor: status.cursor,
        mode: status.mode,
        dirty: status.dirty,
    }
}

async fn publish_snapshot(
    nvim: &Nvim,
    lines: &[String],
    revision: &mut u64,
    shared_snapshot: &ArcSwap<EditorSnapshot>,
    event_tx: &mpsc::UnboundedSender<EditorEvent>,
) -> anyhow::Result<Arc<EditorSnapshot>> {
    let status = query_status(nvim).await?;
    *revision = revision.saturating_add(1);
    let snapshot = Arc::new(build_snapshot(*revision, lines, status));
    shared_snapshot.store(Arc::clone(&snapshot));
    let _ = event_tx.send(EditorEvent::SnapshotUpdated);
    Ok(snapshot)
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
) {
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
                    }
                }
            }
            "cmdline_hide" => {
                *cmdline_state = None;
                let _ = event_tx.send(EditorEvent::CmdlineHide);
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
}
