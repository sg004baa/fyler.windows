//! バッファ行フォーマット(in-buffer ID方式)の正典。**実装済み。再実装禁止。**
//!
//! DESIGN.md「行フォーマット」「バッファ文法の決定事項」に対応する。
//! parse層(fyler-pipeline)もconceal層(fyler-gui)も必ずこのモジュールを使うこと。
//! 双方が独自にプレフィックスを解釈すると、描画とdiffの解釈がズレる事故になる。
//!
//! ## 文法
//!
//! ```text
//! line      = [ id-prefix ] indent name
//! id-prefix = "/" digits " "     ; 行頭のみ。数字1桁以上 + 半角スペース1個
//! indent    = ("  ")*            ; 半角スペース2個 = 1階層
//! name      = 編集可能な名前。ディレクトリは末尾 "/"
//! 空行      = 無視(警告なしでスキップ)
//! ```
//!
//! 例(DESIGN.mdより):
//!
//! ```text
//! /012 src/
//! /013   main.rs
//! /014   lib.rs
//! 新規ファイル.txt
//! ```
//!
//! - IDのない行 = CREATE候補
//! - `/` で始まるのにこの文法に一致しない行(例: `/0` だけ、`/12x`、スペース欠落)は
//!   **Broken**。validateエラーとして保存を中断する(推測実行しない)
//! - アイコン・git status・罫線はバッファ文字列に含めない(Rust側の描画装飾)

use crate::id::EntryId;

/// 1階層ぶんのインデント幅(半角スペース数)。
pub const INDENT_WIDTH: usize = 2;

/// ディレクトリを表す名前の末尾サフィックス。
pub const DIR_SUFFIX: char = '/';

/// IDプレフィックスの開始文字。
pub const ID_PREFIX_CHAR: char = '/';

/// IDプレフィックスの標準桁数(ゼロ埋め)。`/012 ` のように最低3桁で出力する。
/// 表示幅を安定させ、カーソル列補正を単純にするため。1000以上は自然に桁が増える。
/// パース側は桁数を問わない。
pub const ID_MIN_DIGITS: usize = 3;

/// [`split_id_prefix`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixParse<'a> {
    /// IDプレフィックス付きの行。`rest` はプレフィックス直後(インデント含む)。
    WithId { id: EntryId, rest: &'a str },
    /// IDプレフィックスのない行(CREATE候補)。`rest` は行全体。
    NoId { rest: &'a str },
    /// `/` で始まるが文法に一致しない = 部分的に破壊されたプレフィックス。
    /// validateエラー(`ValidateError::BrokenIdPrefix`)として保存を中断すること。
    Broken,
}

/// 行頭のIDプレフィックスを分離する。
pub fn split_id_prefix(line: &str) -> PrefixParse<'_> {
    let Some(body) = line.strip_prefix(ID_PREFIX_CHAR) else {
        return PrefixParse::NoId { rest: line };
    };
    let digits_end = body
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(body.len());
    if digits_end == 0 {
        return PrefixParse::Broken; // "/", "/x..."
    }
    let Some(rest) = body[digits_end..].strip_prefix(' ') else {
        return PrefixParse::Broken; // "/12"(スペース欠落), "/12x..."
    };
    match body[..digits_end].parse::<u64>() {
        Ok(n) => PrefixParse::WithId {
            id: EntryId(n),
            rest,
        },
        Err(_) => PrefixParse::Broken, // u64を超える桁あふれ
    }
}

/// IDをバッファ行のプレフィックス文字列(`/012 ` 形式)にする。
pub fn format_id_prefix(id: EntryId) -> String {
    format!("{}{:0width$} ", ID_PREFIX_CHAR, id.0, width = ID_MIN_DIGITS)
}

/// 行のIDプレフィックス部分のバイト長(conceal・カーソル列補正用)。
/// プレフィックスがない行・壊れている行は0。
pub fn id_prefix_len(line: &str) -> usize {
    match split_id_prefix(line) {
        PrefixParse::WithId { rest, .. } => line.len() - rest.len(),
        _ => 0,
    }
}

/// 先頭のインデント(半角スペース)を分離する。`(スペース数, 残り)` を返す。
/// スペース数が [`INDENT_WIDTH`] の倍数でない行の扱い(InvalidIndent)はparse層の責務。
pub fn split_indent(s: &str) -> (usize, &str) {
    let spaces = s.len() - s.trim_start_matches(' ').len();
    (spaces, &s[spaces..])
}

/// 名前末尾のディレクトリサフィックスを分離する。`(名前, is_dir)` を返す。
pub fn split_dir_suffix(name: &str) -> (&str, bool) {
    match name.strip_suffix(DIR_SUFFIX) {
        Some(stripped) => (stripped, true),
        None => (name, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_id_directory_line() {
        // DESIGN.mdの例そのまま
        assert_eq!(
            split_id_prefix("/012 src/"),
            PrefixParse::WithId {
                id: EntryId(12),
                rest: "src/"
            }
        );
    }

    #[test]
    fn with_id_indented_line() {
        let PrefixParse::WithId { id, rest } = split_id_prefix("/013   main.rs") else {
            panic!("expected WithId");
        };
        assert_eq!(id, EntryId(13));
        // プレフィックス終端スペースの後にインデント2個が残る
        assert_eq!(rest, "  main.rs");
        let (spaces, name) = split_indent(rest);
        assert_eq!(spaces, INDENT_WIDTH);
        assert_eq!(name, "main.rs");
    }

    #[test]
    fn no_id_is_create_candidate() {
        assert_eq!(
            split_id_prefix("新規ファイル.txt"),
            PrefixParse::NoId {
                rest: "新規ファイル.txt"
            }
        );
        assert_eq!(
            split_id_prefix("  child.txt"),
            PrefixParse::NoId {
                rest: "  child.txt"
            }
        );
    }

    #[test]
    fn broken_prefixes_are_rejected() {
        // DESIGN.md: 「/0だけ残っている等」は保存中断
        assert_eq!(split_id_prefix("/"), PrefixParse::Broken);
        assert_eq!(split_id_prefix("/0"), PrefixParse::Broken); // スペース欠落
        assert_eq!(split_id_prefix("/x foo"), PrefixParse::Broken);
        assert_eq!(split_id_prefix("/12x foo"), PrefixParse::Broken);
    }

    #[test]
    fn format_roundtrip() {
        let line = format!("{}src/", format_id_prefix(EntryId(12)));
        assert_eq!(line, "/012 src/");
        assert_eq!(
            split_id_prefix(&line),
            PrefixParse::WithId {
                id: EntryId(12),
                rest: "src/"
            }
        );
        // 3桁を超えるIDは自然に桁が増える
        assert_eq!(format_id_prefix(EntryId(1234)), "/1234 ");
    }

    #[test]
    fn prefix_len_for_conceal() {
        assert_eq!(id_prefix_len("/012 src/"), 5);
        assert_eq!(id_prefix_len("新規ファイル.txt"), 0);
        assert_eq!(id_prefix_len("/0"), 0); // Brokenはconceal対象外(そのまま見せる)
    }

    #[test]
    fn dir_suffix_split() {
        assert_eq!(split_dir_suffix("src/"), ("src", true));
        assert_eq!(split_dir_suffix("main.rs"), ("main.rs", false));
    }
}
