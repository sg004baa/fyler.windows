//! eframeアプリ本体。毎フレーム、エンジンのsnapshotだけを描画する。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;

use eframe::egui;
use fyler_core::editor::{CmdlineState, EditorEngine, EditorEvent, EditorMessage};
use fyler_core::fileinfo::FileInfo;
use fyler_core::gitstatus::GitBadge;
use fyler_core::id::EntryId;
use fyler_core::plan::OperationPlan;
use fyler_core::report::CommitReport;
use fyler_core::validate::ValidateError;

use crate::confirm::{ConfirmChoice, ConfirmDetail, IconStyle};
use crate::{cmdline, confirm, input, modeline, tree_view};

const CJK_FONT_NAME: &str = "fyler-cjk";

/// app層からGUI起動時に渡す表示設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiOptions {
    /// 保存確認ダイアログの操作一覧詳細度。
    pub confirm_detail: ConfirmDetail,
    /// ユーザーが明示した日本語fallbackフォントの絶対パス。
    pub font_path: Option<PathBuf>,
    /// ツリーへ描画するファイルアイコンのスタイル。
    pub icon_style: IconStyle,
}

/// app層からGUIへ渡す描画指示。
#[derive(Debug, Clone)]
pub enum GuiEvent {
    Editor(EditorEvent),
    /// app層で表示ルートが切り替わったことをモードラインへ反映する。
    RootChanged(PathBuf),
    /// baselineのエントリIDに対応するGit装飾を全件差し替える。
    GitBadges(HashMap<EntryId, GitBadge>),
    /// 表示中のエントリIDに対応する表示用メタデータを全件差し替える。
    FileInfos(HashMap<EntryId, FileInfo>),
    /// 指定された表示用パスをクリップボードへコピーする。
    CopyPath(String),
    /// 保存planと実行前に確認すべき警告を表示する。
    ShowPlan {
        plan: OperationPlan,
        warnings: Vec<String>,
    },
    ShowReport(CommitReport),
    ShowValidationErrors(Vec<ValidateError>),
    FatalError(String),
    CloseDialog,
}

#[derive(Debug, Clone)]
enum DialogState {
    Plan {
        plan: OperationPlan,
        warnings: Vec<String>,
    },
    Report(CommitReport),
    ValidationErrors(Vec<ValidateError>),
}

/// fylerのGUIアプリケーション。
///
/// 描画契約:
/// - 毎フレーム [`EditorEngine::snapshot`] を1回だけ取得し、そのsnapshotのみで
///   描画する(lines/cursor/modeを別々のタイミングで読まない。整合性のため)
/// - RPC完了を同期待ちしない。入力は [`EditorEngine::send`] へ投げるだけ
pub struct FylerApp {
    pub engine: Arc<dyn EditorEngine>,
    event_rx: mpsc::Receiver<GuiEvent>,
    cmdline: Option<CmdlineState>,
    message: Option<EditorMessage>,
    root: PathBuf,
    git_badges: HashMap<EntryId, GitBadge>,
    file_infos: HashMap<EntryId, FileInfo>,
    pending_copy: Option<String>,
    engine_error: Option<String>,
    dialog: Option<DialogState>,
    confirm_tx: mpsc::Sender<ConfirmChoice>,
    confirm_detail: ConfirmDetail,
    icon_style: IconStyle,
}

impl FylerApp {
    fn new(
        engine: Arc<dyn EditorEngine>,
        gui_events: mpsc::Receiver<GuiEvent>,
        confirm_tx: mpsc::Sender<ConfirmChoice>,
        confirm_detail: ConfirmDetail,
        icon_style: IconStyle,
        repaint_context: egui::Context,
    ) -> anyhow::Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();
        thread::Builder::new()
            .name("fyler-editor-events".to_owned())
            .spawn(move || {
                while let Ok(event) = gui_events.recv() {
                    if event_tx.send(event).is_err() {
                        return;
                    }
                    repaint_context.request_repaint();
                }
            })
            .map_err(|error| anyhow::anyhow!("エディタイベント監視を開始できません: {error}"))?;

