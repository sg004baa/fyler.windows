//! OSの「このアプリで開く」候補列挙と指定ハンドラ起動。

use std::path::Path;

/// 「このアプリで開く」候補1件。
///
/// COM型やShell APIの型は公開境界へ出さず、表示名と再列挙時に照合する
/// ハンドラ識別キーだけをapp層へ渡す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWithHandler {
    /// 表示名。
    pub display_name: String,
    /// ハンドラ識別キー。実行時に再列挙したハンドラとの照合に使う。
    pub key: String,
}

/// 拡張子に関連づく起動候補を列挙する。
///
/// WindowsではShellの関連付けハンドラを読み取る。非Windowsでは常に空を返す。
/// ファイルに拡張子が無い場合も空を返す。
#[cfg(windows)]
pub fn enumerate_handlers(path: &Path) -> anyhow::Result<Vec<OpenWithHandler>> {
    let _com = ComApartment::initialize()?;
    enumerate_assoc_handlers(path)?
        .iter()
        .map(handler_info)
        .collect()
}

/// 拡張子に関連づく起動候補を列挙する。
///
/// 非Windowsでは常に空を返す。
#[cfg(not(windows))]
pub fn enumerate_handlers(_path: &Path) -> anyhow::Result<Vec<OpenWithHandler>> {
    Ok(Vec::new())
}

/// `key` で指定したハンドラでファイルを開く。
///
/// Windowsではハンドラを再列挙し、識別キーが一致したハンドラへShellの
/// `IDataObject` を渡して起動する。対象ファイルや表示ツリーは変更しない。
#[cfg(windows)]
pub fn open_with_handler(path: &Path, key: &str) -> anyhow::Result<()> {
    use windows::Win32::System::Com::IDataObject;
    use windows::Win32::UI::Shell::{BHID_DataObject, IShellItem, SHCreateItemFromParsingName};
    use windows::core::PCWSTR;

    let _com = ComApartment::initialize()?;
    let path_wide = path_to_wide(path);
    // SAFETY: `path_wide` は呼び出し中有効なNUL終端UTF-16文字列であり、
    // bind contextは不要なのでnullを渡す。
    let shell_item: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(path_wide.as_ptr()), None) }
            .map_err(|error| anyhow::anyhow!("Failed to create Shell item: {error}"))?;
    // SAFETY: `BHID_DataObject` はShellが定義するbind handler IDであり、戻り値型は
    // `IDataObject` に固定している。
    let data_object: IDataObject = unsafe { shell_item.BindToHandler(None, &BHID_DataObject) }
        .map_err(|error| anyhow::anyhow!("Failed to create Shell data object: {error}"))?;

    for handler in enumerate_assoc_handlers(path)? {
        let handler_key = handler_key(&handler)?;
        if handler_key == key {
            // SAFETY: `data_object` はShell itemから取得した有効なIDataObject。
            unsafe { handler.Invoke(&data_object) }.map_err(|error| {
                anyhow::anyhow!("Failed to open with selected application: {error}")
            })?;
            return Ok(());
        }
    }

    anyhow::bail!(
        "Selected open-with handler was not found: {key} ({})",
        path.display()
    )
}

/// `key` で指定したハンドラでファイルを開く。
///
/// 非Windowsでは指定ハンドラ列挙を提供しないため常にエラーを返す。
#[cfg(not(windows))]
pub fn open_with_handler(path: &Path, key: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "Selecting an open-with handler is not supported on this OS: {key} ({})",
        path.display()
    )
}

