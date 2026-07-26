//! Windows既知フォルダ(Known Folders)の実パス解決(issue #38「Downloadsの既定ソート」)。
//!
//! Explorerは`Downloads`既知フォルダを更新日時の降順で既定表示する。fyler-appは
//! ルートがこの既知フォルダかどうかを判定するために実パスを必要とするが、Win32 API
//! (`SHGetKnownFolderPath`)に触れてよいのは`fyler-fsops`だけ(AGENTS.md 絶対ルール3
//! 周辺の境界)なので、解決はこのモジュールに閉じ込める。

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// ユーザーの`Downloads`既知フォルダの実パスを返す。
///
/// Windowsでは`SHGetKnownFolderPath(FOLDERID_Downloads, ...)`で解決した実パスを返す。
/// 解決に失敗した場合は`None`(`USERPROFILE\Downloads`のような推測フォールバックはしない)。
///
/// 非Windowsでは常に`None`を返す。Windows既知フォルダは非Windowsには存在しないため、
/// これは代替経路ではなく「無い」ことの表明である。
///
/// 結果は[`LazyLock`]で1度だけ解決してキャッシュする(毎ナビゲーションでCOMを叩かない)。
pub fn downloads_dir() -> Option<&'static Path> {
    static CACHE: LazyLock<Option<PathBuf>> = LazyLock::new(resolve_downloads_dir);
    CACHE.as_deref()
}

#[cfg(windows)]
fn resolve_downloads_dir() -> Option<PathBuf> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStringExt;

    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

    // SAFETY: `SHGetKnownFolderPath`はCoTaskMemで確保したNUL終端UTF-16文字列を`PWSTR`で返す。
    // `htoken = None`は現在のユーザーを意味する。
    let pwstr = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None) }.ok()?;
    if pwstr.0.is_null() {
        return None;
    }

    let mut len = 0usize;
    // SAFETY: Shell APIはNUL終端UTF-16文字列を返す。
    while unsafe { *pwstr.0.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len`は直前にNUL終端まで走査して得た範囲。
    let slice = unsafe { std::slice::from_raw_parts(pwstr.0, len) };
    let path = PathBuf::from(std::ffi::OsString::from_wide(slice));
    // SAFETY: `pwstr`はShell APIがCoTaskMemで返した文字列。解放漏れは不可。
    unsafe { CoTaskMemFree(Some(pwstr.0.cast::<c_void>())) };

    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path)
    }
}

#[cfg(not(windows))]
fn resolve_downloads_dir() -> Option<PathBuf> {
    None
}
