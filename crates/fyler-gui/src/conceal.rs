//! IDプレフィックスの隠蔽とカーソル列補正(DESIGN.md「行フォーマット」)。
//!
//! nvimのconcealは使わない(Rust側描画なので漏れない)。バッファの生テキストから
//! `/012 ` プレフィックスを取り除いて表示し、カーソル列をそのぶん補正する。
//! **プレフィックスの解釈は必ず `fyler_core::grammar` を使う**(再実装禁止)。

use fyler_core::editor::Cursor;

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
    todo!("M1: grammar::id_prefix_len を使ったプレフィックス隠蔽")
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
    todo!("M1: カーソル列補正(M0スパイクの検証結果に従う)")
}
