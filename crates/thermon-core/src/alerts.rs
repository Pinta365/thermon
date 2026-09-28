//! Debounced transitions from health findings to alert events.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    config::AlertConfig,
    health::{Finding, Verdict},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertState {
    Firing,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    pub ts_ms: u64,
    pub state: AlertState,
    pub key: String,
    pub finding: Finding,
}

#[derive(Debug, Clone)]
enum Track {
    Pending {
        first_seen_ms: u64,
        updates: u64,
        present_updates: u64,
        absent_since_ms: Option<u64>,
    },
    Firing {
        first_fired_wall_ms: u64,
        highest_fired: crate::health::Severity,
        latest: Finding,
        absent_since_ms: Option<u64>,
    },
}

pub struct AlertTracker {
    config: AlertConfig,
    tracks: BTreeMap<String, Track>,
}

impl AlertTracker {
    pub fn new(cfg: &AlertConfig) -> Self {
        Self {
            config: cfg.clone(),
            tracks: BTreeMap::new(),
        }
    }

    pub fn set_config(&mut self, cfg: &AlertConfig) {
        self.config = cfg.clone();
    }

    /// Feed one verdict. Holds are timed on `mono_ms`; alerts are stamped with
    /// `wall_ms`. A key fires again only when its severity rises.
    pub fn update(&mut self, mono_ms: u64, wall_ms: u64, verdict: &Verdict) -> Vec<Alert> {
        if !self.config.enabled {
            let alerts = self
                .tracks
                .iter()
                .filter_map(|(key, track)| match track {
                    Track::Firing { latest, .. } => Some(Alert {
                        ts_ms: wall_ms,
                        state: AlertState::Resolved,
                        key: key.clone(),
                        finding: latest.clone(),
                    }),
                    Track::Pending { .. } => None,
                })
                .collect();
            self.tracks.clear();
            return alerts;
        }

        let mut present = BTreeMap::new();
        for finding in &verdict.findings {
            if finding.severity >= self.config.min_severity {
                let key = finding_key(finding);
                present
                    .entry(key)
                    .and_modify(|existing: &mut Finding| {
                        if finding.severity > existing.severity {
                            *existing = finding.clone();
                        }
                    })
                    .or_insert_with(|| finding.clone());
            }
        }

        let hold_ms = self.config.hold_s.saturating_mul(1_000);
        let mut alerts = Vec::new();
        for key in self.tracks.keys().cloned().collect::<Vec<_>>() {
            let track = self.tracks.remove(&key).unwrap();
            match (track, present.remove(&key)) {
                (
                    Track::Pending {
                        first_seen_ms,
                        updates,
                        present_updates,
                        absent_since_ms: _,
                    },
                    Some(finding),
                ) => {
                    let updates = updates + 1;
                    let present_updates = present_updates + 1;
                    if mono_ms.saturating_sub(first_seen_ms) >= hold_ms
                        && present_updates.saturating_mul(2) >= updates
                    {
                        let alert = firing_alert(wall_ms, &key, finding.clone());
                        alerts.push(alert);
                        self.tracks.insert(
                            key,
                            Track::Firing {
                                first_fired_wall_ms: wall_ms,
                                highest_fired: finding.severity,
                                latest: finding,
                                absent_since_ms: None,
                            },
                        );
                    } else {
                        self.tracks.insert(
                            key,
                            Track::Pending {
                                first_seen_ms,
                                updates,
                                present_updates,
                                absent_since_ms: None,
                            },
                        );
                    }
                }
                (
                    Track::Pending {
                        first_seen_ms,
                        updates,
                        present_updates,
                        absent_since_ms,
                    },
                    None,
                ) => {
                    let absent_since_ms = absent_since_ms.unwrap_or(mono_ms);
                    if mono_ms.saturating_sub(absent_since_ms) < hold_ms {
                        self.tracks.insert(
                            key,
                            Track::Pending {
                                first_seen_ms,
                                updates: updates + 1,
                                present_updates,
                                absent_since_ms: Some(absent_since_ms),
                            },
                        );
                    }
                }
                (
                    Track::Firing {
                        first_fired_wall_ms,
                        mut highest_fired,
                        latest: _,
                        absent_since_ms: _,
                    },
                    Some(finding),
                ) => {
                    if finding.severity > highest_fired {
                        highest_fired = finding.severity;
                        alerts.push(firing_alert(wall_ms, &key, finding.clone()));
                    }
                    self.tracks.insert(
                        key,
                        Track::Firing {
                            first_fired_wall_ms,
                            highest_fired,
                            latest: finding,
                            absent_since_ms: None,
                        },
                    );
                }
                (
                    Track::Firing {
                        first_fired_wall_ms,
                        highest_fired,
                        latest,
                        absent_since_ms,
                    },
                    None,
                ) => {
                    let absent_since_ms = absent_since_ms.unwrap_or(mono_ms);
                    if mono_ms.saturating_sub(absent_since_ms) >= hold_ms {
                        alerts.push(Alert {
                            ts_ms: wall_ms,
                            state: AlertState::Resolved,
                            key,
                            finding: latest,
                        });
                    } else {
                        self.tracks.insert(
                            key,
                            Track::Firing {
                                first_fired_wall_ms,
                                highest_fired,
                                latest,
                                absent_since_ms: Some(absent_since_ms),
                            },
                        );
                    }
                }
            }
        }

        for (key, finding) in present {
            if hold_ms == 0 {
                alerts.push(firing_alert(wall_ms, &key, finding.clone()));
                self.tracks.insert(
                    key,
                    Track::Firing {
                        first_fired_wall_ms: wall_ms,
                        highest_fired: finding.severity,
                        latest: finding,
                        absent_since_ms: None,
                    },
                );
            } else {
                self.tracks.insert(
                    key,
                    Track::Pending {
                        first_seen_ms: mono_ms,
                        updates: 1,
                        present_updates: 1,
                        absent_since_ms: None,
                    },
                );
            }
        }
        alerts
    }

