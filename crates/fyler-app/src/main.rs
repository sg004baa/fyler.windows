#![cfg_attr(windows, windows_subsystem = "windows")]

//! fyler — エントリポイント。各レイヤーの配線だけを行う(ロジックを書かない)。
//!
//! 各レイヤーの役割はAGENTS.mdの依存境界表を参照。ここに書いてよいのは
//! 「起動」「イベントの受け渡し」「ユーザー操作の各レイヤーへの配線」
//! 「保存状態機械の副作用(SaveEffect)の実行」のみ。

mod config;
mod drag_flow;
mod feedback;
mod import_flow;
mod nvim_locate;
mod pane_runtime;
mod picker;
mod queue_stats;
pub mod save_flow;
mod session;
mod transfer_flow;
mod undo_format;
mod undo_journal;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

use fyler_core::editor::{EditorCommand, EditorEngine, EditorEvent, EditorMessage, MessageKind};
use fyler_core::feedback::FeedbackKind;
use fyler_core::gitstatus::GitBadge;
use fyler_core::grammar::PrefixParse;
use fyler_core::id::EntryId;
#[cfg(test)]
use fyler_core::id::IdAllocator;
use fyler_core::options::{SortKey, TerminalKind};
use fyler_core::pane::PaneId;
use fyler_core::path::TreePath;
use fyler_core::report::{ApplyProgress, CommitReport};
use fyler_core::transfer::{DragOutcome, DropEffect, ImportOp, TransferOp};
use fyler_core::tree::EntryKind;
use fyler_core::undo::{UndoStep, UndoTransaction};
use fyler_fsops::openwith::OpenWithHandler;
use fyler_fsops::watch::ExternalChange;
use fyler_gui::app::{GuiEvent, PickerAction, TreeContextItem, TreeRowClickKind};
use fyler_gui::confirm::ConfirmChoice;

use crate::queue_stats::CountingSender;
use crate::save_flow::{RevealResult, SaveController, SaveFlowResult, ToggleCollapseResult};

enum AppEvent {
    Editor(PaneId, EditorEvent),
    Confirm(ConfirmChoice),
    PickerSelect {
        pane_id: PaneId,
        path: TreePath,
        action: PickerAction,
    },
    PickerQuery {
        pane_id: PaneId,
        query: String,
    },
    PickerClosed(PaneId),
    CatalogChanged {
        root: PathBuf,
        error: Option<String>,
    },
    FeedbackSubmit {
        kind: FeedbackKind,
        body: String,
    },
    FeedbackClosed,
    FeedbackFinished(feedback::FeedbackOutcome),
    ExternalChange(PaneId, ExternalChange),
    WatchDegraded(PaneId, String),
    OfflineRetryTick,
    GitStatus {
        pane_id: PaneId,
        root: PathBuf,
        branch: Option<String>,
        statuses: HashMap<PathBuf, GitBadge>,
    },
    LoaderProgress(PaneId, usize),
    LoaderFinished {
        pane_id: PaneId,
        root: PathBuf,
        result: anyhow::Result<Option<fyler_core::tree::BaselineTree>>,
    },
    LoaderCancel,
    ApplyProgress(PaneId, ApplyProgress),
    ApplyFinished(PaneId, CommitReport, Option<UndoTransaction>),
    UndoProgress(PaneId, ApplyProgress<UndoStep>),
    UndoFinished(PaneId, CommitReport<UndoStep>),
    TransferProgress(ApplyProgress<TransferOp>),
    TransferFinished(CommitReport<TransferOp>),
    ImportProgress(ApplyProgress<ImportOp>),
    ImportFinished(CommitReport<ImportOp>),
    FilesDropped {
        pane_id: PaneId,
        line: Option<usize>,
        paths: Vec<PathBuf>,
        effect: DropEffect,
    },
    Shutdown {
        save_session: bool,
        window: Option<fyler_core::WindowGeometry>,
    },
    /// ユーザーがpaneのツリー領域(行または空白部分)をクリックしてfocusを要求した。
    RequestPaneFocus(PaneId),
    /// ツリー行のクリック(single/double/shift)。
    TreeRowClicked {
        pane_id: PaneId,
        line: usize,
        kind: TreeRowClickKind,
    },
    /// ツリーのcontext menu項目実行要求。
    TreeContextAction {
        pane_id: PaneId,
        line: usize,
        item: TreeContextItem,
    },
    /// GUI window内で開始したtree行dragがwindow境界を離れた
    /// (OLE drag-outを開始する)。
    TreeDragOut {
        pane_id: PaneId,
        line: usize,
    },
    /// OLE drag-out(使い捨てSTAスレッド)の完了。`existing`はdragした絶対パス
    /// のうち`perform_drag`直後もまだFS上に存在するもの。
    TreeDragFinished {
        pane_id: PaneId,
        outcome: DragOutcome,
        existing: Vec<PathBuf>,
        /// `perform_drag`自体の失敗(OleInitialize等)。発生時のみSome。
        /// outcomeはCancelled扱いだが、silent fallbackにしないためユーザーへ表示する。
        error: Option<String>,
    },
    /// 承認後のごみ箱退避workerの完了。
    TreeDragCleanupFinished {
        pane_id: PaneId,
        errors: Vec<String>,
    },
    /// GUI window内で完結したtree行drag(pane間)。
    TreeDragDrop {
        source_pane: PaneId,
        source_line: usize,
        target_pane: PaneId,
        target_line: Option<usize>,
        copy: bool,
    },
}

