//! validate: DesiredTreeの妥当性検査(DESIGN.md「validateで弾くもの」)。

use std::collections::{HashMap, HashSet};

use fyler_core::id::EntryId;
use fyler_core::path::TreePath;
use fyler_core::tree::{BaselineTree, DesiredTree, EditContext, EntryKind};
use fyler_core::validate::ValidateError;
use fyler_core::win_naming;

/// DesiredTreeを検査し、見つかった問題を**すべて**返す(最初の1件で止めない)。
/// 1件でもあれば呼び出し側(保存状態機械)は保存を中断する。
///
/// 実装契約 — 検出すべきもの:
/// - 同一ディレクトリ内の名前重複(`DuplicateName`)。
///   collapsedなディレクトリの「見えない子孫」との衝突は、move/copyの着地先が
///   baseline上の既存エントリと重なる場合に検出する
/// - ディレクトリの自分自身・自分の子孫への移動(`MoveIntoSelf`)。
///   baselineパスとdesiredパスの関係から判定する(`TreePath::is_strict_ancestor_of`)
/// - 一時名なしでは安全に逐次実行できないMove循環(`MoveCycle`)
/// - 名前規則(必ず `fyler_core::win_naming` を使う):
///   - 予約文字・制御文字(`ReservedChar`)
///   - 空の名前(`EmptyName`)
///   - 予約名 CON, PRN, AUX, NUL, COM1-9, LPT1-9(拡張子付き含む)(`ReservedName`)
///   - 末尾のスペース・ピリオド(`InvalidTrailing`)
///
/// BrokenIdPrefix / InvalidIndent はparse段階(`parse::to_desired_tree`)で検出済み。
pub fn validate(
    baseline: &BaselineTree,
    desired: &DesiredTree,
    ctx: &EditContext,
) -> Vec<ValidateError> {
    let mut errors = Vec::new();
    let mut duplicate_paths = HashSet::new();

    // 完全一致の重複は常にエラー。fold一致(case違い)の重複は、Windowsの既定が
    // case-insensitiveのため applyのrenameが黙って上書きする危険がある。ただし
    // 「baselineから動いていないエントリ同士」のfold一致は、case-sensitive
    // ディレクトリ(WSL等)で実在し得るため許す(触っていないツリーの保存を妨げない)。
    let mut seen_exact = HashSet::new();
    let mut fold_groups: HashMap<Vec<String>, Vec<&fyler_core::tree::DesiredEntry>> =
        HashMap::new();
    for entry in &desired.entries {
        if !seen_exact.insert(entry.path.clone()) && duplicate_paths.insert(entry.path.clone()) {
            errors.push(ValidateError::DuplicateName {
                path: entry.path.clone(),
            });
        }
        fold_groups
            .entry(case_fold_path(&entry.path))
            .or_default()
            .push(entry);
    }
    for group in fold_groups.values() {
        if group.len() < 2 {
            continue;
        }
        let any_changed = group.iter().any(|entry| {
            entry
                .id
                .and_then(|id| baseline.get(id))
                .is_none_or(|original| original.path != entry.path)
        });
        if !any_changed {
            continue;
        }
        for entry in group {
            if duplicate_paths.insert(entry.path.clone()) {
                errors.push(ValidateError::DuplicateName {
                    path: entry.path.clone(),
                });
            }
        }
    }

    for entry in &desired.entries {
        let Some(name) = entry.path.name() else {
            continue;
        };
        if name.is_empty() {
            errors.push(ValidateError::EmptyName { line: entry.line });
        }

        if let Some(ch) = win_naming::find_reserved_char(name) {
            errors.push(ValidateError::ReservedChar {
                line: entry.line,
                name: name.to_owned(),
                ch,
            });
        }
        if win_naming::is_reserved_name(name) {
            errors.push(ValidateError::ReservedName {
                line: entry.line,
                name: name.to_owned(),
            });
        }
        if win_naming::has_invalid_trailing(name) {
            errors.push(ValidateError::InvalidTrailing {
                line: entry.line,
                name: name.to_owned(),
            });
        }

        let Some(id) = entry.id else {
            continue;
        };
        let Some(original) = baseline.get(id) else {
            continue;
        };
        if original.kind == EntryKind::Dir && original.path.is_strict_ancestor_of(&entry.path) {
            errors.push(ValidateError::MoveIntoSelf {
                id,
                from: original.path.clone(),
                to: entry.path.clone(),
            });
        }
    }

    let hidden_entries = hidden_entries_at_desired_paths(baseline, desired, ctx);
    for entry in &desired.entries {
        let Some(id) = entry.id else {
            continue;
        };
        let Some(original) = baseline.get(id) else {
            continue;
        };
        if original.path == entry.path {
            continue;
        }

        if hidden_entries.iter().any(|(hidden_id, hidden_path)| {
            *hidden_id != id && case_fold_path(hidden_path) == case_fold_path(&entry.path)
        }) && duplicate_paths.insert(entry.path.clone())
        {
            errors.push(ValidateError::DuplicateName {
                path: entry.path.clone(),
            });
        }
    }

    errors.extend(detect_move_cycles(baseline, desired));

    errors
}

