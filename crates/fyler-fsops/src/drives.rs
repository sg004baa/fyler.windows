//! 利用可能なドライブの列挙(Windows専用。他OSでは空)。

use std::path::PathBuf;

/// 利用可能なドライブのルートパスを列挙する。
///
/// Windowsでは`GetLogicalDrives`のビットマスクから`C:\`形式のパスを
/// 生成して昇順で返す。Windows以外では空を返す。
#[cfg(windows)]
pub fn list_drives() -> Vec<PathBuf> {
    // SAFETY: 引数を取らず、呼び出し元が保持すべきポインタもないWin32 APIである。
    let mask = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    drive_paths_from_mask(mask)
}

/// 利用可能なドライブのルートパスを列挙する。
///
/// Windowsでは`GetLogicalDrives`のビットマスクから`C:\`形式のパスを
/// 生成して昇順で返す。Windows以外では空を返す。
#[cfg(not(windows))]
pub fn list_drives() -> Vec<PathBuf> {
    drive_paths_from_mask(0)
}

fn drive_paths_from_mask(mask: u32) -> Vec<PathBuf> {
    (0..26)
        .filter(|index| mask & (1 << index) != 0)
        .map(|index| {
            let letter = char::from(b'A' + index as u8);
            PathBuf::from(format!("{letter}:\\"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_mask_has_no_drives() {
        assert!(drive_paths_from_mask(0).is_empty());
    }

    #[test]
    fn mask_bits_map_to_sorted_drive_roots() {
        assert_eq!(
            drive_paths_from_mask(0b1100),
            [PathBuf::from("C:\\"), PathBuf::from("D:\\")]
        );
    }
}
