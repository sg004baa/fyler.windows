//! OSの既定アプリケーションでファイルを開く。

use std::path::Path;

#[cfg(not(windows))]
use anyhow::Context;

/// `path` をOSに関連付けられた既定アプリケーションで開く。
///
/// WindowsではShellの`open` verbへ委譲し、戻り値が32以下なら失敗として返す。
/// 非Windowsでは開発時の検証用に`xdg-open`を起動する。どちらも対象ファイルの
/// 内容やメタデータを変更せず、起動したアプリケーションの終了は待たない。
#[cfg(windows)]
pub fn open_with_default_app(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use anyhow::bail;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: `path_wide` は呼び出し中有効なNUL終端UTF-16文字列であり、
    // 残りの文字列引数には静的なNUL終端文字列またはnullを渡している。
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code <= 32 {
        bail!(
            "既定アプリケーションで開けませんでした (ShellExecuteW={code}): {}",
            path.display()
        );
    }
    Ok(())
}

/// `path` をOSに関連付けられた既定アプリケーションで開く。
///
/// 非Windowsでは開発時の検証用に`xdg-open`を起動する。対象ファイルの内容や
/// メタデータを変更せず、起動したアプリケーションの終了は待たない。
#[cfg(not(windows))]
pub fn open_with_default_app(path: &Path) -> anyhow::Result<()> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .with_context(|| {
            format!(
                "xdg-openで既定アプリケーションを起動できません: {}",
                path.display()
            )
        })?;
    Ok(())
}
