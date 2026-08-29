//! When a Media Scheduler's items are due: the pure clock arithmetic, with no
//! thread, no file and no widget in it.
//!
//! ## The clock is the local wall clock
//!
//! A scheduled item is "play the nine o'clock chime at nine o'clock", not "play
//! it in six hours", so everything here is expressed in local civil time —
//! year, month, day, hour, minute — and converted to Unix seconds only at the
//! edges, through [`crate::schedule::local_to_unix`], which asks Windows for the
//! machine's own time zone *and its DST rules for that particular date*. Doing
//! the arithmetic on `Instant` instead would silently slip by however long the
//! machine was suspended; doing it on Unix seconds directly would put the DST
//! transitions in the wrong place twice a year.
//!
//! ## Occurrences are identified, not counted
//!
//! [`Trigger::last_occurrence`] answers "the most recent moment at or before
//! `now` that this item was due", and the worker remembers the Unix second it
//! last fired. An item fires when the latest occurrence is one it has not fired
//! yet — never on a timer it might drift off, and never twice for the same
//! occurrence. That is what makes the awkward cases fall out rather than need
//! handling: a tick missed under load still fires (the occurrence has not
//! moved), a scene switched away and back does not re-fire the last one (the
//! worker primes itself on startup), and a machine that slept through six
//! hourly chimes wakes up owing exactly one, which [`STALE_GRACE`] then
//! discards as too late to be worth playing.
//!
//! ## Why a minute grid
//!
//! Every trigger fires at a whole minute, which is what makes the arithmetic
//! integer arithmetic: a day is 1440 minutes, and each trigger is a rule over
//! that grid. Seconds exist only in `now`, to decide how late an occurrence is.

use crate::config::ScheduleTrigger;

/// Minutes in a day. Every occurrence lands on this grid.
pub const MINUTES_PER_DAY: u32 = 24 * 60;

/// How late an occurrence may be and still be played, in seconds.
///
/// The reason this has to exist is the machine sleeping. Waking a laptop at
/// twenty past nine and hearing the nine o'clock chime is worse than not
/// hearing it: a time announcement that is wrong is not a late announcement, it
/// is a false one. Inside the grace the item is merely late — a tick missed
/// under a long-running dialog, a file that had to queue behind another — and
/// plays; outside it the occurrence is recorded as fired without being played,
/// and logged.
///
/// Thirty seconds rather than the two minutes [`crate::schedule::STALE_GRACE`]
/// allows a scheduled *stream*, because these items are usually announcements
/// of the very time they would be late by.
pub const STALE_GRACE: u64 = 30;

/// A local wall-clock time, to the second.
///
/// Deliberately not a `SYSTEMTIME`: this is the type the arithmetic is done in,
/// and it has to be constructible in a test without a machine set to the right
/// time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i32,
    /// 1-based, as every Windows and human-facing API has it.
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl LocalTime {
    /// The machine's current local time.
    pub fn now() -> Self {
        // SAFETY: `GetLocalTime` only writes the `SYSTEMTIME` it returns.
        let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        Self {
            year: i32::from(t.wYear),
            month: u32::from(t.wMonth),
            day: u32::from(t.wDay),
            hour: u32::from(t.wHour),
            minute: u32::from(t.wMinute),
            second: u32::from(t.wSecond),
        }
    }

    /// How far into the day this is, on the minute grid. Seconds are dropped.
    pub fn minute_of_day(&self) -> u32 {
        self.hour * 60 + self.minute
    }

    /// The same date at `minute` past midnight, on the second.
    pub fn at_minute_of_day(&self, minute: u32) -> Self {
        Self {
            hour: minute / 60,
            minute: minute % 60,
            second: 0,
            ..*self
        }
    }

    /// The day before, keeping the time of day. Month and year roll correctly,
    /// leap years included — see [`civil_from_days`].
    pub fn previous_day(&self) -> Self {
        let (year, month, day) =
            civil_from_days(days_from_civil(self.year, self.month, self.day) - 1);
        Self {
            year,
            month,
            day,
            ..*self
        }
    }

    /// The day after, keeping the time of day.
    pub fn next_day(&self) -> Self {
        let (year, month, day) =
            civil_from_days(days_from_civil(self.year, self.month, self.day) + 1);
        Self {
            year,
            month,
            day,
            ..*self
        }
    }

    /// This instant as Unix seconds, or `None` if the local time does not exist
    /// — the hour a spring-forward skips, or a date the calendar does not have.
    pub fn to_unix(self) -> Option<u64> {
        crate::schedule::local_to_unix(
            self.year,
            u16::try_from(self.month).ok()?,
            u16::try_from(self.day).ok()?,
            u16::try_from(self.hour).ok()?,
            u16::try_from(self.minute).ok()?,
            u16::try_from(self.second).ok()?,
        )
    }
}

