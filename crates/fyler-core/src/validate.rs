//! validateエラーの型(DESIGN.md「validateで弾くもの」)。
//! 検出ロジックは fyler-pipeline::validate / parse にある
//! (`TargetOccupiedByDirectory`のみfyler-fsops::preflightで検出)。
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
    #[error("line {line}: broken ID prefix; undo or delete this line")]
    BrokenIdPrefix { line: usize },

    /// インデントが不正: 奇数スペース、親を飛ばした深いインデント、
    /// 親がディレクトリでない(ファイルの下にネスト)、など。
    #[error("line {line}: invalid indentation")]
    InvalidIndent { line: usize },

    /// 同一ディレクトリ内の名前重複。
    #[error("duplicate entry name: {path}")]
    DuplicateName { path: TreePath },
    /// 名前が空。IDプレフィックスだけの行(`/001 `)や空ディレクトリ名を実FS操作へ流さない。
    #[error("line {line}: empty name")]
    EmptyName { line: usize },

    /// Windows予約文字(`< > : " / \ | ? *`)・制御文字を含む名前。
    /// 判定は `fyler_core::win_naming::find_reserved_char`。
    #[error("line {line}: name contains a Windows-reserved character {ch:?}: {name}")]
    ReservedChar { line: usize, name: String, ch: char },

    /// Windows予約名(CON, PRN, AUX, NUL, COM1-9, LPT1-9。拡張子付きも不可)。
    /// 判定は `fyler_core::win_naming::is_reserved_name`。
    #[error("line {line}: Windows-reserved name is not allowed: {name}")]
    ReservedName { line: usize, name: String },

    /// 名前の末尾がスペースまたはピリオド。
    /// 判定は `fyler_core::win_naming::has_invalid_trailing`。
    #[error("line {line}: trailing spaces or periods are not allowed: {name:?}")]
    InvalidTrailing { line: usize, name: String },

    /// ディレクトリの自分自身(またはその子孫)への移動。
    #[error("cannot move a directory into itself (id={id}, {from} → {to})")]
    MoveIntoSelf {
        id: EntryId,
        from: TreePath,
        to: TreePath,
    },
    /// 一時名なしでは安全に逐次実行できないMove循環。
    #[error("rename cycles cannot be applied in one save: {path}")]
    MoveCycle { path: TreePath },

    /// 移動先の実FSに既存のディレクトリが存在する(上書き不可)。
    /// baselineに現れない実体(隠しファイル設定で非表示のディレクトリ等)との衝突。
    /// 検出はfyler-fsopsのpreflight走査(plan確定時)で行う。
    #[error("target is occupied by an existing directory and cannot be overwritten: {path}")]
    TargetOccupiedByDirectory { path: TreePath },
}