/// OSの「プログラムから開く」ダイアログへ委譲する。
///
/// WindowsではShellの`openas` verbを使う。非Windowsでは既定アプリ起動へ
/// フォールバックする。
#[cfg(windows)]
pub fn open_with_system_dialog(path: &Path) -> anyhow::Result<()> {
    use anyhow::bail;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    let path_wide = path_to_wide(path);
    // SAFETY: `path_wide` は呼び出し中有効なNUL終端UTF-16文字列であり、
    // 残りの文字列引数には静的なNUL終端文字列またはnullを渡している。
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("openas"),
            PCWSTR(path_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code <= 32 {
        bail!(
            "Failed to open the open-with dialog (ShellExecuteW={code}): {}",
            path.display()
        );
    }
    Ok(())
}

/// OSの「プログラムから開く」ダイアログへ委譲する。
///
/// 非Windowsでは既定アプリ起動へフォールバックする。
#[cfg(not(windows))]
pub fn open_with_system_dialog(path: &Path) -> anyhow::Result<()> {
    crate::open::open_with_default_app(path)
}

#[cfg(windows)]
fn enumerate_assoc_handlers(
    path: &Path,
) -> anyhow::Result<Vec<windows::Win32::UI::Shell::IAssocHandler>> {
    use windows::Win32::UI::Shell::{ASSOC_FILTER_RECOMMENDED, IAssocHandler, SHAssocEnumHandlers};
    use windows::core::PCWSTR;

    let Some(extension) = extension_for_assoc(path) else {
        return Ok(Vec::new());
    };
    // SAFETY: `extension` は呼び出し中有効なNUL終端UTF-16文字列。
    let enum_handlers =
        unsafe { SHAssocEnumHandlers(PCWSTR(extension.as_ptr()), ASSOC_FILTER_RECOMMENDED) }
            .map_err(|error| {
                anyhow::anyhow!("Failed to enumerate open-with candidates: {error}")
            })?;

    let mut handlers = Vec::new();
    loop {
        let mut fetched = 0;
        let mut slot: [Option<IAssocHandler>; 1] = [None];
        // SAFETY: `slot` は1要素分の出力領域で、`fetched` は呼び出し中有効。
        unsafe { enum_handlers.Next(&mut slot, Some(&mut fetched)) }
            .map_err(|error| anyhow::anyhow!("Failed to read open-with candidate: {error}"))?;
        if fetched == 0 {
            break;
        }
        if let Some(handler) = slot[0].take() {
            handlers.push(handler);
        }
    }
    Ok(handlers)
}

#[cfg(windows)]
fn handler_info(
    handler: &windows::Win32::UI::Shell::IAssocHandler,
) -> anyhow::Result<OpenWithHandler> {
    let key = handler_key(handler)?;
    let display_name = handler_display_name(handler)?;
    Ok(OpenWithHandler { display_name, key })
}

#[cfg(windows)]
fn handler_key(handler: &windows::Win32::UI::Shell::IAssocHandler) -> anyhow::Result<String> {
    // SAFETY: Shell APIが返すCoTaskMem文字列を読み取り、直後に解放する。
    unsafe { take_cotaskmem_string(handler.GetName()) }
        .map_err(|error| anyhow::anyhow!("Failed to get open-with handler name: {error}"))
}

#[cfg(windows)]
fn handler_display_name(
    handler: &windows::Win32::UI::Shell::IAssocHandler,
) -> anyhow::Result<String> {
    // SAFETY: Shell APIが返すCoTaskMem文字列を読み取り、直後に解放する。
    unsafe { take_cotaskmem_string(handler.GetUIName()) }
        .map_err(|error| anyhow::anyhow!("Failed to get open-with display name: {error}"))
}

#[cfg(windows)]
fn extension_for_assoc(path: &Path) -> Option<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let extension = path.extension().filter(|extension| !extension.is_empty())?;
    let mut wide = Vec::with_capacity(extension.len() + 2);
    wide.push('.' as u16);
    wide.extend(extension.encode_wide());
    wide.push(0);
    Some(wide)
}

#[cfg(windows)]
fn path_to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
unsafe fn take_cotaskmem_string(
    value: windows::core::Result<windows::core::PWSTR>,
) -> windows::core::Result<String> {
    use std::ffi::c_void;

    use windows::Win32::System::Com::CoTaskMemFree;

    let value = value?;
    let text = if value.0.is_null() {
        String::new()
    } else {
        let mut len = 0;
        // SAFETY: Shell APIはNUL終端UTF-16文字列を返す。
        while unsafe { *value.0.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` は直前にNUL終端まで走査して得た範囲。
        let slice = unsafe { std::slice::from_raw_parts(value.0, len) };
        String::from_utf16_lossy(slice)
    };
    // SAFETY: `value` はShell APIがCoTaskMemで返した文字列。
    unsafe { CoTaskMemFree(Some(value.0.cast::<c_void>())) };
    Ok(text)
}

/// COM apartment初期化のRAIIガード。[`crate::shortcut`]とも共有する。
#[cfg(windows)]
pub(crate) struct ComApartment {
    uninitialize: bool,
}

#[cfg(windows)]
impl ComApartment {
    pub(crate) fn initialize() -> anyhow::Result<Self> {
        use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
        use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

        // SAFETY: COM初期化は現在スレッドに対する呼び出し。成功時のみDropで
        // CoUninitializeする。RPC_E_CHANGED_MODEは既存COMモードを尊重して続行する。
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result == RPC_E_CHANGED_MODE {
            return Ok(Self {
                uninitialize: false,
            });
        }
        result
            .ok()
            .map_err(|error| anyhow::anyhow!("Failed to initialize COM: {error}"))?;
        Ok(Self { uninitialize: true })
    }
}

#[cfg(windows)]
impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.uninitialize {
            // SAFETY: `initialize` で成功したCOM初期化と対応する終了処理。
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn enumerate_handlers_returns_empty_on_non_windows() {
        assert_eq!(
            enumerate_handlers(Path::new("sample.txt")).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn open_with_handler_errors_on_non_windows() {
        assert!(open_with_handler(Path::new("sample.txt"), "handler").is_err());
    }
}
