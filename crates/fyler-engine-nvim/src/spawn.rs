//! nvimプロセスの起動設定(DESIGN.md「プロセス管理」)。

use std::path::PathBuf;

use fyler_core::keymap::{KeyBinding, default_bindings};

/// nvim起動引数。**この組み合わせから変えないこと**(DESIGN.md):
///
/// - `--embed` 単体はUI attachを待ってブロックするため `--headless` を併用し、
///   非UI embedderとして起動する
/// - `--clean` はユーザー設定を除外するが組み込みプラグインはロードするため、
///   `-u NONE -i NONE --noplugin` で挙動を完全固定する
pub const NVIM_ARGS: &[&str] = &[
    "--embed",
    "--headless",
    "-u",
    "NONE",
    "-i",
    "NONE",
    "--noplugin",
];

/// Windowsでのspawn時に必ず付けるprocess creation flag
/// (コンソールウィンドウの一瞬の表示を防ぐ)。
/// `std::os::windows::process::CommandExt::creation_flags` で指定する。
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// NvimEngineの起動設定。
#[derive(Debug, Clone)]
pub struct NvimConfig {
    /// nvim実行ファイルのパス。配布時は同梱の `nvim.exe`(Apache 2.0、約15MB)。
    /// Neovim本体のバージョンも固定する(nvim-rsと合わせて検証したもの以外を使わない)。
    pub nvim_exe: PathBuf,
    /// ファイラーの表示ルートディレクトリ。
    pub root: PathBuf,
    /// 解決済みkeymapバインディング。既定値は組み込みkeymap。
    pub bindings: Vec<KeyBinding>,
}

impl NvimConfig {
    /// 実行ファイルとルートを指定し、組み込みkeymapで起動設定を作る。
    pub fn new(nvim_exe: PathBuf, root: PathBuf) -> Self {
        Self {
            nvim_exe,
            root,
            bindings: default_bindings(),
        }
    }
}