struct ExternalChangeOutcome {
    invalidated_dialog: bool,
    undo_transaction: Option<UndoTransaction>,
}

/// git statusサブプロセスをappイベントスレッド外で実行し、結果をAppEventで返す。
///
/// paneごとに同時実行1本まで。pane内で再要求されたら完了後に1回だけ再実行する。
struct GitRefresher {
    event_tx: CountingSender<AppEvent>,
    slots: HashMap<PaneId, GitRefreshSlot>,
}

#[derive(Default)]
struct GitRefreshSlot {
    inflight: bool,
    queued: Option<PathBuf>,
}

impl GitRefresher {
    fn new(event_tx: CountingSender<AppEvent>) -> Self {
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
        drop(
            thread::Builder::new()
                .name("fyler-git-status".to_owned())
                // Commandの起動と結果送信だけで再帰処理を持たないworker。
                .stack_size(256 * 1024)
                .spawn(move || {
                    let statuses = fyler_fsops::gitstatus::status_badges(&root).unwrap_or_default();
                    let branch = fyler_fsops::gitstatus::branch(&root);
                    let _ = event_tx.send(AppEvent::GitStatus {
                        pane_id,
                        root,
                        branch,
                        statuses,
                    });
                }),
        );
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
    gui_event_tx: &CountingSender<GuiEvent>,
    git: &mut GitRefresher,
    root: &Path,
) -> Result<ExternalChangeOutcome, mpsc::SendError<GuiEvent>> {
    let result = save_controller.on_external_change(changed_paths);
    let invalidated_dialog = matches!(
        &result,
        SaveFlowResult::PlanInvalidated(_) | SaveFlowResult::UndoInvalidated { .. }
    );
    let undo_transaction = match &result {
        SaveFlowResult::UndoInvalidated { transaction, .. } => Some(transaction.clone()),
        _ => None,
    };
    if !matches!(&result, SaveFlowResult::NoChanges) {
        send_save_result(gui_event_tx, pane_id, result)?;
    }
    send_view_state(gui_event_tx, pane_id, save_controller)?;
    git.request(pane_id, root.to_path_buf());
    Ok(ExternalChangeOutcome {
        invalidated_dialog,
        undo_transaction,
    })
}

fn after_root_change(
    pane_id: PaneId,
    gui_event_tx: &CountingSender<GuiEvent>,
    save_controller: &mut SaveController,
    git: &mut GitRefresher,
    root: &Path,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    gui_event_tx.send(GuiEvent::RootChanged {
        pane_id,
        root: root.to_path_buf(),
    })?;
    gui_event_tx.send(GuiEvent::GitBadges {
        pane_id,
        branch: None,
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

/// 先頭/末尾の`!`をreverse指定として解釈し、sortキー名を正規化する。
fn parse_sort_query(query: &str) -> Result<(SortKey, bool), String> {
    let query = query.trim();
    let leading_bang = query.starts_with('!');
    let query = query.strip_prefix('!').unwrap_or(query).trim_start();
    let (query, trailing_bang) = match query.strip_suffix('!') {
        Some(query) => (query.trim_end(), true),
        None => (query, false),
    };
    let reverse = leading_bang || trailing_bang;
    let key = match query {
        "name" => SortKey::Name,
        "date" => SortKey::Date,
        "size" => SortKey::Size,
        "ext" => SortKey::Extension,
        _ => return Err(format!("Unknown sort key: {query} (name|date|size|ext)")),
    };
    Ok((key, reverse))
}

fn sort_state_message(key: SortKey, reverse: bool) -> String {
    if reverse {
        format!("sort: {} (reverse)", sort_key_name(key))
    } else {
        format!("sort: {}", sort_key_name(key))
    }
}

fn sort_key_name(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "name",
        SortKey::Date => "date",
        SortKey::Size => "size",
        SortKey::Extension => "ext",
    }
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
        "No bookmarks or recent roots".to_owned()
    } else {
        entries.join(" | ")
    }
}

/// `handle_activate_line` の結果。呼び出し側(pane_runtime)が後続処理を分岐する。
enum ActivateOutcome {
    /// ファイルを開いた・エラー通知済み等、追加処理なし。
    Done,
    /// ディレクトリの折りたたみ/展開を適用した(git badgeの再マップが必要)。
    Toggled,
    /// 未ロードディレクトリの展開にはロードが必要。
    Load(TreePath),
}

fn handle_activate_line(
    pane_id: PaneId,
    save_controller: &mut SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    line: usize,
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<ActivateOutcome, mpsc::SendError<GuiEvent>> {
    let snapshot = engine.snapshot();
    let Some(editor_line) = snapshot.lines.get(line) else {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "Line to open was not found",
        )?;
        return Ok(ActivateOutcome::Done);
    };

    match fyler_core::grammar::split_id_prefix(&editor_line.text) {
        PrefixParse::NoId { .. } => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "This line has not been saved",
            )?;
            return Ok(ActivateOutcome::Done);
        }
        PrefixParse::Broken => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                "Cannot open a line with a broken ID prefix",
            )?;
            return Ok(ActivateOutcome::Done);
        }
        PrefixParse::WithId { .. } => {}
    }

    let Some((path, kind)) = save_controller.resolve_line(&snapshot.lines, line) else {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "File for this line was not found in the current tree",
        )?;
        return Ok(ActivateOutcome::Done);
    };

    match kind {
        EntryKind::File | EntryKind::Symlink => {
            let path = path.to_fs_path(root);
            if let Err(error) = fyler_fsops::open::open_with_default_app(&path) {
                send_gui_message(
                    gui_event_tx,
                    pane_id,
                    MessageKind::Error,
                    format!("Failed to open file: {error:#}"),
                )?;
            }
        }
        EntryKind::Dir => {
            if snapshot.dirty {
                send_gui_message(
                    gui_event_tx,
                    pane_id,
                    MessageKind::Info,
                    "Cannot change folds while editing. Save or discard changes.",
                )?;
                return Ok(ActivateOutcome::Done);
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
                            format!("Failed to update folded view: {error:#}"),
                        )?;
                    }
                    send_view_state(gui_event_tx, pane_id, save_controller)?;
                    return Ok(ActivateOutcome::Toggled);
                }
                ToggleCollapseResult::NotADirectory => {
                    send_gui_message(
                        gui_event_tx,
                        pane_id,
                        MessageKind::Error,
                        "Target line is not a directory",
                    )?;
                }
                ToggleCollapseResult::CannotExpandIncomplete => {
                    send_gui_message(
                        gui_event_tx,
                        pane_id,
                        MessageKind::Info,
                        "Cannot expand: directory could not be read (access denied or unavailable)",
                    )?;
                }
                ToggleCollapseResult::NeedsLoad { dir } => return Ok(ActivateOutcome::Load(dir)),
                ToggleCollapseResult::NotFound => {
                    send_gui_message(
                        gui_event_tx,
                        pane_id,
                        MessageKind::Error,
                        "Directory for this line was not found in the current tree",
                    )?;
                }
                ToggleCollapseResult::Busy => {}
            }
        }
    }
    Ok(ActivateOutcome::Done)
}

