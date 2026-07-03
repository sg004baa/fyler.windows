//! fyler-engine-nvim — `EditorEngine` のNeovim実装(方式A)。
//!
//! **このクレートだけが nvim-rs / nvim固有概念(keycode表記・changedtick・
//! autocmd・msgpack-RPC等)に依存してよい**(AGENTS.md 絶対ルール2)。
//! 公開シグネチャに nvim-rs の型を出さないこと。外へ出すのは
//! `fyler_core::editor` のエンジン非依存型のみ。
//!
//! Neovimは「Vim編集状態マシン」としてのみ使う。描画はしない(grid系イベントは
//! すべて無視)。nvim-rsはAPIをunstableと明言しているため、RPCクライアント部分は
//! `rpc` モジュールに薄く隔離し、バージョンは固定する(Cargo.tomlで `=0.9.2`)。

// scaffolding: todo!()スタブの引数警告を抑制。実装が入り次第このallowを削除する。
#![allow(unused_variables)]

pub mod engine;
pub mod guard;
pub mod spawn;
pub mod translate;

pub use engine::NvimEngine;
pub use spawn::NvimConfig;
