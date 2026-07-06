//! モードラインへ表示するファイル更新日時の整形。

use std::time::SystemTime;

/// スキャン時に取得した更新日時をローカル時刻の表示文字列へ整形する。
#[cfg(windows)]
pub fn format_modified_time(modified: SystemTime) -> Option<String> {
    use std::time::UNIX_EPOCH;

    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

    const WINDOWS_EPOCH_OFFSET_TICKS: i64 = 116_444_736_000_000_000;
    const TICKS_PER_SECOND: i64 = 10_000_000;

    let duration_ticks = |duration: std::time::Duration| {
        let seconds = i64::try_from(duration.as_secs()).ok()?;
        seconds
            .checked_mul(TICKS_PER_SECOND)?
            .checked_add(i64::from(duration.subsec_nanos() / 100))
    };
    let ticks = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => WINDOWS_EPOCH_OFFSET_TICKS.checked_add(duration_ticks(duration)?)?,
        Err(error) => WINDOWS_EPOCH_OFFSET_TICKS.checked_sub(duration_ticks(error.duration())?)?,
    };
    let last_write_time = u64::try_from(ticks).ok()?;
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

/// スキャン時に取得した更新日時をUTCの表示文字列へ整形する。
#[cfg(not(windows))]
pub fn format_modified_time(modified: SystemTime) -> Option<String> {
    use std::time::UNIX_EPOCH;

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
    use super::*;

    #[test]
    fn current_system_time_can_be_formatted() {
        assert!(format_modified_time(SystemTime::now()).is_some());
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_epoch_uses_existing_output_format() {
        assert_eq!(
            format_modified_time(std::time::UNIX_EPOCH).as_deref(),
            Some("1970-01-01 00:00")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn time_before_unix_epoch_is_supported() {
        let modified = std::time::UNIX_EPOCH - std::time::Duration::from_secs(60);
        assert_eq!(
            format_modified_time(modified).as_deref(),
            Some("1969-12-31 23:59")
        );
    }
}
