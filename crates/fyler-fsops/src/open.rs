//! OSの既定アプリケーションでファイルを開く(管理者権限起動を含む)。

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
            "Failed to open with the default application (ShellExecuteW={code}): {}",
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
                "Failed to launch default application with xdg-open: {}",
                path.display()
            )
        })?;
    Ok(())
}

/// `path` を管理者権限(UAC昇格)で開く。
///
/// WindowsではShellの`runas` verbへ`ShellExecuteExW`で委譲する。起動した
/// プロセスの終了は待たず、ハンドルも保持しない(`SEE_MASK_NOCLOSEPROCESS`
/// 不使用)。UACダイアログでユーザーがキャンセルした場合(`ERROR_CANCELLED`)は
/// 「Elevation was cancelled」を含む明確なエラーを返す。対象ファイルの内容や
/// メタデータは変更しない。
#[cfg(windows)]
pub fn open_as_admin(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use anyhow::bail;
    use windows::Win32::Foundation::ERROR_CANCELLED;
    use windows::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>())
            .expect("SHELLEXECUTEINFOW size fits in u32"),
        lpVerb: w!("runas"),
        lpFile: PCWSTR(path_wide.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` は呼び出し中有効で、`lpFile` は呼び出し中有効なNUL終端
    // UTF-16文字列を指す。`lpVerb` は静的なNUL終端文字列。
    if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
        if error.code() == ERROR_CANCELLED.to_hresult() {
            bail!("Elevation was cancelled: {}", path.display());
        }
        bail!(
            "Failed to open as administrator ({error}): {}",
            path.display()
        );
    }
    Ok(())
}

/// `path` を管理者権限で開く(非Windows)。
///
/// 非WindowsにUAC相当はないため、明示的なエラーを返す(silent fallback禁止)。
#[cfg(not(windows))]
pub fn open_as_admin(path: &Path) -> anyhow::Result<()> {
    anyhow::bail!(
        "Open as administrator is only supported on Windows: {}",
        path.display()
    );
}
