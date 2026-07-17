//! Windows Shell file clipboard(CF_HDROP + "Preferred DropEffect")。
//!
//! Windowsクリップボード API(`windows`クレート)に触れてよいのはfyler-fsopsだけ
//! (AGENTS.md 絶対ルール3周辺)。この境界を守るため、`read` / `write` の公開
//! シグネチャは std のパス型と [`fyler_core::transfer::DropEffect`] だけを使う。
//!
//! **意図的な例外**: clipboardへ渡すパスは素の絶対パス(`\\?\` を付けない)。
//! Explorerとの相互運用のための意図的な例外であり、`crates/fyler-fsops/src/
//! terminal.rs` のcwdと同じ扱い。実FS操作直前の [`crate::long_path::to_fs`]
//! はここでは適用しない(絶対ルール3: `\\?\` はlong_pathモジュールの1か所だけ)。
//!
//! # レイヤー構成
//! - 純粋関数層(このモジュール冒頭): CF_HDROP payload(`DROPFILES`構造体)と
//!   Preferred DropEffect(DWORD)のencode/decode。cfg非依存で、Linux上でも
//!   unit testできる。
//! - `cfg(windows)`層: `OpenClipboard` / `EmptyClipboard` / `SetClipboardData` /
//!   `GetClipboardData` / `RegisterClipboardFormatW` / `GlobalAlloc` /
//!   `GlobalLock` によるクリップボードI/O。`OpenClipboard`の競合は短いretryで
//!   吸収し、それでも失敗したら明確なエラーを返す。
//! - `cfg(not(windows))`層: 明示的な`Err`を返す(silent fallback禁止)。

use std::path::PathBuf;

use anyhow::{Context, bail};
use fyler_core::transfer::DropEffect;

/// Win32標準クリップボード形式 `CF_HDROP`(WinUser.h)。値は全Windows版で固定。
/// OLE drag(`crate::drag`)のFORMATETCでも同じ値を使う。
#[cfg(windows)]
pub(crate) const CF_HDROP: u32 = 15;

/// `RegisterClipboardFormatW` へ渡すカスタム形式名。Explorerが読み書きする
/// drag effect(copy/move)のヒント。OLE drag(`crate::drag`)とも共有する。
#[cfg(windows)]
pub(crate) const PREFERRED_DROPEFFECT_FORMAT: &str = "Preferred DropEffect";

/// `DROPFILES` 構造体のヘッダ長(バイト)。
/// `DWORD pFiles; POINT pt; BOOL fNC; BOOL fWide;` = 4 + 8 + 4 + 4 = 20。
const DROPFILES_HEADER_LEN: usize = 20;

/// clipboardから読み取ったファイル一覧と取り込み効果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardFiles {
    /// 絶対パス列(clipboardに書かれた順)。
    pub paths: Vec<PathBuf>,
    /// `Preferred DropEffect` 形式が無ければ [`DropEffect::Copy`] を既定とする。
    pub effect: DropEffect,
}

// ---------------------------------------------------------------------
// 純粋関数層(cfg非依存)
// ---------------------------------------------------------------------

/// 絶対パス列を `CF_HDROP`(`DROPFILES`構造体)payloadへencodeする。
///
/// `fWide=1`(UTF-16)固定。各パスはNUL終端し、リスト全体を追加のNULで終端する
/// (double-null-terminated list)。パスがUnicodeとして無効な場合はエラー
/// (silent lossy変換はしない)。
pub fn encode_hdrop(paths: &[PathBuf]) -> anyhow::Result<Vec<u8>> {
    let mut units: Vec<u16> = Vec::new();
    for path in paths {
        let text = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Path is not valid Unicode: {}", path.display()))?;
        units.extend(text.encode_utf16());
        units.push(0);
    }
    units.push(0); // リスト終端の追加NUL(空リストでも二重NULを保証)

    let mut bytes = Vec::with_capacity(DROPFILES_HEADER_LEN + units.len() * 2);
    bytes.extend((DROPFILES_HEADER_LEN as u32).to_le_bytes()); // pFiles
    bytes.extend(0i32.to_le_bytes()); // pt.x
    bytes.extend(0i32.to_le_bytes()); // pt.y
    bytes.extend(0i32.to_le_bytes()); // fNC
    bytes.extend(1i32.to_le_bytes()); // fWide = TRUE
    for unit in units {
        bytes.extend(unit.to_le_bytes());
    }
    Ok(bytes)
}

