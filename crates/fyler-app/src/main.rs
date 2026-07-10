#![cfg_attr(windows, windows_subsystem = "windows")]

//! fyler — エントリポイント。各レイヤーの配線だけを行う(ロジックを書かない)。
//!
//! 各レイヤーの役割はAGENTS.mdの依存境界表を参照。ここに書いてよいのは
//! 「起動」「イベントの受け渡し」「ユーザー操作の各レイヤーへの配線」
//! 「保存状態機械の副作用(SaveEffect)の実行」のみ。

mod config;
mod pane_runtime;
pub mod save_flow;
mod transfer_flow;

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;

use fyler_core::editor::{EditorCommand, EditorEngine, EditorEvent, EditorMessage, MessageKind};
use fyler_core::gitstatus::GitBadge;
use fyler_core::grammar::PrefixParse;
use fyler_core::id::{EntryId, IdAllocator};
use fyler_core::pane::PaneId;
use fyler_core::report::{ApplyProgress, CommitReport};
use fyler_core::transfer::TransferOp;
use fyler_core::tree::EntryKind;
use fyler_fsops::watch::{ExternalChange, FsWatcher};
use fyler_gui::app::{GuiEvent, PickerAction};
use fyler_gui::confirm::ConfirmChoice;

use crate::save_flow::{RevealResult, SaveController, SaveFlowResult, ToggleCollapseResult};

enum AppEvent {
    Editor(PaneId, EditorEvent),
    Confirm(ConfirmChoice),
    PickerSelect {
        pane_id: PaneId,
        entry_id: fyler_core::id::EntryId,
        action: PickerAction,
    },
    ExternalChange(PaneId, ExternalChange),
    GitStatus {
        pane_id: PaneId,
        root: PathBuf,
        statuses: HashMap<PathBuf, GitBadge>,
    },
    ApplyProgress(PaneId, ApplyProgress),
    ApplyFinished(PaneId, CommitReport),
    TransferProgress(ApplyProgress<TransferOp>),
    TransferFinished(CommitReport<TransferOp>),
    Shutdown,
}

/// git statusサブプロセスをappイベントスレッド外で実行し、結果をAppEventで返す。
///
/// paneごとに同時実行1本まで。pane内で再要求されたら完了後に1回だけ再実行する。
struct GitRefresher {
    event_tx: mpsc::Sender<AppEvent>,
    slots: HashMap<PaneId, GitRefreshSlot>,
}

#[derive(Default)]
struct GitRefreshSlot {
    inflight: bool,
    queued: Option<PathBuf>,
}

impl GitRefresher {
    fn new(event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self {
            event_tx,
            slots: HashMap::new(),
        }
    }

    fn request(&mut self, pane_id: PaneId, root: PathBuf) {
        let Some(root) = self.prepare_request(pane_id, root) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        drop(thread::spawn(move || {
            let statuses = fyler_fsops::gitstatus::status_badges(&root).unwrap_or_default();
            let _ = event_tx.send(AppEvent::GitStatus {
                pane_id,
                root,
                statuses,
            });
        }));
    }

    fn prepare_request(&mut self, pane_id: PaneId, root: PathBuf) -> Option<PathBuf> {
        let slot = self.slots.entry(pane_id).or_default();
        if slot.inflight {
            slot.queued = Some(root);
            return None;
        }
        slot.inflight = true;
        Some(root)
    }

    fn on_finished(&mut self, pane_id: PaneId) -> Option<PathBuf> {
        let slot = self.slots.get_mut(&pane_id)?;
        slot.inflight = false;
        slot.queued.take()
    }

    fn remove(&mut self, pane_id: PaneId) {
        self.slots.remove(&pane_id);
    }
}

fn default_root() -> anyhow::Result<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("home directory is not set"))
}

fn main() {
    // `windows_subsystem = "windows"` によりコンソールが無いため、`run` が
    // GUI起動前に返す早期エラー(nvim未検出・scan失敗等)は放置すると無音で
    // 終了してしまう。ネイティブダイアログとログファイルで必ず可視化する。
    if let Err(error) = run() {
        report_startup_error(&error);
        std::process::exit(1);
    }
}

/// 早期起動エラーを標準エラー・ログファイル・ネイティブダイアログへ出す。
fn report_startup_error(error: &anyhow::Error) {
    // 標準エラーは非Windows/開発時にのみ見える(Windows GUIでは出力先が無い)。
    eprintln!("fyler: {error:#}");

    let log_path = write_startup_error_log(error);
    let mut message = format!("fyler could not start.\n\n{error:#}");
    if let Some(path) = &log_path {
        message.push_str(&format!("\n\nLog: {}", path.display()));
    }
    fyler_fsops::dialog::show_error_dialog("fyler failed to start", &message);
}