        Ok(Self {
            engine,
            event_rx,
            cmdline: None,
            message: None,
            root: PathBuf::new(),
            git_badges: HashMap::new(),
            file_infos: HashMap::new(),
            pending_copy: None,
            engine_error: None,
            dialog: None,
            confirm_tx,
            confirm_detail,
            icon_style,
        })
    }

    fn receive_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                GuiEvent::Editor(event) => match event {
                    EditorEvent::SnapshotUpdated => {}
                    EditorEvent::ActivateLine { .. } => {}
                    EditorEvent::YankPath { .. } => {}
                    EditorEvent::NavigateParent => {}
                    EditorEvent::ToggleHidden => {}
                    EditorEvent::JumpBookmark { .. } => {}
                    EditorEvent::CommitRequested { .. } => {}
                    EditorEvent::CmdlineShow(state) => self.cmdline = Some(state),
                    EditorEvent::CmdlineHide => self.cmdline = None,
                    EditorEvent::Message(message) => self.message = Some(message),
                    EditorEvent::EngineCrashed { reason } => {
                        self.engine_error = Some(format!("編集エンジンが停止しました: {reason}"));
                    }
                },
                GuiEvent::RootChanged(root) => self.root = root,
                GuiEvent::GitBadges(git_badges) => self.git_badges = git_badges,
                GuiEvent::FileInfos(file_infos) => self.file_infos = file_infos,
                GuiEvent::CopyPath(path) => self.pending_copy = Some(path),
                GuiEvent::ShowPlan { plan, warnings } => {
                    self.dialog = Some(DialogState::Plan { plan, warnings });
                }
                GuiEvent::ShowReport(report) => {
                    self.dialog = Some(DialogState::Report(report));
                }
                GuiEvent::ShowValidationErrors(errors) => {
                    self.dialog = Some(DialogState::ValidationErrors(errors));
                }
                GuiEvent::FatalError(error) => {
                    self.engine_error = Some(error);
                }
                GuiEvent::CloseDialog => self.dialog = None,
            }
        }
    }
}

impl eframe::App for FylerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive_events();
        if let Some(path) = self.pending_copy.take() {
            ctx.copy_text(path.clone());
            self.message = Some(EditorMessage {
                kind: fyler_core::editor::MessageKind::Info,
                text: format!("コピーしました: {path}"),
            });
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 描画と入力判断は、フレーム冒頭に1回だけ得た同一snapshotを使う。
        let snapshot = self.engine.snapshot();

        if self.engine_error.is_none()
            && self.dialog.is_none()
            && let Err(error) = input::forward_input(ui.ctx(), self.engine.as_ref(), &snapshot.mode)
        {
            self.engine_error = Some(format!("編集エンジンへ入力を送信できません: {error}"));
        }

        egui::Panel::bottom("modeline").show(ui, |ui| {
            modeline::draw(ui, &snapshot, &self.root, &self.file_infos);
            if let Some(state) = &self.cmdline {
                cmdline::draw_cmdline(ui, state);
            } else if let Some(message) = &self.message {
                cmdline::draw_message(ui, message);
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(error) = &self.engine_error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            } else {
                tree_view::draw(ui, &snapshot, &self.git_badges, self.icon_style);
            }
        });

        let mut confirm_choice = None;
        let mut dismiss_errors = false;
        let mut dismiss_report = false;
        match &self.dialog {
            Some(DialogState::Plan { plan, warnings }) => {
                confirm_choice = confirm::draw_plan(ui, plan, warnings, self.confirm_detail);
            }
            Some(DialogState::Report(report)) => {
                dismiss_report = confirm::draw_report(ui, report);
            }
            Some(DialogState::ValidationErrors(errors)) => {
                let dismiss_from_keyboard = ui.ctx().input(|input| {
                    input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape)
                });
                dismiss_errors = egui::Modal::new(egui::Id::new("save-validation-errors"))
                    .show(ui.ctx(), |ui| {
                        ui.heading("保存できません");
                        ui.add_space(8.0);
                        confirm::draw_validation_errors(ui, errors);
                        ui.add_space(12.0);
                        ui.button("Dismiss (Enter / Esc)").clicked() || dismiss_from_keyboard
                    })
                    .inner;
            }
            None => {}
        }

        if dismiss_errors {
            self.dialog = None;
        }
        if dismiss_report {
            self.dialog = None;
        }
        if let Some(choice) = confirm_choice
            && self.confirm_tx.send(choice).is_err()
        {
            self.engine_error = Some("確認結果をアプリへ送信できません".to_owned());
        }
    }
}

