//! IDプレフィックスの隠蔽とカーソル列補正(DESIGN.md「行フォーマット」)。
//!
//! nvimのconcealは使わない(Rust側描画なので漏れない)。バッファの生テキストから
//! `/012 ` プレフィックスを取り除いて表示し、カーソル列をそのぶん補正する。
//! **プレフィックスの解釈は必ず `fyler_core::grammar` を使う**(再実装禁止)。

use fyler_core::editor::Cursor;
use fyler_core::grammar;

/// 1行の表示形。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcealedLine<'a> {
    /// 表示するテキスト(IDプレフィックス除去後。インデントと名前)。
    pub display: &'a str,
    /// 隠したプレフィックスのバイト長(`grammar::id_prefix_len`)。
    pub concealed_bytes: usize,
}

/// 行の表示形を作る。
///
/// 実装契約:
/// - `grammar::split_id_prefix` が `WithId` → プレフィックスを隠す
/// - `NoId` → そのまま表示
/// - `Broken` → **隠さずそのまま表示**する(ユーザーが壊れたプレフィックスを
///   視認・修復できるように。validateエラーにもなる)
pub fn conceal_line(raw: &str) -> ConcealedLine<'_> {
    // grammar::id_prefix_len は WithId のみ非0、NoId/Broken は0(そのまま表示)。
    let concealed_bytes = grammar::id_prefix_len(raw);
    ConcealedLine {
        display: &raw[concealed_bytes..],
        concealed_bytes,
    }
}

/// 生バッファ座標のカーソルを表示座標(conceal補正済みの列)へ変換する。
///
/// 実装契約:
/// - `cursor.col`(バイトオフセット)からその行の `concealed_bytes` を引く
/// - カーソルがプレフィックス領域内にある場合は表示列0へクランプする
///   (M0で「カーソルがプレフィックス領域に入らない補正」の実現方法を検証。
///   破綻したらDESIGN.md「リスクと撤退ルート」のfallbackへ)
/// - バイトオフセット → 表示列の変換はUTF-8文字境界を考慮する
pub fn display_cursor(raw_line: &str, cursor: Cursor) -> Cursor {
    let concealed_bytes = conceal_line(raw_line).concealed_bytes;
    // col < concealed_bytes(プレフィックス領域内)→ 表示列0へクランプ。
    // それ以外はプレフィックス長ぶん左へずらす。M0 #2 で col と disp が
    // 同じUTF-8境界に乗ることを実機確認済み(可変桁 /{id} で成立)。
    Cursor {
        line: cursor.line,
        col: cursor.col.saturating_sub(concealed_bytes),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use fyler_core::editor::Cursor;

    #[test]
    fn conceal_line_hides_id_prefix() {
        // WithId 行: プレフィックス `/012 `(5バイト)を隠し、名前だけ表示する。
        assert_eq!(
            conceal_line("/012 src/"),
            ConcealedLine {
                display: "src/",
                concealed_bytes: 5,
            }
        );
    }

    #[test]
    fn conceal_line_keeps_indent_in_display() {
        // 隠すのはプレフィックスのみ。tabインデントは名前側なので display に残る。
        assert_eq!(
            conceal_line("/013 	main.rs"),
            ConcealedLine {
                display: "\tmain.rs",
                concealed_bytes: 5,
            }
        );
    }

    #[test]
    fn conceal_line_hides_prefix_before_multibyte_name() {
        // マルチバイト名でもプレフィックスのバイト長は5で不変、display は名前そのまま。
        assert_eq!(
            conceal_line("/012 新規ファイル.txt"),
            ConcealedLine {
                display: "新規ファイル.txt",
                concealed_bytes: 5,
            }
        );
    }

    #[test]
    fn conceal_line_passes_through_create_candidate() {
        // IDなし行(CREATE候補)は何も隠さず入力そのまま。マルチバイト名でも変わらない。
        assert_eq!(
            conceal_line("新規ファイル.txt"),
            ConcealedLine {
                display: "新規ファイル.txt",
                concealed_bytes: 0,
            }
        );
    }

    #[test]
    fn conceal_line_shows_broken_prefix_verbatim() {
        // 破損プレフィックスは隠さずそのまま見せる(ユーザーが視認・修復できるように)。
        assert_eq!(
            conceal_line("/0"),
            ConcealedLine {
                display: "/0",
                concealed_bytes: 0,
            }
        );
    }

    #[test]
    fn display_cursor_shifts_by_prefix_at_utf8_boundaries() {
        // "/012 新規ファイル.txt": concealed=5。名前部の文字境界(バイト5,8,11)は
        // 表示列 0,3,6 へ左シフトされる(M0実機確定値。各日本語文字=3バイト)。
        let raw = "/012 新規ファイル.txt";
        for (raw_col, disp_col) in [(5usize, 0usize), (8, 3), (11, 6)] {
            assert_eq!(
                display_cursor(
                    raw,
                    Cursor {
                        line: 0,
                        col: raw_col
                    }
                ),
                Cursor {
                    line: 0,
                    col: disp_col,
                },
                "raw col {raw_col} should map to display col {disp_col}"
            );
        }
    }

    #[test]
    fn display_cursor_clamps_inside_prefix_region() {
        // プレフィックス領域内(col 0..=4, いずれも <5)は表示列0へクランプする。
        let raw = "/012 新規ファイル.txt";
        for raw_col in 0..=4 {
            assert_eq!(
                display_cursor(
                    raw,
                    Cursor {
                        line: 7,
                        col: raw_col
                    }
                ),
                Cursor { line: 7, col: 0 },
                "raw col {raw_col} inside prefix must clamp to display col 0"
            );
        }
    }

    #[test]
    fn display_cursor_preserves_line() {
        // line は補正対象外。渡した値のまま保持する。
        let raw = "/012 src/";
        assert_eq!(
            display_cursor(raw, Cursor { line: 42, col: 5 }),
            Cursor { line: 42, col: 0 }
        );
    }

    #[test]
    fn display_cursor_is_identity_without_prefix() {
        // IDなし行は concealed=0 なので col は不変(補正なし)。line も保持。
        let raw = "新規ファイル.txt";
        for col in [0usize, 3, 6, 15] {
            assert_eq!(
                display_cursor(raw, Cursor { line: 3, col }),
                Cursor { line: 3, col },
                "no prefix: col {col} must be unchanged"
            );
        }
    }
}
