//! fyler-gui — egui/eframeによる描画層(DESIGN.md「GUI(egui)」)。
//!
//! 描画はすべてRust側で自前(neovide方式は不採用。nvimのgrid描画は使わない)。
//! ツリー描画・アイコン・git status・インデントガイド・モードライン・カーソル・
//! cmdline・messages・確認ダイアログをここで描く。
//!
//! 依存境界: エンジンには `fyler_core::editor` のトレイト/型経由でのみ触れる。
//! nvim固有概念をここに書いたら絶対ルール2違反。

pub mod app;
pub(crate) mod chrome;
pub mod cmdline;
pub mod conceal;
pub mod confirm;
pub mod icon;
pub mod input;
pub mod modeline;
pub(crate) mod theme;
pub mod tree_view;

pub use app::FylerApp;