/// 早期起動エラーをログファイルへ書き出し、そのパスを返す。書けなければ`None`。
///
/// 保存先は `%LOCALAPPDATA%\fyler`(無ければOSの一時ディレクトリ)。
fn write_startup_error_log(error: &anyhow::Error) -> Option<PathBuf> {
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("fyler"))
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("fyler-startup-error.log");
    std::fs::write(&path, format!("fyler startup error:\n{error:#}\n")).ok()?;
    Some(path)
}

fn run() -> anyhow::Result<()> {
    pane_runtime::run()
}

/// 外部変更を再スキャン経路へ流し、表示メタデータとGit装飾の更新を要求する。
fn handle_external_change(
    pane_id: PaneId,
    changed_paths: &BTreeSet<PathBuf>,
    save_controller: &mut SaveController,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    git: &mut GitRefresher,
    root: &Path,
) -> Result<bool, mpsc::SendError<GuiEvent>> {
    let result = save_controller.on_external_change(changed_paths);
    let invalidated_dialog = matches!(
        &result,
        SaveFlowResult::PlanInvalidated(_) | SaveFlowResult::UndoInvalidated { .. }
    );
    if !matches!(&result, SaveFlowResult::NoChanges) {
        send_save_result(gui_event_tx, pane_id, result)?;
    }
    send_view_state(gui_event_tx, pane_id, save_controller)?;
    git.request(pane_id, root.to_path_buf());
    Ok(invalidated_dialog)
}

#[allow(clippy::too_many_arguments)] // ルート差し替えの全状態とカーソル復元対象を明示する。
fn change_root_to(
    pane_id: PaneId,
    new_root: PathBuf,
    cursor_target: Option<&OsStr>,
    root: &mut PathBuf,
    watcher: &mut FsWatcher,
    watch_tx: &mpsc::Sender<ExternalChange>,
    shared_ids: &Arc<std::sync::Mutex<IdAllocator>>,
    save_controller: &mut SaveController,
    engine: &dyn EditorEngine,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
) -> Result<bool, mpsc::SendError<GuiEvent>> {
    let new_root = match normalize_root(&new_root) {
        Ok(new_root) => new_root,
        Err(error) => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                format!(
                    "表示ルートを正規化できません ({}): {error}",
                    new_root.display()
                ),
            )?;
            return Ok(false);
        }
    };

    if engine.snapshot().dirty {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Info,
            "編集中です。保存または破棄してからディレクトリを移動してください",
        )?;
        return Ok(false);
    }
    if !save_controller.is_idle() {
        return Ok(false);
    }

    let scan_options = save_controller.scan_options();
    let new_baseline = match shared_ids.lock() {
        Ok(mut ids) => fyler_fsops::scan::scan_baseline_with(&new_root, &mut ids, &scan_options),
        Err(_) => Err(anyhow::anyhow!("ID採番器のロックが破損しています")),
    };
    let new_baseline = match new_baseline {
        Ok(baseline) => baseline,
        Err(error) => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                format!(
                    "表示ルートを読み込めません ({}): {error:#}",
                    new_root.display()
                ),
            )?;
            return Ok(false);
        }
    };

    // 新しい監視の作成に失敗した場合、現在のroot/baseline/watcherを
    // そのまま維持できるよう、状態差し替え前に準備だけ済ませる。
    let new_watcher = match fyler_fsops::watch::watch(&new_root, watch_tx.clone()) {
        Ok(watcher) => watcher,
        Err(error) => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                format!(
                    "表示ルートを監視できません ({}): {error:#}",
                    new_root.display()
                ),
            )?;
            return Ok(false);
        }
    };

    if let Err(error) =
        save_controller.change_root_preserving_allocator(new_root.clone(), new_baseline)
    {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            format!("表示ルートを変更できません: {error:#}"),
        )?;
        return Ok(false);
    }
    save_controller.collapse_all_dirs();
    let cursor_line = cursor_target.and_then(|name| save_controller.find_top_level_line(name));
    let new_lines = save_controller.visible_lines();

    *root = new_root;
    *watcher = new_watcher;
    if let Err(error) = engine.send(EditorCommand::SetLines {
        lines: new_lines,
        cursor_line,
    }) {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            format!("新しいディレクトリを表示できません: {error:#}"),
        )?;
    }
    // GUIへのpane tag付けは呼び出し元の`after_root_change`で行う。
    if let Err(error) = config::record_recent_root(root) {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Warn,
            format!("最近使ったルートを記録できません: {error:#}"),
        )?;
    }
    Ok(true)
}

fn after_root_change(
    pane_id: PaneId,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    save_controller: &SaveController,
    git: &mut GitRefresher,
    root: &Path,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    gui_event_tx.send(GuiEvent::RootChanged {
        pane_id,
        root: root.to_path_buf(),
    })?;
    gui_event_tx.send(GuiEvent::GitBadges {
        pane_id,
        badges: HashMap::new(),
    })?;
    send_view_state(gui_event_tx, pane_id, save_controller)?;
    git.request(pane_id, root.to_path_buf());
    Ok(())
}

