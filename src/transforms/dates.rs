//! Date transforms ported from Boop's `DateToTimestamp.js` and `DateToUTC.js`.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};

use super::{Scope, Transform, TransformResult};

pub const TRANSFORMS: &[Transform] = &[
    Transform {
        name: "Date to Timestamp",
        description: "Parse a date/time to Unix seconds; naive inputs are treated as UTC, sub-second precision is dropped, and unparseable input is an error.",
        tags: "date,time,calendar,unix,timestamp",
        scope: Scope::SelectionOrFullText,
        apply: date_to_timestamp,
    },
    Transform {
        name: "Date to UTC",
        description: "Parse a date/time or Unix seconds and format it as a GMT string; naive inputs are treated as UTC.",
        tags: "date,time,calendar,unix,timestamp",
        scope: Scope::SelectionOrFullText,
        apply: date_to_utc,
    },
];

/// Footprint of JS `Date#toUTCString()`.
///
/// `GMT` is written literally because chrono has no zone abbreviation that
/// renders exactly `GMT` for `Utc` (chrono's `%Z` is `UTC`).
const UTC_FORMAT: &str = "%a, %d %b %Y %H:%M:%S GMT";

/// Extra formats tried after RFC 3339 and RFC 2822.
///
/// Deviations from `Date.parse`, which accepts a much larger legacy grammar and
/// resolves naive date-times in the browser's local zone:
///
/// - naive date-times are interpreted as UTC (no timezone database is available
///   in this build);
/// - `m/d/Y` is tried before `d/m/Y` so ambiguous inputs like `05/06/2020`
///   resolve the way JS does, but the `d/m/Y` fallback is more lenient than the
///   ECMAScript grammar, which only defines month-first slashed dates;
/// - the `%.f` variants accept fractional seconds on naive inputs, which JS
///   also accepts.
const NAIVE_FORMATS: &[&str] = &[
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%dT%H:%M:%S",
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y-%m-%dT%H:%M:%S%.f",
    "%Y-%m-%d",
    "%m/%d/%Y",
    "%d/%m/%Y",
];

/// Approximates `Date.parse` and returns the instant in UTC.
fn parse_datetime(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return Some(parsed.with_timezone(&Utc));
    }
    if let Ok(parsed) = DateTime::parse_from_rfc2822(text) {
        return Some(parsed.with_timezone(&Utc));
    }

    for format in NAIVE_FORMATS {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, format) {
            return Some(Utc.from_utc_datetime(&naive));
        }
        if let Ok(date) = NaiveDate::parse_from_str(text, format) {
            let naive = date.and_hms_opt(0, 0, 0)?;
            return Some(Utc.from_utc_datetime(&naive));
        }
    }

    None
}

/// Boop's `input.text = parsedDate / 1000` fallback: a bare number is unix
/// seconds. JS truncates `parseInt(string * 1000)` toward zero; a saturating
/// cast plus `from_timestamp_millis` rejects out-of-range values.
fn parse_unix_seconds(text: &str) -> Option<DateTime<Utc>> {
    let seconds: f64 = text.parse().ok()?;
    if !seconds.is_finite() {
        return None;
    }
    let millis = (seconds * 1000.0).trunc();
    // `as` saturates, so values outside `i64` land on an invalid timestamp
    // instead of wrapping.
    DateTime::<Utc>::from_timestamp_millis(millis as i64)
}

fn date_to_timestamp(input: &str) -> TransformResult {
    match parse_datetime(input) {
        // Boop divides Date.parse's milliseconds by 1000; sub-second input is
        // therefore truncated to whole seconds here.
        Some(parsed) => TransformResult::replace(parsed.timestamp().to_string()),
        None => TransformResult::error("Invalid Date"),
    }
}

fn date_to_utc(input: &str) -> TransformResult {
    match parse_datetime(input).or_else(|| parse_unix_seconds(input.trim())) {
        Some(parsed) => TransformResult::replace(parsed.format(UTC_FORMAT).to_string()),
        None => TransformResult::error("Invalid Date"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(result: TransformResult) -> String {
        match result {
            TransformResult::Replace(text) => text,
            _ => panic!("expected a replacement"),
        }
    }

    fn error(result: TransformResult) -> String {
        match result {
            TransformResult::Error(message) => message,
            _ => panic!("expected an error"),
        }
    }

    #[test]
    fn timestamp_from_iso_date() {
        assert_eq!(text(date_to_timestamp("2015-10-21")), "1445385600");
    }

    #[test]
    fn timestamp_from_rfc3339_and_rfc2822() {
        let expected = "1445412480";
        assert_eq!(text(date_to_timestamp("2015-10-21T07:28:00Z")), expected);
        assert_eq!(text(date_to_timestamp("Wed, 21 Oct 2015 07:28:00 GMT")), expected);
    }

    #[test]
    fn timestamp_from_naive_datetime_is_utc() {
        let expected = "1445412480";
        assert_eq!(text(date_to_timestamp("2015-10-21 07:28:00")), expected);
        assert_eq!(text(date_to_timestamp("2015-10-21T07:28:00")), expected);
    }

    #[test]
    fn timestamp_trims_surrounding_whitespace() {
        assert_eq!(text(date_to_timestamp("  2015-10-21  ")), "1445385600");
    }

    #[test]
    fn slashed_dates_prefer_month_first() {
        // Ambiguous input resolves month-first, as in JS.
        assert_eq!(text(date_to_timestamp("10/21/2015")), "1445385600");
        // A month above 12 only fits day-first, which JS's grammar omits.
        assert_eq!(text(date_to_timestamp("21/10/2015")), "1445385600");
    }

    #[test]
    fn timestamp_rejects_invalid_dates() {
        assert_eq!(error(date_to_timestamp("not a date")), "Invalid Date");
        assert_eq!(error(date_to_timestamp("")), "Invalid Date");
    }

    #[test]
    fn utc_matches_to_utc_string() {
        let expected = "Wed, 21 Oct 2015 07:28:00 GMT";
        assert_eq!(text(date_to_utc("2015-10-21T07:28:00Z")), expected);
        assert_eq!(text(date_to_utc("Wed, 21 Oct 2015 07:28:00 GMT")), expected);
        assert_eq!(text(date_to_utc("2015-10-21")), "Wed, 21 Oct 2015 00:00:00 GMT");
    }

    #[test]
    fn utc_converts_offsets_to_gmt() {
        let expected = "Wed, 21 Oct 2015 07:28:00 GMT";
        assert_eq!(text(date_to_utc("2015-10-21T09:28:00+02:00")), expected);
    }

    #[test]
    fn utc_accepts_unix_seconds() {
        assert_eq!(text(date_to_utc("1445412480")), "Wed, 21 Oct 2015 07:28:00 GMT");
        assert_eq!(text(date_to_utc("-86400")), "Wed, 31 Dec 1969 00:00:00 GMT");
    }

    #[test]
    fn utc_rejects_invalid_dates() {
        assert_eq!(error(date_to_utc("not a date")), "Invalid Date");
        // Numeric but far outside chrono's representable range.
        assert_eq!(error(date_to_utc("1e300")), "Invalid Date");
    }
}
