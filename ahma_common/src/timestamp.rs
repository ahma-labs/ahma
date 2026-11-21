//! Wall-clock timestamps, spelled exactly as ahma has always written them.
//!
//! Every RFC 3339 timestamp ahma writes — audit records (`permissions.jsonl`,
//! the execution audit log, a task vault's `audit.jsonl`, the web-egress log),
//! settings-change records, operation JSON, `setup --plan` stamps, the TUI's
//! chat transcripts — goes through [`rfc3339`]. The spelling is the one
//! `chrono`'s `to_rfc3339()` produced before ahma moved to `jiff`
//! (RUSTSEC-2026-0543), because files written by earlier versions sit next to
//! new lines in the same log and are compared and sorted as text:
//!
//! * the offset is always `±HH:MM`, so UTC is `+00:00`, never `Z`
//!   (`jiff::Timestamp`'s own `Display` writes `Z`);
//! * the fraction is omitted when it is zero, and otherwise has exactly 3, 6
//!   or 9 digits — the fewest of those that hold it exactly (`jiff`'s own
//!   `Display` trims trailing zeros instead, writing `.12` where chrono wrote
//!   `.120`).
//!
//! "Local" is [`local_tz`], resolved as `chrono::Local` resolved it; every
//! surface (CLI, MCP server, hooks, TUI, vault) takes the current local time
//! from [`local_now`], never from `jiff::Zoned::now()`. This is R-TIME.1 and
//! R-TIME.2 (`ahma_common/SPEC.md`); the tests below pin each spelling byte
//! for byte.

use std::time::SystemTime;

use jiff::fmt::temporal::DateTimePrinter;
use jiff::tz::{Offset, TimeZone};
use jiff::{Timestamp, Zoned};

/// The local time zone, resolved the way `chrono::Local` resolved it.
///
/// On Unix [`TimeZone::try_system`] honours `TZ` and `/etc/localtime`, as
/// chrono did. Two divergences are restored:
///
/// * when `TZ` is set but does not resolve, jiff falls back to UTC where
///   chrono fell back to the OS zone, so a typo in `TZ` would shift every
///   recorded time;
/// * on Windows chrono ignored `TZ` and used the OS zone, which jiff reads
///   only after `TZ` (and maps to UTC when it has no IANA name).
///
/// The OS zone is found as chrono's own fallback found it: its IANA name from
/// the OS ([`iana_time_zone`]), else `/etc/localtime`. The answer is kept for
/// a minute per `TZ` value, so a misconfigured `TZ` does not repeat its failed
/// lookups (blocking I/O, possibly on an async worker) for every stamp.
pub fn local_tz() -> TimeZone {
    type Cached = (Option<std::ffi::OsString>, std::time::Instant, TimeZone);
    static CACHE: parking_lot::Mutex<Option<Cached>> = parking_lot::const_mutex(None);
    const FRESH: std::time::Duration = std::time::Duration::from_secs(60);
    let tz_env = std::env::var_os("TZ");
    if let Some((env, at, tz)) = CACHE.lock().as_ref()
        && *env == tz_env
        && at.elapsed() < FRESH
    {
        return tz.clone();
    }
    let tz = resolve_local_tz();
    *CACHE.lock() = Some((tz_env, std::time::Instant::now(), tz.clone()));
    tz
}

fn resolve_local_tz() -> TimeZone {
    // Windows: the OS zone, whatever `TZ` says, as chrono read it.
    #[cfg(windows)]
    if let Some(tz) = os_zone() {
        return tz;
    }
    TimeZone::try_system()
        .ok()
        .or_else(os_zone)
        .unwrap_or(TimeZone::UTC)
}