/// The trigger arithmetic, over the minute grid of a day.
///
/// Four questions, each answered without allocating: an interval of one minute
/// has 1440 occurrences a day, and these are asked once a second.
pub trait Trigger {
    /// The latest occurrence at or before `minute` on the same day, if the day
    /// has one that early.
    fn last_minute_at_or_before(&self, minute: u32) -> Option<u32>;
    /// The earliest occurrence strictly after `minute` on the same day.
    fn next_minute_after(&self, minute: u32) -> Option<u32>;
    /// The last occurrence of any day, for rolling back over midnight.
    fn last_minute_of_day(&self) -> u32;
    /// The first occurrence of any day, for rolling forward over midnight.
    fn first_minute_of_day(&self) -> u32;

    /// The most recent moment at or before `now` that this trigger was due.
    ///
    /// Always answers: every trigger fires at least once a day, so the worst
    /// case is yesterday's last occurrence.
    fn last_occurrence(&self, now: LocalTime) -> LocalTime {
        match self.last_minute_at_or_before(now.minute_of_day()) {
            Some(minute) => now.at_minute_of_day(minute),
            None => now
                .previous_day()
                .at_minute_of_day(self.last_minute_of_day()),
        }
    }

    /// The first moment strictly after `now` that this trigger is due.
    fn next_occurrence(&self, now: LocalTime) -> LocalTime {
        match self.next_minute_after(now.minute_of_day()) {
            Some(minute) => now.at_minute_of_day(minute),
            None => now.next_day().at_minute_of_day(self.first_minute_of_day()),
        }
    }
}

impl Trigger for ScheduleTrigger {
    fn last_minute_at_or_before(&self, minute: u32) -> Option<u32> {
        match *self {
            // Midnight is always on the grid, so an interval always has an
            // occurrence earlier in the same day.
            ScheduleTrigger::EveryMinutes { minutes } => {
                let step = clamp_interval(minutes);
                Some(minute - minute % step)
            }
            ScheduleTrigger::Hourly { minute: at } => {
                let at = clamp_minute(at);
                let (hour, past) = (minute / 60, minute % 60);
                if past >= at {
                    Some(hour * 60 + at)
                } else {
                    hour.checked_sub(1).map(|previous| previous * 60 + at)
                }
            }
            ScheduleTrigger::DailyAt { hour, minute: at } => {
                let due = clamp_hour(hour) * 60 + clamp_minute(at);
                (due <= minute).then_some(due)
            }
        }
    }

    fn next_minute_after(&self, minute: u32) -> Option<u32> {
        match *self {
            ScheduleTrigger::EveryMinutes { minutes } => {
                let step = clamp_interval(minutes);
                let next = minute - minute % step + step;
                (next < MINUTES_PER_DAY).then_some(next)
            }
            ScheduleTrigger::Hourly { minute: at } => {
                let at = clamp_minute(at);
                let (hour, past) = (minute / 60, minute % 60);
                if at > past {
                    Some(hour * 60 + at)
                } else {
                    (hour + 1 < 24).then(|| (hour + 1) * 60 + at)
                }
            }
            ScheduleTrigger::DailyAt { hour, minute: at } => {
                let due = clamp_hour(hour) * 60 + clamp_minute(at);
                (due > minute).then_some(due)
            }
        }
    }

    fn last_minute_of_day(&self) -> u32 {
        match *self {
            ScheduleTrigger::EveryMinutes { minutes } => {
                let step = clamp_interval(minutes);
                (MINUTES_PER_DAY - 1) / step * step
            }
            ScheduleTrigger::Hourly { minute } => 23 * 60 + clamp_minute(minute),
            ScheduleTrigger::DailyAt { hour, minute } => {
                clamp_hour(hour) * 60 + clamp_minute(minute)
            }
        }
    }

    fn first_minute_of_day(&self) -> u32 {
        match *self {
            ScheduleTrigger::EveryMinutes { .. } => 0,
            ScheduleTrigger::Hourly { minute } => clamp_minute(minute),
            ScheduleTrigger::DailyAt { hour, minute } => {
                clamp_hour(hour) * 60 + clamp_minute(minute)
            }
        }
    }
}

/// The intervals the dialog offers, in minutes.
///
/// A list rather than a free number box because every one of these divides a
/// day evenly, which is what keeps an interval landing where a listener expects
/// it — "every 25 minutes" walks around the clock face and arrives somewhere
/// different every day. The stored value is the number of minutes, so
/// reordering this list cannot invalidate anyone's settings, and a hand-edited
/// file may still name an interval that is not on it.
pub const INTERVALS: &[u32] = &[
    1, 2, 3, 5, 6, 10, 12, 15, 20, 30, 60, 90, 120, 180, 240, 360, 480, 720,
];