fn normalize_root(root: &Path) -> std::io::Result<PathBuf> {
    std::path::absolute(root)
}

/// `:cd`の引数を移動先の絶対パスへ解決する。
///
/// 絶対パスはそのまま返し、`~`単独と`~/...`はホームディレクトリ基準に
/// 展開する。それ以外は現在ルートからの相対パスとして解決する。
/// `~user`形式と、ホームディレクトリ不明時の`~`は解決できない。
fn resolve_cd_target(query: &str, root: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if query == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(relative) = query.strip_prefix("~/") {
        return home.map(|home| home.join(relative));
    }
    if query.starts_with('~') {
        return None;
    }

    let path = Path::new(query);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    })
}

fn format_drive_paths(drives: &[PathBuf]) -> String {
    drives
        .iter()
        .map(|drive| drive.display().to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BookmarkResolution {
    Resolved(PathBuf),
    Ambiguous(Vec<String>),
    NotFound,
}

fn resolve_bookmark_query(
    query: &str,
    bookmarks: &[(String, PathBuf)],
    recent: &[PathBuf],
) -> BookmarkResolution {
    if let Some((_, path)) = bookmarks.iter().find(|(name, _)| name == query) {
        return BookmarkResolution::Resolved(path.clone());
    }

    let prefix_matches = bookmarks
        .iter()
        .filter(|(name, _)| name.starts_with(query))
        .collect::<Vec<_>>();
    if let [(_, path)] = prefix_matches.as_slice() {
        return BookmarkResolution::Resolved((*path).clone());
    }

    if let Ok(index) = query.parse::<usize>()
        && let Some(path) = index.checked_sub(1).and_then(|index| recent.get(index))
    {
        return BookmarkResolution::Resolved(path.clone());
    }

    if prefix_matches.len() > 1 {
        return BookmarkResolution::Ambiguous(
            prefix_matches
                .into_iter()
                .map(|(name, _)| name.clone())
                .collect(),
        );
    }
    BookmarkResolution::NotFound
}

fn bookmark_list_message(bookmarks: &[(String, PathBuf)], recent: &[PathBuf]) -> String {
    let mut entries = bookmarks
        .iter()
        .map(|(name, path)| format!("b:{name}={}", path.display()))
        .collect::<Vec<_>>();
    entries.extend(
        recent
            .iter()
            .enumerate()
            .map(|(index, path)| format!("{}:{}", index + 1, path.display())),
    );
    if entries.is_empty() {
        "ブックマークと最近使ったルートはありません".to_owned()
    } else {
        entries.join(" | ")
    }
}

fn handle_activate_line(
    pane_id: PaneId,
    save_controller: &mut SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    line: usize,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    let snapshot = engine.snapshot();
    let Some(editor_line) = snapshot.lines.get(line) else {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "開く対象の行が見つかりません",
        );
    };

    match fyler_core::grammar::split_id_prefix(&editor_line.text) {
        PrefixParse::NoId { .. } => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "保存されていない行です",
            );
        }
        PrefixParse::Broken => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                "壊れたIDプレフィックスの行は開けません",
            );
        }
        PrefixParse::WithId { .. } => {}
    }

    let Some((path, kind)) = save_controller.resolve_line(&snapshot.lines, line) else {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "行に対応するファイルが現在のツリーに見つかりません",
        );
    };

    match kind {
        EntryKind::File | EntryKind::Symlink => {
            let path = path.to_fs_path(root);
            if let Err(error) = fyler_fsops::open::open_with_default_app(&path) {
                send_gui_message(
                    gui_event_tx,
                    pane_id,
                    MessageKind::Error,
                    format!("ファイルを開けません: {error:#}"),
                )?;
            }
        }
        EntryKind::Dir => {
            if snapshot.dirty {
                return send_gui_message(
                    gui_event_tx,
                    pane_id,
                    MessageKind::Info,
                    "編集中は折りたたみできません。保存または破棄してください",
                );
            }

            match save_controller.toggle_collapse(&snapshot.lines, line) {
                ToggleCollapseResult::Toggled(lines) => {
                    // 折りたたみ/展開した行は差し替え後も同じindexに残るため、
                    // カーソルをその行へ戻す(先頭へ飛ばさない)。
                    if let Err(error) = engine.send(EditorCommand::SetLines {
                        lines,
                        cursor_line: Some(line),
                    }) {
                        send_gui_message(
                            gui_event_tx,
                            pane_id,
                            MessageKind::Error,
                            format!("折りたたみ表示を更新できません: {error:#}"),
                        )?;
                    }
                    send_view_state(gui_event_tx, pane_id, save_controller)?;
                }
                ToggleCollapseResult::NotADirectory => {
                    send_gui_message(
                        gui_event_tx,
                        pane_id,
                        MessageKind::Error,
                        "対象行はディレクトリではありません",
                    )?;
                }
                ToggleCollapseResult::NotFound => {
                    send_gui_message(
                        gui_event_tx,
                        pane_id,
                        MessageKind::Error,
                        "行に対応するディレクトリが現在のツリーに見つかりません",
                    )?;
                }
                ToggleCollapseResult::Busy => {}
            }
        }
    }
    Ok(())
}

