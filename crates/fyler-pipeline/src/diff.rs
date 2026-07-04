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
use fyler_core::validate::ValidateError;

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
///   Move/Copyの読み取り元を壊さない、既存pathを空けてからCreate/Move/Copyする、
///   親をMove/Deleteする前に対象子孫のDeleteを済ませる。
/// - Move同士の循環はvalidateで拒否済みだが、Create/Deleteを挟んだ再作成循環は
///   ここで初めて確定するため、逐次実行不能なplanは `Err(MoveCycle)` で返す
///   (保存フローはvalidateエラーと同様に保存を中断する)。
pub fn build_plan(
    baseline: &BaselineTree,
    desired: &DesiredTree,
    ctx: &EditContext,
) -> Result<OperationPlan, Vec<ValidateError>> {
    let desired_ids = desired
        .entries
        .iter()
        .filter_map(|entry| entry.id)
        .collect::<HashSet<_>>();
    let moved_dirs = planned_directory_moves(baseline, desired);

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
                if copied_with_ancestor(&original.path, &occurrence.path, baseline, desired) {
                    continue;
                }
                operations.push(FsOperation::Copy {
                    src: id,
                    from: original.path.clone(),
                    to: occurrence.path.clone(),
                });
            }
        }

        let origin = occurrences[origin_index];
        if moved_with_ancestor(&original.path, &origin.path, &moved_dirs) {
            continue;
        }

        if origin.path != original.path {
            operations.push(FsOperation::Move {
                id,
                from: original.path.clone(),
                to: origin.path.clone(),
            });
        }
    }

    operations.extend(
        baseline
            .entries
            .iter()
            .filter(|entry| {
                !desired_ids.contains(&entry.id)
                    && !is_hidden_by_collapsed_dir(&entry.path, baseline, ctx)
            })
            .map(|entry| FsOperation::Delete {
                id: entry.id,
                path: entry.path.clone(),
            }),
    );

    rewrite_targets_to_pre_move_paths(&mut operations, &moved_dirs);

    match order_operations(operations) {
        Ok(ops) => Ok(OperationPlan { ops }),
        Err(cycle_targets) => Err(cycle_targets
            .into_iter()
            .map(|path| ValidateError::MoveCycle { path })
            .collect()),
    }
}

fn is_hidden_by_collapsed_dir(path: &TreePath, baseline: &BaselineTree, ctx: &EditContext) -> bool {
    ctx.collapsed_dirs.iter().any(|id| {
        baseline.get(*id).is_some_and(|entry| {
            entry.kind == EntryKind::Dir && entry.path.is_strict_ancestor_of(path)
        })
    })
}

fn planned_directory_moves(
    baseline: &BaselineTree,
    desired: &DesiredTree,
) -> Vec<(TreePath, TreePath)> {
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
        if original.kind != EntryKind::Dir {
            continue;
        }

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
            moves.push((original.path.clone(), origin.path.clone()));
        }
    }

    moves
}

fn moved_with_ancestor(
    original_path: &TreePath,
    desired_path: &TreePath,
    moved_dirs: &[(TreePath, TreePath)],
) -> bool {
    moved_dirs.iter().any(|(from, to)| {
        if !from.is_strict_ancestor_of(original_path) {
            return false;
        }
        let components = to
            .components()
            .iter()
            .chain(original_path.components()[from.depth()..].iter())
            .cloned();
        TreePath::from_components(components) == *desired_path
    })
}

/// 展開済みディレクトリのCopy(yy→pでブロックごと複製)では、親ディレクトリの
/// Copyが子孫を再帰コピーするため、子孫の個別Copyをplanに入れない。
/// 条件: 祖先にも同様のCopy(from祖先→to祖先)があり、自分のfrom/toが
/// その祖先ペアの同じ相対サフィックスになっていること。
fn copied_with_ancestor(
    original_path: &TreePath,
    copy_target: &TreePath,
    baseline: &BaselineTree,
    desired: &DesiredTree,
) -> bool {
    desired.entries.iter().any(|ancestor| {
        let Some(ancestor_id) = ancestor.id else {
            return false;
        };
        let Some(ancestor_original) = baseline.get(ancestor_id) else {
            return false;
        };
        if ancestor_original.kind != EntryKind::Dir
            || !ancestor.path.is_strict_ancestor_of(copy_target)
            || !ancestor_original.path.is_strict_ancestor_of(original_path)
            || ancestor.path == ancestor_original.path
        {
            return false;
        }
        let expected_target = TreePath::from_components(
            ancestor
                .path
                .components()
                .iter()
                .chain(original_path.components()[ancestor_original.path.depth()..].iter())
                .cloned(),
        );
        expected_target == *copy_target
    })
}

