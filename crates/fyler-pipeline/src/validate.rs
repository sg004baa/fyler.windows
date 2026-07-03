//! validate: DesiredTreeの妥当性検査(DESIGN.md「validateで弾くもの」)。

use fyler_core::tree::{BaselineTree, DesiredTree, EditContext};
use fyler_core::validate::ValidateError;

/// DesiredTreeを検査し、見つかった問題を**すべて**返す(最初の1件で止めない)。
/// 1件でもあれば呼び出し側(保存状態機械)は保存を中断する。
///
/// 実装契約 — 検出すべきもの:
/// - 同一ディレクトリ内の名前重複(`DuplicateName`)。
///   collapsedなディレクトリの「見えない子孫」との衝突は、move/copyの着地先が
///   baseline上の既存エントリと重なる場合に検出する
/// - ディレクトリの自分自身・自分の子孫への移動(`MoveIntoSelf`)。
///   baselineパスとdesiredパスの関係から判定する(`TreePath::is_strict_ancestor_of`)
/// - 名前規則(必ず `fyler_core::win_naming` を使う):
///   - 予約文字・制御文字(`ReservedChar`)
///   - 予約名 CON, PRN, AUX, NUL, COM1-9, LPT1-9(拡張子付き含む)(`ReservedName`)
///   - 末尾のスペース・ピリオド(`InvalidTrailing`)
///
/// BrokenIdPrefix / InvalidIndent はparse段階(`parse::to_desired_tree`)で検出済み。
pub fn validate(
    baseline: &BaselineTree,
    desired: &DesiredTree,
    ctx: &EditContext,
) -> Vec<ValidateError> {
    todo!("M2: validateルールの実装(tests/spec_m2.rs参照)")
}