fn handle_yank_path(
    pane_id: PaneId,
    save_controller: &SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    line: usize,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    let snapshot = engine.snapshot();
    let Some(editor_line) = snapshot.lines.get(line) else {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "コピー対象の行が見つかりません",
        );
    };

    match fyler_core::grammar::split_id_prefix(&editor_line.text) {
        PrefixParse::NoId { .. } => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "保存されていない行です",
            );
        }
        PrefixParse::Broken => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                "壊れたIDプレフィックスの行はコピーできません",
            );
        }
        PrefixParse::WithId { .. } => {}
    }

    let Some((path, _)) = save_controller.resolve_line(&snapshot.lines, line) else {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "行に対応するファイルが現在のツリーに見つかりません",
        );
    };
    let path = path.to_fs_path(root);
    gui_event_tx.send(GuiEvent::CopyPath(path.to_string_lossy().into_owned()))
}

fn open_file_picker_rejection(
    dialog_open: bool,
    apply_running: bool,
    transfer_awaiting: bool,
    transfer_running: bool,
    crashed: bool,
    save_idle: bool,
) -> Option<&'static str> {
    if dialog_open || apply_running || transfer_awaiting || transfer_running {
        Some("別のダイアログまたはファイル操作が進行中のため、検索を開始できません")
    } else if crashed {
        Some("editor engineが停止しているため、検索を開始できません")
    } else if !save_idle {
        Some("保存処理中のため、検索を開始できません")
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_open_file_picker(
    pane_id: PaneId,
    save_controller: &SaveController,
    crashed: bool,
    dialog_open: bool,
    apply_running: bool,
    transfer_awaiting: bool,
    transfer_running: bool,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    if let Some(message) = open_file_picker_rejection(
        dialog_open,
        apply_running,
        transfer_awaiting,
        transfer_running,
        crashed,
        save_controller.is_idle(),
    ) {
        return send_gui_message(gui_event_tx, pane_id, MessageKind::Info, message);
    }
    gui_event_tx.send(GuiEvent::ShowFilePicker {
        pane_id,
        candidates: fyler_core::search::build_candidates(save_controller.baseline()),
    })
}

fn visible_line_for_entry(save_controller: &SaveController, entry_id: EntryId) -> Option<usize> {
    save_controller.visible_lines().iter().position(|line| {
        matches!(
            fyler_core::grammar::split_id_prefix(&line.text),
            PrefixParse::WithId { id, .. } if id == entry_id
        )
    })
}

fn snapshot_line_matches_entry(
    snapshot: &fyler_core::editor::EditorSnapshot,
    line: usize,
    entry_id: EntryId,
) -> bool {
    snapshot.lines.get(line).is_some_and(|editor_line| {
        matches!(
            fyler_core::grammar::split_id_prefix(&editor_line.text),
            PrefixParse::WithId { id, .. } if id == entry_id
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_picker_select_with(
    pane_id: PaneId,
    entry_id: EntryId,
    action: PickerAction,
    save_controller: &mut SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    open_path: &mut dyn FnMut(&Path) -> anyhow::Result<()>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    let Some(entry) = save_controller.baseline().get(entry_id).cloned() else {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Warn,
            "検索候補が見つかりません。外部変更された可能性があります",
        );
    };

    if action == PickerAction::Open {
        let path = entry.path.to_fs_path(root);
        if let Err(error) = open_path(&path) {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                format!("対象を開けません: {error:#}"),
            )?;
        }
        return Ok(());
    }

    let snapshot = engine.snapshot();
    if let Some(line) = visible_line_for_entry(save_controller, entry_id) {
        if !snapshot_line_matches_entry(&snapshot, line, entry_id) {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "編集中の行位置が検索候補と一致しないため移動できません",
            );
        }
        if let Err(error) = engine.send(EditorCommand::SetCursorLine(line)) {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                format!("検索候補へ移動できません: {error:#}"),
            )?;
        }
        return Ok(());
    }

    if snapshot.dirty {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Info,
            "編集中は折りたたまれた検索候補を展開できません。保存または破棄してください",
        );
    }

    match save_controller.reveal_entry(entry_id) {
        RevealResult::AlreadyVisible { line } => {
            if let Err(error) = engine.send(EditorCommand::SetCursorLine(line)) {
                send_gui_message(
                    gui_event_tx,
                    pane_id,
                    MessageKind::Error,
                    format!("検索候補へ移動できません: {error:#}"),
                )?;
            }
        }
        RevealResult::Revealed { lines, line } => {
            if let Err(error) = engine.send(EditorCommand::SetLines {
                lines,
                cursor_line: Some(line),
            }) {
                send_gui_message(
                    gui_event_tx,
                    pane_id,
                    MessageKind::Error,
                    format!("検索候補を展開できません: {error:#}"),
                )?;
            }
            send_view_state(gui_event_tx, pane_id, save_controller)?;
        }
        RevealResult::NotFound => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Warn,
                "検索候補が見つかりません。外部変更された可能性があります",
            )?;
        }
        RevealResult::Busy => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "保存処理中のため検索候補へ移動できません",
            )?;
        }
    }
    Ok(())
}

