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

use fyler_core::plan::OperationPlan;
use fyler_core::tree::{BaselineTree, DesiredTree, EditContext};

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
    todo!("M2(rename) → M3(create/delete) → M4(move/copy)の順で実装(tests/spec_m2.rs参照)")
}
