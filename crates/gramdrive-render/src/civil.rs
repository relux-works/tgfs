//! Deterministic civil-time conversion from persisted IANA zones (SYNC-031).
//!
//! One instant-to-calendar computation, shared by every consumer that must
//! agree on where a message falls: the Markdown renderer groups messages by
//! civil day with it, and the incremental render planner (the engine) maps a
//! message's send instant to the calendar month whose transcript it belongs to
//! with the *same* function. A separate copy on either side could drift a
//! message near a month boundary into a partition the renderer would never
//! group it under; sharing this one function makes that disagreement
//! unrepresentable.
//!
//! Like the rest of the renderer it is a pure transform of its input — no
//! locale and no clock. IANA transition data is required because the persisted
//! account policy is a zone name rather than a fixed offset.

use jiff::{Timestamp, tz::TimeZone};

/// A civil date and wall-clock time, already resolved into a fixed UTC offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Civil {
    /// Proleptic Gregorian year.
    pub(crate) year: i64,
    /// Month, 1–12.
    pub(crate) month: u32,
    /// Day of month, 1–31.
    pub(crate) day: u32,
    /// Hour, 0–23.
    pub(crate) hour: u32,
    /// Minute, 0–59.
    pub(crate) minute: u32,
    /// Second, 0–59.
    pub(crate) second: u32,
}

impl Civil {
    /// Converts a millisecond instant to civil time in a fixed offset (seconds
    /// east of UTC). Uses floor division throughout, so a pre-1970 instant
    /// resolves correctly rather than truncating toward zero.
    pub(crate) fn from_millis(instant_ms: i64, offset_seconds: i32) -> Self {
        let local_ms = instant_ms + i64::from(offset_seconds) * 1000;
        let total_seconds = local_ms.div_euclid(1000);
        let days = total_seconds.div_euclid(86_400);
        let second_of_day = total_seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: (second_of_day / 3_600) as u32,
            minute: ((second_of_day % 3_600) / 60) as u32,
            second: (second_of_day % 60) as u32,
        }
    }

    /// Converts a Telegram millisecond instant through an IANA timezone.
    ///
    /// TDLib dates are signed 32-bit seconds and fit Jiff's civil range. The
    /// UTC fallback keeps corrupt out-of-contract rows non-panicking so repair
    /// can still inspect them.
    pub(crate) fn in_timezone(instant_ms: i64, timezone: &TimeZone) -> Self {
        let Ok(timestamp) = Timestamp::from_millisecond(instant_ms) else {
            return Self::from_millis(instant_ms, 0);
        };
        let datetime = timestamp.to_zoned(timezone.clone()).datetime();
        Self {
            year: i64::from(datetime.year()),
            month: u32::try_from(datetime.month()).unwrap_or_default(),
            day: u32::try_from(datetime.day()).unwrap_or_default(),
            hour: u32::try_from(datetime.hour()).unwrap_or_default(),
            minute: u32::try_from(datetime.minute()).unwrap_or_default(),
            second: u32::try_from(datetime.second()).unwrap_or_default(),
        }
    }

    /// `YYYY-MM-DD`.
    pub(crate) fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// `HH:MM:SS`.
    pub(crate) fn time(&self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }

    /// `YYYY-MM-DD HH:MM:SS`.
    pub(crate) fn date_time(&self) -> String {
        format!("{} {}", self.date(), self.time())
    }
}

/// The civil `(year, month)` a millisecond instant falls in, in a fixed offset
/// east of UTC (negative for west) — month is 1–12.
///
/// This is the partition key of the monthly Markdown transcript (SYNC-031,
/// DOM-023): the render planner maps a message's `sent_at_ms` to the transcript
/// it must regenerate with this exact function, so the month it plans is always
/// the month the renderer would group the message under. The `year` is the full
/// proleptic Gregorian year as an `i64`; the caller narrows it to the partition
/// type's range.
pub fn year_month(instant_ms: i64, offset_seconds: i32) -> (i64, u32) {
    let civil = Civil::from_millis(instant_ms, offset_seconds);
    (civil.year, civil.month)
}

