//! The `Retry-After` response header: a whole number of seconds, or an IMF-fixdate.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, RETRY_AFTER};

/// The wait the provider asked for in `Retry-After`, if the header is present and parseable.
/// Both forms HTTP allows are understood (RFC 7231 §7.1.3): a whole number of seconds, or an
/// IMF-fixdate. An unparseable value is ignored, and the core falls back to its own backoff.
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?;
    parse_retry_after(value.to_str().ok()?, SystemTime::now())
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = parse_http_date(value)?;
    // A date in the past means "retry now"; the caller still backs off one step.
    Some(date.duration_since(now).unwrap_or_default())
}

/// `Sun, 06 Nov 1994 08:49:37 GMT` (IMF-fixdate). The weekday is ignored: a parser that
/// validates it has to do calendar arithmetic the sender may have got wrong, and the date
/// itself is what matters.
fn parse_http_date(value: &str) -> Option<SystemTime> {
    let rest = value.strip_suffix(" GMT")?;
    let (weekday, rest) = rest.split_once(", ")?;
    if weekday.len() != 3 {
        return None;
    }
    let mut parts = rest.split(' ');
    let day: i64 = parts.next()?.parse().ok()?;
    let month = month_number(parts.next()?)?;
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let (hour, rest) = time.split_once(':')?;
    let (minute, second) = rest.split_once(':')?;
    let hour: i64 = hour.parse().ok()?;
    let minute: i64 = minute.parse().ok()?;
    let second: i64 = second.parse().ok()?;
    if !(1..=31).contains(&day) || !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    // 60 is a leap second.
    if !(0..=60).contains(&second) {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3600 + minute * 60 + second)?;
    if seconds < 0 {
        return Some(UNIX_EPOCH);
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
}

fn month_number(name: &str) -> Option<i64> {
    Some(match name {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

/// Days from 1970-01-01 to `year-month-day`, by Howard Hinnant's civil-calendar algorithm.
/// `None` for a month outside 1..=12.
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400; // [0, 399]
    let mp = if month > 2 { month - 3 } else { month + 9 }; // March = 0
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(unix_seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(unix_seconds)
    }

    #[test]
    fn retry_after_seconds_are_taken_as_is() {
        assert_eq!(
            parse_retry_after("12", at(0)),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            parse_retry_after(" 3 ", at(0)),
            Some(Duration::from_secs(3))
        );
        assert_eq!(parse_retry_after("0", at(0)), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_accepts_an_imf_fixdate() {
        // The example date from RFC 7231 §7.1.1.1 is 784,111,777 seconds after the epoch.
        let target = 784_111_777;
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", at(target - 30)),
            Some(Duration::from_secs(30))
        );
        // A date already in the past means "now".
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", at(target + 5)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn an_unparseable_retry_after_is_ignored() {
        for value in [
            "",
            "soon",
            "-1",
            "Sat, 06 Nov 1994 08:49:37 GMT", // a weekday is not the point; this parses
            "Sun, 06 Nov 1994 08:49:37",     // no `GMT`
            "Sun 06 Nov 1994 08:49:37 GMT",  // no comma
            "Sun, 06 Foo 1994 08:49:37 GMT", // unknown month
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 06 Nov 1994 08:61:00 GMT",
            "Sun, 00 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:37 GMT extra",
        ] {
            let parsed = parse_retry_after(value, at(0));
            // The one date with a wrong weekday is still a valid IMF-fixdate; the rest are not.
            if value.starts_with("Sat") {
                assert!(parsed.is_some(), "{value:?} must parse");
            } else {
                assert_eq!(parsed, None, "{value:?} must be ignored");
            }
        }
    }

    #[test]
    fn days_from_civil_matches_known_epochs() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(1970, 1, 2), Some(1));
        assert_eq!(days_from_civil(1969, 12, 31), Some(-1));
        assert_eq!(days_from_civil(2000, 3, 1), Some(11_017));
        assert_eq!(days_from_civil(1994, 11, 6), Some(9_075));
    }
}
