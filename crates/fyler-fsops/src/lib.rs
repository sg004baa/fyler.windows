//! fyler-fsops — Windowsファイル操作層(DESIGN.md「Windowsファイル操作層(FsOps)」)。
//!
//! - **絶対ルール1**: 実FSへの書き込みは [`apply::apply_plan`] だけが行い、
//!   保存状態機械の `Applying`(= 確認ダイアログ承認後)からのみ呼ばれる
//! - **絶対ルール3**: Windowsの拡張長パス変換は [`long_path`] モジュールの
//!   1か所に閉じ込める
//! - Win32 API(windowsクレート)に触れてよいのはこのクレートだけ。
//!   公開シグネチャには std のパス型と fyler-core の型だけを出す

pub mod apply;
pub mod case;
pub mod classify;
pub mod drives;
pub mod gitstatus;
pub mod info;
pub mod long_path;
pub mod onedrive;
pub mod open;
pub mod recycle;
pub mod scan;
pub mod watch;

mod winattr;
