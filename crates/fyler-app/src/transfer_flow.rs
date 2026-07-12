//! pane間transferのapp層フロー。保存状態機械とは独立に確認・実行を直列化する。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fyler_core::editor::EditorLine;
use fyler_core::grammar::PrefixParse;
use fyler_core::pane::PaneId;
use fyler_core::path::TreePath;
use fyler_core::report::CommitReport;
use fyler_core::transfer::{TransferKind, TransferOp, TransferPlan};
use fyler_core::tree::EntryKind;
use fyler_gui::confirm::ConfirmChoice;

use crate::save_flow::SaveController;

#[derive(Debug, Clone, Copy)]
pub(super) struct TransferPaneState {
    pub dirty: bool,
    pub idle: bool,
    pub crashed: bool,
    pub offline: bool,
}

pub(super) fn resolve_target(
    source: PaneId,
    last_active: PaneId,
    pane_ids: impl IntoIterator<Item = PaneId>,
) -> Option<PaneId> {
    let pane_ids = pane_ids.into_iter().collect::<Vec<_>>();
    (pane_ids.len() >= 2 && last_active != source && pane_ids.contains(&last_active))
        .then_some(last_active)
}

pub(super) fn start_rejection(
    source: TransferPaneState,
    target: TransferPaneState,
    globally_busy: bool,
) -> Option<&'static str> {
    if globally_busy {
        Some("Another save or transfer is in progress")
    } else if source.offline || target.offline {
        Some("Cannot start a transfer with an offline or unreachable pane")
    } else if source.crashed || target.crashed {
        Some("Cannot start a transfer with a stopped pane")
    } else if source.dirty || target.dirty {
        Some("Cannot start a transfer with a pane being edited. Save or discard changes first.")
    } else if !source.idle || !target.idle {
        Some("Cannot start a transfer with a pane that is saving")
    } else {
        None
    }
}