/// `CF_HDROP` payloadを絶対パス列へdecodeする。
///
/// `fWide=0`(ANSI)のpayloadは明示的にエラーとする(fylerは常にwideで書き、
/// 読み取りもwide専用として扱う。silent fallback禁止)。
pub fn decode_hdrop(bytes: &[u8]) -> anyhow::Result<Vec<PathBuf>> {
    if bytes.len() < DROPFILES_HEADER_LEN {
        bail!(
            "CF_HDROP payload is smaller than the DROPFILES header: {} bytes",
            bytes.len()
        );
    }
    let p_files = u32::from_le_bytes(bytes[0..4].try_into().expect("4-byte slice")) as usize;
    let f_wide = i32::from_le_bytes(bytes[16..20].try_into().expect("4-byte slice"));
    if f_wide == 0 {
        bail!("CF_HDROP payload is ANSI, not UTF-16 (fWide=0); this is not supported");
    }
    if p_files > bytes.len() || (bytes.len() - p_files) % 2 != 0 {
        bail!(
            "CF_HDROP file list offset ({p_files}) is out of range for a {}-byte payload",
            bytes.len()
        );
    }

    let list = &bytes[p_files..];
    let units = list
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));

    let mut paths = Vec::new();
    let mut current = Vec::new();
    for unit in units {
        if unit == 0 {
            if current.is_empty() {
                break; // 二重NUL = リスト終端
            }
            let text = String::from_utf16(&current)
                .context("CF_HDROP file list contains invalid UTF-16")?;
            paths.push(PathBuf::from(text));
            current.clear();
        } else {
            current.push(unit);
        }
    }
    Ok(paths)
}

/// `DropEffect` を `Preferred DropEffect` 形式のDWORD payloadへencodeする
/// (`DROPEFFECT_COPY=1` / `DROPEFFECT_MOVE=2`。Shell共通の慣例値)。
pub fn encode_drop_effect(effect: DropEffect) -> [u8; 4] {
    let value: u32 = match effect {
        DropEffect::Copy => 1,
        DropEffect::Move => 2,
    };
    value.to_le_bytes()
}

/// `Preferred DropEffect` 形式のDWORD payloadを`DropEffect`へdecodeする。
pub fn decode_drop_effect(bytes: &[u8]) -> anyhow::Result<DropEffect> {
    let raw: [u8; 4] = bytes.try_into().map_err(|_| {
        anyhow::anyhow!(
            "Preferred DropEffect payload must be 4 bytes, got {}",
            bytes.len()
        )
    })?;
    match u32::from_le_bytes(raw) {
        1 => Ok(DropEffect::Copy),
        2 => Ok(DropEffect::Move),
        other => bail!("Unsupported DROPEFFECT value: {other}"),
    }
}

// ---------------------------------------------------------------------
// プラットフォーム層
// ---------------------------------------------------------------------

/// clipboardへ絶対パス列とeffectを書き込む(`CF_HDROP` + `Preferred DropEffect`)。
/// 実FSは一切変更しない。
#[cfg(windows)]
pub fn write(paths: &[PathBuf], effect: DropEffect) -> anyhow::Result<()> {
    if paths.is_empty() {
        bail!("Cannot write an empty file list to the clipboard");
    }
    let hdrop_payload = encode_hdrop(paths)?;
    let effect_payload = encode_drop_effect(effect);
    let effect_format = win::register_preferred_dropeffect_format()?;

    let _clipboard = win::ClipboardGuard::open_with_retry()?;
    // SAFETY: 直前に`ClipboardGuard`でclipboardを開いている。
    unsafe { windows::Win32::System::DataExchange::EmptyClipboard() }
        .map_err(|error| anyhow::anyhow!("Failed to clear the clipboard: {error}"))?;
    win::set_clipboard_global(CF_HDROP, &hdrop_payload)?;
    win::set_clipboard_global(effect_format, &effect_payload)?;
    Ok(())
}