/// The OS's own zone, ignoring `TZ`.
fn os_zone() -> Option<TimeZone> {
    if let Some(tz) = iana_time_zone::get_timezone()
        .ok()
        .and_then(|name| TimeZone::get(&name).ok())
    {
        return Some(tz);
    }
    #[cfg(unix)]
    {
        let data = std::fs::read("/etc/localtime").ok()?;
        TimeZone::tzif("Local", &data).ok()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The current local time: replaces `chrono::Local::now()`. Use it where one
/// reading is written twice (an audit `at` and a `granted_at` date), so both
/// spellings name the same instant.
pub fn local_now() -> Zoned {
    Timestamp::now().to_zoned(local_tz())
}

/// `ts` at `offset` as RFC 3339, in chrono's `to_rfc3339()` spelling (see the
/// module docs). An offset with non-zero seconds is rounded to the nearest
/// minute, as chrono did.
pub fn rfc3339(ts: Timestamp, offset: Offset) -> String {
    // The digits printed are the civil time's fraction, which is never
    // negative (a pre-1970 `Timestamp` has a negative `subsec_nanosecond`).
    let nanos = offset.to_datetime(ts).subsec_nanosecond();
    let precision = if nanos == 0 {
        0
    } else if nanos % 1_000_000 == 0 {
        3
    } else if nanos % 1_000 == 0 {
        6
    } else {
        9
    };
    DateTimePrinter::new()
        .precision(Some(precision))
        .timestamp_with_offset_to_string(&ts, offset)
}

/// `zdt` as RFC 3339 at its own offset, in chrono's spelling.
pub fn rfc3339_zoned(zdt: &Zoned) -> String {
    rfc3339(zdt.timestamp(), zdt.offset())
}

/// Now, at the system time zone's offset: what `chrono::Local::now().to_rfc3339()` wrote.
pub fn rfc3339_local_now() -> String {
    rfc3339_zoned(&local_now())
}

/// Now, in UTC: what `chrono::Utc::now().to_rfc3339()` wrote (`…+00:00`, not `…Z`).
pub fn rfc3339_utc_now() -> String {
    rfc3339(Timestamp::now(), Offset::UTC)
}

/// `t` at the system time zone's offset as RFC 3339, in chrono's spelling.
///
/// Fails only for a time outside jiff's range (roughly years -9999..=9999),
/// where chrono's `DateTime::<Local>::from(SystemTime)` panicked.
pub fn rfc3339_local(t: SystemTime) -> Result<String, jiff::Error> {
    let ts = Timestamp::try_from(t)?;
    Ok(rfc3339(ts, local_tz().to_offset(ts)))
}

/// Parse an RFC 3339 timestamp with any offset (`Z`, `z`, `+00:00`, `-05:00`)
/// — everything [`rfc3339`] and chrono's `to_rfc3339()` ever wrote.
pub fn parse_rfc3339(s: &str) -> Result<SystemTime, jiff::Error> {
    s.parse::<Timestamp>().map(SystemTime::from)
}

/// Parse an RFC 3339 timestamp with any offset into the local time zone
/// ([`local_tz`]): what `DateTime::parse_from_rfc3339(s)?.with_timezone(&Local)`
/// did. `None` when `s` is not RFC 3339 with an offset, or is outside jiff's
/// range.
pub fn parse_rfc3339_local(s: &str) -> Option<Zoned> {
    let ts: Timestamp = s.parse().ok()?;
    Some(ts.to_zoned(local_tz()))
}

/// `zdt`'s civil date as `YYYY-MM-DD` (what `format("%Y-%m-%d")` wrote).
pub fn date(zdt: &Zoned) -> String {
    zdt.strftime("%Y-%m-%d").to_string()
}

/// Today's date in the system time zone, `YYYY-MM-DD`.
pub fn local_date_today() -> String {
    date(&local_now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::tz;

    /// 2026-11-21T09:05:03Z plus `nanos`.
    fn at(nanos: i32) -> Timestamp {
        Timestamp::new(1_795_251_903, nanos).unwrap()
    }

    #[test]
    fn utc_is_plus_zero_zero_never_z() {
        assert_eq!(rfc3339(at(0), Offset::UTC), "2026-11-21T09:05:03+00:00");
    }

    #[test]
    fn fraction_is_three_six_or_nine_digits_like_chrono() {
        // Exactly the cases where jiff's own Display would differ (`.12`, `.0001`).
        assert_eq!(
            rfc3339(at(120_000_000), Offset::UTC),
            "2026-11-21T09:05:03.120+00:00"
        );
        assert_eq!(
            rfc3339(at(1_000_000), Offset::UTC),
            "2026-11-21T09:05:03.001+00:00"
        );
        assert_eq!(
            rfc3339(at(123_456_000), Offset::UTC),
            "2026-11-21T09:05:03.123456+00:00"
        );
        assert_eq!(
            rfc3339(at(100_000), Offset::UTC),
            "2026-11-21T09:05:03.000100+00:00"
        );
        assert_eq!(
            rfc3339(at(123_456_789), Offset::UTC),
            "2026-11-21T09:05:03.123456789+00:00"
        );
        assert_eq!(
            rfc3339(at(1), Offset::UTC),
            "2026-11-21T09:05:03.000000001+00:00"
        );
    }

    #[test]
    fn offsets_are_colon_separated_and_rounded_to_the_minute() {
        assert_eq!(rfc3339(at(0), tz::offset(-5)), "2026-11-21T04:05:03-05:00");
        let nepal = Offset::from_seconds(5 * 3600 + 45 * 60).unwrap();
        assert_eq!(rfc3339(at(0), nepal), "2026-11-21T14:50:03+05:45");
        // chrono rounded an offset's seconds to the nearest minute; so does jiff.
        let up = Offset::from_seconds(5 * 3600 + 30 * 60 + 31).unwrap();
        let down = Offset::from_seconds(5 * 3600 + 30 * 60 + 29).unwrap();
        assert!(rfc3339(at(0), up).ends_with("+05:31"));
        assert!(rfc3339(at(0), down).ends_with("+05:30"));
    }

    #[test]
    fn pre_epoch_fraction_is_the_civil_fraction() {
        // 1969-12-31T23:59:59.5Z: the timestamp's subsec is -500_000_000.
        let ts = Timestamp::new(0, -500_000_000).unwrap();
        assert_eq!(rfc3339(ts, Offset::UTC), "1969-12-31T23:59:59.500+00:00");
    }

    #[test]
    fn zoned_uses_its_own_offset() {
        let zdt = at(0).to_zoned(TimeZone::fixed(tz::offset(9)));
        assert_eq!(rfc3339_zoned(&zdt), "2026-11-21T18:05:03+09:00");
        assert_eq!(date(&zdt), "2026-11-21");
        let west = at(0).to_zoned(TimeZone::fixed(tz::offset(-10)));
        assert_eq!(
            date(&west),
            "2026-11-20",
            "the date is the zone's civil date"
        );
    }

    #[test]
    fn parses_everything_earlier_versions_wrote() {
        let want = SystemTime::from(at(123_456_789));
        for s in [
            "2026-11-21T09:05:03.123456789+00:00", // chrono Utc::to_rfc3339
            "2026-11-21T10:05:03.123456789+01:00", // chrono Local::to_rfc3339
            "2026-11-21T04:05:03.123456789-05:00",
            "2026-11-21T09:05:03.123456789Z",
            "2026-11-21t09:05:03.123456789z",
            "2026-11-21 09:05:03.123456789+00:00",
        ] {
            assert_eq!(parse_rfc3339(s).unwrap(), want, "{s}");
        }
        assert!(
            parse_rfc3339("2026-11-21T09:05:03").is_err(),
            "an offset is required"
        );
    }

    #[test]
    fn round_trips_through_text() {
        for nanos in [0, 1, 1_000, 1_000_000, 120_000_000, 999_999_999] {
            let t = SystemTime::from(at(nanos));
            assert_eq!(parse_rfc3339(&rfc3339_local(t).unwrap()).unwrap(), t);
        }
    }

    #[test]
    fn now_helpers_have_the_pinned_shape() {
        let utc = rfc3339_utc_now();
        assert!(utc.ends_with("+00:00"), "{utc}");
        assert!(parse_rfc3339(&utc).is_ok());
        let local = rfc3339_local_now();
        assert!(!local.ends_with('Z'), "{local}");
        assert!(parse_rfc3339(&local).is_ok());
        let today = local_date_today();
        assert_eq!(today.len(), 10, "{today}");
        assert_eq!(&today[4..5], "-");
        assert_eq!(&today[7..8], "-");
    }

    /// 2026-11-21T10:15:30 civil time at a fixed offset, plus `nanos`.
    fn civil_at(offset_secs: i32, nanos: i32) -> Zoned {
        let tz = TimeZone::fixed(Offset::from_seconds(offset_secs).unwrap());
        jiff::civil::date(2026, 11, 21)
            .at(10, 15, 30, nanos)
            .to_zoned(tz)
            .unwrap()
    }

    /// Golden strings are chrono 0.4.45's `to_rfc3339()` for the same instants.
    #[test]
    fn zoned_rfc3339_matches_chrono_byte_for_byte() {
        let cases = [
            (7200, 0, "2026-11-21T10:15:30+02:00"),
            (7200, 1, "2026-11-21T10:15:30.000000001+02:00"),
            (7200, 120_000_000, "2026-11-21T10:15:30.120+02:00"),
            (7200, 123_456_000, "2026-11-21T10:15:30.123456+02:00"),
            (7200, 123_456_789, "2026-11-21T10:15:30.123456789+02:00"),
            (-12600, 120_000_000, "2026-11-21T10:15:30.120-03:30"),
            (0, 0, "2026-11-21T10:15:30+00:00"),
            (0, 123_456_000, "2026-11-21T10:15:30.123456+00:00"),
        ];
        for (off, ns, want) in cases {
            assert_eq!(
                rfc3339_zoned(&civil_at(off, ns)),
                want,
                "offset {off}s, {ns}ns"
            );
        }
    }

    /// Every strftime pattern ahma writes (file names, dates, log rows) gives
    /// chrono's `format` output: zero-padded, no fraction.
    #[test]
    fn strftime_patterns_match_chrono() {
        let z = civil_at(-12600, 123_456_789);
        assert_eq!(z.strftime("%Y-%m-%d").to_string(), "2026-11-21");
        assert_eq!(z.strftime("%H:%M:%S").to_string(), "10:15:30");
        assert_eq!(
            z.strftime("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-11-21 10:15:30"
        );
        assert_eq!(z.strftime("%Y%m%d-%H%M%S").to_string(), "20261121-101530");
    }

    /// Everything chrono wrote parses to the same instant, in the local zone.
    #[test]
    fn parse_rfc3339_local_reads_every_spelling_chrono_wrote() {
        let want = civil_at(7200, 0).timestamp();
        for s in [
            "2026-11-21T10:15:30+02:00",
            "2026-11-21T08:15:30+00:00",
            "2026-11-21T08:15:30Z",
            "2026-11-21T08:15:30-00:00",
            "2026-11-21t08:15:30z",
            "2026-11-21 08:15:30Z",
        ] {
            let got = parse_rfc3339_local(s).unwrap();
            assert_eq!(got.timestamp(), want, "{s}");
            assert_eq!(got.offset(), local_tz().to_offset(want), "{s}");
        }
        for (s, ns) in [
            ("2026-11-21T10:15:30.1+02:00", 100_000_000),
            ("2026-11-21T10:15:30.120+02:00", 120_000_000),
            ("2026-11-21T10:15:30.123456+02:00", 123_456_000),
            ("2026-11-21T10:15:30.123456789+02:00", 123_456_789),
        ] {
            assert_eq!(
                parse_rfc3339_local(s).map(|z| z.timestamp()),
                Some(civil_at(7200, ns).timestamp()),
                "{s}"
            );
        }
        assert!(parse_rfc3339_local("2026-11-21T10:15:30").is_none());
        assert!(parse_rfc3339_local("not a time").is_none());
    }

    #[test]
    fn zoned_round_trips_through_its_own_spelling() {
        let z = civil_at(-12600, 123_456_000);
        assert_eq!(parse_rfc3339_local(&rfc3339_zoned(&z)).unwrap(), z);
        let n = local_now();
        assert_eq!(parse_rfc3339_local(&rfc3339_zoned(&n)).unwrap(), n);
    }

    /// A `TZ` that does not resolve falls back to the OS zone, as chrono did,
    /// not to UTC (and on Windows the OS zone is used whatever `TZ` says, as
    /// chrono did). Sets `TZ`, and jiff caches the system zone per process, so
    /// this must be the first zone lookup in its process: relies on nextest's
    /// process per test (AGENTS.md).
    #[test]
    fn unresolvable_tz_falls_back_to_the_os_zone() {
        // SAFETY: nextest runs each test in its own process; nothing else in
        // this process reads the environment concurrently.
        unsafe { std::env::set_var("TZ", "Not/A_Zone") };
        let now = Timestamp::now();
        let os = os_zone().unwrap_or(TimeZone::UTC);
        assert_eq!(local_tz().to_offset(now), os.to_offset(now));
    }
}