/// GUIを起動する(メインスレッドで呼ぶこと。eframeの制約)。
///
/// 実装契約(M1):
/// - `eframe::run_native` で [`FylerApp`] を起動する
/// - エンジンのイベント(`EditorEvent`)受信で `ctx.request_repaint()` を呼び、
///   ポーリングなしで再描画されるようにする
pub fn run(
    engine: Arc<dyn EditorEngine>,
    event_rx: mpsc::Receiver<GuiEvent>,
    confirm_tx: mpsc::Sender<ConfirmChoice>,
    gui_options: GuiOptions,
) -> anyhow::Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "fyler",
        native_options,
        Box::new(move |creation_context| {
            let GuiOptions {
                confirm_detail,
                font_path,
                icon_style,
            } = gui_options;
            install_fallback_font(&creation_context.egui_ctx, font_path.as_deref());
            let app = FylerApp::new(
                Arc::clone(&engine),
                event_rx,
                confirm_tx,
                confirm_detail,
                icon_style,
                creation_context.egui_ctx.clone(),
            )
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!("GUIを起動できません: {error}"))
}

/// 指定パスを優先し、存在しなければ候補列の先頭から利用可能なパスを返す。
fn resolve_font_path(configured: Option<&Path>, candidates: &[PathBuf]) -> Option<PathBuf> {
    configured
        .filter(|path| path.exists())
        .map(Path::to_path_buf)
        .or_else(|| candidates.iter().find(|path| path.exists()).cloned())
}

fn install_fallback_font(context: &egui::Context, configured: Option<&Path>) {
    let candidates = default_font_candidates();
    let Some(path) = resolve_font_path(configured, &candidates) else {
        return;
    };
    let Ok(bytes) = fs::read(path) else {
        return;
    };

    let mut definitions = egui::FontDefinitions::default();
    definitions.font_data.insert(
        CJK_FONT_NAME.to_owned(),
        Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        definitions
            .families
            .entry(family)
            .or_default()
            .push(CJK_FONT_NAME.to_owned());
    }
    context.set_fonts(definitions);
}

fn default_font_candidates() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let Some(windows_directory) = std::env::var_os("WINDIR").map(PathBuf::from) else {
            return Vec::new();
        };
        let fonts = windows_directory.join("Fonts");
        vec![
            fonts.join("YuGothM.ttc"),
            fonts.join("meiryo.ttc"),
            fonts.join("msgothic.ttc"),
        ]
    }

    #[cfg(not(windows))]
    {
        vec![PathBuf::from(
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        )]
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fyler-gui-font-test-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn resolve_font_path_prefers_existing_configured_path() {
        let directory = TestDirectory::new();
        let configured = directory.path().join("configured.ttf");
        let candidate = directory.path().join("candidate.ttf");
        fs::write(&configured, b"configured").unwrap();
        fs::write(&candidate, b"candidate").unwrap();

        assert_eq!(
            resolve_font_path(Some(&configured), &[candidate]),
            Some(configured)
        );
    }

    #[test]
    fn resolve_font_path_falls_back_to_first_existing_candidate() {
        let directory = TestDirectory::new();
        let missing_configured = directory.path().join("missing-configured.ttf");
        let missing_candidate = directory.path().join("missing-candidate.ttf");
        let existing_candidate = directory.path().join("existing-candidate.ttf");
        fs::write(&existing_candidate, b"candidate").unwrap();

        assert_eq!(
            resolve_font_path(
                Some(&missing_configured),
                &[missing_candidate, existing_candidate.clone()]
            ),
            Some(existing_candidate)
        );
    }

    #[test]
    fn resolve_font_path_returns_none_when_every_path_is_missing() {
        let directory = TestDirectory::new();

        assert_eq!(
            resolve_font_path(
                Some(&directory.path().join("missing-configured.ttf")),
                &[
                    directory.path().join("missing-a.ttf"),
                    directory.path().join("missing-b.ttf"),
                ]
            ),
            None
        );
    }
}