/// Holds an interval to something that divides a day and cannot be zero.
///
/// Zero would divide by zero on the grid, and anything longer than a day would
/// never come round.
pub fn clamp_interval(minutes: u32) -> u32 {
    minutes.clamp(1, MINUTES_PER_DAY)
}

pub fn clamp_minute(minute: u32) -> u32 {
    minute.min(59)
}

pub fn clamp_hour(hour: u32) -> u32 {
    hour.min(23)
}

/// `HH:MM` on a 24-hour clock, from a minute of the day.
///
/// Deliberately not a 12-hour form: a schedule is read back as a list of times
/// that have to be comparable at a glance, and both languages Pubsplash ships
/// in write times this way.
pub fn format_time(minute_of_day: u32) -> String {
    let minute_of_day = minute_of_day % MINUTES_PER_DAY;
    format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60)
}

/// Days since 1970-01-01 for a civil date, and back again. Howard Hinnant's
/// algorithm, exact for every date the calendar has, leap years and century
/// rules included — the arithmetic behind [`LocalTime::previous_day`], which is
/// the one place a schedule has to know what a month is.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    ((y + i64::from(m <= 2)) as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: u32, minute: u32, second: u32) -> LocalTime {
        LocalTime {
            year: 2026,
            month: 8,
            day: 29,
            hour,
            minute,
            second,
        }
    }

    fn hourly(minute: u32) -> ScheduleTrigger {
        ScheduleTrigger::Hourly { minute }
    }

    fn daily(hour: u32, minute: u32) -> ScheduleTrigger {
        ScheduleTrigger::DailyAt { hour, minute }
    }

    fn every(minutes: u32) -> ScheduleTrigger {
        ScheduleTrigger::EveryMinutes { minutes }
    }

    #[test]
    fn a_quarter_hourly_interval_lands_on_the_quarters() {
        let t = every(15);
        assert_eq!(t.last_occurrence(at(9, 7, 30)), at(9, 0, 0));
        assert_eq!(t.last_occurrence(at(9, 15, 0)), at(9, 15, 0));
        assert_eq!(t.last_occurrence(at(9, 59, 59)), at(9, 45, 0));
        assert_eq!(t.next_occurrence(at(9, 7, 30)), at(9, 15, 0));
        assert_eq!(t.next_occurrence(at(9, 45, 0)), at(10, 0, 0));
    }

    /// Midnight is on every interval grid, so an interval never has to look at
    /// yesterday.
    #[test]
    fn an_interval_at_midnight_has_an_occurrence_today() {
        assert_eq!(every(15).last_occurrence(at(0, 0, 3)), at(0, 0, 0));
        assert_eq!(every(720).last_occurrence(at(0, 0, 0)), at(0, 0, 0));
    }

    /// The last interval of the day wraps into tomorrow, not back to today.
    #[test]
    fn an_interval_wraps_at_midnight() {
        let next = every(15).next_occurrence(at(23, 45, 10));
        assert_eq!(next.day, 30, "tomorrow");
        assert_eq!((next.hour, next.minute), (0, 0));
    }

    #[test]
    fn hourly_looks_back_within_the_hour_then_to_the_hour_before() {
        let t = hourly(30);
        assert_eq!(t.last_occurrence(at(9, 30, 0)), at(9, 30, 0));
        assert_eq!(t.last_occurrence(at(9, 45, 0)), at(9, 30, 0));
        assert_eq!(t.last_occurrence(at(9, 29, 59)), at(8, 30, 0));
        assert_eq!(t.next_occurrence(at(9, 30, 0)), at(10, 30, 0));
        assert_eq!(t.next_occurrence(at(9, 0, 0)), at(9, 30, 0));
    }

    /// The case the previous-day arithmetic exists for: an hourly item early
    /// enough in the day that the hour before it is yesterday.
    #[test]
    fn hourly_before_its_minute_in_the_first_hour_reaches_yesterday() {
        let last = hourly(30).last_occurrence(at(0, 10, 0));
        assert_eq!((last.day, last.hour, last.minute), (28, 23, 30));
    }

    #[test]
    fn a_daily_item_reaches_back_to_yesterday_until_its_time_comes_round() {
        let t = daily(9, 0);
        assert_eq!(t.last_occurrence(at(9, 0, 0)), at(9, 0, 0));
        assert_eq!(t.last_occurrence(at(23, 59, 59)), at(9, 0, 0));
        let before = t.last_occurrence(at(8, 59, 59));
        assert_eq!((before.day, before.hour, before.minute), (28, 9, 0));
        let next = t.next_occurrence(at(9, 0, 0));
        assert_eq!((next.day, next.hour, next.minute), (30, 9, 0));
    }

    /// Firing is decided by comparing occurrences, so the same occurrence has to
    /// compare equal across every `now` that belongs to it.
    #[test]
    fn an_occurrence_is_stable_across_the_hour_it_belongs_to() {
        let t = hourly(0);
        let first = t.last_occurrence(at(9, 0, 0));
        for minute in 0..60 {
            for second in 0..60 {
                assert_eq!(t.last_occurrence(at(9, minute, second)), first);
            }
        }
    }

    /// Every occurrence of a day, walked a minute at a time: `last` and `next`
    /// have to agree about where the boundaries are, or an item fires twice at
    /// one and not at all at the other.
    #[test]
    fn last_and_next_agree_across_a_whole_day() {
        for trigger in [every(15), every(60), hourly(0), hourly(30), daily(13, 45)] {
            for minute in 0..MINUTES_PER_DAY {
                let now = at(minute / 60, minute % 60, 30);
                let last = trigger.last_occurrence(now);
                let next = trigger.next_occurrence(now);
                assert!(
                    last.minute_of_day() <= minute || last.day == 28,
                    "{trigger:?} at {minute}: last is in the future"
                );
                assert!(
                    next.minute_of_day() > minute || next.day == 30,
                    "{trigger:?} at {minute}: next is in the past"
                );
                // Standing on the occurrence itself, the next one must be a
                // different moment — for a once-a-day item that is the same
                // minute tomorrow, which is why the whole `LocalTime` is
                // compared and not just the time of day.
                let on_it = trigger.last_occurrence(now);
                assert_ne!(
                    trigger.next_occurrence(on_it),
                    on_it,
                    "{trigger:?} would fire twice at {minute}"
                );
            }
        }
    }

    #[test]
    fn a_month_boundary_rolls_the_month_and_the_year() {
        let first_of_month = LocalTime {
            year: 2026,
            month: 9,
            day: 1,
            hour: 0,
            minute: 10,
            second: 0,
        };
        let back = hourly(30).last_occurrence(first_of_month);
        assert_eq!((back.year, back.month, back.day), (2026, 8, 31));

        let new_year = LocalTime {
            year: 2027,
            month: 1,
            day: 1,
            ..first_of_month
        };
        let back = hourly(30).last_occurrence(new_year);
        assert_eq!((back.year, back.month, back.day), (2026, 12, 31));
    }

    /// 2024 is a leap year and 2100 is not, which is the rule a naive
    /// "divisible by four" gets wrong.
    #[test]
    fn the_date_arithmetic_knows_the_leap_year_rules() {
        for (year, month, day, expect) in [
            (2024, 3, 1, (2024, 2, 29)),
            (2025, 3, 1, (2025, 2, 28)),
            (2100, 3, 1, (2100, 2, 28)),
            (2000, 3, 1, (2000, 2, 29)),
        ] {
            let previous = LocalTime {
                year,
                month,
                day,
                hour: 12,
                minute: 0,
                second: 0,
            }
            .previous_day();
            assert_eq!((previous.year, previous.month, previous.day), expect);
        }
    }

    #[test]
    fn civil_days_round_trip() {
        for days in -20_000..20_000 {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
    }

    /// A hand-edited settings file must not be able to divide by zero or park an
    /// item on an hour that does not exist.
    #[test]
    fn nonsense_values_are_held_to_the_grid() {
        assert_eq!(every(0).last_occurrence(at(9, 7, 0)), at(9, 7, 0));
        assert_eq!(every(99_999).last_occurrence(at(9, 7, 0)), at(0, 0, 0));
        assert_eq!(daily(99, 99).last_minute_of_day(), 23 * 60 + 59);
    }

    #[test]
    fn times_are_written_on_a_twenty_four_hour_clock() {
        assert_eq!(format_time(0), "00:00");
        assert_eq!(format_time(9 * 60), "09:00");
        assert_eq!(format_time(13 * 60 + 5), "13:05");
        assert_eq!(format_time(23 * 60 + 59), "23:59");
    }

    /// Every interval the dialog offers has to divide the day, or its last slot
    /// before midnight would be short and the schedule would jump there.
    #[test]
    fn every_offered_interval_divides_a_day() {
        for &interval in INTERVALS {
            assert_eq!(MINUTES_PER_DAY % interval, 0, "{interval} does not");
        }
    }
}
