//! Formatting of git timestamps for display.

use std::time::{SystemTime, UNIX_EPOCH};

use gitez_git::Timestamp;

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const TWO_DAYS: i64 = 2 * DAY;
const WEEK: i64 = 7 * DAY;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Seconds since the Unix epoch, now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

/// A short description of when `time` was, relative to `now` (seconds since
/// the epoch): "12 min ago", "yesterday", "Oct 10", "Oct 10, 2024".
///
/// Calendar dates are in the timestamp's own UTC offset, as `git log` shows
/// them.
pub fn relative(time: Timestamp, now: i64) -> String {
    let elapsed = now - time.seconds;
    match elapsed {
        // A little clock skew between machines still reads as "now".
        ..MINUTE if elapsed >= -MINUTE => "just now".to_owned(),
        MINUTE..HOUR => format!("{} min ago", elapsed / MINUTE),
        HOUR..DAY => format!("{} h ago", elapsed / HOUR),
        DAY..TWO_DAYS => "yesterday".to_owned(),
        TWO_DAYS..WEEK => format!("{} days ago", elapsed / DAY),
        _ => {
            let (year, month, day) = civil_date(time.seconds, time.offset);
            let (this_year, _, _) = civil_date(now, time.offset);
            let month = MONTHS[month as usize - 1];
            if year == this_year {
                format!("{month} {day}")
            } else {
                format!("{month} {day}, {year}")
            }
        }
    }
}

/// The full date and time of `time` in its own UTC offset, such as
/// "Oct 14, 2026 · 16:42".
pub fn absolute(time: Timestamp) -> String {
    let (year, month, day) = civil_date(time.seconds, time.offset);
    let seconds_of_day = (time.seconds + i64::from(time.offset)).rem_euclid(DAY);
    let (hour, minute) = (seconds_of_day / HOUR, seconds_of_day % HOUR / MINUTE);
    let month = MONTHS[month as usize - 1];
    format!("{month} {day}, {year} · {hour:02}:{minute:02}")
}

/// The calendar date `(year, month 1–12, day 1–31)` of `seconds` since the
/// epoch at UTC offset `offset`.
fn civil_date(seconds: i64, offset: i32) -> (i64, u32, u32) {
    // Howard Hinnant's `civil_from_days`, for the proleptic Gregorian calendar.
    let days = (seconds + i64::from(offset)).div_euclid(DAY) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-23 12:00:00 UTC.
    const NOW: i64 = 1_792_756_800;

    fn ago(seconds: i64) -> String {
        relative(
            Timestamp {
                seconds: NOW - seconds,
                offset: 0,
            },
            NOW,
        )
    }

    #[test]
    fn recent_times_are_relative() {
        assert_eq!(ago(-30), "just now");
        assert_eq!(ago(59), "just now");
        assert_eq!(ago(12 * MINUTE), "12 min ago");
        assert_eq!(ago(5 * HOUR + 59 * MINUTE), "5 h ago");
        assert_eq!(ago(DAY), "yesterday");
        assert_eq!(ago(2 * DAY), "2 days ago");
        assert_eq!(ago(6 * DAY + 23 * HOUR), "6 days ago");
    }

    #[test]
    fn absolute_times_use_the_timestamp_offset() {
        let time = Timestamp {
            seconds: NOW,
            offset: 2 * 3600 + 30 * 60,
        };
        assert_eq!(absolute(time), "Oct 23, 2026 · 14:30");
        let time = Timestamp {
            seconds: NOW,
            offset: -13 * 3600,
        };
        assert_eq!(absolute(time), "Oct 22, 2026 · 23:00");
    }

    #[test]
    fn older_times_are_dates() {
        assert_eq!(ago(13 * DAY), "Oct 10");
        assert_eq!(ago(365 * DAY), "Oct 23, 2025");
        assert_eq!(ago(-3 * DAY), "Oct 26");
    }

    #[test]
    fn dates_use_the_timestamp_offset() {
        // 2026-01-01 00:30 at UTC+1 is still 2025-12-31 in UTC.
        let seconds = 1_767_223_800;
        assert_eq!(civil_date(seconds, 0), (2025, 12, 31));
        assert_eq!(civil_date(seconds, 3600), (2026, 1, 1));
    }

    #[test]
    fn civil_dates_match_known_days() {
        assert_eq!(civil_date(0, 0), (1970, 1, 1));
        assert_eq!(civil_date(951_782_400, 0), (2000, 2, 29));
        assert_eq!(civil_date(NOW, 0), (2026, 10, 23));
        assert_eq!(civil_date(-DAY, 0), (1969, 12, 31));
    }
}