fn handle_open_with(
    pane_id: PaneId,
    save_controller: &SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    line: usize,
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<Option<(PathBuf, Vec<OpenWithHandler>)>, mpsc::SendError<GuiEvent>> {
    let snapshot = engine.snapshot();
    let Some(editor_line) = snapshot.lines.get(line) else {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "Line for open-with was not found",
        )?;
        return Ok(None);
    };

    match fyler_core::grammar::split_id_prefix(&editor_line.text) {
        PrefixParse::NoId { .. } => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "This line has not been saved",
            )?;
            return Ok(None);
        }
        PrefixParse::Broken => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                "Cannot use open-with on a line with a broken ID prefix",
            )?;
            return Ok(None);
        }
        PrefixParse::WithId { .. } => {}
    }

    let Some((path, kind)) = save_controller.resolve_line(&snapshot.lines, line) else {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "File for this line was not found in the current tree",
        )?;
        return Ok(None);
    };

    if kind == EntryKind::Dir {
        send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Info,
            "Open-with is for files only",
        )?;
        return Ok(None);
    }

    let path = path.to_fs_path(root);
    let handlers = match fyler_fsops::openwith::enumerate_handlers(&path) {
        Ok(handlers) => handlers,
        Err(error) => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                format!("Failed to enumerate open-with candidates: {error:#}"),
            )?;
            return Ok(None);
        }
    };
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    gui_event_tx.send(GuiEvent::ShowOpenWith {
        file_name,
        choices: open_with_choices(&handlers),
    })?;
    Ok(Some((path, handlers)))
}

fn open_with_choices(handlers: &[OpenWithHandler]) -> Vec<String> {
    let mut choices = handlers
        .iter()
        .map(|handler| handler.display_name.clone())
        .collect::<Vec<_>>();
    choices.push("Open with... (Windows dialog)".to_owned());
    choices
}

fn handle_yank_path(
    pane_id: PaneId,
    save_controller: &SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    line: usize,
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    let snapshot = engine.snapshot();
    let Some(editor_line) = snapshot.lines.get(line) else {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "Line to copy was not found",
        );
    };

    match fyler_core::grammar::split_id_prefix(&editor_line.text) {
        PrefixParse::NoId { .. } => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "This line has not been saved",
            );
        }
        PrefixParse::Broken => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                "Cannot copy a line with a broken ID prefix",
            );
        }
        PrefixParse::WithId { .. } => {}
    }

    let Some((path, _)) = save_controller.resolve_line(&snapshot.lines, line) else {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "File for this line was not found in the current tree",
        );
    };
    let path = path.to_fs_path(root);
    gui_event_tx.send(GuiEvent::CopyPath(path.to_string_lossy().into_owned()))
}