    /// Currently firing alerts, sorted worst first then key. Each finding is
    /// the latest observation, while `ts_ms` remains its original fire time.
    pub fn active(&self) -> Vec<Alert> {
        let mut alerts = self
            .tracks
            .iter()
            .filter_map(|(key, track)| match track {
                Track::Firing {
                    first_fired_wall_ms,
                    latest,
                    ..
                } => Some(Alert {
                    ts_ms: *first_fired_wall_ms,
                    state: AlertState::Firing,
                    key: key.clone(),
                    finding: latest.clone(),
                }),
                Track::Pending { .. } => None,
            })
            .collect::<Vec<_>>();
        alerts.sort_by(|left, right| {
            right
                .finding
                .severity
                .cmp(&left.finding.severity)
                .then_with(|| left.key.cmp(&right.key))
        });
        alerts
    }
}

fn finding_key(finding: &Finding) -> String {
    if finding.code.starts_with("temp.")
        && let Some(sensor) = &finding.sensor
    {
        return format!("temp:{sensor}");
    }
    finding.code.clone()
}

fn firing_alert(wall_ms: u64, key: &str, finding: Finding) -> Alert {
    Alert {
        ts_ms: wall_ms,
        state: AlertState::Firing,
        key: key.into(),
        finding,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::Severity;

    fn config(hold_s: u64) -> AlertConfig {
        AlertConfig {
            hold_s,
            ..AlertConfig::default()
        }
    }

    fn finding(severity: Severity) -> Finding {
        Finding {
            severity,
            code: "memory.low".into(),
            title: "memory".into(),
            detail: "test finding".into(),
            sensor: None,
        }
    }

    fn verdict(findings: Vec<Finding>) -> Verdict {
        Verdict {
            severity: findings
                .iter()
                .map(|f| f.severity)
                .max()
                .unwrap_or(Severity::Ok),
            findings,
        }
    }

    fn temp_finding(sensor: &str, severity: Severity) -> Finding {
        Finding {
            severity,
            code: format!(
                "temp.{}",
                if severity == Severity::Crit {
                    "crit"
                } else {
                    "warn"
                }
            ),
            title: sensor.into(),
            detail: "test temperature".into(),
            sensor: Some(sensor.into()),
        }
    }

    #[test]
    fn hold_zero_fires_immediately() {
        let mut tracker = AlertTracker::new(&config(0));
        let alerts = tracker.update(1, 123, &verdict(vec![finding(Severity::Warn)]));
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].state, AlertState::Firing);
        assert_eq!(alerts[0].ts_ms, 123);
    }

    #[test]
    fn debounce_fires_once_after_the_hold() {
        let mut tracker = AlertTracker::new(&config(10));
        let warning = verdict(vec![finding(Severity::Warn)]);
        assert!(tracker.update(0, 100, &warning).is_empty());
        assert!(tracker.update(9_000, 9_100, &warning).is_empty());
        assert_eq!(tracker.update(10_000, 10_100, &warning).len(), 1);
        assert!(tracker.update(20_000, 20_100, &warning).is_empty());
    }

    #[test]
    fn escalation_fires_only_when_severity_increases() {
        let mut tracker = AlertTracker::new(&config(0));
        let warning = verdict(vec![finding(Severity::Warn)]);
        let critical = verdict(vec![finding(Severity::Crit)]);
        assert_eq!(tracker.update(0, 100, &warning).len(), 1);
        assert_eq!(tracker.update(1, 200, &critical).len(), 1);
        assert!(tracker.update(2, 300, &warning).is_empty());
    }

    #[test]
    fn resolves_after_a_full_absence_hold() {
        let mut tracker = AlertTracker::new(&config(10));
        let warning = verdict(vec![finding(Severity::Warn)]);
        let clear = verdict(vec![]);
        tracker.update(0, 0, &warning);
        tracker.update(10_000, 10_000, &warning);
        assert!(tracker.update(15_000, 15_000, &clear).is_empty());
        let alerts = tracker.update(25_000, 25_000, &clear);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].state, AlertState::Resolved);
    }

    #[test]
    fn return_within_resolve_window_does_not_refire() {
        let mut tracker = AlertTracker::new(&config(10));
        let warning = verdict(vec![finding(Severity::Warn)]);
        let clear = verdict(vec![]);
        tracker.update(0, 0, &warning);
        tracker.update(10_000, 10_000, &warning);
        tracker.update(15_000, 15_000, &clear);
        assert!(tracker.update(19_000, 19_000, &warning).is_empty());
        assert_eq!(tracker.active().len(), 1);
    }

    #[test]
    fn ignores_findings_below_minimum_severity() {
        let mut cfg = config(0);
        cfg.min_severity = Severity::Crit;
        let mut tracker = AlertTracker::new(&cfg);
        assert!(
            tracker
                .update(0, 0, &verdict(vec![finding(Severity::Warn)]))
                .is_empty()
        );
        assert_eq!(
            tracker
                .update(1, 1, &verdict(vec![finding(Severity::Crit)]))
                .len(),
            1
        );
    }

    #[test]
    fn temperature_findings_merge_by_sensor() {
        let mut tracker = AlertTracker::new(&config(0));
        let merged = verdict(vec![
            temp_finding("cpu/tctl", Severity::Warn),
            temp_finding("cpu/tctl", Severity::Crit),
        ]);
        let alerts = tracker.update(0, 0, &merged);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].key, "temp:cpu/tctl");
        assert_eq!(alerts[0].finding.severity, Severity::Crit);

        let separate = verdict(vec![
            temp_finding("cpu/tctl", Severity::Crit),
            temp_finding("gpu/edge", Severity::Crit),
        ]);
        let mut tracker = AlertTracker::new(&config(0));
        assert_eq!(tracker.update(0, 0, &separate).len(), 2);
    }

    #[test]
    fn disabling_alerts_resolves_firing_tracks_then_stays_silent() {
        let mut tracker = AlertTracker::new(&config(0));
        let warning = verdict(vec![finding(Severity::Warn)]);
        tracker.update(0, 0, &warning);
        let mut disabled = config(0);
        disabled.enabled = false;
        tracker.set_config(&disabled);
        let alerts = tracker.update(1, 100, &warning);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].state, AlertState::Resolved);
        assert!(tracker.update(2, 200, &warning).is_empty());
    }

    #[test]
    fn active_sorts_worst_severity_then_key() {
        let mut tracker = AlertTracker::new(&config(0));
        let critical = Finding {
            code: "zeta".into(),
            ..finding(Severity::Crit)
        };
        let warning = Finding {
            code: "alpha".into(),
            ..finding(Severity::Warn)
        };
        tracker.update(0, 0, &verdict(vec![warning.clone()]));
        tracker.update(1, 1, &verdict(vec![warning, critical]));
        let active = tracker.active();
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].key, "zeta");
        assert_eq!(active[1].key, "alpha");
    }

    #[test]
    fn alert_states_serde_round_trip() {
        for state in [AlertState::Firing, AlertState::Resolved] {
            let alert = Alert {
                ts_ms: 42,
                state,
                key: "memory.low".into(),
                finding: finding(Severity::Warn),
            };
            let json = serde_json::to_string(&alert).unwrap();
            let state_name = match state {
                AlertState::Firing => "firing",
                AlertState::Resolved => "resolved",
            };
            assert!(json.contains(&format!(r#""state":"{state_name}""#)));
            assert_eq!(serde_json::from_str::<Alert>(&json).unwrap(), alert);
        }
    }

    #[test]
    fn l5_monotonic_hold_ignores_wall_clock_steps() {
        let mut tracker = AlertTracker::new(&config(10));
        let warning = verdict(vec![finding(Severity::Warn)]);
        assert!(tracker.update(0, 3_600_000, &warning).is_empty());
        let fired = tracker.update(10_000, 0, &warning);
        assert_eq!(fired[0].ts_ms, 0);

        let mut tracker = AlertTracker::new(&config(10));
        tracker.update(0, 0, &warning);
        assert!(tracker.update(1_000, 3_600_000, &warning).is_empty());
    }

    #[test]
    fn l2_flapping_uses_duty_cycle_and_continuous_presence() {
        let mut tracker = AlertTracker::new(&config(10));
        let warning = verdict(vec![finding(Severity::Warn)]);
        let clear = verdict(vec![]);
        for second in 0..10 {
            let present = !matches!(second, 3 | 8);
            assert!(
                tracker
                    .update(
                        second * 1_000,
                        second * 1_000,
                        if present { &warning } else { &clear }
                    )
                    .is_empty()
            );
        }
        assert_eq!(tracker.update(10_000, 10_000, &warning).len(), 1);

        let mut sparse = AlertTracker::new(&config(10));
        for second in 0..=30 {
            let v = if second % 10 == 0 { &warning } else { &clear };
            assert!(sparse.update(second * 1_000, second * 1_000, v).is_empty());
        }

        let mut continuous = AlertTracker::new(&config(10));
        assert!(continuous.update(0, 0, &warning).is_empty());
        assert_eq!(continuous.update(10_000, 10_000, &warning).len(), 1);
    }

    #[test]
    fn l3_active_uses_latest_finding_and_first_fire_time() {
        let mut tracker = AlertTracker::new(&config(0));
        let warning = verdict(vec![finding(Severity::Warn)]);
        let critical = verdict(vec![finding(Severity::Crit)]);
        tracker.update(1, 100, &warning);
        tracker.update(2, 200, &critical);
        tracker.update(3, 300, &warning);
        let active = tracker.active();
        assert_eq!(active[0].ts_ms, 100);
        assert_eq!(active[0].finding.severity, Severity::Warn);
    }
}
