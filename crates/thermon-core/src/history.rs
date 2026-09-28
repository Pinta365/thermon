//! Tiered in-memory history of every numeric series.
//!
//! Each series keeps one ring per tier. Tier 0 stores every sample; coarser
//! tiers store the mean of `step` base samples. Tiers flush on a shared tick
//! counter, so all series stay aligned in time. Missing values are NaN.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
    /// Base samples per point.
    pub step: u32,
    /// Points kept.
    pub capacity: usize,
}

/// Tiers covering five minutes at the sampling interval, then one hour at
/// roughly ten seconds and 24 hours at roughly one minute.
pub fn default_tiers(base: Duration) -> Vec<Tier> {
    const MINUTE_MS: u128 = 60_000;
    let base_ms = base.as_millis().max(1);
    let tier = |point_ms: u128, span_ms: u128, minimum_capacity: usize| {
        let step = ((point_ms + base_ms / 2) / base_ms)
            .max(1)
            .min(u128::from(u32::MAX)) as u32;
        let actual_ms = base_ms.saturating_mul(u128::from(step)).max(1);
        let capacity = span_ms
            .saturating_add(actual_ms - 1)
            .checked_div(actual_ms)
            .unwrap_or(u128::MAX)
            .max(minimum_capacity as u128)
            .min(usize::MAX as u128) as usize;
        Tier { step, capacity }
    };
    vec![
        tier(base_ms, 5 * MINUTE_MS, 2),
        tier(10_000, 60 * MINUTE_MS, 1),
        tier(MINUTE_MS, 24 * 60 * MINUTE_MS, 1),
    ]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryResponse {
    /// Spacing between points.
    pub interval_ms: u64,
    /// Timestamp of the last point; point `i` of `n` is at
    /// `end_ts_ms - (n - 1 - i) * interval_ms`. A coarse point is the mean of
    /// its window and is stamped at the window's middle.
    pub end_ts_ms: u64,
    /// Oldest first. `null` where the value was missing. Series may be shorter
    /// than others if they appeared later; they still end at `end_ts_ms`.
    pub series: BTreeMap<String, Vec<Option<f32>>>,
}

#[derive(Debug)]
struct Series {
    rings: Vec<VecDeque<f32>>,
    /// Per coarse tier (index = tier - 1): running (sum, count of non-NaN).
    acc: Vec<(f32, u32)>,
}

#[derive(Debug)]
pub struct History {
    base: Duration,
    tiers: Vec<Tier>,
    series: HashMap<String, Series>,
    tick: u64,
    /// Timestamp of the last point written to each tier (window middle).
    tier_end_ms: Vec<u64>,
    /// Wall time of the first sample in each tier's open window.
    window_start_ms: Vec<Option<u64>>,
    /// (wall, boot) time of the previous input, for gap detection.
    last_input: Option<(u64, u64)>,
}

impl History {
    pub fn new(base: Duration, tiers: &[Tier]) -> Self {
        assert!(
            !tiers.is_empty() && tiers[0].step == 1,
            "tier 0 must store every sample"
        );
        History {
            base,
            tiers: tiers.to_vec(),
            series: HashMap::new(),
            tick: 0,
            tier_end_ms: vec![0; tiers.len()],
            window_start_ms: vec![None; tiers.len()],
            last_input: None,
        }
    }

    /// Record one base sample. Known series absent from `values` get NaN.
    /// Uses `ts_ms` both to stamp and to detect gaps; see [`Self::push_clocked`].
    pub fn push(&mut self, ts_ms: u64, values: &[(String, f32)]) {
        self.push_clocked(ts_ms, ts_ms, values);
    }

    /// Record one sample stamped `wall_ms`. Gaps are measured on `boot_ms`
    /// (counts suspend, never steps) and filled with empty points.
    pub fn push_clocked(&mut self, wall_ms: u64, boot_ms: u64, values: &[(String, f32)]) {
        if let Some((prev_wall, prev_boot)) = self.last_input {
            let base_ms = self.base.as_millis().max(1).min(u128::from(u64::MAX)) as u64;
            let gap_ms = boot_ms.saturating_sub(prev_boot);
            if boot_ms > prev_boot && gap_ms.saturating_mul(2) > base_ms.saturating_mul(3) {
                let missing = gap_ms
                    .saturating_add(base_ms / 2)
                    .checked_div(base_ms)
                    .unwrap_or(u64::MAX)
                    .saturating_sub(1);
                let largest_span_samples = self
                    .tiers
                    .iter()
                    .map(|tier| u64::from(tier.step).saturating_mul(tier.capacity as u64))
                    .max()
                    .unwrap_or(0);
                for index in 1..=missing.min(largest_span_samples) {
                    self.push_tick(prev_wall.saturating_add(index.saturating_mul(base_ms)), &[]);
                }
            }
        }
        self.push_tick(wall_ms, values);
        self.last_input = Some((wall_ms, boot_ms));
    }

    fn push_tick(&mut self, ts_ms: u64, values: &[(String, f32)]) {
        let n_tiers = self.tiers.len();
        for (name, _) in values {
            if !self.series.contains_key(name) {
                self.series.insert(
                    name.clone(),
                    Series {
                        rings: self
                            .tiers
                            .iter()
                            .map(|t| VecDeque::with_capacity(t.capacity))
                            .collect(),
                        acc: vec![(0.0, 0); n_tiers - 1],
                    },
                );
            }
        }
        let current: HashMap<&str, f32> = values.iter().map(|(n, v)| (n.as_str(), *v)).collect();

        self.tick += 1;
        for start in &mut self.window_start_ms {
            start.get_or_insert(ts_ms);
        }
        let flush: Vec<bool> = self
            .tiers
            .iter()
            .map(|t| self.tick.is_multiple_of(u64::from(t.step)))
            .collect();

        for (name, s) in &mut self.series {
            let v = current.get(name.as_str()).copied().unwrap_or(f32::NAN);
            for (i, tier) in self.tiers.iter().enumerate() {
                let point = if i == 0 {
                    v
                } else {
                    let acc = &mut s.acc[i - 1];
                    if !v.is_nan() {
                        acc.0 += v;
                        acc.1 += 1;
                    }
                    if !flush[i] {
                        continue;
                    }
                    let mean = if acc.1 > 0 {
                        acc.0 / acc.1 as f32
                    } else {
                        f32::NAN
                    };
                    *acc = (0.0, 0);
                    mean
                };
                let ring = &mut s.rings[i];
                if ring.len() == tier.capacity {
                    ring.pop_front();
                }
                ring.push_back(point);
            }
        }
        for (i, f) in flush.iter().enumerate() {
            if *f {
                let start = self.window_start_ms[i].take().unwrap_or(ts_ms);
                self.tier_end_ms[i] = start + ts_ms.saturating_sub(start) / 2;
            }
        }
        if flush.last().copied().unwrap_or(false) {
            self.series.retain(|_, series| {
                series
                    .rings
                    .iter()
                    .any(|ring| ring.iter().any(|value| !value.is_nan()))
            });
        }
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.series.keys().cloned().collect();
        names.sort();
        names
    }

    /// The last `range` of `names` (all series if empty) from the finest tier
    /// that covers it. Unknown names are omitted.
    pub fn query(&self, names: &[String], range: Duration) -> HistoryResponse {
        let base_ms = self.base.as_millis() as u64;
        let range_ms = range.as_millis() as u64;
        let tier = self
            .tiers
            .iter()
            .position(|t| {
                u64::from(t.step)
                    .saturating_mul(base_ms)
                    .saturating_mul(t.capacity as u64)
                    >= range_ms
            })
            .unwrap_or(self.tiers.len() - 1);
        let interval_ms = u64::from(self.tiers[tier].step).saturating_mul(base_ms);
        let points = range_ms.div_ceil(interval_ms.max(1)).max(1) as usize;

        let all;
        let names = if names.is_empty() {
            all = self.names();
            &all
        } else {
            names
        };
        let series = names
            .iter()
            .filter_map(|n| {
                let ring = &self.series.get(n)?.rings[tier];
                let skip = ring.len().saturating_sub(points);
                let values = ring
                    .iter()
                    .skip(skip)
                    .map(|v| (!v.is_nan()).then_some(*v))
                    .collect();
                Some((n.clone(), values))
            })
            .collect();

        HistoryResponse {
            interval_ms,
            end_ts_ms: self.tier_end_ms[tier],
            series,
        }
    }
}

/// Parse `90s`, `15m`, `1h`, `24h`, or plain seconds.
pub fn parse_range(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last()? {
        (i, 's') => (&s[..i], 1),
        (i, 'm') => (&s[..i], 60),
        (i, 'h') => (&s[..i], 3600),
        (i, 'd') => (&s[..i], 86_400),
        _ => (s, 1),
    };
    let n: u64 = num.parse().ok()?;
    let seconds = n.checked_mul(mult)?;
    (seconds > 0 && seconds <= 7 * 86_400).then(|| Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIERS: [Tier; 2] = [
        Tier {
            step: 1,
            capacity: 4,
        },
        Tier {
            step: 3,
            capacity: 2,
        },
    ];

    fn h() -> History {
        History::new(Duration::from_secs(1), &TIERS)
    }

    fn push(h: &mut History, ts: u64, vals: &[(&str, f32)]) {
        let vals: Vec<(String, f32)> = vals.iter().map(|(n, v)| (n.to_string(), *v)).collect();
        h.push(ts, &vals);
    }

    #[test]
    fn fine_tier_rolls_over() {
        let mut h = h();
        for i in 1..=6 {
            push(&mut h, i * 1000, &[("a", i as f32)]);
        }
        let r = h.query(&["a".into()], Duration::from_secs(4));
        assert_eq!(r.interval_ms, 1000);
        assert_eq!(r.end_ts_ms, 6000);
        assert_eq!(r.series["a"], [Some(3.0), Some(4.0), Some(5.0), Some(6.0)]);

        // Asking for less returns fewer points.
        let r = h.query(&["a".into()], Duration::from_secs(2));
        assert_eq!(r.series["a"], [Some(5.0), Some(6.0)]);
    }

    #[test]
    fn coarse_tier_averages_and_skips_nan() {
        let mut h = h();
        push(&mut h, 1000, &[("a", 1.0), ("b", 10.0)]);
        push(&mut h, 2000, &[("a", 2.0)]); // b missing -> NaN, excluded from mean
        push(&mut h, 3000, &[("a", 3.0), ("b", 20.0)]);
        push(&mut h, 4000, &[("a", 4.0)]);
        push(&mut h, 5000, &[("a", 5.0)]);
        push(&mut h, 6000, &[("a", 6.0)]);

        // 5 s > tier 0's 4 s capacity -> tier 1.
        let r = h.query(&[], Duration::from_secs(5));
        assert_eq!(r.interval_ms, 3000);
        // Mean of the samples at 4 s, 5 s and 6 s, stamped at their middle.
        assert_eq!(r.end_ts_ms, 5000);
        assert_eq!(r.series["a"], [Some(2.0), Some(5.0)]);
        assert_eq!(r.series["b"], [Some(15.0), None]);

        let r = h.query(&["b".into()], Duration::from_secs(2));
        assert_eq!(r.series["b"], [None, None]);
    }

    #[test]
    fn late_series_is_shorter_but_aligned() {
        let mut h = h();
        push(&mut h, 1000, &[("a", 1.0)]);
        push(&mut h, 2000, &[("a", 2.0), ("new", 7.0)]);
        let r = h.query(&[], Duration::from_secs(4));
        assert_eq!(r.series["a"].len(), 2);
        assert_eq!(r.series["new"], [Some(7.0)]);
    }

    #[test]
    fn unknown_names_omitted_and_huge_range_uses_last_tier() {
        let mut h = h();
        push(&mut h, 1000, &[("a", 1.0)]);
        let r = h.query(&["nope".into(), "a".into()], Duration::from_secs(86_400));
        assert_eq!(r.interval_ms, 3000);
        assert!(!r.series.contains_key("nope"));
        assert_eq!(r.series["a"], Vec::<Option<f32>>::new());
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_range("15m"), Some(Duration::from_secs(900)));
        assert_eq!(parse_range("24h"), Some(Duration::from_secs(86_400)));
        assert_eq!(parse_range("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_range("0m"), None);
        assert_eq!(parse_range("m"), None);
        assert_eq!(parse_range(""), None);
        assert_eq!(parse_range("999999999999999999d"), None);
        assert_eq!(parse_range("8d"), None);
    }

    #[test]
    fn default_tiers_cover_a_day_at_supported_intervals() {
        for base_ms in [200, 1_000, 2_000, 60_000] {
            let base = Duration::from_millis(base_ms);
            let tiers = default_tiers(base);
            assert_eq!(tiers[0].step, 1);
            let last = tiers.last().unwrap();
            let covered_ms = u64::from(last.step)
                .saturating_mul(base_ms)
                .saturating_mul(last.capacity as u64);
            assert!(covered_ms >= Duration::from_secs(24 * 60 * 60).as_millis() as u64);
        }
    }

    #[test]
    fn long_gap_preserves_recent_nulls_and_old_coarse_data() {
        let tiers = default_tiers(Duration::from_secs(1));
        let mut h = History::new(Duration::from_secs(1), &tiers);
        for second in 0..10 {
            push(&mut h, second * 1_000, &[("a", second as f32)]);
        }
        push(&mut h, 7_200_000, &[("a", 42.0)]);

        let recent = h.query(&["a".into()], Duration::from_secs(5 * 60));
        assert_eq!(recent.series["a"].last(), Some(&Some(42.0)));
        assert!(
            recent.series["a"][..recent.series["a"].len() - 1]
                .iter()
                .all(Option::is_none)
        );

        let day = h.query(&["a".into()], Duration::from_secs(24 * 60 * 60));
        assert!(day.series["a"].iter().any(Option::is_some));
    }

    #[test]
    fn gaps_are_measured_on_the_boot_clock() {
        let tiers = default_tiers(Duration::from_secs(1));
        // Wall clock jumps an hour ahead (NTP) while the boot clock ticks 1 s.
        let mut h = History::new(Duration::from_secs(1), &tiers);
        for s in 0..5u64 {
            h.push_clocked(s * 1000, s * 1000, &[("a".into(), 1.0)]);
        }
        h.push_clocked(3_600_000 + 5000, 5000, &[("a".into(), 2.0)]);
        let r = h.query(&["a".into()], Duration::from_secs(10));
        assert_eq!(
            r.series["a"],
            [Some(1.0); 5]
                .into_iter()
                .chain([Some(2.0)])
                .collect::<Vec<_>>()
        );

        // A real 30 s suspend: both clocks move; 29 empty points appear.
        let mut h = History::new(Duration::from_secs(1), &tiers);
        h.push_clocked(0, 0, &[("a".into(), 1.0)]);
        h.push_clocked(30_000, 30_000, &[("a".into(), 2.0)]);
        let r = h.query(&["a".into()], Duration::from_secs(60));
        assert_eq!(r.series["a"].len(), 31);
        assert_eq!(r.series["a"].iter().filter(|v| v.is_none()).count(), 29);
    }

    #[test]
    fn top_tier_flush_prunes_stale_series() {
        let mut h = h();
        for index in 0..1_000 {
            push(
                &mut h,
                1_000,
                &[(format!("transient-{index}").as_str(), 1.0)],
            );
        }
        for tick in 2..=12 {
            push(&mut h, tick * 1_000, &[]);
        }
        assert!(h.names().is_empty());
    }
}
