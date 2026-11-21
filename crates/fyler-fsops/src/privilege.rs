//! Permission classification and explicit consent to open a separate elevated fyler.

use std::path::Path;

/// Inspect typed causes before reports turn errors into display strings.
pub fn is_permission_denied(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return true;
            }
            #[cfg(windows)]
            if matches!(error.raw_os_error(), Some(5 | 1314)) {
                return true;
            }
        }
        #[cfg(windows)]
        {
            if let Some(error) = cause.downcast_ref::<windows::core::Error>() {
                return permission_hresult(error.code().0);
            }
            if let Some(trash::Error::Os { code, .. }) = cause.downcast_ref::<trash::Error>() {
                return matches!(*code, 5 | 1314) || permission_hresult(*code);
            }
        }
        #[cfg(all(
            unix,
            not(target_os = "macos"),
            not(target_os = "ios"),
            not(target_os = "android")
        ))]
        if let Some(trash::Error::FileSystem { source, .. }) = cause.downcast_ref::<trash::Error>()
        {
            return source.kind() == std::io::ErrorKind::PermissionDenied;
        }
        false
    })
}

#[cfg(windows)]
fn permission_hresult(code: i32) -> bool {
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PRIVILEGE_NOT_HELD};
    code == ERROR_ACCESS_DENIED.to_hresult().0 || code == ERROR_PRIVILEGE_NOT_HELD.to_hresult().0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElevationOutcome {
    Unavailable,
    AlreadyElevated,
    Declined,
    Opened,
    UacCancelled,
}

/// Blocking native consent and UAC. Call only from a worker, never the GUI thread.
/// This launches current_exe with one positional root argument, never a file from the tree.
#[cfg(windows)]
pub fn offer_elevation(root: &Path) -> anyhow::Result<ElevationOutcome> {
    decide_elevation(
        is_elevated()?,
        || confirm_elevation(root),
        || launch_elevated(root),
    )
}

#[cfg(not(windows))]
pub fn offer_elevation(_root: &Path) -> anyhow::Result<ElevationOutcome> {
    Ok(ElevationOutcome::Unavailable)
}

#[cfg(any(windows, test))]
fn decide_elevation(
    already_elevated: bool,
    confirm: impl FnOnce() -> anyhow::Result<bool>,
    launch: impl FnOnce() -> anyhow::Result<ElevationOutcome>,
) -> anyhow::Result<ElevationOutcome> {
    if already_elevated {
        return Ok(ElevationOutcome::AlreadyElevated);
    }
    if !confirm()? {
        return Ok(ElevationOutcome::Declined);
    }
    launch()
}

#[cfg(windows)]
fn is_elevated() -> anyhow::Result<bool> {
    use anyhow::Context;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: token is writable; the pseudo process handle remains valid.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .context("Failed to inspect administrator privileges")?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0;
    // SAFETY: elevation is a correctly sized writable TOKEN_ELEVATION. token is owned here.
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    // SAFETY: the token handle was opened above and is no longer used.
    unsafe { CloseHandle(token) }.context("Failed to close privilege query handle")?;
    result.context("Failed to inspect administrator privileges")?;
    Ok(elevation.TokenIsElevated != 0)
}

#[cfg(windows)]
fn confirm_elevation(root: &Path) -> anyhow::Result<bool> {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDCANCEL, IDOK, MB_DEFBUTTON2, MB_ICONWARNING, MB_OKCANCEL, MessageBoxW,
    };
    use windows::core::{PCWSTR, w};

    let message = format!(
        "A file operation was denied permission.\n\nOpen this directory in a separate administrator fyler window?\n{}\n\nThe failed operation will NOT be retried automatically. Some operations may already have succeeded. Review the report before trying again.\n\nThis window and its editor state stay open. Unsaved edits are not transferred. The new window and its Neovim configuration will run with administrator privileges.\n\nChoose OK to continue to Windows UAC, or Cancel to stay here.",
        root.display()
    );
    let message = message
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: title and message are valid NUL-terminated UTF-16 for this blocking call.
    let choice = unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            w!("fyler - Administrator privileges"),
            MB_OKCANCEL | MB_ICONWARNING | MB_DEFBUTTON2,
        )
    };
    match choice {
        IDOK => Ok(true),
        IDCANCEL => Ok(false),
        _ => Err(anyhow::Error::from(std::io::Error::last_os_error())),
    }
}

