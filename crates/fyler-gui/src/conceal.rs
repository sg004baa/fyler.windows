//! IDプレフィックスの隠蔽とカーソル列補正(DESIGN.md「行フォーマット」)。
//!
//! nvimのconcealは使わない(Rust側描画なので漏れない)。バッファの生テキストから
//! `/012 ` プレフィックスとインデントタブを取り除いて表示し、
//! カーソル列をそのぶん補正する。
//! **プレフィックスの解釈は必ず `fyler_core::grammar` を使う**(再実装禁止)。

use fyler_core::editor::Cursor;
use fyler_core::grammar;

/// 1行の表示形。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcealedLine<'a> {
    /// 名前部分(タブの後)から始まる表示テキスト。
    pub display: &'a str,
    /// 隠したバイト数(IDプレフィックス + インデントタブ)。
    pub concealed_bytes: usize,
    /// インデント深さ(タブ数)。アイコンの描画位置決定に使う。
    pub depth: usize,
}

/// 行の表示形を作る。
///
/// 実装契約:
/// - `grammar::split_id_prefix` が `WithId` → プレフィックスを隠す
/// - `NoId` → 行頭インデントだけ隠す
/// - `Broken` → IDとしては隠さず、行頭インデントだけ隠す
pub fn conceal_line(raw: &str) -> ConcealedLine<'_> {
    // grammar::id_prefix_len は WithId のみ非0、NoId/Broken は0。
    let prefix_bytes = grammar::id_prefix_len(raw);
    let rest = &raw[prefix_bytes..];
    let (depth, display) = grammar::split_indent(rest);
    let indent_bytes = rest.len() - display.len();
    ConcealedLine {
        display,
        concealed_bytes: prefix_bytes + indent_bytes,
        depth,
    }
}

/// 生バッファ座標のカーソルを表示座標(conceal補正済みの列)へ変換する。
///
/// 実装契約:
/// - `cursor.col`(バイトオフセット)からその行の `concealed_bytes` を引く
/// - カーソルがプレフィックス/インデント領域内にある場合は表示列0へクランプする
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
                depth: 0,
            }
        );
    }

    #[test]
    fn conceal_line_hides_indent_tabs_after_id_prefix() {
        // プレフィックス直後のtabインデントは装飾扱いとして隠す。
        assert_eq!(
            conceal_line("/013 \t\tmain.rs"),
            ConcealedLine {
                display: "main.rs",
                concealed_bytes: 7,
                depth: 2,
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
                depth: 0,
            }
        );
    }

    #[test]
    fn conceal_line_passes_through_create_candidate() {
        // インデントなしのIDなし行(CREATE候補)は入力そのまま。
        // マルチバイト名でも変わらない。
        assert_eq!(
            conceal_line("新規ファイル.txt"),
            ConcealedLine {
                display: "新規ファイル.txt",
                concealed_bytes: 0,
                depth: 0,
            }
        );
    }

    #[test]
    fn conceal_line_hides_indent_tabs_without_id_prefix() {
        // IDなしCREATE候補でも行頭tabは装飾扱いとして隠す。
        assert_eq!(
            conceal_line("\tnew.txt"),
            ConcealedLine {
                display: "new.txt",
                concealed_bytes: 1,
                depth: 1,
            }
        );
    }

    #[test]
    fn conceal_line_shows_broken_prefix_but_still_hides_leading_indent() {
        // 破損プレフィックス自体は見せる。行頭tabだけは装飾扱いとして隠す。
        assert_eq!(
            conceal_line("/0"),
            ConcealedLine {
                display: "/0",
                concealed_bytes: 0,
                depth: 0,
            }
        );
        assert_eq!(
            conceal_line("\t/0"),
            ConcealedLine {
                display: "/0",
                concealed_bytes: 1,
                depth: 1,
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
    fn display_cursor_shifts_by_prefix_and_indent() {
        let raw = "/013 \tmain.rs";
        assert_eq!(
            display_cursor(raw, Cursor { line: 1, col: 6 }),
            Cursor { line: 1, col: 0 }
        );
        assert_eq!(
            display_cursor(raw, Cursor { line: 1, col: 10 }),
            Cursor { line: 1, col: 4 }
        );
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
        // インデントなしのIDなし行は concealed=0 なので col は不変。line も保持。
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