/// Windowsのcase-insensitive比較の近似(Unicode simple case fold相当の小文字化)。
/// FILE_CASE_SENSITIVE_DIRの実測はfsops層の責務のため、純粋層では保守側に倒す。
fn case_fold_path(path: &TreePath) -> Vec<String> {
    path.components()
        .iter()
        .map(|component| component.to_lowercase())
        .collect()
}

fn hidden_entries_at_desired_paths(
    baseline: &BaselineTree,
    desired: &DesiredTree,
    ctx: &EditContext,
) -> Vec<(fyler_core::EntryId, TreePath)> {
    let mut hidden = Vec::new();

    let collapsed_ids = ctx
        .collapsed_dirs
        .iter()
        .copied()
        .chain(
            baseline
                .incomplete_dirs()
                .keys()
                .filter_map(|path| baseline.get_by_path(path).map(|entry| entry.id)),
        )
        .collect::<HashSet<_>>();

    for collapsed_id in collapsed_ids {
        let Some(collapsed) = baseline.get(collapsed_id) else {
            continue;
        };
        if collapsed.kind != EntryKind::Dir {
            continue;
        }

        for desired_root in desired
            .entries
            .iter()
            .filter(|entry| entry.id == Some(collapsed_id))
        {
            for descendant in baseline
                .entries()
                .iter()
                .filter(|entry| collapsed.path.is_strict_ancestor_of(&entry.path))
            {
                let components = desired_root
                    .path
                    .components()
                    .iter()
                    .chain(descendant.path.components()[collapsed.path.depth()..].iter())
                    .cloned();
                hidden.push((descendant.id, TreePath::from_components(components)));
            }
        }
    }

    hidden
}

fn detect_move_cycles(baseline: &BaselineTree, desired: &DesiredTree) -> Vec<ValidateError> {
    let moves = planned_moves(baseline, desired);
    if moves.is_empty() {
        return Vec::new();
    }

    let moves_by_source = moves
        .iter()
        .map(|planned| (planned.from.clone(), planned))
        .collect::<HashMap<_, _>>();
    let mut remaining_dependencies = moves
        .iter()
        .map(|planned| {
            (
                planned.id,
                moves_by_source
                    .get(&planned.to)
                    .map(|dependency| dependency.id),
            )
        })
        .collect::<HashMap<_, _>>();

    let mut changed = true;
    while changed {
        changed = false;
        let ready = remaining_dependencies
            .iter()
            .filter_map(|(id, dependency)| {
                dependency
                    .filter(|dependency_id| remaining_dependencies.contains_key(dependency_id))
                    .is_none()
                    .then_some(*id)
            })
            .collect::<Vec<_>>();

        for id in ready {
            changed |= remaining_dependencies.remove(&id).is_some();
        }
    }

    moves
        .iter()
        .filter(|planned| remaining_dependencies.contains_key(&planned.id))
        .map(|planned| ValidateError::MoveCycle {
            path: planned.to.clone(),
        })
        .collect()
}

#[derive(Debug)]
struct PlannedMove {
    id: EntryId,
    from: TreePath,
    to: TreePath,
}

fn planned_moves(baseline: &BaselineTree, desired: &DesiredTree) -> Vec<PlannedMove> {
    let mut planned_ids = HashSet::new();
    let mut moves = Vec::new();

    for entry in &desired.entries {
        let Some(id) = entry.id else {
            continue;
        };
        if !planned_ids.insert(id) {
            continue;
        }

        let Some(original) = baseline.get(id) else {
            continue;
        };
        let occurrences = desired
            .entries
            .iter()
            .filter(|candidate| candidate.id == Some(id))
            .collect::<Vec<_>>();
        let origin_index = occurrences
            .iter()
            .position(|candidate| candidate.path == original.path)
            .unwrap_or(0);
        let origin = occurrences[origin_index];

        if origin.path != original.path {
            moves.push(PlannedMove {
                id,
                from: original.path.clone(),
                to: origin.path.clone(),
            });
        }
    }

    moves
}
