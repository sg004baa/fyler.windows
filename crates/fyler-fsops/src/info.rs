//! モードラインへ表示するファイルメタデータの収集。

use std::fs::Metadata;
use std::path::Path;

use anyhow::Context;
use fyler_core::fileinfo::FileInfo;

/// パスの表示用メタデータを収集する。
///
/// プレースホルダ判定を最初に属性だけで行い、ファイル内容は読まない。
/// メタデータはリンク先へ潜らず取得し、ディレクトリのサイズは返さない。
pub fn file_info(path: &Path) -> anyhow::Result<FileInfo> {
    let is_placeholder = crate::onedrive::is_cloud_placeholder(path)?;
    let fs_path = crate::long_path::to_fs(path);
    let metadata = std::fs::symlink_metadata(&fs_path)
        .with_context(|| format!("表示用メタデータを取得できません: {}", path.display()))?;

    Ok(FileInfo {
        size: (!metadata.is_dir()).then_some(metadata.len()),
        modified: format_modified(&metadata),
        is_placeholder,
    })
}

#[cfg(windows)]
fn format_modified(metadata: &Metadata) -> Option<String> {
    use std::os::windows::fs::MetadataExt;

    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

    let last_write_time = metadata.last_write_time();
    let file_time = FILETIME {
        dwLowDateTime: last_write_time as u32,
        dwHighDateTime: (last_write_time >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&file_time, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }

    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    ))
}

#[cfg(not(windows))]
fn format_modified(metadata: &Metadata) -> Option<String> {
    use std::time::UNIX_EPOCH;

    let modified = metadata.modified().ok()?;
    let unix_seconds = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).ok()?,
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs()).ok()?;
            -seconds - i64::from(duration.subsec_nanos() != 0)
        }
    };
    let days = unix_seconds.div_euclid(86_400);
    let seconds_of_day = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;

    Some(format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}"
    ))
}

#[cfg(not(windows))]
fn civil_from_days(days_since_unix_epoch: i64) -> (i64, i64, i64) {
    // Howard Hinnantのcivil_from_days。入力は1970-01-01からの日数。
    let days = days_since_unix_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn ordinary_file_has_size_and_modified_time_without_placeholder() {
        let root = tempdir().unwrap();
        let path = root.path().join("ordinary.txt");
        fs::write(&path, b"content").unwrap();

        let info = file_info(&path).unwrap();

        assert_eq!(info.size, Some(7));
        assert!(info.modified.is_some());
        assert!(!info.is_placeholder);
    }

    #[test]
    fn directory_has_no_size() {
        let root = tempdir().unwrap();
        let path = root.path().join("directory");
        fs::create_dir(&path).unwrap();

        let info = file_info(&path).unwrap();

        assert_eq!(info.size, None);
        assert!(info.modified.is_some());
        assert!(!info.is_placeholder);
    }
}