/// Copy/Cut(`Ctrl+C`/`Ctrl+X`)を実行する。実在entry(IDなし行を除く)を絶対パスへ
/// 解決し、Windows Shell clipboardへ書き込む。実FSは一切変更しない。
fn handle_clipboard_copy_or_cut(
    pane_id: PaneId,
    save_controller: &SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    effect: fyler_core::transfer::DropEffect,
    selected_lines: &[usize],
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    let snapshot = engine.snapshot();
    let selected = match crate::transfer_flow::resolve_selection(
        save_controller,
        &snapshot.lines,
        selected_lines,
    ) {
        Ok(selected) => selected,
        Err(crate::transfer_flow::SelectionError::UnsavedLine) => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "Cannot copy because the selection contains an unsaved new line. Save first.",
            );
        }
        Err(crate::transfer_flow::SelectionError::Empty) => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "No item is selected",
            );
        }
        Err(crate::transfer_flow::SelectionError::MissingLine) => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                "Line to copy was not found",
            );
        }
        Err(crate::transfer_flow::SelectionError::UnknownId) => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                "Failed to resolve the selection in the current file list",
            );
        }
    };
    let paths: Vec<PathBuf> = crate::transfer_flow::dedupe_ancestors(&selected)
        .into_iter()
        .map(|(path, _)| path.to_fs_path(root))
        .collect();
    let count = paths.len();
    match fyler_fsops::clipboard::write(&paths, effect) {
        Ok(()) => {
            let verb = match effect {
                fyler_core::transfer::DropEffect::Copy => "Copied",
                fyler_core::transfer::DropEffect::Move => "Cut",
            };
            let noun = if count == 1 { "item" } else { "items" };
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                format!("{verb} {count} {noun} to clipboard"),
            )
        }
        Err(error) => send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            format!("Failed to write to the clipboard: {error:#}"),
        ),
    }
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
        Some("Cannot start search while another dialog or file operation is active")
    } else if crashed {
        Some("Cannot start search because the editor engine has stopped")
    } else if !save_idle {
        Some("Cannot start search while saving")
    } else {
        None
    }
}

/// 外部terminalのcwdをカーソル行から解決する。
///
/// ディレクトリ行はそのパス、ファイル・symlink行は親ディレクトリを返す。
/// 未保存行、stale行、範囲外など解決できない行は現在rootへフォールバックする。
fn resolve_terminal_cwd(
    save_controller: &SaveController,
    lines: &[fyler_core::editor::EditorLine],
    line: usize,
    root: &Path,
) -> PathBuf {
    match save_controller.resolve_line(lines, line) {
        Some((path, EntryKind::Dir)) => path.to_fs_path(root),
        Some((path, _)) => {
            let fs_path = path.to_fs_path(root);
            fs_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root.to_path_buf())
        }
        None => root.to_path_buf(),
    }
}

fn open_terminal_rejection(
    dialog_open: bool,
    apply_running: bool,
    transfer_awaiting: bool,
    transfer_running: bool,
    crashed: bool,
    save_idle: bool,
) -> Option<&'static str> {
    if dialog_open || apply_running || transfer_awaiting || transfer_running {
        Some("Cannot open terminal while another dialog or file operation is active")
    } else if crashed {
        Some("Cannot open terminal because the editor engine has stopped")
    } else if !save_idle {
        Some("Cannot open terminal while saving")
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_open_terminal(
    pane_id: PaneId,
    save_controller: &SaveController,
    lines: &[fyler_core::editor::EditorLine],
    root: &Path,
    line: usize,
    terminal_kind: TerminalKind,
    crashed: bool,
    dialog_open: bool,
    apply_running: bool,
    transfer_awaiting: bool,
    transfer_running: bool,
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    if let Some(message) = open_terminal_rejection(
        dialog_open,
        apply_running,
        transfer_awaiting,
        transfer_running,
        crashed,
        save_controller.is_idle(),
    ) {
        return send_gui_message(gui_event_tx, pane_id, MessageKind::Info, message);
    }

    let cwd = resolve_terminal_cwd(save_controller, lines, line, root);
    if let Err(error) = fyler_fsops::terminal::open(&cwd, terminal_kind) {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            format!("Failed to start terminal: {error:#}"),
        );
    }
    Ok(())
}

/// Rename / Mark for deletion(バッファ編集のみのcontext menu項目)の権威判定。
/// `open_terminal_rejection` と同じ形((dialog/apply/transfer/crashed/save)。
fn tree_edit_rejection(
    dialog_open: bool,
    apply_running: bool,
    transfer_awaiting: bool,
    transfer_running: bool,
    crashed: bool,
    save_idle: bool,
) -> Option<&'static str> {
    if dialog_open || apply_running || transfer_awaiting || transfer_running {
        Some("Cannot edit the tree while another dialog or file operation is active")
    } else if crashed {
        Some("Cannot edit the tree because the editor engine has stopped")
    } else if !save_idle {
        Some("Cannot edit the tree while saving")
    } else {
        None
    }
}

/// Rename(`EditorCommand::BeginNameEdit`)の実行。実FSへは一切触れない。
fn handle_begin_name_edit(
    pane_id: PaneId,
    engine: &dyn EditorEngine,
    line: usize,
    rejection: Option<&'static str>,
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    if let Some(message) = rejection {
        return send_gui_message(gui_event_tx, pane_id, MessageKind::Info, message);
    }

    let snapshot = engine.snapshot();
    let Some(editor_line) = snapshot.lines.get(line) else {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "Line to rename was not found",
        );
    };

    match fyler_core::grammar::split_id_prefix(&editor_line.text) {
        PrefixParse::NoId { .. } => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "This line has not been saved",
            );
        }
        PrefixParse::Broken => {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                "Cannot rename a line with a broken ID prefix",
            );
        }
        PrefixParse::WithId { .. } => {}
    }

    if let Err(error) = engine.send(EditorCommand::BeginNameEdit { line }) {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            format!("Failed to start rename: {error:#}"),
        );
    }
    Ok(())
}

