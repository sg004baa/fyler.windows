//! fyler-fsops — Windowsファイル操作層(DESIGN.md「Windowsファイル操作層(FsOps)」)。
//!
//! - **絶対ルール1**: 実FSへの書き込みは [`apply::apply_plan`] だけが行い、
//!   保存状態機械の `Applying`(= 確認ダイアログ承認後)からのみ呼ばれる
//! - **絶対ルール3**: `\\?\` 等のパス変換は [`long_path`] モジュールの1か所に
//!   閉じ込める。他モジュール・他クレートに `\\?\` を書かない
//! - Win32 API(windowsクレート)に触れてよいのはこのクレートだけ。
//!   公開シグネチャには std のパス型と fyler-core の型だけを出す

// scaffolding: todo!()スタブの引数警告を抑制。実装が入り次第このallowを削除する。
#![allow(unused_variables)]

pub mod apply;
pub mod case;
pub mod classify;
pub mod long_path;
pub mod onedrive;
pub mod recycle;
pub mod scan;
pub mod watch;
