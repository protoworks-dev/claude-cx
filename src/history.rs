//! Usage over time: one line per reading in `history.jsonl`, and the sums
//! the dashboard draws from it (trend, burn rate, time to the switch line,
//! pace against an even week).
//!
//! A reading is written when a number moved or ten minutes have passed, so
//! an idle account costs six lines an hour. Lines older than two weeks are
//! dropped when the file grows past a few megabytes.

use crate::store::Usage;
use crate::util;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// How long a reading is kept, in milliseconds: two weeks. [`History::load`]
/// skips anything older.
pub const KEEP_MS: i64 = 14 * 86_400_000;
/// The longest gap between two readings of one account, in milliseconds:
/// ten minutes. A reading that has not moved is written once this much
/// time has passed since the last one.
pub const SAMPLE_EVERY_MS: i64 = 10 * 60_000;
const COMPACT_ABOVE_BYTES: u64 = 4 * 1024 * 1024;

/// One usage reading of one account: a line of `history.jsonl`. The field
/// names are short because the file holds many of them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// When the reading was taken, in milliseconds since the Unix epoch.
    pub t: i64,
    /// The number of the account the reading is for.
    pub a: u32,
    /// Percent of the 5-hour window used, 0 to 100. `None` when the reading
    /// had no such window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h5: Option<f64>,
    /// Percent of the weekly window used, 0 to 100. `None` when the reading
    /// had no such window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<f64>,
    /// Percent used of each per-model weekly limit, by model name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub m: BTreeMap<String, f64>,
}

/// Which limit a series is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// The rolling 5-hour window.
    FiveHour,
    /// The weekly window across all models.
    Week,
    /// The weekly limit of the named model. The name is matched without
    /// regard to case.
    Model(String),
}

impl Sample {
    /// The reading to record for `account` at `now_ms`. A window whose reset
    /// time has passed is recorded as 0 percent.
    pub fn of(account: u32, usage: &Usage, now_ms: i64) -> Sample {
        Sample {
            t: now_ms,
            a: account,
            h5: usage.five_hour.as_ref().and_then(|w| w.pct_at(now_ms)),
            w: usage.seven_day.as_ref().and_then(|w| w.pct_at(now_ms)),
            m: usage
                .models
                .iter()
                .filter_map(|m| m.window.pct_at(now_ms).map(|p| (m.name.clone(), p)))
                .collect(),
        }
    }

    /// The percent used of one limit in this reading. `None` when the reading
    /// does not have that limit.
    pub fn get(&self, key: &Key) -> Option<f64> {
        match key {
            Key::FiveHour => self.h5,
            Key::Week => self.w,
            Key::Model(name) => self
                .m
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| *v),
        }
    }

    /// Did any number move since `earlier`?
    pub fn moved_since(&self, earlier: &Sample) -> bool {
        let moved = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => (a - b).abs() > 0.05,
            (None, None) => false,
            _ => true,
        };
        moved(self.h5, earlier.h5)
            || moved(self.w, earlier.w)
            || self.m.len() != earlier.m.len()
            || self.m.iter().any(|(k, v)| moved(Some(*v), earlier.m.get(k).copied()))
    }
}

fn path(root: &Path) -> PathBuf {
    root.join("history.jsonl")
}

/// Add readings to the file. Called with the state lock held, so two cx
/// processes never interleave a compaction with an append.
pub fn append(root: &Path, samples: &[Sample]) -> Result<()> {
    if samples.is_empty() {
        return Ok(());
    }
    let file = path(root);
    if std::fs::metadata(&file).is_ok_and(|m| m.len() > COMPACT_ABOVE_BYTES) {
        let kept = History::load(root);
        let mut text = String::new();
        let mut all: Vec<&Sample> = kept.by_account.values().flatten().collect();
        all.sort_by_key(|s| s.t);
        for sample in all {
            text.push_str(&serde_json::to_string(sample)?);
            text.push('\n');
        }
        util::atomic_write(&file, text.as_bytes(), 0o600)?;
    }
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&file)?;
    let mut text = String::new();
    for sample in samples {
        text.push_str(&serde_json::to_string(sample)?);
        text.push('\n');
    }
    out.write_all(text.as_bytes())?;
    Ok(())
}

/// The readings kept in `history.jsonl`, grouped by account.
#[derive(Clone, Debug, Default)]
pub struct History {
    /// Each account's readings, by account number, oldest first.
    pub by_account: BTreeMap<u32, Vec<Sample>>,
}