fn handle_picker_select(
    pane_id: PaneId,
    entry_id: EntryId,
    action: PickerAction,
    save_controller: &mut SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    gui_event_tx: &mpsc::Sender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    handle_picker_select_with(
        pane_id,
        entry_id,
        action,
        save_controller,
        engine,
        root,
        gui_event_tx,
        &mut fyler_fsops::open::open_with_default_app,
    )
}

fn send_gui_message(
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    pane_id: PaneId,
    kind: MessageKind,
    text: impl Into<String>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    gui_event_tx.send(GuiEvent::Editor {
        pane_id,
        event: EditorEvent::Message(EditorMessage {
            kind,
            text: text.into(),
        }),
    })
}

/// 表示行に対応する表示状態(ファイル情報・折りたたみ集合)をGUIへ送る。
///
/// 可視行の集合が変わるたびに呼ぶ。折りたたみ集合は展開/折りたたみアイコンの
/// 正典で、子を持たない空ディレクトリの展開状態も正しく描画するために必要。
fn send_view_state(
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    pane_id: PaneId,
    save_controller: &SaveController,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    gui_event_tx.send(GuiEvent::FileInfos {
        pane_id,
        infos: save_controller.visible_file_infos(),
    })?;
    gui_event_tx.send(GuiEvent::CollapsedDirs {
        pane_id,
        dirs: save_controller.collapsed_dirs(),
    })
}