pub(super) fn destination_directory(
    target_is_empty: bool,
    resolved: Option<(TreePath, EntryKind)>,
) -> Option<TreePath> {
    if target_is_empty {
        return Some(TreePath::root());
    }
    let (path, kind) = resolved?;
    match kind {
        EntryKind::Dir => Some(path),
        EntryKind::File | EntryKind::Symlink => path.parent().or_else(|| Some(TreePath::root())),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectionError {
    MissingLine,
    UnsavedLine,
    UnknownId,
    Empty,
}

pub(super) fn resolve_selection(
    controller: &SaveController,
    lines: &[EditorLine],
    selected_lines: &[usize],
) -> Result<Vec<(TreePath, EntryKind)>, SelectionError> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for &line in selected_lines {
        if !seen.insert(line) {
            continue;
        }
        let editor_line = lines.get(line).ok_or(SelectionError::MissingLine)?;
        if !matches!(
            fyler_core::grammar::split_id_prefix(&editor_line.text),
            PrefixParse::WithId { .. }
        ) {
            return Err(SelectionError::UnsavedLine);
        }
        selected.push(
            controller
                .resolve_line(lines, line)
                .ok_or(SelectionError::UnknownId)?,
        );
    }
    if selected.is_empty() {
        Err(SelectionError::Empty)
    } else {
        Ok(selected)
    }
}

pub(super) fn build_plan(
    kind: TransferKind,
    from_root: PathBuf,
    to_root: PathBuf,
    destination: &TreePath,
    selected: Vec<(TreePath, EntryKind)>,
) -> TransferPlan {
    let selected = selected
        .iter()
        .enumerate()
        .filter(|(index, (path, _))| {
            !selected
                .iter()
                .enumerate()
                .any(|(other_index, (other, _))| {
                    *index != other_index && other.is_strict_ancestor_of(path)
                })
        })
        .map(|(_, entry)| entry.clone());
    let ops = selected
        .filter_map(|(from, entry_kind)| {
            let name = from.name()?.to_owned();
            Some(TransferOp {
                kind,
                from,
                to: destination.child(name),
                entry_kind,
            })
        })
        .collect();
    TransferPlan {
        from_root,
        to_root,
        ops,
    }
}

#[derive(Debug)]
enum TransferState {
    Idle,
    Awaiting {
        source: PaneId,
        target: PaneId,
        plan: TransferPlan,
        overwrites: Vec<PathBuf>,
    },
    Running {
        source: PaneId,
        target: PaneId,
        cancel: Arc<AtomicBool>,
    },
}

#[derive(Debug)]
pub(super) enum TransferFlowResult {
    StartApply {
        source: PaneId,
        target: PaneId,
        plan: TransferPlan,
        overwrites: HashSet<PathBuf>,
        cancel: Arc<AtomicBool>,
    },
    Cancelled,
    CancelRequested,
    Finished {
        source: PaneId,
        target: PaneId,
        report: CommitReport<TransferOp>,
    },
    Ignored,
}

#[derive(Debug)]
pub(super) struct TransferController {
    state: TransferState,
}

impl TransferController {
    pub fn new() -> Self {
        Self {
            state: TransferState::Idle,
        }
    }

    pub fn begin(
        &mut self,
        source: PaneId,
        target: PaneId,
        plan: TransferPlan,
        overwrites: Vec<PathBuf>,
    ) {
        debug_assert!(matches!(self.state, TransferState::Idle));
        self.state = TransferState::Awaiting {
            source,
            target,
            plan,
            overwrites,
        };
    }

    pub fn on_choice(&mut self, choice: ConfirmChoice) -> TransferFlowResult {
        if let TransferState::Running { cancel, .. } = &self.state {
            if choice == ConfirmChoice::Cancel {
                cancel.store(true, Ordering::Relaxed);
                return TransferFlowResult::CancelRequested;
            }
            return TransferFlowResult::Ignored;
        }
        let state = std::mem::replace(&mut self.state, TransferState::Idle);
        let TransferState::Awaiting {
            source,
            target,
            plan,
            overwrites,
        } = state
        else {
            return TransferFlowResult::Ignored;
        };
        if choice == ConfirmChoice::Cancel {
            return TransferFlowResult::Cancelled;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.state = TransferState::Running {
            source,
            target,
            cancel: Arc::clone(&cancel),
        };
        TransferFlowResult::StartApply {
            source,
            target,
            plan,
            overwrites: overwrites.into_iter().collect(),
            cancel,
        }
    }

    pub fn on_finished(&mut self, report: CommitReport<TransferOp>) -> TransferFlowResult {
        let state = std::mem::replace(&mut self.state, TransferState::Idle);
        let TransferState::Running { source, target, .. } = state else {
            return TransferFlowResult::Ignored;
        };
        TransferFlowResult::Finished {
            source,
            target,
            report,
        }
    }

    pub fn invalidate_if_involves(&mut self, pane: PaneId) -> bool {
        let involves = matches!(
            self.state,
            TransferState::Awaiting { source, target, .. }
                if source == pane || target == pane
        );
        if involves {
            self.state = TransferState::Idle;
        }
        involves
    }

    pub fn is_awaiting(&self) -> bool {
        matches!(self.state, TransferState::Awaiting { .. })
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, TransferState::Running { .. })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Mutex;

    use fyler_core::editor::{EditorCommand, EditorEngine, EditorSnapshot};
    use fyler_core::id::IdAllocator;
    use fyler_core::tree::{BaselineEntry, BaselineTree};
    use fyler_fsops::scan::ScanOptions;
    use tempfile::tempdir;

    use crate::save_flow::SaveController;

    use super::*;

    #[derive(Default)]
    struct RecordingEngine {
        commands: Mutex<Vec<EditorCommand>>,
    }

    impl EditorEngine for RecordingEngine {
        fn send(&self, command: EditorCommand) -> anyhow::Result<()> {
            self.commands.lock().unwrap().push(command);
            Ok(())
        }

        fn snapshot(&self) -> Arc<EditorSnapshot> {
            Arc::new(EditorSnapshot::empty())
        }
    }

    fn pane(dirty: bool, idle: bool, crashed: bool) -> TransferPaneState {
        TransferPaneState {
            dirty,
            idle,
            crashed,
            offline: false,
        }
    }

    #[test]
    fn start_rejects_offline_source_or_target() {
        let mut offline = pane(false, true, false);
        offline.offline = true;
        assert_eq!(
            start_rejection(offline, pane(false, true, false), false),
            Some("Cannot start a transfer with an offline or unreachable pane")
        );
        assert_eq!(
            start_rejection(pane(false, true, false), offline, false),
            Some("Cannot start a transfer with an offline or unreachable pane")
        );
    }

    #[test]
    fn target_is_last_active_and_requires_two_distinct_panes() {
        let one = PaneId::new(1);
        let two = PaneId::new(2);
        assert_eq!(resolve_target(one, two, [one, two]), Some(two));
        assert_eq!(resolve_target(one, one, [one, two]), None);
        assert_eq!(resolve_target(one, one, [one]), None);
    }

    #[test]
    fn destination_uses_dir_file_parent_and_empty_root() {
        assert_eq!(destination_directory(true, None), Some(TreePath::root()));
        assert_eq!(
            destination_directory(false, Some((TreePath::parse("dest/sub"), EntryKind::Dir))),
            Some(TreePath::parse("dest/sub"))
        );
        assert_eq!(
            destination_directory(
                false,
                Some((TreePath::parse("dest/file.txt"), EntryKind::File))
            ),
            Some(TreePath::parse("dest"))
        );
    }

    #[test]
    fn gate_rejects_every_busy_or_unsafe_condition() {
        let ready = pane(false, true, false);
        assert!(start_rejection(ready, ready, false).is_none());
        assert!(start_rejection(ready, ready, true).is_some());
        assert!(start_rejection(pane(true, true, false), ready, false).is_some());
        assert!(start_rejection(ready, pane(true, true, false), false).is_some());
        assert!(start_rejection(pane(false, false, false), ready, false).is_some());
        assert!(start_rejection(ready, pane(false, false, false), false).is_some());
        assert!(start_rejection(pane(false, true, true), ready, false).is_some());
        assert!(start_rejection(ready, pane(false, true, true), false).is_some());
    }

    #[test]
    fn selection_rejects_unsaved_and_unknown_id_lines() {
        let root = PathBuf::from("root");
        let mut ids = IdAllocator::new();
        let id = ids.allocate();
        let mut baseline = BaselineTree::new(&root);
        baseline.insert(BaselineEntry {
            id,
            path: TreePath::parse("saved.txt"),
            kind: EntryKind::File,
        });
        let engine = Arc::new(RecordingEngine::default());
        let controller = SaveController::new(root, ids, baseline, engine);
        let lines = vec![
            EditorLine::new(format!(
                "{}saved.txt",
                fyler_core::grammar::format_id_prefix(id)
            )),
            EditorLine::new("new.txt"),
            EditorLine::new("/999 missing.txt"),
        ];
        assert_eq!(
            resolve_selection(&controller, &lines, &[0, 1]),
            Err(SelectionError::UnsavedLine)
        );
        assert_eq!(
            resolve_selection(&controller, &lines, &[2]),
            Err(SelectionError::UnknownId)
        );
    }

    #[test]
    fn plan_drops_selected_descendants_and_preserves_names() {
        let plan = build_plan(
            TransferKind::Move,
            PathBuf::from("source"),
            PathBuf::from("target"),
            &TreePath::parse("inbox"),
            vec![
                (TreePath::parse("dir/child.txt"), EntryKind::File),
                (TreePath::parse("other.txt"), EntryKind::File),
                (TreePath::parse("dir"), EntryKind::Dir),
            ],
        );
        assert_eq!(plan.ops.len(), 2);
        assert_eq!(plan.ops[0].from, TreePath::parse("other.txt"));
        assert_eq!(plan.ops[0].to, TreePath::parse("inbox/other.txt"));
        assert_eq!(plan.ops[1].from, TreePath::parse("dir"));
        assert_eq!(plan.ops[1].to, TreePath::parse("inbox/dir"));
    }

    #[test]
    fn confirmation_approval_cancel_and_finish_are_app_local() {
        let source = PaneId::new(1);
        let target = PaneId::new(2);
        let plan = build_plan(
            TransferKind::Copy,
            PathBuf::from("source"),
            PathBuf::from("target"),
            &TreePath::root(),
            vec![(TreePath::parse("a.txt"), EntryKind::File)],
        );
        let mut flow = TransferController::new();
        flow.begin(source, target, plan.clone(), Vec::new());
        assert!(flow.is_awaiting());
        let TransferFlowResult::StartApply {
            plan: started,
            cancel,
            ..
        } = flow.on_choice(ConfirmChoice::Approve)
        else {
            panic!("approval did not start transfer");
        };
        assert_eq!(started, plan);
        assert!(flow.is_running());
        assert!(!cancel.load(Ordering::Relaxed));
        assert!(matches!(
            flow.on_choice(ConfirmChoice::Cancel),
            TransferFlowResult::CancelRequested
        ));
        assert!(cancel.load(Ordering::Relaxed));
        assert!(matches!(
            flow.on_finished(CommitReport::default()),
            TransferFlowResult::Finished { source: s, target: t, .. }
                if s == source && t == target
        ));
        assert!(!flow.is_running());
    }

    #[test]
    fn external_change_invalidates_only_involved_confirmation() {
        let source = PaneId::new(1);
        let target = PaneId::new(2);
        let other = PaneId::new(3);
        let plan = build_plan(
            TransferKind::Copy,
            PathBuf::from("source"),
            PathBuf::from("target"),
            &TreePath::root(),
            vec![(TreePath::parse("a.txt"), EntryKind::File)],
        );
        let mut flow = TransferController::new();
        flow.begin(source, target, plan, Vec::new());
        assert!(!flow.invalidate_if_involves(other));
        assert!(flow.is_awaiting());
        assert!(flow.invalidate_if_involves(target));
        assert!(!flow.is_awaiting());
        assert!(matches!(
            flow.on_choice(ConfirmChoice::Approve),
            TransferFlowResult::Ignored
        ));
    }

    #[test]
    fn approved_move_reports_and_reconciles_both_panes() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("source");
        let target_root = temp.path().join("target");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&target_root).unwrap();
        fs::write(source_root.join("a.txt"), b"content").unwrap();

        let ids = Arc::new(Mutex::new(IdAllocator::new()));
        let source_baseline = fyler_fsops::scan::scan_baseline_with(
            &source_root,
            &mut ids.lock().unwrap(),
            &ScanOptions::default(),
        )
        .unwrap();
        let target_baseline = fyler_fsops::scan::scan_baseline_with(
            &target_root,
            &mut ids.lock().unwrap(),
            &ScanOptions::default(),
        )
        .unwrap();
        let source_engine = Arc::new(RecordingEngine::default());
        let target_engine = Arc::new(RecordingEngine::default());
        let mut source_controller = SaveController::new_shared(
            source_root.clone(),
            Arc::clone(&ids),
            source_baseline,
            source_engine.clone(),
        );
        let mut target_controller = SaveController::new_shared(
            target_root.clone(),
            Arc::clone(&ids),
            target_baseline,
            target_engine.clone(),
        );
        let source_lines = source_controller.visible_lines();
        let selected = vec![
            source_controller
                .resolve_line(&source_lines, 0)
                .expect("source line should resolve"),
        ];
        let plan = build_plan(
            TransferKind::Move,
            source_root,
            target_root,
            &TreePath::root(),
            selected,
        );
        let preflight = fyler_fsops::preflight_transfer(&plan);
        assert!(preflight.blocked.is_empty());

        let source_id = PaneId::new(1);
        let target_id = PaneId::new(2);
        let mut flow = TransferController::new();
        flow.begin(source_id, target_id, plan, preflight.overwritable);
        let TransferFlowResult::StartApply {
            plan,
            overwrites,
            cancel,
            ..
        } = flow.on_choice(ConfirmChoice::Approve)
        else {
            panic!("approval should start the worker payload");
        };
        source_engine
            .send(EditorCommand::SetModifiable(false))
            .unwrap();
        target_engine
            .send(EditorCommand::SetModifiable(false))
            .unwrap();
        let report = fyler_fsops::apply::apply_transfer_plan_cancellable(
            &plan,
            &overwrites,
            &cancel,
            &mut |_| {},
        );
        assert!(report.all_succeeded());
        let TransferFlowResult::Finished { report, .. } = flow.on_finished(report) else {
            panic!("worker report should finish the transfer");
        };
        assert!(report.all_succeeded());

        source_controller.reconcile_after_transfer().unwrap();
        target_controller.reconcile_after_transfer().unwrap();
        source_engine
            .send(EditorCommand::SetModifiable(true))
            .unwrap();
        target_engine
            .send(EditorCommand::SetModifiable(true))
            .unwrap();
        assert!(source_controller.visible_lines().is_empty());
        assert_eq!(target_controller.visible_lines().len(), 1);
        assert!(target_controller.visible_lines()[0].text.ends_with("a.txt"));
        for engine in [source_engine, target_engine] {
            let commands = engine.commands.lock().unwrap();
            assert!(matches!(
                commands.first(),
                Some(EditorCommand::SetModifiable(false))
            ));
            assert!(
                commands
                    .iter()
                    .any(|command| matches!(command, EditorCommand::SetLines { .. }))
            );
            assert!(matches!(
                commands.last(),
                Some(EditorCommand::SetModifiable(true))
            ));
        }
    }
}
