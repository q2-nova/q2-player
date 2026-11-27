//! Conservative Last-Modified validation for a resumable representation.
//!
//! RFC 9110 §8.8.2.2 defines when a date validator can be treated as strong.
//! We conservatively require a 60-second separation in the same response.
//! The caller owns that binding,
//! validator matching, and all subsequent range/length checks.
use std::time::Duration;

pub(super) fn strong_last_modified(last_modified: &str, date: &str) -> bool {
    const MAX_HTTP_DATE_BYTES: usize = 128;
    if last_modified.len() > MAX_HTTP_DATE_BYTES || date.len() > MAX_HTTP_DATE_BYTES
        || last_modified.trim() != last_modified || date.trim() != date
        || !last_modified.is_ascii() || !date.is_ascii() {
        return false;
    }
    let (Ok(last_modified), Ok(date)) = (httpdate::parse_http_date(last_modified), httpdate::parse_http_date(date)) else {
        return false;
    };
    date.duration_since(last_modified).is_ok_and(|age| age >= Duration::from_secs(60))
}

#[cfg(test)]
mod tests {
    use super::strong_last_modified;

    #[test]
    fn exact_minute_is_the_minimum_and_future_dates_are_rejected() {
        let last = "Sun, 06 Nov 1994 08:49:37 GMT";
        assert!(strong_last_modified(last, "Sun, 06 Nov 1994 08:50:37 GMT"));
        assert!(strong_last_modified(last, "Sun, 06 Nov 1994 08:50:38 GMT"));
        assert!(!strong_last_modified(last, "Sun, 06 Nov 1994 08:50:36 GMT"));
        assert!(!strong_last_modified(last, last));
        assert!(!strong_last_modified(last, "Sun, 06 Nov 1994 08:49:36 GMT"));
    }

    #[test]
    fn handles_calendar_boundaries_and_both_legacy_http_dates() {
        assert!(strong_last_modified("Thu, 29 Feb 2024 23:59:30 GMT", "Fri, 01 Mar 2024 00:00:30 GMT"));
        assert!(!strong_last_modified("Wed, 29 Feb 2023 23:59:30 GMT", "Thu, 01 Mar 2023 00:00:30 GMT"));
        assert!(strong_last_modified("Sunday, 06-Nov-94 08:49:37 GMT", "Sun Nov  6 08:50:37 1994"));
        assert!(strong_last_modified("Sun Nov  6 08:49:37 1994", "Sunday, 06-Nov-94 08:50:37 GMT"));
    }

    #[test]
    fn malformed_zones_calendar_weekdays_and_oversized_values_are_not_validators() {
        let valid = "Sun, 06 Nov 1994 08:50:37 GMT";
        for invalid in ["", "not a date", "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 06 Nov 1994 08:49:37 +0000", "Mon, 06 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:37 GMT trailing", "Sun, 06 Nov 1994 08:49:60 GMT",
            "Sun, 06 Nov 1994 08:49:37 GMT\r\n", " Sun, 06 Nov 1994 08:49:37 GMT"] {
            assert!(!strong_last_modified(invalid, valid), "last-modified {invalid:?}");
            assert!(!strong_last_modified(valid, invalid), "date {invalid:?}");
        }
        assert!(!strong_last_modified(&"x".repeat(129), valid));
        assert!(!strong_last_modified(valid, &"x".repeat(129)));
    }
}