/// Mark for deletion(`EditorCommand::DeleteLine`)の実行。バッファから行を
/// 除去するだけで、実FSへは一切触れない(通常のdirty→`:w`→確認→apply経路に乗る)。
/// IDなし(unsaved)行も対象にできる。
fn handle_mark_for_deletion(
    pane_id: PaneId,
    engine: &dyn EditorEngine,
    line: usize,
    rejection: Option<&'static str>,
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    if let Some(message) = rejection {
        return send_gui_message(gui_event_tx, pane_id, MessageKind::Info, message);
    }

    let snapshot = engine.snapshot();
    if snapshot.lines.get(line).is_none() {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            "Line to remove was not found",
        );
    }

    if let Err(error) = engine.send(EditorCommand::DeleteLine { line }) {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Error,
            format!("Failed to remove line: {error:#}"),
        );
    }
    Ok(())
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
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<bool, mpsc::SendError<GuiEvent>> {
    if let Some(message) = open_file_picker_rejection(
        dialog_open,
        apply_running,
        transfer_awaiting,
        transfer_running,
        crashed,
        save_controller.is_idle(),
    ) {
        send_gui_message(gui_event_tx, pane_id, MessageKind::Info, message)?;
        return Ok(false);
    }
    gui_event_tx.send(GuiEvent::ShowFilePicker { pane_id })?;
    Ok(true)
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
    path: TreePath,
    action: PickerAction,
    save_controller: &mut SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    gui_event_tx: &CountingSender<GuiEvent>,
    open_path: &mut dyn FnMut(&Path) -> anyhow::Result<()>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    if action == PickerAction::Open {
        let path = path.to_fs_path(root);
        if let Err(error) = open_path(&path) {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                format!("Failed to open target: {error:#}"),
            )?;
        }
        return Ok(());
    }

    let Some(entry) = save_controller.baseline().get_by_path(&path).cloned() else {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Warn,
            "Search candidate was not found. It may have changed externally.",
        );
    };
    let entry_id = entry.id;

    let snapshot = engine.snapshot();
    if let Some(line) = visible_line_for_entry(save_controller, entry_id) {
        if !snapshot_line_matches_entry(&snapshot, line, entry_id) {
            return send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "Cannot navigate because the edited line position does not match the search candidate",
            );
        }
        if let Err(error) = engine.send(EditorCommand::SetCursorLine(line)) {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Error,
                format!("Failed to navigate to search candidate: {error:#}"),
            )?;
        }
        return Ok(());
    }

    if snapshot.dirty {
        return send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Info,
            "Cannot reveal a folded search candidate while editing. Save or discard changes.",
        );
    }

    match save_controller.reveal_entry(entry_id) {
        RevealResult::AlreadyVisible { line } => {
            if let Err(error) = engine.send(EditorCommand::SetCursorLine(line)) {
                send_gui_message(
                    gui_event_tx,
                    pane_id,
                    MessageKind::Error,
                    format!("Failed to navigate to search candidate: {error:#}"),
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
                    format!("Failed to reveal search candidate: {error:#}"),
                )?;
            }
            send_view_state(gui_event_tx, pane_id, save_controller)?;
        }
        RevealResult::NotFound => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Warn,
                "Search candidate was not found. It may have changed externally.",
            )?;
        }
        RevealResult::Busy => {
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Info,
                "Cannot navigate to search candidate while saving",
            )?;
        }
    }
    Ok(())
}

fn next_picker_reveal_directory(
    baseline: &fyler_core::tree::BaselineTree,
    target: &TreePath,
) -> Option<TreePath> {
    let parent_depth = target.depth().saturating_sub(1);
    (1..=parent_depth)
        .map(|depth| TreePath::from_components(target.components()[..depth].iter().cloned()))
        .find(|path| baseline.is_unloaded(path))
}

fn handle_picker_select(
    pane_id: PaneId,
    path: TreePath,
    action: PickerAction,
    save_controller: &mut SaveController,
    engine: &dyn EditorEngine,
    root: &Path,
    gui_event_tx: &CountingSender<GuiEvent>,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    handle_picker_select_with(
        pane_id,
        path,
        action,
        save_controller,
        engine,
        root,
        gui_event_tx,
        &mut fyler_fsops::open::open_with_default_app,
    )
}

fn send_gui_message(
    gui_event_tx: &CountingSender<GuiEvent>,
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
    gui_event_tx: &CountingSender<GuiEvent>,
    pane_id: PaneId,
    save_controller: &mut SaveController,
) -> Result<(), mpsc::SendError<GuiEvent>> {
    gui_event_tx.send(GuiEvent::FileInfos {
        pane_id,
        infos: save_controller.visible_file_infos(),
    })?;
    gui_event_tx.send(GuiEvent::CollapsedDirs {
        pane_id,
        dirs: save_controller.collapsed_dirs(),
    })?;
    gui_event_tx.send(GuiEvent::IncompleteDirs {
        pane_id,
        dirs: save_controller.incomplete_dir_ids(),
    })?;
    gui_event_tx.send(GuiEvent::PaneHealth {
        pane_id,
        offline: save_controller.is_offline(),
        unreadable: save_controller.unreadable_count(),
    })?;
    if let Some((kind, message)) = save_controller.take_scan_health_message() {
        send_gui_message(gui_event_tx, pane_id, kind, message)?;
    }
    Ok(())
}

