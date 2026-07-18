//! fyler-fsops — Windowsファイル操作層(DESIGN.md「Windowsファイル操作層(FsOps)」)。
//!
//! - **絶対ルール1**: 実FSへの書き込みは [`apply`] モジュールだけが行い、
//!   保存状態機械の `Applying`(= 確認ダイアログ承認後)からのみ呼ばれる
//! - **絶対ルール3**: Windowsの拡張長パス変換は [`long_path`] モジュールの
//!   1か所に閉じ込める
//! - Win32 API(windowsクレート)に触れてよいのはこのクレートだけ。
//!   公開シグネチャには std のパス型と fyler-core の型だけを出す

pub mod apply;
pub mod backup;
pub mod case;
pub mod catalog;
pub mod classify;
pub mod clipboard;
pub mod dialog;
pub mod dirsize;
#[cfg(windows)]
pub mod display;
pub mod drag;
pub mod drives;
pub mod extract;
pub mod gitstatus;
pub mod identity;
pub mod info;
pub mod long_path;
pub mod onedrive;
pub mod open;
pub mod openwith;
pub mod preflight;
pub mod recycle;
pub mod scan;
pub mod shortcut;
pub mod terminal;
pub mod undo;
pub mod watch;

mod winattr;

pub use apply::{apply_import_plan_cancellable, apply_transfer_plan_cancellable};
pub use preflight::{ImportPreflight, TransferPreflight, preflight_import, preflight_transfer};
pub use undo::{UndoRecorder, apply_undo_cancellable, preflight_undo};
