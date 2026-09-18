//! Time: epoch milliseconds, the wall clock `/usage` prints, and the injectable [`Clock`].

use chrono::{DateTime, Datelike, Local, Month, NaiveDate, NaiveDateTime, TimeDelta, Timelike};
use std::time::{SystemTime, UNIX_EPOCH};

/// The current epoch in milliseconds.
///
/// Times are **passed around as plain `u64`** — a newtype would protect nothing, since every time in
/// this repo (`deadline_ms` / `spawned_at_ms` / `until_ms` …) is epoch ms.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// `2026-07-27T12:00:00.000Z`
pub fn iso8601(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

// ── wall clock ─────────────────────────────────────────────────────────────
// The Bridge and the TUI run on the same host in the same zone, so no zone conversion is needed:
// "subtracting wall clock from wall clock" is enough. The calendar itself is left to `chrono`; all
// that lives here is "how to read what `/usage` writes".

/// A wall clock without a time zone (to the minute — no seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WallClock(NaiveDateTime);

impl WallClock {
    /// The time zone is **fixed to Asia/Tokyo**. The display side ([`limited_notice`](crate::bridge::turn::limited_notice)) already is
    /// (the text says "（Asia/Tokyo）"), and the Bridge and the TUI run on the same host in the same zone.
    /// To move to another zone, change both places together.
    const TOKYO_OFFSET_MS: i64 = 9 * 3_600_000;

    /// One wall-clock time to the minute. Nonexistent dates (Feb 30 etc.) are None.
    pub fn new(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Option<Self> {
        NaiveDate::from_ymd_opt(year, month, day)?
            .and_hms_opt(hour, minute, 0)
            .map(Self)
    }

    /// Now on this host, **truncated to the minute** (everything below compares at minute granularity).
    pub fn now() -> Self {
        Self::of(Local::now().naive_local())
    }

    /// Epoch ms → Tokyo wall clock. Shift, then read as a plain wall clock (= local time at +9:00).
    /// Unrepresentable values fall back to the epoch, which never happens with real epoch ms.
    pub fn tokyo(ms: u64) -> Self {
        let shifted = ms as i64 + Self::TOKYO_OFFSET_MS;
        Self::of(
            DateTime::from_timestamp_millis(shifted)
                .unwrap_or_default()
                .naive_utc(),
        )
    }

    /// Wraps it, dropping anything below a second.
    fn of(t: NaiveDateTime) -> Self {
        Self(
            t.with_second(0)
                .and_then(|t| t.with_nanosecond(0))
                .unwrap_or(t),
        )
    }

    /// The epoch ms of this Tokyo wall-clock time.
    fn epoch_ms(&self) -> Option<u64> {
        u64::try_from(self.0.and_utc().timestamp_millis() - Self::TOKYO_OFFSET_MS).ok()
    }

    /// N minutes later.
    pub(super) fn plus_minutes(&self, minutes: i64) -> Self {
        Self(
            self.0
                .checked_add_signed(TimeDelta::minutes(minutes))
                .unwrap_or(self.0),
        )
    }

    /// Minutes elapsed if `to` is at or after `from`, None if `to` is earlier.
    pub fn minutes_to(from: &WallClock, to: &WallClock) -> Option<i64> {
        let diff = (to.0 - from.0).num_minutes();
        (diff >= 0).then_some(diff)
    }

    /// Turns `/usage`'s `resets …` clause into "the next time that wall clock comes around". Handles the two
    /// forms the TUI prints (`Jun 28 at 5:30pm (Asia/Tokyo)` and a bare `5pm` / `3:59am`); None if unreadable
    /// (the caller treats None as "not enough data" and errs on the safe side).
    pub fn parse_reset(reset: &str, now: &WallClock) -> Option<WallClock> {
        let (hour, minute) = Self::clock_time(reset)?;
        if let Some((month, day)) = Self::month_day(reset) {
            // With month and day: put it in this year, and if that is past, next year (the Dec→Jan window seen from the old year)
            let cand = Self::new(now.year(), month, day, hour, minute)?;
            return Some(if WallClock::minutes_to(now, &cand).is_some() {
                cand
            } else {
                Self::new(now.year() + 1, month, day, hour, minute)?
            });
        }
        // Time only: that time today, or tomorrow if it has passed (including exactly now)
        let cand = Self::new(now.year(), now.month(), now.day(), hour, minute)?;
        Some(
            if WallClock::minutes_to(now, &cand).is_some_and(|m| m > 0) {
                cand
            } else {
                cand.plus_minutes(24 * 60)
            },
        )
    }

    /// Turns the reset time Claude Code announced into epoch ms. The parsing itself is left to the same
    /// [`Self::parse_reset`] as `/usage` — it is the same format from the same TUI, so with two copies
    /// one always gets fixed alone and they drift apart.
    pub fn parse_reset_epoch(text: &str, now_ms: u64) -> Option<u64> {
        Self::parse_reset(text, &Self::tokyo(now_ms))?.epoch_ms()
    }

    /// The RFC3339 Claude Code writes to history (`2026-07-30T14:00:00.000Z`) to epoch ms.
    pub(crate) fn parse_iso8601_ms(s: &str) -> Option<u64> {
        u64::try_from(DateTime::parse_from_rfc3339(s).ok()?.timestamp_millis()).ok()
    }

    /// Same style as `/usage`'s `resets …` (`Jul 1 at 5:00 pm`).
    pub(super) fn reset_like(&self) -> String {
        self.format("%b %-d at %-I:%M %P").to_string()
    }

    /// Hand-written `\b(\d{1,2})(?::(\d{2}))?\s*(am|pm)\b` → 24-hour `(hour, minute)`.
    fn clock_time(s: &str) -> Option<(u32, u32)> {
        let b = s.as_bytes();
        for i in 0..b.len() {
            // `\b` — if the character before the digit is a word character, we are mid-number (the regex wouldn't start here either)
            if !b[i].is_ascii_digit() || (i > 0 && Self::is_word(b[i - 1] as char)) {
                continue;
            }
            let digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits > 2 {
                continue; // `\d{1,2}` can't be followed by another digit
            }
            let mut j = i + digits;
            let mut minute = 0;
            if b.get(j) == Some(&b':')
                && b[j + 1..]
                    .iter()
                    .take(2)
                    .filter(|c| c.is_ascii_digit())
                    .count()
                    == 2
            {
                minute = s[j + 1..j + 3].parse().unwrap_or(0);
                j += 3;
            }
            while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                j += 1;
            }
            let Some(tag) = s.get(j..j + 2) else { continue };
            let pm = tag.eq_ignore_ascii_case("pm");
            if !pm && !tag.eq_ignore_ascii_case("am") {
                continue;
            }
            if b.get(j + 2).is_some_and(|c| Self::is_word(*c as char)) {
                continue; // the `am` in `spam` is not am
            }
            let hour12: u32 = s[i..i + digits].parse().unwrap_or(0);
            if !(1..=12).contains(&hour12) {
                return None; // a "broken time" is given up on (no search for the next candidate)
            }
            return Some((
                match (hour12, pm) {
                    (12, false) => 0,
                    (12, true) => 12,
                    (h, true) => h + 12,
                    (h, false) => h,
                },
                minute,
            ));
        }
        None
    }

    /// Reads the **first** match of `\b([A-Za-z]{3,})\s+(\d{1,2})\b` as a month name. If the first
    /// match isn't a month name (`tomorrow 8am`), give up there and fall back to "time only".
    fn month_day(s: &str) -> Option<(u32, u32)> {
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if !b[i].is_ascii_alphabetic() || (i > 0 && Self::is_word(b[i - 1] as char)) {
                i += 1;
                continue;
            }
            let word = b[i..]
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .count();
            let mut j = i + word;
            if word < 3 || !b.get(j).is_some_and(u8::is_ascii_whitespace) {
                i += word;
                continue;
            }
            while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                j += 1;
            }
            let digits = b[j..].iter().take_while(|c| c.is_ascii_digit()).count();
            // `\d{1,2}\b` — no match with 3+ digits, or a word character right after the digits
            if digits == 0
                || digits > 2
                || b.get(j + digits).is_some_and(|c| Self::is_word(*c as char))
            {
                i = j;
                continue;
            }
            let month = s[i..i + 3].parse::<Month>().ok()?.number_from_month();
            return Some((month, s[j..j + digits].parse().ok()?));
        }
        None
    }

    /// The regex `\w` (word character). `\b` checks both sides with this.
    fn is_word(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_'
    }
}