/// 移動先ディレクトリ(まだ存在しないパス)配下を対象とするCreate/Move/Copyを、
/// **移動前の座標**へ書き換える。親ディレクトリのMoveが後から実行されると、
/// 書き換えた操作の成果物ごと最終位置へ移動する。
///
/// 例: `dir → newdir` + `dir/a.txt → newdir/b.txt` は
/// `Move(dir/a.txt → dir/b.txt)` → `Move(dir → newdir)` の2段で実行可能になる。
/// 実行順序は [`must_precede`](Move source配下を対象とする操作 → そのMove) が保証する。
fn rewrite_targets_to_pre_move_paths(
    operations: &mut [FsOperation],
    moved_dirs: &[(TreePath, TreePath)],
) {
    if moved_dirs.is_empty() {
        return;
    }

    for operation in operations.iter_mut() {
        let target = match operation {
            FsOperation::Create { path, .. } => path,
            FsOperation::Move { to, .. } | FsOperation::Copy { to, .. } => to,
            FsOperation::Delete { .. } => continue,
        };

        // 連鎖move(`a→b/x` かつ `b→c` 等)に備えfixpointまで巻き戻す。
        // 1回のループで高々1つのmoved_dirにマッチするため、moves数+1回で必ず収束する。
        for _ in 0..=moved_dirs.len() {
            let Some((from, to)) = moved_dirs
                .iter()
                .find(|(_, to)| to.is_strict_ancestor_of(target))
            else {
                break;
            };
            *target = TreePath::from_components(
                from.components()
                    .iter()
                    .chain(target.components()[to.depth()..].iter())
                    .cloned(),
            );
        }
    }
}

fn order_operations(operations: Vec<FsOperation>) -> Result<Vec<FsOperation>, Vec<TreePath>> {
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
        let Some(next) =
            (0..operations.len()).find(|index| !emitted[*index] && predecessor_counts[*index] == 0)
        else {
            // validateのMoveCycle検出はMove同士の循環のみを見る。Create/Deleteを
            // 挟んだ「再作成循環」はここで初めて確定するため、panicせずエラーで返す。
            return Err((0..operations.len())
                .filter(|index| !emitted[*index])
                .filter_map(|index| operation_target(&operations[index]).cloned())
                .collect());
        };

        emitted[next] = true;
        ordered.push(operations[next].clone());
        for successor in &successors[next] {
            predecessor_counts[*successor] = predecessor_counts[*successor].saturating_sub(1);
        }
    }

    Ok(ordered)
}

fn must_precede(before: &FsOperation, after: &FsOperation) -> bool {
    if let FsOperation::Delete { path, .. } = before {
        if let FsOperation::Delete {
            path: later_path, ..
        } = after
        {
            if path.is_strict_ancestor_of(later_path) {
                return false;
            }
            if later_path.is_strict_ancestor_of(path) {
                return true;
            }
        }

        if operation_target(after)
            .is_some_and(|target| target == path || path.is_strict_ancestor_of(target))
        {
            return true;
        }

        if let FsOperation::Move { from, .. } = after
            && from.is_strict_ancestor_of(path)
        {
            return true;
        }
    }

    if let Some(source) = operation_source(before)
        && let FsOperation::Delete { path, .. } = after
        && (path == source || path.is_strict_ancestor_of(source))
    {
        return true;
    }

    if let FsOperation::Create {
        path,
        kind: EntryKind::Dir,
    } = before
        && operation_target(after).is_some_and(|target| path.is_strict_ancestor_of(target))
    {
        return true;
    }

    // 移動前座標を対象とする操作(rewrite_targets_to_pre_move_paths参照)は、
    // その親ディレクトリのMoveが子孫ごと動かす前に完了していなければならない。
    if let FsOperation::Move { from, .. } = after
        && operation_target(before).is_some_and(|target| from.is_strict_ancestor_of(target))
    {
        return true;
    }

    let Some(source) = operation_source(before) else {
        return false;
    };

    if let FsOperation::Move { from, .. } = after
        && (from == source || from.is_strict_ancestor_of(source))
    {
        return true;
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
