//! Windowsショートカット(.lnk)の作成。
//!
//! Win32 COM(`IShellLinkW` + `IPersistFile`)に触れてよいのはfyler-fsopsだけ
//! (AGENTS.md 絶対ルール3周辺)。公開シグネチャは std のパス型だけを使う。
//!
//! **意図的な例外**: Shell(`IShellLinkW::SetPath` / `IPersistFile::Save`)へ
//! 渡すパスは素の絶対パス(`\\?\` を付けない)。Explorer・Shellとの相互運用の
//! ための意図的な例外であり、`crates/fyler-fsops/src/clipboard.rs` /
//! `terminal.rs` と同じ扱い。実FS操作直前の [`crate::long_path::to_fs`] は
//! ここでは適用しない(絶対ルール3: `\\?\` はlong_pathモジュールの1か所だけ)。
//!
//! # レイヤー構成
//! - 純粋関数層(このモジュール冒頭): `.lnk` ファイル名と衝突時の連番名の
//!   生成。cfg非依存で、Linux上でもunit testできる。
//! - `cfg(windows)`層: `CoCreateInstance` + `IShellLinkW` + `IPersistFile` に
//!   よる実作成。COM初期化は [`crate::openwith::ComApartment`] を共有する。
//! - `cfg(not(windows))`層: 明示的な`Err`を返す(silent fallback禁止)。

use std::path::Path;

use anyhow::bail;

// ---------------------------------------------------------------------
// 純粋関数層(cfg非依存)
// ---------------------------------------------------------------------

/// 対象エントリ名からショートカットファイル名を生成する(`<name>.lnk`)。
pub fn shortcut_file_name(target_name: &str) -> String {
    format!("{target_name}.lnk")
}

/// 衝突時の連番付きショートカットファイル名(`<name> (2).lnk`)。
pub fn numbered_shortcut_file_name(target_name: &str, number: u32) -> String {
    format!("{target_name} ({number}).lnk")
}

/// 既存名と衝突しないショートカットファイル名を返す。
///
/// まず `<name>.lnk`、衝突したら `<name> (2).lnk`、`<name> (3).lnk` … と
/// Explorerの慣例に合わせて連番を進める。`exists` は候補名(ファイル名のみ)が
/// 既に使われているかを返す述語で、FSアクセスは呼び出し側の責務。
pub fn available_shortcut_name(target_name: &str, exists: impl Fn(&str) -> bool) -> String {
    let candidate = shortcut_file_name(target_name);
    if !exists(&candidate) {
        return candidate;
    }
    for number in 2.. {
        let candidate = numbered_shortcut_file_name(target_name, number);
        if !exists(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 numbering space exhausted for shortcut names");
}

// ---------------------------------------------------------------------
// cfg(windows)層
// ---------------------------------------------------------------------

/// `target` を指すショートカットを `lnk_path` へ作成する。
///
/// **絶対ルール1**: 実FSへの書き込みなので、app層の確認ダイアログ承認後に
/// 限って呼ぶこと。`lnk_path` が既存なら上書きせずエラーを返す(衝突回避は
/// 呼び出し側が [`available_shortcut_name`] で済ませている前提の最終防衛)。
#[cfg(windows)]
pub fn create_shortcut(target: &Path, lnk_path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use anyhow::Context;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{Interface, PCWSTR};

    if lnk_path.exists() {
        bail!(
            "Shortcut destination already exists: {}",
            lnk_path.display()
        );
    }

    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    };
    let target_wide = wide(target);
    let lnk_wide = wide(lnk_path);

    let _apartment = crate::openwith::ComApartment::initialize()?;
    // SAFETY: COMは`_apartment`で初期化済み。渡すポインタはすべて呼び出し中
    // 有効なNUL終端UTF-16文字列。
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .context("Failed to create the ShellLink COM object")?;
        link.SetPath(PCWSTR(target_wide.as_ptr()))
            .with_context(|| format!("Failed to set shortcut target: {}", target.display()))?;
        if let Some(parent) = target.parent() {
            let parent_wide = wide(parent);
            link.SetWorkingDirectory(PCWSTR(parent_wide.as_ptr()))
                .with_context(|| {
                    format!(
                        "Failed to set shortcut working directory: {}",
                        parent.display()
                    )
                })?;
        }
        let persist: IPersistFile = link
            .cast()
            .context("ShellLink does not implement IPersistFile")?;
        persist
            .Save(PCWSTR(lnk_wide.as_ptr()), true)
            .with_context(|| format!("Failed to save shortcut: {}", lnk_path.display()))?;
    }
    Ok(())
}

/// `target` を指すショートカットを作成する(非Windows)。
///
/// .lnkはWindows Shell固有のため、明示的なエラーを返す(silent fallback禁止)。
#[cfg(not(windows))]
pub fn create_shortcut(target: &Path, lnk_path: &Path) -> anyhow::Result<()> {
    let _ = target;
    bail!(
        "Shortcut creation is only supported on Windows: {}",
        lnk_path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_file_name_appends_lnk_extension() {
        assert_eq!(shortcut_file_name("report.txt"), "report.txt.lnk");
        assert_eq!(shortcut_file_name("photos"), "photos.lnk");
    }

    #[test]
    fn available_shortcut_name_returns_plain_name_when_vacant() {
        assert_eq!(
            available_shortcut_name("report.txt", |_| false),
            "report.txt.lnk"
        );
    }

    #[test]
    fn available_shortcut_name_numbers_from_two_on_collision() {
        let taken = ["report.txt.lnk"];
        assert_eq!(
            available_shortcut_name("report.txt", |name| taken.contains(&name)),
            "report.txt (2).lnk"
        );

        let taken = ["report.txt.lnk", "report.txt (2).lnk", "report.txt (3).lnk"];
        assert_eq!(
            available_shortcut_name("report.txt", |name| taken.contains(&name)),
            "report.txt (4).lnk"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn create_shortcut_errors_on_non_windows() {
        assert!(create_shortcut(Path::new("a.txt"), Path::new("a.txt.lnk")).is_err());
    }
}
