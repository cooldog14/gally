//! EXIF capture time: `DateTimeOriginal`, converted to Unix milliseconds.
//!
//! Photos sort by when they were taken, not when they were uploaded. Files
//! without a usable timestamp return `None`; callers fall back to upload time.

use std::path::Path;

/// Capture time of the photo at `path`, in Unix milliseconds.
pub(crate) fn taken_millis(path: &Path) -> Option<i64> {
    let file = std::fs::File::open(path).ok()?;
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::BufReader::new(&file))
        .ok()?;
    let field = exif.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)?;
    let exif::Value::Ascii(values) = &field.value else {
        return None;
    };
    let mut dt = exif::DateTime::from_ascii(values.first()?).ok()?;
    // Sub-second and UTC-offset tags are optional refinements; malformed
    // values leave the corresponding component unset.
    if let Some(f) = exif.get_field(exif::Tag::SubSecTimeOriginal, exif::In::PRIMARY)
        && let exif::Value::Ascii(v) = &f.value
        && let Some(raw) = v.first()
    {
        let _ = dt.parse_subsec(raw);
    }
    if let Some(f) = exif.get_field(exif::Tag::OffsetTimeOriginal, exif::In::PRIMARY)
        && let exif::Value::Ascii(v) = &f.value
        && let Some(raw) = v.first()
    {
        let _ = dt.parse_offset(raw);
    }
    datetime_millis(&dt)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Unix milliseconds for a parsed EXIF timestamp. Rejects impossible dates
/// and times; `offset` (minutes east of UTC) is applied when present, and a
/// bare local time is treated as UTC.
fn datetime_millis(dt: &exif::DateTime) -> Option<i64> {
    let (year, month, day) = (i64::from(dt.year), i64::from(dt.month), i64::from(dt.day));
    if year < 1 || !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let (hour, minute, second) = (
        i64::from(dt.hour),
        i64::from(dt.minute),
        i64::from(dt.second),
    );
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut ms =
        (days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000;
    ms += i64::from(dt.nanosecond.unwrap_or(0) / 1_000_000);
    if let Some(minutes) = dt.offset {
        ms -= i64::from(minutes) * 60_000;
    }
    Some(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_time_original_to_millis() {
        let dt = exif::DateTime::from_ascii(b"2020:01:02 03:04:05").unwrap();
        assert_eq!(datetime_millis(&dt), Some(1_577_934_245_000));
    }

    #[test]
    fn subsec_and_offset_apply() {
        let mut dt = exif::DateTime::from_ascii(b"2020:01:02 03:04:05").unwrap();
        dt.parse_subsec(b"250").unwrap();
        dt.parse_offset(b"+02:00").unwrap();
        assert_eq!(datetime_millis(&dt), Some(1_577_927_045_250));
    }

    #[test]
    fn impossible_values_are_rejected() {
        for raw in [
            b"2020:13:02 03:04:05".as_slice(),
            b"2020:02:30 03:04:05",
            b"2020:01:02 24:04:05",
            b"0000:01:02 03:04:05",
        ] {
            let dt = exif::DateTime::from_ascii(raw).unwrap();
            assert_eq!(datetime_millis(&dt), None, "{}", dt);
        }
    }
}