/// clipboardから `CF_HDROP` + `Preferred DropEffect` を読み取る。
///
/// `CF_HDROP`形式がない、またはfile一覧が空なら `Ok(None)`
/// (呼び出し側は「貼り付ける物がない」として扱う)。`Preferred DropEffect`
/// 形式が無ければ [`DropEffect::Copy`] を既定とする(通常のExplorer Ctrl+Cと
/// 同じ既定効果)。
#[cfg(windows)]
pub fn read() -> anyhow::Result<Option<ClipboardFiles>> {
    let _clipboard = win::ClipboardGuard::open_with_retry()?;
    // SAFETY: 直前にclipboardを開いている。
    if unsafe { windows::Win32::System::DataExchange::IsClipboardFormatAvailable(CF_HDROP) }
        .is_err()
    {
        return Ok(None);
    }
    let hdrop_bytes = win::get_clipboard_global(CF_HDROP)?;
    let paths = decode_hdrop(&hdrop_bytes)?;
    if paths.is_empty() {
        return Ok(None);
    }
    let effect_format = win::register_preferred_dropeffect_format()?;
    // SAFETY: 直前にclipboardを開いている。
    let effect = if unsafe {
        windows::Win32::System::DataExchange::IsClipboardFormatAvailable(effect_format)
    }
    .is_ok()
    {
        decode_drop_effect(&win::get_clipboard_global(effect_format)?)?
    } else {
        DropEffect::Copy
    };
    Ok(Some(ClipboardFiles { paths, effect }))
}

#[cfg(windows)]
pub(crate) mod win {
    use std::time::Duration;