impl History {
    /// Everything from the last two weeks. A line that does not parse (a
    /// write cut short by a crash) is skipped.
    pub fn load(root: &Path) -> History {
        let mut history = History::default();
        let Ok(text) = std::fs::read_to_string(path(root)) else {
            return history;
        };
        let oldest = util::now_ms() - KEEP_MS;
        for line in text.lines() {
            if let Ok(sample) = serde_json::from_str::<Sample>(line)
                && sample.t >= oldest
            {
                history.by_account.entry(sample.a).or_default().push(sample);
            }
        }
        for samples in history.by_account.values_mut() {
            samples.sort_by_key(|s| s.t);
        }
        history
    }

    /// Time and percent for one account and one limit, oldest first.
    pub fn series(&self, account: u32, key: &Key, since_ms: i64) -> Vec<(i64, f64)> {
        self.by_account
            .get(&account)
            .map(|samples| {
                samples
                    .iter()
                    .filter(|s| s.t >= since_ms)
                    .filter_map(|s| s.get(key).map(|p| (s.t, p)))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Percent per hour over the recent past. Readings before the window's last
/// reset are left out, and the last reading is taken to hold until now, so
/// an account that went quiet shows a falling rate.
pub fn burn_rate(series: &[(i64, f64)], now_ms: i64, lookback_ms: i64) -> Option<f64> {
    let recent: Vec<(i64, f64)> = series
        .iter()
        .copied()
        .filter(|(t, _)| *t >= now_ms - lookback_ms && *t <= now_ms)
        .collect();
    // Start after the last drop: a drop is a reset, not negative use.
    let start = recent
        .windows(2)
        .rposition(|pair| pair[1].1 < pair[0].1 - 1.0)
        .map(|i| i + 1)
        .unwrap_or(0);
    let run = &recent[start..];
    let (first, last) = (run.first()?, run.last()?);
    let span_ms = now_ms - first.0;
    if run.len() < 2 || span_ms < 5 * 60_000 {
        return None;
    }
    Some(((last.1 - first.1) / (span_ms as f64 / 3_600_000.0)).max(0.0))
}

/// When an account will reach its switch line if use carries on at the
/// current rate. Made by [`forecast`].
#[derive(Clone, Debug, PartialEq)]
pub struct Forecast {
    /// The rate of use the forecast is based on, in percent per hour.
    pub rate_per_h: f64,
    /// Seconds until the switch line at this rate. `None` when use is flat.
    pub to_line_s: Option<i64>,
    /// The window resets before the line is reached.
    pub reset_first: bool,
}

/// Work out a [`Forecast`] for a window that is `pct` percent used and
/// filling at `rate_per_h` percent per hour, with the switch line at `line`
/// percent. `resets_in_s` is the time in seconds until the window resets,
/// when known. A rate of 0.05 percent per hour or less counts as flat. A
/// flat forecast has `reset_first` set even when the reset time is unknown.
pub fn forecast(pct: f64, rate_per_h: f64, line: f64, resets_in_s: Option<i64>) -> Forecast {
    let to_line_s = if pct >= line {
        Some(0)
    } else if rate_per_h > 0.05 {
        Some(((line - pct) / rate_per_h * 3600.0) as i64)
    } else {
        None
    };
    let reset_first = match (to_line_s, resets_in_s) {
        (Some(line), Some(reset)) => reset > 0 && reset < line,
        (None, _) => true,
        _ => false,
    };
    Forecast {
        rate_per_h,
        to_line_s,
        reset_first,
    }
}

/// The series cut into `count` equal spans between two times. Each span
/// holds its highest reading; a span with none carries the one before it.
pub fn buckets(series: &[(i64, f64)], from_ms: i64, to_ms: i64, count: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; count];
    if count == 0 || to_ms <= from_ms {
        return out;
    }
    let span = (to_ms - from_ms) as f64 / count as f64;
    // The last reading before the range starts the carry.
    let mut carry = series.iter().rev().find(|(t, _)| *t < from_ms).map(|(_, p)| *p);
    let mut index = 0;
    for (i, slot) in out.iter_mut().enumerate() {
        let end = from_ms + (span * (i + 1) as f64) as i64;
        let mut peak: Option<f64> = None;
        while index < series.len() && series[index].0 < end {
            if series[index].0 >= from_ms {
                peak = Some(peak.map_or(series[index].1, |p: f64| p.max(series[index].1)));
                carry = Some(series[index].1);
            }
            index += 1;
        }
        *slot = peak.or(carry);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;

    #[test]
    fn burn_rate_counts_from_the_first_reading_to_now() {
        let now = 100 * HOUR;
        let series = [(now - 60 * MIN, 10.0), (now - 30 * MIN, 20.0), (now - 10 * MIN, 30.0)];
        let rate = burn_rate(&series, now, 2 * HOUR).unwrap();
        assert!((rate - 20.0).abs() < 0.01, "{rate}");
    }

    #[test]
    fn burn_rate_starts_again_after_a_reset() {
        let now = 100 * HOUR;
        let series = [
            (now - 90 * MIN, 80.0),
            (now - 60 * MIN, 95.0),
            (now - 40 * MIN, 0.0),
            (now - 10 * MIN, 5.0),
        ];
        let rate = burn_rate(&series, now, 2 * HOUR).unwrap();
        assert!((rate - 7.5).abs() < 0.01, "{rate}");
    }

    #[test]
    fn burn_rate_needs_two_readings_and_five_minutes() {
        let now = 100 * HOUR;
        assert_eq!(burn_rate(&[(now - 30 * MIN, 10.0)], now, HOUR), None);
        assert_eq!(burn_rate(&[(now - 2 * MIN, 10.0), (now - MIN, 12.0)], now, HOUR), None);
        assert_eq!(burn_rate(&[], now, HOUR), None);
    }

    #[test]
    fn forecast_says_when_the_line_comes_and_whether_the_reset_wins() {
        let f = forecast(70.0, 10.0, 90.0, Some(3 * 3600));
        assert_eq!(f.to_line_s, Some(2 * 3600));
        assert!(!f.reset_first);
        let f = forecast(70.0, 10.0, 90.0, Some(3600));
        assert!(f.reset_first);
        let f = forecast(70.0, 0.0, 90.0, Some(3600));
        assert_eq!(f.to_line_s, None);
        assert!(f.reset_first);
        assert_eq!(forecast(95.0, 3.0, 90.0, None).to_line_s, Some(0));
    }

    #[test]
    fn buckets_keep_the_peak_and_carry_gaps() {
        let series = [(-5, 7.0), (0, 10.0), (5, 30.0), (9, 20.0), (35, 50.0)];
        assert_eq!(
            buckets(&series, 0, 40, 4),
            vec![Some(30.0), Some(20.0), Some(20.0), Some(50.0)]
        );
        assert_eq!(buckets(&[(-5, 7.0)], 0, 40, 2), vec![Some(7.0), Some(7.0)]);
        assert_eq!(buckets(&[], 0, 40, 2), vec![None, None]);
    }

    #[test]
    fn samples_round_trip_through_the_file_and_old_ones_are_dropped() {
        let root = std::env::temp_dir().join(format!("cx-history-{}-{}", std::process::id(), fastrand::u32(..)));
        std::fs::create_dir_all(&root).unwrap();
        let now = util::now_ms();
        let mut fresh = Sample {
            t: now - HOUR,
            a: 2,
            h5: Some(12.0),
            w: Some(40.0),
            ..Sample::default()
        };
        fresh.m.insert("Fable".into(), 8.0);
        let old = Sample {
            t: now - KEEP_MS - HOUR,
            a: 2,
            w: Some(1.0),
            ..Sample::default()
        };
        append(&root, &[old, fresh.clone()]).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(path(&root))
            .unwrap()
            .write_all(b"{\"t\": 12, \"a\"")
            .unwrap();
        let history = History::load(&root);
        assert_eq!(history.by_account[&2], vec![fresh]);
        assert_eq!(
            history.series(2, &Key::Model("fable".into()), 0),
            vec![(now - HOUR, 8.0)]
        );
        assert!(history.series(3, &Key::Week, 0).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_sample_notices_any_number_that_moved() {
        let a = Sample {
            h5: Some(10.0),
            w: Some(20.0),
            ..Sample::default()
        };
        let mut b = a.clone();
        assert!(!b.moved_since(&a));
        b.w = Some(20.5);
        assert!(b.moved_since(&a));
        let mut c = a.clone();
        c.h5 = None;
        assert!(c.moved_since(&a));
    }
}
