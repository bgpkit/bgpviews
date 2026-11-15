//! Calendar arithmetic without a date library.
//!
//! The archive names files by UTC minute. That needs one conversion, not a
//! dependency, so it lives here and both the collector and the replay use it.

/// Convert seconds since the Unix epoch to `(year, month, day, hour, minute)` in UTC.
#[must_use]
pub fn civil_from_unix(seconds: i64) -> (i64, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;

    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_epochs_convert_to_known_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0));
        assert_eq!(civil_from_unix(1_000_000_000), (2001, 9, 9, 1, 46));
        assert_eq!(civil_from_unix(1_794_702_169), (2026, 11, 15, 0, 22));
        assert_eq!(
            civil_from_unix(1_794_702_169 + 86_400),
            (2026, 11, 16, 0, 22)
        );
    }
}