    use anyhow::anyhow;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };
    use windows::core::{Free, PCWSTR};

    const OPEN_ATTEMPTS: u32 = 5;
    const OPEN_RETRY_DELAY: Duration = Duration::from_millis(20);

    /// `OpenClipboard` を短いretryで確保し、Dropで必ず`CloseClipboard`する。
    pub(super) struct ClipboardGuard;

    impl ClipboardGuard {
        pub(super) fn open_with_retry() -> anyhow::Result<Self> {
            let mut last_error = None;
            for attempt in 0..OPEN_ATTEMPTS {
                if attempt > 0 {
                    std::thread::sleep(OPEN_RETRY_DELAY);
                }
                // SAFETY: `None`はカレントタスクにclipboardを関連付ける(特定windowを
                // 所有者にしない)。成功したらこのguardのDropで必ずCloseClipboardする。
                match unsafe { OpenClipboard(Option::<HWND>::None) } {
                    Ok(()) => return Ok(Self),
                    Err(error) => last_error = Some(error),
                }
            }
            Err(anyhow!(
                "Failed to open the clipboard after {OPEN_ATTEMPTS} attempts (another process may be holding it){}",
                last_error
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default()
            ))
        }
    }

    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            // SAFETY: このguardは`OpenClipboard`成功時のみ構築される。
            let _ = unsafe { CloseClipboard() };
        }
    }

    fn to_wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn register_preferred_dropeffect_format() -> anyhow::Result<u32> {
        register_clipboard_format(super::PREFERRED_DROPEFFECT_FORMAT)
    }

    /// 名前付きclipboard形式を登録してformat値を返す(OLE dragのFORMATETCとも
    /// 同じatom空間を共有する)。
    pub(crate) fn register_clipboard_format(name: &str) -> anyhow::Result<u32> {
        let wide = to_wide(name);
        // SAFETY: `wide`はこの呼び出し中有効なNUL終端UTF-16文字列。
        let format = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
        if format == 0 {
            anyhow::bail!("Failed to register the \"{name}\" clipboard format");
        }
        Ok(format)
    }

    /// `payload` をGMEM_MOVEABLEハンドルへ確保・コピーして返す。所有権は呼び出し元
    /// (clipboard/OLEへ渡すか、失敗経路で`Free`する責務も呼び出し元)。
    pub(crate) fn alloc_hglobal_bytes(payload: &[u8]) -> anyhow::Result<HGLOBAL> {
        // SAFETY: サイズ0確保を避けるためmax(1)。成功時のhandleは呼び出し元が所有する。
        let handle: HGLOBAL = unsafe { GlobalAlloc(GMEM_MOVEABLE, payload.len().max(1)) }
            .map_err(|error| anyhow!("Failed to allocate global memory: {error}"))?;
        let write_result: anyhow::Result<()> = (|| {
            // SAFETY: 直前に`GlobalAlloc`で確保した有効なhandle。
            let ptr = unsafe { GlobalLock(handle) };
            if ptr.is_null() {
                anyhow::bail!("Failed to lock global memory for writing");
            }
            // SAFETY: `ptr`は`GlobalAlloc(payload.len())`以上確保済みの領域。
            unsafe {
                std::ptr::copy_nonoverlapping(payload.as_ptr(), ptr.cast::<u8>(), payload.len());
            }
            // SAFETY: 直前にlockした同じhandle。最終unlockはNO_ERRORでも偽を返し得る
            // ため戻り値は無視する(MSDN `GlobalUnlock` の仕様どおり)。
            let _ = unsafe { GlobalUnlock(handle) };
            Ok(())
        })();
        if let Err(error) = write_result {
            let mut handle = handle;
            // SAFETY: まだ誰にも渡していないため所有権はこちらにある。
            unsafe { handle.free() };
            return Err(error);
        }
        Ok(handle)
    }

    /// HGLOBALの内容を丸ごとコピーして返す。handleの所有権は移らない。
    ///
    /// # Safety
    /// `hglobal` は有効なグローバルメモリハンドルであること。
    pub(crate) unsafe fn read_hglobal_bytes(hglobal: HGLOBAL) -> anyhow::Result<Vec<u8>> {
        // SAFETY: 呼び出し元が有効性を保証するhandle。
        let size = unsafe { GlobalSize(hglobal) };
        if size == 0 {
            return Ok(Vec::new());
        }
        // SAFETY: 同じ有効なhandle。
        let ptr = unsafe { GlobalLock(hglobal) };
        if ptr.is_null() {
            anyhow::bail!("Failed to lock global memory for reading");
        }
        // SAFETY: `ptr`は`GlobalSize`が報告した`size`バイト以上有効。
        let bytes =
            unsafe { std::slice::from_raw_parts(ptr.cast::<u8>() as *const u8, size) }.to_vec();
        // SAFETY: 直前にlockした同じhandle。戻り値は無視する(上記と同じ理由)。
        let _ = unsafe { GlobalUnlock(hglobal) };
        Ok(bytes)
    }

    /// `payload` をGMEM_MOVEABLEハンドルへ確保・コピーし、`SetClipboardData` で
    /// clipboardへ渡す。成功後の所有権はclipboardが持つため`GlobalFree`しない。
    /// 失敗経路では確保した領域を必ず解放する。
    pub(super) fn set_clipboard_global(format: u32, payload: &[u8]) -> anyhow::Result<()> {
        // SAFETY: 呼び出し元がclipboardを開いた状態でのみ呼ぶ。
        let handle = alloc_hglobal_bytes(payload)?;
        // SAFETY: `handle`はロック解除済みでSetClipboardDataへ渡す前提を満たす。
        let set_result = unsafe { SetClipboardData(format, Some(HANDLE(handle.0))) };
        if let Err(error) = set_result {
            let mut handle = handle;
            // SAFETY: `SetClipboardData`が失敗した場合、所有権は移らないため
            // こちらで解放する。
            unsafe { handle.free() };
            return Err(anyhow!("Failed to set clipboard data: {error}"));
        }
        Ok(())
    }

    /// `GetClipboardData` で取得したhandleの内容を丸ごとコピーして返す。
    /// handleの所有権はclipboardにあるため`GlobalFree`しない
    /// (`CloseClipboard`まで有効、`ClipboardGuard`のDropが管理範囲)。
    pub(super) fn get_clipboard_global(format: u32) -> anyhow::Result<Vec<u8>> {
        // SAFETY: 呼び出し元がclipboardを開いた状態でのみ呼ぶ。
        let handle = unsafe { GetClipboardData(format) }
            .map_err(|error| anyhow!("Failed to read clipboard data: {error}"))?;
        // SAFETY: `GetClipboardData`が返した有効なhandle。
        unsafe { read_hglobal_bytes(HGLOBAL(handle.0)) }
    }

}