fn send_save_result(
    gui_event_tx: &mpsc::Sender<GuiEvent>,
    pane_id: PaneId,
    result: SaveFlowResult,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    match result {
        SaveFlowResult::ShowPlan {
            plan,
            warnings,
            overwrites,
        } => gui_event_tx.send(GuiEvent::ShowPlan {
            plan,
            warnings,
            overwrites,
        }),
        SaveFlowResult::ShowValidationErrors(errors) => {
            gui_event_tx.send(GuiEvent::ShowValidationErrors(errors))
        }
        SaveFlowResult::ShowReport(report) => gui_event_tx.send(GuiEvent::ShowReport(report)),
        SaveFlowResult::ShowUndoPlan { .. } => {
            // TODO(M12-D): undo確認ダイアログをGUIへ配線する。
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "undo確認UIは次セッションで配線します",
            )
        }
        SaveFlowResult::UndoNothingLeft { reasons } => {
            let message = if reasons.is_empty() {
                "undoできる操作がありません".to_owned()
            } else {
                format!("undoできる操作がありません: {}", reasons.join(" / "))
            };
            send_gui_message(gui_event_tx, pane_id, MessageKind::Info, message)
        }
        SaveFlowResult::ShowUndoReport(_) => {
            // TODO(M12-D): undo結果ダイアログをGUIへ配線する。
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "undo結果UIは次セッションで配線します",
            )
        }
        SaveFlowResult::ReconcileFailed { report, error } => {
            gui_event_tx.send(GuiEvent::ShowReport(report))?;
            gui_event_tx.send(GuiEvent::FatalError(format!(
                "実行後の再読込に失敗しました。安全のため編集を停止します: {error}"
            )))
        }
        SaveFlowResult::ExternalChanged => Ok(()),
        SaveFlowResult::ExternalChangeNotified(text) => {
            send_gui_message(gui_event_tx, pane_id, MessageKind::Info, text)
        }
        SaveFlowResult::ExternalChangeFailed(text) => {
            send_gui_message(gui_event_tx, pane_id, MessageKind::Error, text)
        }
        SaveFlowResult::PlanInvalidated(text) => {
            gui_event_tx.send(GuiEvent::CloseDialog)?;
            send_gui_message(gui_event_tx, pane_id, MessageKind::Warn, text)
        }
        SaveFlowResult::UndoInvalidated { message, .. } => {
            gui_event_tx.send(GuiEvent::CloseDialog)?;
            send_gui_message(gui_event_tx, pane_id, MessageKind::Warn, message)
        }
        SaveFlowResult::NoChanges => {
            send_gui_message(gui_event_tx, pane_id, MessageKind::Info, "変更はありません")
        }
        SaveFlowResult::Cancelled => gui_event_tx.send(GuiEvent::CloseDialog),
        SaveFlowResult::UndoCancelled { .. } => gui_event_tx.send(GuiEvent::CloseDialog),
        SaveFlowResult::StartApply { .. } => {
            debug_assert!(
                false,
                "StartApplyはConfirm armで処理済みである必要があります"
            );
            Ok(())
        }
        SaveFlowResult::StartUndo { .. } => {
            // TODO(M12-D): undo worker起動をConfirm armへ配線する。
            debug_assert!(
                false,
                "StartUndoはConfirm armで処理済みである必要があります"
            );
            Ok(())
        }
        SaveFlowResult::ApplyCancelRequested => {
            debug_assert!(
                false,
                "ApplyCancelRequestedはConfirm armで処理済みである必要があります"
            );
            Ok(())
        }
        SaveFlowResult::Ignored => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use fyler_core::editor::{EditorLine, EditorSnapshot};
    use fyler_core::tree::{BaselineEntry, BaselineTree};

    use super::*;

    struct PickerEngine {
        snapshot: Mutex<EditorSnapshot>,
        commands: Mutex<Vec<EditorCommand>>,
    }

    impl Default for PickerEngine {
        fn default() -> Self {
            Self {
                snapshot: Mutex::new(EditorSnapshot::empty()),
                commands: Mutex::new(Vec::new()),
            }
        }
    }

    impl EditorEngine for PickerEngine {
        fn send(&self, command: EditorCommand) -> anyhow::Result<()> {
            self.commands.lock().unwrap().push(command);
            Ok(())
        }

        fn snapshot(&self) -> Arc<EditorSnapshot> {
            Arc::new(self.snapshot.lock().unwrap().clone())
        }
    }

    impl PickerEngine {
        fn set_snapshot(&self, lines: Vec<EditorLine>, dirty: bool) {
            let mut snapshot = EditorSnapshot::empty();
            snapshot.lines = lines.into();
            snapshot.dirty = dirty;
            *self.snapshot.lock().unwrap() = snapshot;
        }

        fn commands(&self) -> Vec<EditorCommand> {
            self.commands.lock().unwrap().clone()
        }
    }

    fn picker_baseline() -> BaselineTree {
        let mut baseline = BaselineTree::new("root");
        for (id, path, kind) in [
            (1, "dir", EntryKind::Dir),
            (2, "dir/file.txt", EntryKind::File),
            (3, "link", EntryKind::Symlink),
            (4, "other", EntryKind::Dir),
        ] {
            baseline.insert(BaselineEntry {
                id: EntryId(id),
                path: fyler_core::path::TreePath::parse(path),
                kind,
            });
        }
        baseline
    }

    fn picker_controller(collapsed: bool, dirty: bool) -> (SaveController, Arc<PickerEngine>) {
        let engine = Arc::new(PickerEngine::default());
        let save_engine: Arc<dyn EditorEngine> = engine.clone();
        let mut controller = SaveController::new(
            PathBuf::from("root"),
            IdAllocator::new(),
            picker_baseline(),
            save_engine,
        );
        if collapsed {
            controller.collapse_all_dirs();
        }
        engine.set_snapshot(controller.visible_lines(), dirty);
        (controller, engine)
    }

    fn received_message(receiver: &mpsc::Receiver<GuiEvent>) -> EditorMessage {
        let GuiEvent::Editor {
            event: EditorEvent::Message(message),
            ..
        } = receiver.recv().unwrap()
        else {
            panic!("expected GUI message")
        };
        message
    }

    #[test]
    fn normalize_root_removes_joined_current_directory_component() {
        let current_dir = std::env::current_dir().unwrap();

        assert_eq!(normalize_root(&current_dir.join(".")).unwrap(), current_dir);
    }

    #[test]
    fn resolve_cd_target_keeps_absolute_paths() {
        let absolute = std::env::current_dir().unwrap().join("absolute-target");

        assert_eq!(
            resolve_cd_target(&absolute.to_string_lossy(), Path::new("ignored"), None),
            Some(absolute)
        );
    }

    #[test]
    fn resolve_cd_target_expands_home_paths() {
        let home = Path::new("home");

        assert_eq!(
            resolve_cd_target("~", Path::new("root"), Some(home)),
            Some(PathBuf::from("home"))
        );
        assert_eq!(
            resolve_cd_target("~/sub", Path::new("root"), Some(home)),
            Some(PathBuf::from("home").join("sub"))
        );
        assert_eq!(resolve_cd_target("~", Path::new("root"), None), None);
        assert_eq!(
            resolve_cd_target("~user", Path::new("root"), Some(home)),
            None
        );
    }

    #[test]
    fn resolve_cd_target_joins_relative_paths_to_root_without_normalizing() {
        let root = Path::new("root").join("current");

        assert_eq!(resolve_cd_target("..", &root, None), Some(root.join("..")));
        assert_eq!(
            resolve_cd_target("sub/dir", &root, None),
            Some(root.join("sub").join("dir"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolve_cd_target_recognizes_windows_absolute_paths() {
        assert_eq!(
            resolve_cd_target(r"C:\x", Path::new(r"D:\root"), None),
            Some(PathBuf::from(r"C:\x"))
        );
    }

    #[test]
    fn git_refresher_coalesces_inflight_requests_to_latest_root() {
        let (event_tx, _event_rx) = mpsc::channel();
        let mut git = GitRefresher::new(event_tx);
        let first = PathBuf::from("first");
        let second = PathBuf::from("second");
        let latest = PathBuf::from("latest");
        let pane = PaneId::new(1);

        assert_eq!(git.prepare_request(pane, first.clone()), Some(first));
        assert!(git.slots[&pane].inflight);
        assert_eq!(git.prepare_request(pane, second), None);
        assert_eq!(git.prepare_request(pane, latest.clone()), None);
        assert_eq!(git.slots[&pane].queued, Some(latest.clone()));

        assert_eq!(git.on_finished(pane), Some(latest.clone()));
        assert!(!git.slots[&pane].inflight);
        assert_eq!(git.prepare_request(pane, latest.clone()), Some(latest));
        assert!(git.slots[&pane].inflight);
        assert_eq!(git.on_finished(pane), None);
        assert!(!git.slots[&pane].inflight);
    }

    #[test]
    fn git_refresher_routes_slots_by_pane_id() {
        let (event_tx, _event_rx) = mpsc::channel();
        let mut git = GitRefresher::new(event_tx);
        let first = PaneId::new(1);
        let second = PaneId::new(2);

        assert_eq!(
            git.prepare_request(first, PathBuf::from("same")),
            Some(PathBuf::from("same"))
        );
        assert_eq!(
            git.prepare_request(second, PathBuf::from("same")),
            Some(PathBuf::from("same"))
        );
        assert!(git.slots[&first].inflight);
        assert!(git.slots[&second].inflight);
        assert_eq!(git.on_finished(first), None);
        assert!(!git.slots[&first].inflight);
        assert!(git.slots[&second].inflight);
    }

    #[test]
    fn bookmark_resolution_prefers_exact_then_unique_prefix_then_recent_index() {
        let bookmarks = vec![
            ("project".to_owned(), PathBuf::from("/bookmark/project")),
            ("profile".to_owned(), PathBuf::from("/bookmark/profile")),
            ("docs".to_owned(), PathBuf::from("/bookmark/docs")),
            ("1".to_owned(), PathBuf::from("/bookmark/numeric")),
        ];
        let recent = vec![PathBuf::from("/recent/one"), PathBuf::from("/recent/two")];

        assert_eq!(
            resolve_bookmark_query("1", &bookmarks, &recent),
            BookmarkResolution::Resolved(PathBuf::from("/bookmark/numeric"))
        );
        assert_eq!(
            resolve_bookmark_query("doc", &bookmarks, &recent),
            BookmarkResolution::Resolved(PathBuf::from("/bookmark/docs"))
        );
        assert_eq!(
            resolve_bookmark_query("2", &bookmarks, &recent),
            BookmarkResolution::Resolved(PathBuf::from("/recent/two"))
        );
        assert_eq!(
            resolve_bookmark_query("pro", &bookmarks, &recent),
            BookmarkResolution::Ambiguous(vec!["project".to_owned(), "profile".to_owned()])
        );
        assert_eq!(
            resolve_bookmark_query("missing", &bookmarks, &recent),
            BookmarkResolution::NotFound
        );
    }

    #[test]
    fn picker_start_gate_rejects_each_busy_or_crashed_condition() {
        for conditions in [
            (true, false, false, false, false, true),
            (false, true, false, false, false, true),
            (false, false, true, false, false, true),
            (false, false, false, true, false, true),
            (false, false, false, false, true, true),
            (false, false, false, false, false, false),
        ] {
            assert!(
                open_file_picker_rejection(
                    conditions.0,
                    conditions.1,
                    conditions.2,
                    conditions.3,
                    conditions.4,
                    conditions.5,
                )
                .is_some()
            );
        }
        assert!(open_file_picker_rejection(false, false, false, false, false, true).is_none());
    }

    #[test]
    fn dirty_buffer_can_open_picker() {
        let (controller, _engine) = picker_controller(false, true);
        let (gui_tx, gui_rx) = mpsc::channel();

        handle_open_file_picker(
            PaneId::new(1),
            &controller,
            false,
            false,
            false,
            false,
            false,
            &gui_tx,
        )
        .unwrap();

        let GuiEvent::ShowFilePicker {
            pane_id,
            candidates,
        } = gui_rx.recv().unwrap()
        else {
            panic!("expected picker event")
        };
        assert_eq!(pane_id, PaneId::new(1));
        assert_eq!(candidates.len(), 4);
    }

    #[test]
    fn stale_picker_selection_only_notifies() {
        let (mut controller, engine) = picker_controller(false, false);
        let (gui_tx, gui_rx) = mpsc::channel();
        let mut opened = Vec::new();

        handle_picker_select_with(
            PaneId::new(1),
            EntryId(999),
            PickerAction::Open,
            &mut controller,
            engine.as_ref(),
            Path::new("root"),
            &gui_tx,
            &mut |path| {
                opened.push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap();

        assert!(opened.is_empty());
        assert!(engine.commands().is_empty());
        assert!(received_message(&gui_rx).text.contains("外部変更"));
    }

    #[test]
    fn picker_jump_to_visible_entry_sends_cursor_only() {
        let (mut controller, engine) = picker_controller(false, false);
        let (gui_tx, _gui_rx) = mpsc::channel();

        handle_picker_select_with(
            PaneId::new(1),
            EntryId(2),
            PickerAction::Jump,
            &mut controller,
            engine.as_ref(),
            Path::new("root"),
            &gui_tx,
            &mut |_| Ok(()),
        )
        .unwrap();

        assert_eq!(engine.commands(), vec![EditorCommand::SetCursorLine(1)]);
    }

    #[test]
    fn picker_jump_reveals_collapsed_ancestors_and_sends_view_state() {
        let (mut controller, engine) = picker_controller(true, false);
        let (gui_tx, gui_rx) = mpsc::channel();

        handle_picker_select_with(
            PaneId::new(1),
            EntryId(2),
            PickerAction::Jump,
            &mut controller,
            engine.as_ref(),
            Path::new("root"),
            &gui_tx,
            &mut |_| Ok(()),
        )
        .unwrap();

        let commands = engine.commands();
        assert!(matches!(
            commands.as_slice(),
            [EditorCommand::SetLines {
                cursor_line: Some(1),
                ..
            }]
        ));
        assert!(matches!(gui_rx.recv().unwrap(), GuiEvent::FileInfos { .. }));
        let GuiEvent::CollapsedDirs { dirs, .. } = gui_rx.recv().unwrap() else {
            panic!("expected collapsed directory state")
        };
        assert!(!dirs.contains(&EntryId(1)));
    }

    #[test]
    fn picker_jump_rejects_dirty_collapsed_entry() {
        let (mut controller, engine) = picker_controller(true, true);
        let (gui_tx, gui_rx) = mpsc::channel();

        handle_picker_select_with(
            PaneId::new(1),
            EntryId(2),
            PickerAction::Jump,
            &mut controller,
            engine.as_ref(),
            Path::new("root"),
            &gui_tx,
            &mut |_| Ok(()),
        )
        .unwrap();

        assert!(engine.commands().is_empty());
        assert!(received_message(&gui_rx).text.contains("編集中"));
        assert!(controller.collapsed_dirs().contains(&EntryId(1)));
    }

    #[test]
    fn picker_jump_rejects_dirty_visible_line_with_mismatched_id() {
        let (mut controller, engine) = picker_controller(false, true);
        let mut lines = controller.visible_lines();
        lines[1] = EditorLine::new("edited without id");
        engine.set_snapshot(lines, true);
        let (gui_tx, gui_rx) = mpsc::channel();

        handle_picker_select_with(
            PaneId::new(1),
            EntryId(2),
            PickerAction::Jump,
            &mut controller,
            engine.as_ref(),
            Path::new("root"),
            &gui_tx,
            &mut |_| Ok(()),
        )
        .unwrap();

        assert!(engine.commands().is_empty());
        assert!(received_message(&gui_rx).text.contains("一致しない"));
    }

    #[test]
    fn picker_open_uses_default_open_path_for_every_entry_kind() {
        let (mut controller, engine) = picker_controller(true, true);
        let (gui_tx, _gui_rx) = mpsc::channel();
        let mut opened = Vec::new();

        for id in [EntryId(2), EntryId(3), EntryId(1)] {
            handle_picker_select_with(
                PaneId::new(1),
                id,
                PickerAction::Open,
                &mut controller,
                engine.as_ref(),
                Path::new("root"),
                &gui_tx,
                &mut |path| {
                    opened.push(path.to_path_buf());
                    Ok(())
                },
            )
            .unwrap();
        }

        assert_eq!(
            opened,
            vec![
                PathBuf::from("root/dir/file.txt"),
                PathBuf::from("root/link"),
                PathBuf::from("root/dir"),
            ]
        );
    }
}
