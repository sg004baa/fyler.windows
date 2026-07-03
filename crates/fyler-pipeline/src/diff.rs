//! diff: baseline と DesiredTree の比較 → OperationPlan。
//!
//! DESIGN.md「diff判定ルール」:
//!
//! | バッファの状態 | 操作 |
//! |---|---|
//! | ID一致・名前/親ディレクトリが変化 | Move(rename含む) |
//! | baselineに存在したIDがバッファから消滅 | Delete |
//! | IDのない行 | Create |
//! | 同一IDが複数行に出現(yy→p) | 1つを元位置とみなし、残りはCopy |

use std::collections::HashSet;

use fyler_core::path::TreePath;
use fyler_core::plan::{FsOperation, OperationPlan};
use fyler_core::tree::{BaselineTree, DesiredTree, EditContext, EntryKind};

/// planを構築する。**validate通過後にのみ呼ぶ契約**(エラー状態の入力は未定義動作でよい)。
///
/// 実装契約:
///
/// - **Move**: 同一IDでbaselineとdesiredのパスが異なる。renameとmoveを区別しない
/// - **Delete**: baselineのIDがdesiredに現れず、かつ「collapsedなディレクトリの
///   子孫として隠れている」のでもない場合のみ。
///   `ctx.collapsed_dirs` に入っているディレクトリの子孫は、バッファに現れなくても
///   削除ではない(親ディレクトリと一緒に動く)
/// - **collapsedディレクトリのMove**: planには親ディレクトリ1件のMoveだけを入れる。
///   子孫のMoveを個別に入れない(実FSのディレクトリ移動で子孫は一緒に動く)
/// - **Copy**: 同一IDが複数行に出現した場合、baselineと同一パスの行があれば
///   それを元位置(操作なし)とし、なければ最初の出現を元位置(Move)とする。
///   残りの行はCopy(from=baselineパス, to=その行のパス)
/// - **Create**: IDのない行。中間ディレクトリが必要なら、それもIDのない行として
///   バッファに書かれているはずである(書かれていなければparse段階でInvalidIndent)
/// - 変更がなければ空のplanを返す
/// - **順序の契約**(`OperationPlan`のdoc参照): 親Createは子より先、
///   Move/Copyの読み取り元を壊さない、Deleteは最後、Move玉突きは依存順
pub fn build_plan(
    baseline: &BaselineTree,
    desired: &DesiredTree,
    ctx: &EditContext,
) -> OperationPlan {
    let desired_ids = desired
        .entries
        .iter()
        .filter_map(|entry| entry.id)
        .collect::<HashSet<_>>();

    let mut operations = desired
        .entries
        .iter()
        .filter(|entry| entry.id.is_none())
        .map(|entry| FsOperation::Create {
            path: entry.path.clone(),
            kind: entry.kind,
        })
        .collect::<Vec<_>>();

    let mut planned_ids = HashSet::new();
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

        for (index, occurrence) in occurrences.iter().enumerate() {
            if index != origin_index {
                operations.push(FsOperation::Copy {
                    src: id,
                    from: original.path.clone(),
                    to: occurrence.path.clone(),
                });
            }
        }

        let origin = occurrences[origin_index];
        if origin.path != original.path {
            operations.push(FsOperation::Move {
                id,
                from: original.path.clone(),
                to: origin.path.clone(),
            });
        }
    }

    let mut deletes = baseline
        .entries
        .iter()
        .filter(|entry| {
            !desired_ids.contains(&entry.id)
                && !is_hidden_by_collapsed_dir(&entry.path, baseline, ctx)
        })
        .map(|entry| FsOperation::Delete {
            id: entry.id,
            path: entry.path.clone(),
        })
        .collect::<Vec<_>>();
    deletes.sort_by(|left, right| {
        operation_path(right)
            .depth()
            .cmp(&operation_path(left).depth())
    });

    let mut ops = order_non_delete_operations(operations);
    ops.extend(deletes);
    OperationPlan { ops }
}

fn is_hidden_by_collapsed_dir(path: &TreePath, baseline: &BaselineTree, ctx: &EditContext) -> bool {
    ctx.collapsed_dirs.iter().any(|id| {
        baseline.get(*id).is_some_and(|entry| {
            entry.kind == EntryKind::Dir && entry.path.is_strict_ancestor_of(path)
        })
    })
}

fn order_non_delete_operations(operations: Vec<FsOperation>) -> Vec<FsOperation> {
    let mut successors = vec![Vec::new(); operations.len()];
    let mut predecessor_counts = vec![0_usize; operations.len()];

    for before in 0..operations.len() {
        for after in 0..operations.len() {
            if before != after && must_precede(&operations[before], &operations[after]) {
                successors[before].push(after);
                predecessor_counts[after] += 1;
            }
        }
    }

    let mut ordered = Vec::with_capacity(operations.len());
    let mut emitted = vec![false; operations.len()];
    while ordered.len() < operations.len() {
        let next = (0..operations.len())
            .find(|index| !emitted[*index] && predecessor_counts[*index] == 0)
            .or_else(|| (0..operations.len()).find(|index| !emitted[*index]))
            .expect("an un-emitted operation must remain");

        emitted[next] = true;
        ordered.push(operations[next].clone());
        for successor in &successors[next] {
            predecessor_counts[*successor] = predecessor_counts[*successor].saturating_sub(1);
        }
    }

    ordered
}

fn must_precede(before: &FsOperation, after: &FsOperation) -> bool {
    if let FsOperation::Create {
        path,
        kind: EntryKind::Dir,
    } = before
    {
        if operation_target(after).is_some_and(|target| path.is_strict_ancestor_of(target)) {
            return true;
        }
    }

    let Some(source) = operation_source(before) else {
        return false;
    };

    if let FsOperation::Move { from, .. } = after {
        if from == source || from.is_strict_ancestor_of(source) {
            return true;
        }
    }

    operation_target(after)
        .is_some_and(|target| target == source || target.is_strict_ancestor_of(source))
}

fn operation_source(operation: &FsOperation) -> Option<&TreePath> {
    match operation {
        FsOperation::Move { from, .. } | FsOperation::Copy { from, .. } => Some(from),
        FsOperation::Create { .. } | FsOperation::Delete { .. } => None,
    }
}

fn operation_target(operation: &FsOperation) -> Option<&TreePath> {
    match operation {
        FsOperation::Create { path, .. } => Some(path),
        FsOperation::Move { to, .. } | FsOperation::Copy { to, .. } => Some(to),
        FsOperation::Delete { .. } => None,
    }
}

fn operation_path(operation: &FsOperation) -> &TreePath {
    match operation {
        FsOperation::Create { path, .. } | FsOperation::Delete { path, .. } => path,
        FsOperation::Move { to, .. } | FsOperation::Copy { to, .. } => to,
    }
}
