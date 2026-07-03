//! parse: バッファ行 → 行の構造化 → DesiredTree。

use fyler_core::editor::EditorLine;
use fyler_core::id::EntryId;
use fyler_core::tree::DesiredTree;
use fyler_core::validate::ValidateError;

/// バッファ1行のparse結果(空行は含まれない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLine {
    /// 0始まりのバッファ行番号(空行をスキップしても元の行番号を保持する)。
    pub line: usize,
    /// IDプレフィックスから読んだID。ない行(CREATE候補)はNone。
    pub id: Option<EntryId>,
    /// IDプレフィックスが部分的に破壊されている
    /// (`grammar::split_id_prefix` が `Broken` を返した)。
    /// [`to_desired_tree`] で `ValidateError::BrokenIdPrefix` になり保存は中断される。
    pub id_broken: bool,
    /// インデントの半角スペース数(生の値。2で割る前)。
    /// `INDENT_WIDTH` の倍数でない場合は [`to_desired_tree`] でInvalidIndentになる。
    pub indent_spaces: usize,
    /// 表示名。末尾のディレクトリサフィックス `/` は除去済み。
    pub name: String,
    pub is_dir: bool,
}

/// バッファ全行を構造化する。
///
/// 実装契約:
/// - 空行(空文字列・スペースのみの行)は警告なしでスキップする
/// - 1行の分解は必ず `fyler_core::grammar` の
///   `split_id_prefix` → `split_indent` → `split_dir_suffix` の順で行う
/// - この関数はエラーを出さない(Broken等は`ParsedLine`のフラグとして記録し、
///   エラー化は [`to_desired_tree`] に任せる)
pub fn parse(lines: &[EditorLine]) -> Vec<ParsedLine> {
    todo!("M2: バッファ行の構造化(tests/spec_m2.rs参照)")
}

/// 構造化済みの行列からDesiredTreeを組み立てる。
///
/// 実装契約:
/// - インデント(spaces / INDENT_WIDTH)で親子関係を決める。
///   直前のより浅い行のうち最も近いディレクトリが親
/// - 以下はエラー(全行分を集めて返し、保存を中断する):
///   - `id_broken` な行 → `BrokenIdPrefix`
///   - 奇数スペース・親を飛ばした深いインデント・親がファイル → `InvalidIndent`
/// - 同一IDの複数出現はここでは**エラーにしない**(COPY表現。diff層が解釈する)
/// - 名前の妥当性(予約文字等)もここでは見ない(validate層の責務)
pub fn to_desired_tree(parsed: &[ParsedLine]) -> Result<DesiredTree, Vec<ValidateError>> {
    todo!("M2: インデントからツリー構造を復元する(tests/spec_m2.rs参照)")
}