/// The civil `(year, month)` a millisecond instant falls in for an IANA zone.
pub fn year_month_in_timezone(instant_ms: i64, timezone: &TimeZone) -> (i64, u32) {
    let civil = Civil::in_timezone(instant_ms, timezone);
    (civil.year, civil.month)
}

/// Cross-platform-safe account-local attachment timestamp prefix.
///
/// The result is always `YYYY-MM-DD HH-mm-ss`: source timestamps stay absolute
/// while only their filename presentation follows the persisted account zone.
pub fn filename_timestamp_in_timezone(instant_ms: i64, timezone: &TimeZone) -> String {
    let civil = Civil::in_timezone(instant_ms, timezone);
    format!(
        "{} {:02}-{:02}-{:02}",
        civil.date(),
        civil.hour,
        civil.minute,
        civil.second
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian `(year, month, day)`.
///
/// Howard Hinnant's `civil_from_days` (public domain, `chrono`-compatible),
/// which is exact for the full `i64` day range and needs no lookup table.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let month_position = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32; // [1, 31]
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_time_matches_known_instants() {
        // 1_700_000_000_000 ms = 2023-11-14T22:13:20Z (a fixed reference).
        let utc = Civil::from_millis(1_700_000_000_000, 0);
        assert_eq!(utc.date(), "2023-11-14");
        assert_eq!(utc.time(), "22:13:20");

        // The Unix epoch itself.
        let epoch = Civil::from_millis(0, 0);
        assert_eq!(epoch.date_time(), "1970-01-01 00:00:00");
    }

    #[test]
    fn civil_time_applies_offset_and_crosses_the_day_boundary() {
        // +03:00 pushes 22:13:20Z into the next civil day.
        let plus3 = Civil::from_millis(1_700_000_000_000, 3 * 3_600);
        assert_eq!(plus3.date(), "2023-11-15");
        assert_eq!(plus3.time(), "01:13:20");

        // A negative offset can pull an instant back across midnight.
        let minus5 = Civil::from_millis(1_700_000_000_000, -5 * 3_600);
        assert_eq!(minus5.date(), "2023-11-14");
        assert_eq!(minus5.time(), "17:13:20");
    }

    #[test]
    fn civil_time_is_correct_before_the_epoch() {
        // -1 ms is the last second of 1969 under floor division.
        let before = Civil::from_millis(-1, 0);
        assert_eq!(before.date_time(), "1969-12-31 23:59:59");
    }

    #[test]
    fn year_month_matches_the_day_grouping() {
        // Same instant the renderer groups: the planner must see the same month.
        assert_eq!(year_month(1_700_000_000_000, 0), (2023, 11));
        // The offset moves the civil month exactly as it moves the civil day: at
        // UTC this instant is 2023-11-30 23:59:59, and +03:00 tips it into
        // December — the transcript the planner picks follows the offset.
        let last_of_november = 1_701_386_399_000; // 2023-11-30T23:59:59Z
        assert_eq!(year_month(last_of_november, 0), (2023, 11));
        assert_eq!(year_month(last_of_november, 3 * 3_600), (2023, 12));
    }

    #[test]
    fn year_month_resolves_before_the_epoch() {
        // Floor division keeps a pre-1970 instant in its true month.
        assert_eq!(year_month(-1, 0), (1969, 12));
    }

    #[test]
    fn filename_timestamp_is_safe_and_uses_the_named_zone() {
        let timezone = TimeZone::get("Asia/Tbilisi").expect("bundled timezone");
        assert_eq!(
            filename_timestamp_in_timezone(1_700_000_000_000, &timezone),
            "2023-11-15 02-13-20"
        );
    }
}