/// `year()` / `hour()` / `format()` — calendar reading and writing use `chrono`'s as is.
impl std::ops::Deref for WallClock {
    type Target = NaiveDateTime;
    fn deref(&self) -> &NaiveDateTime {
        &self.0
    }
}


// ── the clock ──

/// The time. Implemented by [`SystemClock`]; tests move a `fake::FakeClock` by hand.
pub trait Clock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        crate::clock::now_ms()
    }
}

pub type ClockRef = std::sync::Arc<dyn Clock>;

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub struct FakeClock(pub AtomicU64);

    impl FakeClock {
        pub fn at(ms: u64) -> Arc<FakeClock> {
            Arc::new(FakeClock(AtomicU64::new(ms)))
        }
        pub fn advance(&self, ms: u64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_matches_known_instants() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        // `date -u -j -f %Y-%m-%dT%H:%M:%S 2026-07-27T12:34:56 +%s` = 1785155696
        assert_eq!(iso8601(1_785_155_696_007), "2026-07-27T12:34:56.007Z");
        assert_eq!(iso8601(1_709_164_800_000), "2024-02-29T00:00:00.000Z"); // leap day
    }

    /// One wall-clock time. Tests aren't about the date, so make it writable on one line.
    fn wc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> WallClock {
        WallClock::new(year, month, day, hour, minute).expect("valid wall clock")
    }

    /// Time only means "that time today, or tomorrow if past".
    /// With a month and day, that day. The time zone is fixed to Asia/Tokyo (same assumption as turn::limited_notice).
    #[test]
    fn reset_time_is_read_in_tokyo_time() {
        let now = 1_785_387_600_000u64; // 2026-07-30T05:00:00Z = the 30th 14:00 JST
        // The 30th 23:00 JST = the 30th 14:00Z
        assert_eq!(
            WallClock::parse_reset_epoch("resets at 11pm", now),
            Some(1_785_420_000_000)
        );
        // A time already past rolls to the next day (13:00 JST < 14:00 JST)
        assert_eq!(
            WallClock::parse_reset_epoch("resets at 1pm", now),
            Some(1_785_470_400_000)
        );
        // With month and day: 2026-08-01 09:30 JST = 2026-08-01T00:30:00Z
        assert_eq!(
            WallClock::parse_reset_epoch("resets Aug 1 at 9:30am", now),
            Some(1_785_544_200_000)
        );
        // The 12-hour edge. 12am = 00:00 — today's midnight is past, so tomorrow's 00:00 JST
        assert_eq!(
            WallClock::parse_reset_epoch("resets at 12am", now),
            Some(1_785_423_600_000)
        );
        // `\b` — digits in the middle of a word are not a time
        assert_eq!(WallClock::parse_reset_epoch("at11pm", now), None);
        assert_eq!(WallClock::parse_reset_epoch("resets soon", now), None);
    }

    #[test]
    fn minutes_between_same_day() {
        let from = wc(2026, 7, 29, 10, 0);
        let to = wc(2026, 7, 29, 12, 30);
        assert_eq!(WallClock::minutes_to(&from, &to), Some(150));
        assert_eq!(WallClock::minutes_to(&to, &from), None); // the past is None
    }

    #[test]
    fn minutes_between_crosses_month_and_year() {
        let from = wc(2026, 12, 31, 23, 0);
        let to = wc(2027, 1, 1, 1, 0);
        assert_eq!(WallClock::minutes_to(&from, &to), Some(120));
    }

    #[test]
    fn minutes_between_counts_the_leap_day() {
        // 2028 is a leap year — 2/28 → 3/1 is 2 days (1 day in 2027)
        let day = 24 * 60;
        let span = |year| WallClock::minutes_to(&wc(year, 2, 28, 0, 0), &wc(year, 3, 1, 0, 0));
        assert_eq!(span(2028), Some(2 * day));
        assert_eq!(span(2027), Some(day));
        // A year divisible by 100 but not 400 is not a leap year (2100/2 has 28 days)
        assert_eq!(span(2100), Some(day));
    }

    #[test]
    fn parse_reset_clock_bare_time_rolls_to_tomorrow_if_past() {
        let now = wc(2026, 7, 29, 18, 0);
        let future_today = WallClock::parse_reset("11:30pm", &now).unwrap();
        assert_eq!((future_today.day(), future_today.hour()), (29, 23));
        let past_today = WallClock::parse_reset("5:30pm", &now).unwrap(); // 17:30 is already past (now=18:00)
        assert_eq!(
            past_today.day(),
            30,
            "rolls to tomorrow when the bare time already passed"
        );
    }

    #[test]
    fn parse_reset_clock_bare_time_rolls_over_month_and_year_ends() {
        let eom = wc(2026, 6, 30, 18, 0);
        let next = WallClock::parse_reset("5pm", &eom).unwrap();
        assert_eq!(
            (next.year(), next.month(), next.day(), next.hour()),
            (2026, 7, 1, 17)
        );
        let eoy = wc(2026, 12, 31, 23, 30);
        let next = WallClock::parse_reset("11:00pm", &eoy).unwrap();
        assert_eq!((next.year(), next.month(), next.day()), (2027, 1, 1));
        // The day after 2/28 in a leap year is 2/29
        let leap = wc(2028, 2, 28, 23, 0);
        let next = WallClock::parse_reset("10pm", &leap).unwrap();
        assert_eq!((next.month(), next.day()), (2, 29));
    }

    #[test]
    fn parse_reset_clock_dated_rolls_to_next_year_if_past() {
        let now = wc(2026, 7, 29, 0, 0);
        let past = WallClock::parse_reset("Jun 28 at 5:30pm", &now).unwrap();
        assert_eq!(
            past.year(),
            2027,
            "Jun 28 already passed this year, so it means next year's Jun 28"
        );
        let future = WallClock::parse_reset("Dec 1 at 5:30pm", &now).unwrap();
        assert_eq!(future.year(), 2026);
    }

    #[test]
    fn parse_reset_clock_shapes_and_junk() {
        let now = wc(2026, 7, 29, 9, 0);
        // The full real text (with the time zone note). Also the 12am/12pm wraparound
        let full = WallClock::parse_reset("Jul 1 at 5pm (Asia/Tokyo)", &now).unwrap();
        assert_eq!(
            (
                full.year(),
                full.month(),
                full.day(),
                full.hour(),
                full.minute()
            ),
            (2027, 7, 1, 17, 0)
        );
        assert_eq!(WallClock::parse_reset("12am", &now).unwrap().hour(), 0);
        assert_eq!(WallClock::parse_reset("12:15pm", &now).unwrap().hour(), 12);
        assert_eq!(WallClock::parse_reset("3:59AM", &now).unwrap().minute(), 59);
        // A word that isn't a month name falls back to "time only"
        let tomorrow = WallClock::parse_reset("tomorrow 8am", &now).unwrap();
        assert_eq!((tomorrow.month(), tomorrow.day()), (7, 30));
        assert_eq!(WallClock::parse_reset("", &now), None);
        assert_eq!(WallClock::parse_reset("in 5 hours", &now), None);
        assert_eq!(WallClock::parse_reset("13pm", &now), None); // outside 1–12
        assert_eq!(WallClock::parse_reset("5:30 spam", &now), None); // am/pm word boundary
    }

    #[test]
    fn fake_clock_moves_only_when_told() {
        let c = fake::FakeClock::at(1_000);
        assert_eq!(c.now_ms(), 1_000);
        c.advance(500);
        assert_eq!(c.now_ms(), 1_500);
    }
}
