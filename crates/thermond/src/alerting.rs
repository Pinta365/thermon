//! Turns each sample into alerts and desktop notifications.

use std::collections::VecDeque;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Instant;

use thermon_core::alerts::{Alert, AlertState, AlertTracker};
use thermon_core::config::AlertConfig;
use thermon_core::health::{self, Severity};
use thermon_core::peak::{RecentPeak, THROTTLE_PEAK_WINDOW};
use thermon_core::processes::ProcessInfo;
use thermon_core::sampler::{Inventory, Snapshot};

/// Alert events kept for `alerts` requests and late subscribers.
pub const LOG_LEN: usize = 100;

/// Alert events numbered in order, so each subscriber can pick up where it left off.
#[derive(Default)]
pub struct AlertLog {
    events: VecDeque<(u64, Alert)>,
    next_seq: u64,
    pub active: Vec<Alert>,
}

impl AlertLog {
    pub fn push(&mut self, alert: Alert) {
        self.next_seq += 1;
        if self.events.len() == LOG_LEN {
            self.events.pop_front();
        }
        self.events.push_back((self.next_seq, alert));
    }

    /// Sequence number of the latest event (0 if none).
    pub fn last_seq(&self) -> u64 {
        self.next_seq
    }

    /// Events after `seq`, oldest first.
    pub fn since(&self, seq: u64) -> Vec<(u64, Alert)> {
        self.events
            .iter()
            .filter(|(s, _)| *s > seq)
            .cloned()
            .collect()
    }

    pub fn recent(&self) -> Vec<Alert> {
        self.events.iter().map(|(_, a)| a.clone()).collect()
    }
}

pub struct Alerter {
    tracker: AlertTracker,
    notify: bool,
    /// Highest per-CPU clock over the last minutes, for the throttle heuristic.
    peak_freq: RecentPeak,
    notify_send_missing: bool,
    /// Alert holds are timed on this monotonic clock: it never steps with NTP
    /// or manual changes, and doesn't advance during suspend.
    started: Instant,
}

impl Alerter {
    pub fn new(cfg: &AlertConfig) -> Self {
        Alerter {
            tracker: AlertTracker::new(cfg),
            notify: cfg.notify,
            peak_freq: RecentPeak::new(THROTTLE_PEAK_WINDOW),
            notify_send_missing: false,
            started: Instant::now(),
        }
    }

    pub fn set_config(&mut self, cfg: &AlertConfig) {
        self.tracker.set_config(cfg);
        self.notify = cfg.notify;
    }

    /// Assess one sample. Returns the alerts that happened and what's firing now.
    pub fn update(
        &mut self,
        inv: &Inventory,
        snap: &Snapshot,
        procs: Option<&[ProcessInfo]>,
    ) -> (Vec<Alert>, Vec<Alert>) {
        let peak_freq_khz = snap
            .cpu
            .freq_khz
            .iter()
            .flatten()
            .max()
            .map(|max| self.peak_freq.push(Instant::now(), *max));
        let verdict = health::assess(
            inv,
            snap,
            &health::Context {
                peak_freq_khz,
                processes: procs,
            },
        );
        let mono_ms = self.started.elapsed().as_millis() as u64;
        let events = self.tracker.update(mono_ms, snap.ts_ms, &verdict);
        for a in &events {
            eprintln!(
                "thermond: alert {}: {} — {}",
                match a.state {
                    AlertState::Firing => "firing",
                    AlertState::Resolved => "resolved",
                },
                a.finding.title,
                a.finding.detail
            );
            if self.notify {
                self.send(a);
            }
        }
        (events, self.tracker.active())
    }

    /// Desktop notification through `notify-send`, so it reaches whatever
    /// notification server the session runs (Omarchy's shell, mako, dunst, ...).
    fn send(&mut self, a: &Alert) {
        if self.notify_send_missing {
            return;
        }
        let (urgency, summary, body) = match a.state {
            AlertState::Firing => (
                match a.finding.severity {
                    Severity::Crit => "critical",
                    _ => "normal",
                },
                a.finding.title.clone(),
                a.finding.detail.clone(),
            ),
            AlertState::Resolved => (
                "low",
                format!("Resolved: {}", a.finding.title),
                "Back within limits.".to_string(),
            ),
        };
        let spawned = Command::new("notify-send")
            .args(["--app-name=Thermon", "--icon=utilities-system-monitor"])
            .arg(format!("--urgency={urgency}"))
            // Lets servers that support it replace the previous bubble for this problem.
            .arg(format!(
                "--hint=string:x-canonical-private-synchronous:thermon-{}",
                a.key
            ))
            .arg(summary)
            .arg(body)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            // Reap it off the sampler thread so it never leaves a zombie.
            Ok(mut child) => {
                thread::spawn(move || child.wait());
            }
            Err(e) => {
                eprintln!("thermond: can't run notify-send ({e}); desktop notifications are off");
                self.notify_send_missing = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thermon_core::health::Finding;

    fn alert(n: u32) -> Alert {
        Alert {
            ts_ms: u64::from(n),
            state: AlertState::Firing,
            key: format!("k{n}"),
            finding: Finding {
                severity: Severity::Warn,
                code: "memory.low".into(),
                title: "t".into(),
                detail: "d".into(),
                sensor: None,
            },
        }
    }

    #[test]
    fn log_is_bounded_and_resumable() {
        let mut log = AlertLog::default();
        assert_eq!(log.last_seq(), 0);
        for n in 0..(LOG_LEN as u32 + 5) {
            log.push(alert(n));
        }
        assert_eq!(log.recent().len(), LOG_LEN);
        assert_eq!(log.recent()[0].ts_ms, 5);
        let seq = log.last_seq();
        assert!(log.since(seq).is_empty());
        log.push(alert(999));
        let new = log.since(seq);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].1.ts_ms, 999);
    }
}
