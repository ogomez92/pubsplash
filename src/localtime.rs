//! The local wall clock, and the only module in the app that knows a time zone.
//!
//! This was three separate Win32 calls in three unrelated files — `GetLocalTime`
//! in `mastodon::local_hour` and in two filename stampers, and
//! `TzSpecificLocalTimeToSystemTime` in `schedule::local_to_unix` — each with a
//! comment explaining that a date crate was too much dependency for one
//! conversion. Together they were the app's only reason to ask for the
//! `Win32_System_Time` and `Win32_System_SystemInformation` features, and the
//! only wall-clock reading anywhere. Collecting them here retires all of it and
//! is what makes those four call sites portable.
//!
//! `jiff` carries the real IANA rules rather than the single transition pair
//! Win32 applies, which is a correctness gain on Windows too: a schedule set
//! either side of a DST change lands on the right instant, and a zone whose
//! rules changed mid-history converts as that history says.

use jiff::civil;
use jiff::tz::{AmbiguousOffset, TimeZone};

/// A local date and time, broken into the components the callers want.
///
/// The field types match what the two consumers already used: `wxdragon`'s
/// pickers hand out `u16` components with a **1-based** month, and so did
/// `SYSTEMTIME` before this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Local {
    pub year: i32,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

impl Local {
    /// `yyyy-mm-dd_HH-MM-SS`, the stamp both the recording and the log-archive
    /// filenames are built from.
    ///
    /// Colons are not legal in a filename on Windows and are hostile on macOS
    /// (the Finder shows them as `/`), which is why the time is hyphenated. The
    /// order is what makes a folder of captures sort chronologically by name.
    pub fn file_stamp(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// The current local date and time.
pub fn now() -> Local {
    let zoned = jiff::Zoned::now();
    Local {
        year: i32::from(zoned.year()),
        month: zoned.month() as u16,
        day: zoned.day() as u16,
        hour: zoned.hour() as u16,
        minute: zoned.minute() as u16,
        second: zoned.second() as u16,
    }
}

/// A local date and time as Unix seconds, or `None` if no such moment exists.
///
/// `month` is **1-based**, matching `wxdragon::DateTime::month()` (which
/// converts wx's 0-based value for us at both ends — see the crate's
/// `datetime.rs`), so components read straight off a `DatePickerCtrl` pass
/// through unchanged.
///
/// Three kinds of input have no answer and all return `None`. An out-of-range
/// component — hour 25, month 13 — and an impossible date such as 31 February
/// are both refused by the civil constructors. And the hour a spring-forward
/// skips is a **gap**: no such local time occurred, so there is no instant to
/// convert it to. A **fold**, the hour a fall-back repeats, is a different
/// thing: it happened twice, so it does have an answer, and the earlier of the
/// two is taken (`compatible` is jiff's name for that rule, and it is the same
/// choice Win32 made here before).
///
/// Note `Date::new` and `Time::new`, never `civil::date()`/`Date::at()`: the
/// short constructors **panic** on a component out of range, and this function's
/// whole contract is to answer `None` instead. The pickers cannot produce such a
/// value, but a settings file can.
pub fn to_unix(
    year: i32,
    month: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
) -> Option<u64> {
    let date = civil::Date::new(
        i16::try_from(year).ok()?,
        i8::try_from(month).ok()?,
        i8::try_from(day).ok()?,
    )
    .ok()?;
    let time = civil::Time::new(
        i8::try_from(hour).ok()?,
        i8::try_from(minute).ok()?,
        i8::try_from(second).ok()?,
        0,
    )
    .ok()?;
    let ambiguous = TimeZone::system().to_ambiguous_zoned(date.to_datetime(time));
    if matches!(ambiguous.offset(), AmbiguousOffset::Gap { .. }) {
        return None;
    }
    let seconds = ambiguous.compatible().ok()?.timestamp().as_second();
    // A time before 1970 is not something the pickers can produce, but
    // answering `None` beats wrapping.
    u64::try_from(seconds).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the filename stampers depend on: a fixed width, so the two
    /// tests that assert a filename length keep meaning something.
    #[test]
    fn the_file_stamp_is_fixed_width_and_sortable() {
        let stamp = now().file_stamp();
        assert_eq!(stamp.len(), "0000-00-00_00-00-00".len(), "{stamp}");
        assert!(!stamp.contains(':'), "{stamp}");
    }

    /// `to_unix` against the clock it is meant to agree with.
    ///
    /// A tolerance rather than equality: `now()` and the system clock are two
    /// separate readings and the second can tick between them.
    #[test]
    fn to_unix_agrees_with_the_system_clock() {
        let local = now();
        let converted = to_unix(
            local.year,
            local.month,
            local.day,
            local.hour,
            local.minute,
            local.second,
        )
        .expect("the current local time must be convertible");
        let unix = crate::mastodon::now_unix();
        assert!(
            converted.abs_diff(unix) <= 2,
            "converted {converted} vs now_unix {unix}"
        );
    }

    /// Out-of-range components must answer `None`, not panic — which is what
    /// `Date::at` and `civil::date` would do.
    #[test]
    fn an_impossible_date_or_time_has_no_answer() {
        assert_eq!(to_unix(2026, 2, 31, 12, 0, 0), None, "31 February");
        assert_eq!(to_unix(2026, 13, 1, 12, 0, 0), None, "month 13");
        assert_eq!(to_unix(2026, 1, 1, 25, 0, 0), None, "hour 25");
        assert_eq!(to_unix(2026, 1, 1, 12, 60, 0), None, "minute 60");
        assert_eq!(to_unix(2026, 1, 1, 12, 0, 61), None, "second 61");
    }

    /// The gap and fold rules, asserted against a zone whose transitions are
    /// known rather than against wherever this happens to be running.
    #[test]
    fn a_dst_gap_has_no_answer_but_a_fold_does() {
        let tz = TimeZone::get("America/Los_Angeles").expect("a bundled zone");
        // 2020-03-08 02:30 never happened there.
        let gap = tz.to_ambiguous_zoned(civil::date(2020, 3, 8).at(2, 30, 0, 0));
        assert!(matches!(gap.offset(), AmbiguousOffset::Gap { .. }));
        // 2020-11-01 01:30 happened twice, and the earlier one is the answer.
        let fold = tz.to_ambiguous_zoned(civil::date(2020, 11, 1).at(1, 30, 0, 0));
        assert!(matches!(fold.offset(), AmbiguousOffset::Fold { .. }));
        assert!(fold.compatible().is_ok());
    }
}