fn send_save_result(
    gui_event_tx: &CountingSender<GuiEvent>,
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
        SaveFlowResult::ShowUndoPlan {
            transaction,
            statuses,
        } => gui_event_tx.send(GuiEvent::ShowUndoPlan {
            lines: undo_format::undo_plan_lines(&transaction, &statuses),
        }),
        SaveFlowResult::UndoNothingLeft { reasons } => {
            let message = if reasons.is_empty() {
                "No operations are available to undo".to_owned()
            } else {
                format!(
                    "No operations are available to undo: {}",
                    reasons.join(" / ")
                )
            };
            send_gui_message(gui_event_tx, pane_id, MessageKind::Info, message)
        }
        SaveFlowResult::ShowUndoReport(report) => {
            let (lines, any_failed) = undo_format::undo_report_lines(&report);
            gui_event_tx.send(GuiEvent::ShowUndoReport { lines, any_failed })
        }
        SaveFlowResult::UndoReconcileFailed { report, error } => {
            let (lines, any_failed) = undo_format::undo_report_lines(&report);
            gui_event_tx.send(GuiEvent::ShowUndoReport { lines, any_failed })?;
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Warn,
                format!(
                    "The folder became offline or unreachable after undo. Reconnect and it will refresh automatically. ({error})"
                ),
            )
        }
        SaveFlowResult::ReconcileFailed { report, error } => {
            gui_event_tx.send(GuiEvent::ShowReport(report))?;
            send_gui_message(
                gui_event_tx,
                pane_id,
                MessageKind::Warn,
                format!(
                    "The folder became offline or unreachable after the operation. Reconnect and it will refresh automatically. ({error})"
                ),
            )
        }
        SaveFlowResult::ExternalChanged => Ok(()),
        SaveFlowResult::ExternalChangeNotified(text) => {
            send_gui_message(gui_event_tx, pane_id, MessageKind::Info, text)
        }
        SaveFlowResult::ExternalChangeFailed(text) => {
            send_gui_message(gui_event_tx, pane_id, MessageKind::Error, text)
        }
        SaveFlowResult::WentOffline(text) => send_gui_message(
            gui_event_tx,
            pane_id,
            MessageKind::Warn,
            format!(
                "The folder is offline or unreachable. Reconnect and it will refresh automatically. ({text})"
            ),
        ),
        SaveFlowResult::Reconnected(text) => {
            send_gui_message(gui_event_tx, pane_id, MessageKind::Info, text)
        }
        SaveFlowResult::OfflineRejected(text) => {
            send_gui_message(gui_event_tx, pane_id, MessageKind::Info, text)
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
            send_gui_message(gui_event_tx, pane_id, MessageKind::Info, "No changes")
        }
        SaveFlowResult::Cancelled => gui_event_tx.send(GuiEvent::CloseDialog),
        SaveFlowResult::UndoCancelled { .. } => gui_event_tx.send(GuiEvent::CloseDialog),
        SaveFlowResult::StartApply { .. } => {
            debug_assert!(
                false,
                "StartApply must already be handled by the Confirm arm"
            );
            Ok(())
        }
        SaveFlowResult::StartUndo { .. } => {
            debug_assert!(
                false,
                "StartUndo must already be handled by the Confirm arm"
            );
            Ok(())
        }
        SaveFlowResult::ApplyCancelRequested => {
            debug_assert!(
                false,
                "ApplyCancelRequested must already be handled by the Confirm arm"
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
    use tempfile::tempdir;

    use super::*;

    fn counting_channel<T>() -> (CountingSender<T>, mpsc::Receiver<T>) {
        let (tx, rx) = mpsc::channel();
        let gauge = Arc::new(crate::queue_stats::QueueGauge::new());
        (CountingSender::new(tx, gauge), rx)
    }

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

    fn terminal_line(id: EntryId, name: &str) -> EditorLine {
        EditorLine::new(format!(
            "{}{name}",
            fyler_core::grammar::format_id_prefix(id)
        ))
    }

    #[test]
    fn terminal_cwd_uses_directory_itself() {
        let (controller, _) = picker_controller(false, false);
        let lines = [terminal_line(EntryId(1), "dir/")];

        assert_eq!(
            resolve_terminal_cwd(&controller, &lines, 0, Path::new("root")),
            PathBuf::from("root/dir")
        );
    }

    #[test]
    fn terminal_cwd_uses_file_parent() {
        let (controller, _) = picker_controller(false, false);
        let lines = [terminal_line(EntryId(2), "file.txt")];

        assert_eq!(
            resolve_terminal_cwd(&controller, &lines, 0, Path::new("root")),
            PathBuf::from("root/dir")
        );
    }

    #[test]
    fn terminal_cwd_uses_symlink_parent() {
        let (controller, _) = picker_controller(false, false);
        let lines = [terminal_line(EntryId(3), "link")];

        assert_eq!(
            resolve_terminal_cwd(&controller, &lines, 0, Path::new("root")),
            PathBuf::from("root")
        );
    }

    #[test]
    fn terminal_cwd_falls_back_to_root_for_unsaved_line() {
        let (controller, _) = picker_controller(false, false);
        let lines = [EditorLine::new("new.txt")];

        assert_eq!(
            resolve_terminal_cwd(&controller, &lines, 0, Path::new("root")),
            PathBuf::from("root")
        );
    }

    #[test]
    fn terminal_cwd_falls_back_to_root_for_stale_entry() {
        let (controller, _) = picker_controller(false, false);
        let lines = [terminal_line(EntryId(999), "stale.txt")];

        assert_eq!(
            resolve_terminal_cwd(&controller, &lines, 0, Path::new("root")),
            PathBuf::from("root")
        );
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

    #[test]
    fn parse_sort_query_accepts_keys_and_bang_reverse() {
        assert_eq!(parse_sort_query("name"), Ok((SortKey::Name, false)));
        assert_eq!(parse_sort_query("date!"), Ok((SortKey::Date, true)));
        assert_eq!(parse_sort_query("!date"), Ok((SortKey::Date, true)));
        assert_eq!(parse_sort_query("!date!"), Ok((SortKey::Date, true)));
        assert_eq!(parse_sort_query(" size! "), Ok((SortKey::Size, true)));
        assert_eq!(parse_sort_query("ext"), Ok((SortKey::Extension, false)));
    }

    #[test]
    fn parse_sort_query_rejects_unknown_or_empty_key() {
        assert!(parse_sort_query("mtime").unwrap_err().contains("Unknown"));
        assert!(parse_sort_query("").unwrap_err().contains("Unknown"));
    }

    #[test]
    fn sort_state_message_formats_reverse_suffix() {
        assert_eq!(sort_state_message(SortKey::Date, false), "sort: date");
        assert_eq!(
            sort_state_message(SortKey::Size, true),
            "sort: size (reverse)"
        );
    }

    #[test]
    fn open_with_choices_always_appends_system_dialog() {
        assert_eq!(
            open_with_choices(&[]),
            ["Open with... (Windows dialog)".to_owned()]
        );
        assert_eq!(
            open_with_choices(&[
                OpenWithHandler {
                    display_name: "Editor".to_owned(),
                    key: "editor.exe".to_owned(),
                },
                OpenWithHandler {
                    display_name: "Viewer".to_owned(),
                    key: "viewer.exe".to_owned(),
                },
            ]),
            [
                "Editor".to_owned(),
                "Viewer".to_owned(),
                "Open with... (Windows dialog)".to_owned(),
            ]
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
        let (event_tx, _event_rx) = counting_channel();
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
        let (event_tx, _event_rx) = counting_channel();
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
        let (gui_tx, gui_rx) = counting_channel();

        assert!(
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
            .unwrap()
        );

        let GuiEvent::ShowFilePicker { pane_id } = gui_rx.recv().unwrap() else {
            panic!("expected picker event")
        };
        assert_eq!(pane_id, PaneId::new(1));
    }

    #[test]
    fn stale_picker_path_warns_without_reveal_or_cursor_move() {
        let (mut controller, engine) = picker_controller(true, false);
        let (gui_tx, gui_rx) = counting_channel();
        let mut opened = Vec::new();

        handle_picker_select_with(
            PaneId::new(1),
            TreePath::parse("missing.txt"),
            PickerAction::Jump,
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
        assert!(controller.collapsed_dirs().contains(&EntryId(1)));
        assert!(received_message(&gui_rx).text.contains("externally"));
    }

    #[test]
    fn picker_jump_loads_two_unloaded_ancestors_then_reveals_target() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("first/second")).unwrap();
        std::fs::write(root.path().join("first/second/target.txt"), b"target").unwrap();
        let target = TreePath::parse("first/second/target.txt");
        let mut ids = IdAllocator::new();
        let shallow = fyler_fsops::scan::scan_baseline_shallow_with(
            root.path(),
            &mut ids,
            &fyler_fsops::scan::ScanOptions::default(),
        )
        .unwrap();
        assert_eq!(
            next_picker_reveal_directory(&shallow, &target),
            Some(TreePath::parse("first"))
        );
        let first = fyler_fsops::scan::load_directory(
            root.path(),
            &TreePath::parse("first"),
            &mut ids,
            &shallow,
            &fyler_fsops::scan::ScanOptions::default(),
        )
        .unwrap();
        assert_eq!(
            next_picker_reveal_directory(&first, &target),
            Some(TreePath::parse("first/second"))
        );
        let loaded = fyler_fsops::scan::load_directory(
            root.path(),
            &TreePath::parse("first/second"),
            &mut ids,
            &first,
            &fyler_fsops::scan::ScanOptions::default(),
        )
        .unwrap();
        assert!(loaded.get_by_path(&target).is_some());
        assert_eq!(next_picker_reveal_directory(&loaded, &target), None);

        let engine = Arc::new(PickerEngine::default());
        let save_engine: Arc<dyn EditorEngine> = engine.clone();
        let mut controller =
            SaveController::new(root.path().to_path_buf(), ids, loaded, save_engine);
        controller.collapse_all_dirs();
        engine.set_snapshot(controller.visible_lines(), false);
        let (gui_tx, _gui_rx) = counting_channel();
        handle_picker_select_with(
            PaneId::new(1),
            target,
            PickerAction::Jump,
            &mut controller,
            engine.as_ref(),
            root.path(),
            &gui_tx,
            &mut |_| Ok(()),
        )
        .unwrap();

        assert!(matches!(
            engine.commands().as_slice(),
            [EditorCommand::SetLines {
                cursor_line: Some(2),
                ..
            }]
        ));
    }

    #[test]
    fn picker_reveal_warns_when_target_disappears_during_chain_load() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("first/second")).unwrap();
        let target_path = root.path().join("first/second/target.txt");
        std::fs::write(&target_path, b"target").unwrap();
        let target = TreePath::parse("first/second/target.txt");
        let mut ids = IdAllocator::new();
        let shallow = fyler_fsops::scan::scan_baseline_shallow_with(
            root.path(),
            &mut ids,
            &fyler_fsops::scan::ScanOptions::default(),
        )
        .unwrap();
        let first = fyler_fsops::scan::load_directory(
            root.path(),
            &TreePath::parse("first"),
            &mut ids,
            &shallow,
            &fyler_fsops::scan::ScanOptions::default(),
        )
        .unwrap();
        std::fs::remove_file(target_path).unwrap();
        let loaded = fyler_fsops::scan::load_directory(
            root.path(),
            &TreePath::parse("first/second"),
            &mut ids,
            &first,
            &fyler_fsops::scan::ScanOptions::default(),
        )
        .unwrap();
        assert!(loaded.get_by_path(&target).is_none());
        assert_eq!(next_picker_reveal_directory(&loaded, &target), None);
        let engine = Arc::new(PickerEngine::default());
        let save_engine: Arc<dyn EditorEngine> = engine.clone();
        let mut controller =
            SaveController::new(root.path().to_path_buf(), ids, loaded, save_engine);
        engine.set_snapshot(controller.visible_lines(), false);
        let (gui_tx, gui_rx) = counting_channel();

        handle_picker_select_with(
            PaneId::new(1),
            target,
            PickerAction::Jump,
            &mut controller,
            engine.as_ref(),
            root.path(),
            &gui_tx,
            &mut |_| Ok(()),
        )
        .unwrap();

        assert!(received_message(&gui_rx).text.contains("externally"));
        assert!(engine.commands().is_empty());
    }

    #[test]
    fn picker_jump_to_visible_entry_sends_cursor_only() {
        let (mut controller, engine) = picker_controller(false, false);
        let (gui_tx, _gui_rx) = counting_channel();

        handle_picker_select_with(
            PaneId::new(1),
            TreePath::parse("dir/file.txt"),
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
        let (gui_tx, gui_rx) = counting_channel();

        handle_picker_select_with(
            PaneId::new(1),
            TreePath::parse("dir/file.txt"),
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
        let (gui_tx, gui_rx) = counting_channel();

        handle_picker_select_with(
            PaneId::new(1),
            TreePath::parse("dir/file.txt"),
            PickerAction::Jump,
            &mut controller,
            engine.as_ref(),
            Path::new("root"),
            &gui_tx,
            &mut |_| Ok(()),
        )
        .unwrap();

        assert!(engine.commands().is_empty());
        assert!(received_message(&gui_rx).text.contains("editing"));
        assert!(controller.collapsed_dirs().contains(&EntryId(1)));
    }

    #[test]
    fn picker_jump_rejects_dirty_visible_line_with_mismatched_id() {
        let (mut controller, engine) = picker_controller(false, true);
        let mut lines = controller.visible_lines();
        lines[1] = EditorLine::new("edited without id");
        engine.set_snapshot(lines, true);
        let (gui_tx, gui_rx) = counting_channel();

        handle_picker_select_with(
            PaneId::new(1),
            TreePath::parse("dir/file.txt"),
            PickerAction::Jump,
            &mut controller,
            engine.as_ref(),
            Path::new("root"),
            &gui_tx,
            &mut |_| Ok(()),
        )
        .unwrap();

        assert!(engine.commands().is_empty());
        assert!(received_message(&gui_rx).text.contains("does not match"));
    }

    #[test]
    fn picker_open_uses_default_open_path_for_every_entry_kind() {
        let (mut controller, engine) = picker_controller(true, true);
        let (gui_tx, _gui_rx) = counting_channel();
        let mut opened = Vec::new();

        for path in ["dir/file.txt", "link", "dir"] {
            handle_picker_select_with(
                PaneId::new(1),
                TreePath::parse(path),
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

    #[test]
    fn picker_open_uses_catalog_path_without_loading_baseline() {
        let (mut controller, engine) = picker_controller(true, true);
        let (gui_tx, _gui_rx) = counting_channel();
        let mut opened = Vec::new();

        handle_picker_select_with(
            PaneId::new(1),
            TreePath::parse("unloaded/deep.txt"),
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

        assert_eq!(opened, [PathBuf::from("root/unloaded/deep.txt")]);
        assert!(engine.commands().is_empty());
    }
}