#[cfg(not(windows))]
pub fn write(_paths: &[PathBuf], _effect: DropEffect) -> anyhow::Result<()> {
    bail!("Windows Shell clipboard file operations are not supported on this platform")
}

#[cfg(not(windows))]
pub fn read() -> anyhow::Result<Option<ClipboardFiles>> {
    bail!("Windows Shell clipboard file operations are not supported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdrop_round_trips_ascii_paths() {
        let paths = vec![
            PathBuf::from("C:/Users/alice/a.txt"),
            PathBuf::from("C:/Users/alice/b.txt"),
        ];
        let payload = encode_hdrop(&paths).unwrap();
        assert_eq!(decode_hdrop(&payload).unwrap(), paths);
    }

    #[test]
    fn hdrop_round_trips_japanese_spaces_and_long_paths() {
        let long_component = "a".repeat(200);
        let paths = vec![
            PathBuf::from("C:/ユーザー/資料/日本語 ファイル名.txt"),
            PathBuf::from("D:/space folder/with space.txt"),
            PathBuf::from(format!("C:/deep/{long_component}/{long_component}.dat")),
        ];
        let payload = encode_hdrop(&paths).unwrap();
        assert_eq!(decode_hdrop(&payload).unwrap(), paths);
    }

    #[test]
    fn hdrop_round_trips_single_and_empty_lists() {
        assert_eq!(
            decode_hdrop(&encode_hdrop(&[PathBuf::from("C:/only.txt")]).unwrap()).unwrap(),
            vec![PathBuf::from("C:/only.txt")]
        );
        assert_eq!(
            decode_hdrop(&encode_hdrop(&[]).unwrap()).unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn hdrop_decode_rejects_too_short_payload() {
        assert!(decode_hdrop(&[0u8; 4]).is_err());
    }

    #[test]
    fn hdrop_decode_rejects_ansi_payload() {
        let mut payload = encode_hdrop(&[PathBuf::from("C:/a.txt")]).unwrap();
        payload[16..20].copy_from_slice(&0i32.to_le_bytes()); // fWide = FALSE
        assert!(decode_hdrop(&payload).is_err());
    }

    #[test]
    fn hdrop_decode_rejects_out_of_range_offset() {
        let mut payload = encode_hdrop(&[PathBuf::from("C:/a.txt")]).unwrap();
        payload[0..4].copy_from_slice(&9_999u32.to_le_bytes());
        assert!(decode_hdrop(&payload).is_err());
    }

    #[test]
    fn drop_effect_round_trips_copy_and_move() {
        for effect in [DropEffect::Copy, DropEffect::Move] {
            let payload = encode_drop_effect(effect);
            assert_eq!(decode_drop_effect(&payload).unwrap(), effect);
        }
    }

    #[test]
    fn drop_effect_decode_rejects_bad_length_and_unknown_value() {
        assert!(decode_drop_effect(&[1, 0, 0]).is_err());
        assert!(decode_drop_effect(&5u32.to_le_bytes()).is_err());
        assert!(decode_drop_effect(&0u32.to_le_bytes()).is_err());
    }

    #[test]
    #[cfg(not(windows))]
    fn non_windows_read_write_are_explicit_errors() {
        assert!(write(&[PathBuf::from("/tmp/a")], DropEffect::Copy).is_err());
        assert!(read().is_err());
    }
}