/// Quote one Windows argv argument, including trailing slashes and embedded quotes.
#[cfg(any(windows, test))]
fn quote_argument(argument: impl IntoIterator<Item = u16>) -> anyhow::Result<Vec<u16>> {
    let argument = argument.into_iter();
    let mut result = Vec::with_capacity(argument.size_hint().0 + 3);
    result.push(b'"' as u16);
    let mut slashes = 0;
    for unit in argument {
        anyhow::ensure!(unit != 0, "A launch path contains a NUL character");
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        let count = if unit == b'"' as u16 {
            slashes * 2 + 1
        } else {
            slashes
        };
        result.extend(std::iter::repeat_n(b'\\' as u16, count));
        result.push(unit);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    result.push(b'"' as u16);
    result.push(0);
    Ok(result)
}

#[cfg(windows)]
fn launch_elevated(root: &Path) -> anyhow::Result<ElevationOutcome> {
    use anyhow::Context;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::ERROR_CANCELLED;
    use windows::Win32::UI::Shell::{
        SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    let executable = std::env::current_exe().context("Failed to locate this fyler executable")?;
    let directory = executable
        .parent()
        .context("The fyler executable has no parent directory")?;
    let directory = directory
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let executable = executable
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let root = std::path::absolute(root).context("Failed to resolve the affected directory")?;
    let parameters = quote_argument(root.as_os_str().encode_wide())?;
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(executable.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        lpDirectory: PCWSTR(directory.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: strings remain alive through ShellExecuteExW. NOASYNC permits this worker to exit.
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => Ok(ElevationOutcome::Opened),
        Err(error) if error.code() == ERROR_CANCELLED.to_hresult() => {
            Ok(ElevationOutcome::UacCancelled)
        }
        Err(error) => Err(error).context("Failed to open an administrator fyler window"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_typed_permission_causes_not_messages() {
        let denied =
            anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                .context("copy link");
        assert!(is_permission_denied(&denied));
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::AlreadyExists,
            std::io::ErrorKind::InvalidInput,
            std::io::ErrorKind::StorageFull,
        ] {
            assert!(!is_permission_denied(
                &anyhow::Error::from(std::io::Error::from(kind))
                    .context("Permission denied (os error 1314)")
            ));
        }
        assert!(!is_permission_denied(&anyhow::anyhow!(
            "Access is denied (os error 5)"
        )));
    }

    #[test]
    fn cancellation_and_elevated_process_never_launch() {
        assert_eq!(
            decide_elevation(false, || Ok(false), || panic!("must not launch")).unwrap(),
            ElevationOutcome::Declined
        );
        assert_eq!(
            decide_elevation(
                true,
                || panic!("must not prompt"),
                || panic!("must not launch")
            )
            .unwrap(),
            ElevationOutcome::AlreadyElevated
        );
        assert!(
            decide_elevation(
                false,
                || anyhow::bail!("dialog unavailable"),
                || panic!("must not launch")
            )
            .is_err()
        );
    }

    #[test]
    fn consent_launches_once_and_preserves_uac_cancellation() {
        let mut launches = 0;
        let outcome = decide_elevation(
            false,
            || Ok(true),
            || {
                launches += 1;
                Ok(ElevationOutcome::Opened)
            },
        )
        .unwrap();
        assert_eq!(outcome, ElevationOutcome::Opened);
        assert_eq!(launches, 1);
        assert_eq!(
            decide_elevation(false, || Ok(true), || Ok(ElevationOutcome::UacCancelled)).unwrap(),
            ElevationOutcome::UacCancelled
        );
        assert!(decide_elevation(false, || Ok(true), || anyhow::bail!("launch failed")).is_err());
    }

    #[test]
    fn quotes_one_positional_root_without_shell_interpretation() {
        for (input, expected) in [
            ("", "\"\""),
            ("C:\\", "\"C:\\\\\""),
            ("C:\\日本語 & files\\", "\"C:\\日本語 & files\\\\\""),
            ("a\\\"b", "\"a\\\\\\\"b\""),
            ("\\\\server\\share\\", "\"\\\\server\\share\\\\\""),
        ] {
            let quoted = quote_argument(input.encode_utf16()).unwrap();
            assert_eq!(
                String::from_utf16(&quoted[..quoted.len() - 1]).unwrap(),
                expected
            );
            assert_eq!(quoted.last(), Some(&0));
        }
        assert!(quote_argument([65, 0, 66]).is_err());
        assert_eq!(quote_argument([0xd800]).unwrap(), vec![34, 0xd800, 34, 0]);
    }

    #[cfg(windows)]
    #[test]
    fn classifies_win32_and_hresult_permission_failures() {
        use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PRIVILEGE_NOT_HELD};
        for code in [5, 1314] {
            assert!(is_permission_denied(
                &std::io::Error::from_raw_os_error(code).into()
            ));
        }
        for code in [
            ERROR_ACCESS_DENIED.to_hresult(),
            ERROR_PRIVILEGE_NOT_HELD.to_hresult(),
        ] {
            assert!(is_permission_denied(
                &windows::core::Error::from_hresult(code).into()
            ));
            assert!(is_permission_denied(
                &trash::Error::Os {
                    code: code.0,
                    description: "localized message".into()
                }
                .into()
            ));
        }
        assert!(!is_permission_denied(
            &std::io::Error::from_raw_os_error(32).into()
        ));
        assert!(is_elevated().is_ok());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "Requires a user or UI automation to cancel the native dialog; never launches UAC"]
    fn native_consent_cancel_smoke() {
        assert!(!confirm_elevation(Path::new("C:\\permission-test")).unwrap());
    }
}
