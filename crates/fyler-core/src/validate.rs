//! validateエラーの型(DESIGN.md「validateで弾くもの」)。
//! 検出ロジックは fyler-pipeline::validate / parse にある。
//!
//! いずれか1件でも検出されたら**保存を中断**する(planを作らない・実行しない)。
//! 曖昧な状態から操作を推測して実行しない。

use crate::id::EntryId;
use crate::path::TreePath;

/// 行番号(`line`)はすべて0始まりのバッファ行番号。表示時に+1すること。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidateError {
    /// IDプレフィックスが部分的に破壊されている(例: `/0` だけ残っている)。
    /// 判定は `fyler_core::grammar::split_id_prefix` == `Broken`。
    #[error("行{line}: IDプレフィックスが壊れています。undoで戻すか行を削除してください")]
    BrokenIdPrefix { line: usize },

    /// インデントが不正: 奇数スペース、親を飛ばした深いインデント、
    /// 親がディレクトリでない(ファイルの下にネスト)、など。
    #[error("行{line}: インデントが不正です")]
    InvalidIndent { line: usize },

    /// 同一ディレクトリ内の名前重複。
    #[error("同名エントリが重複しています: {path}")]
    DuplicateName { path: TreePath },
    /// 名前が空。IDプレフィックスだけの行(`/001 `)や空ディレクトリ名を実FS操作へ流さない。
    #[error("行{line}: 名前が空です")]
    EmptyName { line: usize },

    /// Windows予約文字(`< > : " / \ | ? *`)・制御文字を含む名前。
    /// 判定は `fyler_core::win_naming::find_reserved_char`。
    #[error("行{line}: 名前にWindowsで使用できない文字 {ch:?} が含まれています: {name}")]
    ReservedChar { line: usize, name: String, ch: char },

    /// Windows予約名(CON, PRN, AUX, NUL, COM1-9, LPT1-9。拡張子付きも不可)。
    /// 判定は `fyler_core::win_naming::is_reserved_name`。
    #[error("行{line}: Windowsの予約名は使えません: {name}")]
    ReservedName { line: usize, name: String },

    /// 名前の末尾がスペースまたはピリオド。
    /// 判定は `fyler_core::win_naming::has_invalid_trailing`。
    #[error("行{line}: 名前の末尾にスペース・ピリオドは使えません: {name:?}")]
    InvalidTrailing { line: usize, name: String },

    /// ディレクトリの自分自身(またはその子孫)への移動。
    #[error("ディレクトリを自分自身の配下へ移動することはできません (id={id}, {from} → {to})")]
    MoveIntoSelf {
        id: EntryId,
        from: TreePath,
        to: TreePath,
    },
    /// 一時名なしでは安全に逐次実行できないMove循環。
    #[error("ファイル名の入れ替えは一度に実行できません: {path}")]
    MoveCycle { path: TreePath },
}
