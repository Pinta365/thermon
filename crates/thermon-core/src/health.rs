//! Health findings from a snapshot: what was measured, and how bad it is.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    hwmon::{Category, SensorKind},
    processes::ProcessInfo,
    sampler::{Inventory, Snapshot},
};

/// Memory full-stall PSI percentage that is critical over ten seconds.
const MEMORY_FULL_CRIT: f32 = 5.0;
/// Memory some-stall PSI percentage that warrants a warning over ten seconds.
const MEMORY_SOME_WARN: f32 = 10.0;
/// CPU some-stall PSI percentages over ten seconds.
const CPU_SATURATED_INFO: f32 = 40.0;
const CPU_SATURATED_WARN: f32 = 80.0;
/// IO full-stall PSI percentages over ten seconds.
const IO_PRESSURE_INFO: f32 = 5.0;
const IO_PRESSURE_WARN: f32 = 20.0;
/// Available-memory percentages.
const MEMORY_LOW_CRIT_PERCENT: f64 = 5.0;
const MEMORY_LOW_WARN_PERCENT: f64 = 10.0;
/// CPU thermal heuristic inputs.
const THROTTLE_TEMP_MARGIN_C: f64 = 5.0;
const THROTTLE_TEMP_WITHOUT_CRIT_C: f64 = 95.0;
const THROTTLE_CPU_PERCENT: f32 = 50.0;
const THROTTLE_FREQ_RATIO: f64 = 0.8;
/// Zombie count that warrants an informational finding.
const ZOMBIE_INFO_COUNT: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Ok,
    Info,
    Warn,
    Crit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub code: String,
    pub title: String,
    pub detail: String,
    pub sensor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub severity: Severity,
    pub findings: Vec<Finding>,
}

pub struct Context<'a> {
    pub peak_freq_khz: Option<u64>,
    pub processes: Option<&'a [ProcessInfo]>,
}

pub fn assess(inv: &Inventory, snap: &Snapshot, ctx: &Context<'_>) -> Verdict {
    let mut findings = Vec::new();

    assess_sensors(inv, snap, &mut findings);
    assess_memory(snap, &mut findings);
    assess_pressure(snap, &mut findings);
    assess_throttle(inv, snap, ctx, &mut findings);
    assess_zombies(ctx, &mut findings);

    findings.sort_by(|left, right| {
        right
            .severity
            .cmp(&left.severity)
            .then_with(|| left.code.cmp(&right.code))
    });
    let severity = findings
        .iter()
        .map(|finding| finding.severity)
        .max()
        .unwrap_or(Severity::Ok);
    Verdict { severity, findings }
}

fn assess_sensors(inv: &Inventory, snap: &Snapshot, findings: &mut Vec<Finding>) {
    for chip in &inv.chips {
        let chip_title = inv.chip_title(chip);
        for sensor in &chip.sensors {
            if sensor.kind != SensorKind::Temp {
                continue;
            }
            let Some(value) = snap.sensors.get(&sensor.id) else {
                continue;
            };
            let (severity, code, threshold, threshold_name) = match (sensor.crit, sensor.warn) {
                (Some(crit), _) if *value >= crit => {
                    (Severity::Crit, "temp.crit", crit, "critical")
                }
                (_, Some(warn)) if *value >= warn => (Severity::Warn, "temp.warn", warn, "warning"),
                _ => continue,
            };
            findings.push(Finding {
                severity,
                code: code.into(),
                title: format!(
                    "{chip_title} {} is {}",
                    sensor.label,
                    if severity == Severity::Crit {
                        "critical"
                    } else {
                        "hot"
                    }
                ),
                detail: format!("{value:.0} °C; the {threshold_name} level is {threshold:.0} °C."),
                sensor: Some(sensor.id.clone()),
            });
        }
    }
}

fn assess_memory(snap: &Snapshot, findings: &mut Vec<Finding>) {
    if let Some(pressure) = &snap.pressure.memory {
        if pressure
            .full
            .as_ref()
            .is_some_and(|full| full.avg10 >= MEMORY_FULL_CRIT)
        {
            let percent = pressure.full.as_ref().map_or(0.0, |full| full.avg10);
            findings.push(pressure_finding(
                Severity::Crit,
                "memory.pressure",
                "Memory pressure is critical",
                "memory",
                percent,
            ));
        } else if pressure.some.avg10 >= MEMORY_SOME_WARN {
            findings.push(pressure_finding(
                Severity::Warn,
                "memory.pressure",
                "Memory pressure is high",
                "memory",
                pressure.some.avg10,
            ));
        }
    }

    let Some(memory) = &snap.memory else {
        return;
    };
    if memory.total_kib == 0 {
        return;
    }
    let available_percent = memory.available_kib as f64 / memory.total_kib as f64 * 100.0;
    if available_percent < MEMORY_LOW_CRIT_PERCENT {
        findings.push(Finding {
            severity: Severity::Crit,
            code: "memory.low".into(),
            title: "Available memory is critically low".into(),
            detail: format!(
                "Only {available_percent:.1}% of memory is available ({} MiB of {} MiB).",
                memory.available_kib / 1024,
                memory.total_kib / 1024
            ),
            sensor: None,
        });
    } else if memory.swap_total_kib > 0
        && available_percent < MEMORY_LOW_WARN_PERCENT
        && memory.swap_used_kib() > memory.swap_total_kib / 2
    {
        findings.push(Finding {
            severity: Severity::Warn,
            code: "memory.low".into(),
            title: "Available memory is low".into(),
            detail: format!(
                "{available_percent:.1}% of memory is available and {}% of swap is in use.",
                memory.swap_used_kib() * 100 / memory.swap_total_kib
            ),
            sensor: None,
        });
    }
}

fn assess_pressure(snap: &Snapshot, findings: &mut Vec<Finding>) {
    if let Some(pressure) = &snap.pressure.cpu {
        let (severity, title) = if pressure.some.avg10 >= CPU_SATURATED_WARN {
            (Severity::Warn, "CPU is saturated")
        } else if pressure.some.avg10 >= CPU_SATURATED_INFO {
            (Severity::Info, "CPU is busy")
        } else {
            (Severity::Ok, "")
        };
        if severity != Severity::Ok {
            findings.push(pressure_finding(
                severity,
                "cpu.saturated",
                title,
                "CPU",
                pressure.some.avg10,
            ));
        }
    }

    if let Some(pressure) = &snap.pressure.io {
        let (severity, title) = if pressure
            .full
            .as_ref()
            .is_some_and(|full| full.avg10 >= IO_PRESSURE_WARN)
        {
            (Severity::Warn, "IO pressure is high")
        } else if pressure
            .full
            .as_ref()
            .is_some_and(|full| full.avg10 >= IO_PRESSURE_INFO)
        {
            (Severity::Info, "IO pressure is elevated")
        } else {
            (Severity::Ok, "")
        };
        if severity != Severity::Ok {
            let percent = pressure.full.as_ref().map_or(0.0, |full| full.avg10);
            findings.push(pressure_finding(
                severity,
                "io.pressure",
                title,
                "IO",
                percent,
            ));
        }
    }
}

fn pressure_finding(
    severity: Severity,
    code: &str,
    title: &str,
    resource: &str,
    percent: f32,
) -> Finding {
    Finding {
        severity,
        code: code.into(),
        title: title.into(),
        detail: format!(
            "Tasks were stalled waiting for {resource} {percent:.1}% of the last 10 s."
        ),
        sensor: None,
    }
}

fn assess_throttle(
    inv: &Inventory,
    snap: &Snapshot,
    ctx: &Context<'_>,
    findings: &mut Vec<Finding>,
) {
    if let Some(events) = snap.cpu.throttle_events {
        if events > 0 {
            findings.push(Finding {
                severity: Severity::Warn,
                code: "cpu.throttled".into(),
                title: "CPU is thermal throttling".into(),
                detail: format!(
                    "The CPU reported {events} throttling event{} in the last sample.",
                    if events == 1 { "" } else { "s" }
                ),
                sensor: None,
            });
        }
        return;
    }

    let (Some(peak_freq_khz), Some(cpu_usage)) = (ctx.peak_freq_khz, snap.cpu.usage) else {
        return;
    };
    if peak_freq_khz == 0 || cpu_usage < THROTTLE_CPU_PERCENT {
        return;
    }
    let Some(current_freq_khz) = snap.cpu.freq_khz.iter().flatten().copied().max() else {
        return;
    };
    if (current_freq_khz as f64) >= peak_freq_khz as f64 * THROTTLE_FREQ_RATIO {
        return;
    }

    let sensor = inv
        .chips
        .iter()
        .filter(|chip| chip.category == Category::Cpu)
        .find_map(|chip| {
            chip.sensors.iter().find(|sensor| {
                sensor.kind == SensorKind::Temp
                    && is_cpu_package_label(&sensor.label)
                    && snap.sensors.get(&sensor.id).is_some_and(|value| {
                        *value
                            >= sensor.crit.map_or(THROTTLE_TEMP_WITHOUT_CRIT_C, |crit| {
                                crit - THROTTLE_TEMP_MARGIN_C
                            })
                    })
            })
        });
    let Some(sensor) = sensor else {
        return;
    };
    let temperature = snap.sensors[&sensor.id];
    findings.push(Finding {
        severity: Severity::Warn,
        code: "cpu.throttle".into(),
        title: "Possible thermal throttling".into(),
        detail: format!(
            "{} is at {temperature:.0} °C while CPU use is {cpu_usage:.0}% and the clock dropped from {:.2} to {:.2} GHz; this is inferred from temperature and clock drop, not reported by the CPU.",
            sensor.label,
            peak_freq_khz as f64 / 1e6,
            current_freq_khz as f64 / 1e6
        ),
        sensor: Some(sensor.id.clone()),
    });
}

fn assess_zombies(ctx: &Context<'_>, findings: &mut Vec<Finding>) {
    let Some(processes) = ctx.processes else {
        return;
    };
    let zombies = processes
        .iter()
        .filter(|process| process.state == 'Z')
        .collect::<Vec<_>>();
    let zombie_count = zombies.len();
    if zombie_count < ZOMBIE_INFO_COUNT {
        return;
    }
    let mut parents = BTreeMap::<u32, usize>::new();
    for process in zombies {
        *parents.entry(process.ppid).or_default() += 1;
    }
    let mut parents = parents.into_iter().collect::<Vec<_>>();
    parents.sort_by_key(|(pid, count)| (std::cmp::Reverse(*count), *pid));
    let parent_list = parents
        .into_iter()
        .take(3)
        .map(|(pid, count)| format!("{pid} ({count})"))
        .collect::<Vec<_>>()
        .join(", ");
    findings.push(Finding {
        severity: Severity::Info,
        code: "processes.zombies".into(),
        title: "Zombie processes detected".into(),
        detail: format!(
            "{} zombie processes; parent PIDs with the most zombies: {parent_list}.",
            zombie_count
        ),
        sensor: None,
    });
}

fn is_cpu_package_label(label: &str) -> bool {
    let label = label.to_ascii_lowercase();
    ["tctl", "tdie", "package"]
        .iter()
        .any(|prefix| label.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use crate::{
        config::Config,
        procfs::{MemInfo, Pressure, PsiLine},
        sampler::Sampler,
        test_support::fixture,
    };

    use super::*;

    fn fixture_data() -> (Inventory, Snapshot) {
        let mut sampler = Sampler::new(fixture("ryzen-rx9070"), Config::default()).unwrap();
        sampler.sample();
        (sampler.inventory().clone(), sampler.sample())
    }

    fn context<'a>() -> Context<'a> {
        Context {
            peak_freq_khz: None,
            processes: None,
        }
    }

    fn pressure(some: f32, full: Option<f32>) -> Pressure {
        Pressure {
            some: PsiLine {
                avg10: some,
                avg60: 0.0,
                avg300: 0.0,
                total_us: 0,
            },
            full: full.map(|avg10| PsiLine {
                avg10,
                avg60: 0.0,
                avg300: 0.0,
                total_us: 0,
            }),
        }
    }

    fn has(verdict: &Verdict, code: &str, severity: Severity) -> bool {
        verdict
            .findings
            .iter()
            .any(|finding| finding.code == code && finding.severity == severity)
    }

    #[test]
    fn untouched_fixture_is_ok() {
        let (inventory, snapshot) = fixture_data();
        assert_eq!(
            assess(&inventory, &snapshot, &context()).severity,
            Severity::Ok
        );
    }

    #[test]
    fn temperatures_and_sorting_use_sensor_thresholds() {
        let (inventory, mut snapshot) = fixture_data();
        let id = "amdgpu@0000:03:00.0/junction";
        snapshot.sensors.insert(id.into(), 110.0);
        snapshot.pressure.cpu = Some(pressure(80.0, Some(0.0)));
        let verdict = assess(&inventory, &snapshot, &context());
        assert!(has(&verdict, "temp.crit", Severity::Crit));
        assert!(has(&verdict, "cpu.saturated", Severity::Warn));
        assert_eq!(verdict.findings[0].severity, Severity::Crit);

        // Junction's crit is 110, so it warns from 100, not at a fixed 90.
        snapshot.sensors.insert(id.into(), 95.0);
        assert!(!has(
            &assess(&inventory, &snapshot, &context()),
            "temp.warn",
            Severity::Warn
        ));
        snapshot.sensors.insert(id.into(), 100.0);
        let verdict = assess(&inventory, &snapshot, &context());
        assert!(has(&verdict, "temp.warn", Severity::Warn));
        let warn = verdict
            .findings
            .iter()
            .find(|f| f.code == "temp.warn")
            .unwrap();
        assert_eq!(warn.title, "Graphics card junction is hot");
        assert_eq!(warn.detail, "100 °C; the warning level is 100 °C.");
    }

    #[test]
    fn pressure_rules_include_thresholds() {
        let (inventory, mut snapshot) = fixture_data();
        snapshot.pressure.memory = Some(pressure(0.0, Some(5.0)));
        assert!(has(
            &assess(&inventory, &snapshot, &context()),
            "memory.pressure",
            Severity::Crit
        ));
        snapshot.pressure.memory = Some(pressure(10.0, Some(0.0)));
        assert!(has(
            &assess(&inventory, &snapshot, &context()),
            "memory.pressure",
            Severity::Warn
        ));

        snapshot.pressure.cpu = Some(pressure(40.0, Some(0.0)));
        assert!(has(
            &assess(&inventory, &snapshot, &context()),
            "cpu.saturated",
            Severity::Info
        ));
        snapshot.pressure.cpu = Some(pressure(80.0, Some(0.0)));
        assert!(has(
            &assess(&inventory, &snapshot, &context()),
            "cpu.saturated",
            Severity::Warn
        ));

        snapshot.pressure.io = Some(pressure(0.0, Some(5.0)));
        assert!(has(
            &assess(&inventory, &snapshot, &context()),
            "io.pressure",
            Severity::Info
        ));
        snapshot.pressure.io = Some(pressure(0.0, Some(20.0)));
        assert!(has(
            &assess(&inventory, &snapshot, &context()),
            "io.pressure",
            Severity::Warn
        ));
    }

    #[test]
    fn low_memory_rules_use_strict_percentages() {
        let (inventory, mut snapshot) = fixture_data();
        snapshot.memory = Some(MemInfo {
            total_kib: 100,
            available_kib: 4,
            swap_total_kib: 100,
            swap_free_kib: 49,
            ..MemInfo::default()
        });
        assert!(has(
            &assess(&inventory, &snapshot, &context()),
            "memory.low",
            Severity::Crit
        ));
        snapshot.memory.as_mut().unwrap().available_kib = 9;
        assert!(has(
            &assess(&inventory, &snapshot, &context()),
            "memory.low",
            Severity::Warn
        ));
        snapshot.memory.as_mut().unwrap().available_kib = 10;
        assert!(!has(
            &assess(&inventory, &snapshot, &context()),
            "memory.low",
            Severity::Warn
        ));
    }

    #[test]
    fn throttle_is_explicitly_a_heuristic_and_needs_every_input() {
        let (inventory, mut snapshot) = fixture_data();
        snapshot.cpu.usage = Some(50.0);
        snapshot.cpu.freq_khz = vec![Some(700)];
        snapshot
            .sensors
            .insert("k10temp@0000:00:18.3/tctl".into(), 95.0);
        let throttling = Context {
            peak_freq_khz: Some(1_000),
            processes: None,
        };
        let verdict = assess(&inventory, &snapshot, &throttling);
        assert!(has(&verdict, "cpu.throttle", Severity::Warn));
        assert!(verdict.findings.iter().any(|finding| {
            finding.code == "cpu.throttle" && finding.detail.contains("inferred")
        }));

        snapshot.cpu.usage = None;
        assert!(!has(
            &assess(&inventory, &snapshot, &throttling),
            "cpu.throttle",
            Severity::Warn
        ));
        snapshot.cpu.usage = Some(49.0);
        assert!(!has(
            &assess(&inventory, &snapshot, &throttling),
            "cpu.throttle",
            Severity::Warn
        ));
        snapshot.cpu.usage = Some(50.0);
        snapshot.cpu.freq_khz.clear();
        assert!(!has(
            &assess(&inventory, &snapshot, &throttling),
            "cpu.throttle",
            Severity::Warn
        ));
        snapshot.cpu.freq_khz = vec![Some(800)];
        assert!(!has(
            &assess(&inventory, &snapshot, &throttling),
            "cpu.throttle",
            Severity::Warn
        ));
        snapshot.cpu.freq_khz = vec![Some(700)];
        snapshot
            .sensors
            .insert("k10temp@0000:00:18.3/tctl".into(), 94.0);
        assert!(!has(
            &assess(&inventory, &snapshot, &throttling),
            "cpu.throttle",
            Severity::Warn
        ));
        assert!(!has(
            &assess(
                &inventory,
                &snapshot,
                &Context {
                    peak_freq_khz: None,
                    processes: None
                }
            ),
            "cpu.throttle",
            Severity::Warn
        ));
    }

    #[test]
    fn reported_throttle_events_take_priority_over_the_heuristic() {
        let (inventory, mut snapshot) = fixture_data();
        snapshot.cpu.throttle_events = Some(2);
        snapshot.cpu.usage = Some(50.0);
        snapshot.cpu.freq_khz = vec![Some(700)];
        snapshot
            .sensors
            .insert("k10temp@0000:00:18.3/tctl".into(), 95.0);
        let throttling = Context {
            peak_freq_khz: Some(1_000),
            processes: None,
        };
        let verdict = assess(&inventory, &snapshot, &throttling);
        assert!(has(&verdict, "cpu.throttled", Severity::Warn));
        assert!(!has(&verdict, "cpu.throttle", Severity::Warn));
        assert!(verdict.findings.iter().any(|finding| {
            finding.code == "cpu.throttled"
                && finding.detail == "The CPU reported 2 throttling events in the last sample."
        }));

        snapshot.cpu.throttle_events = Some(0);
        assert!(!has(
            &assess(&inventory, &snapshot, &throttling),
            "cpu.throttle",
            Severity::Warn
        ));
        snapshot.cpu.throttle_events = None;
        assert!(has(
            &assess(&inventory, &snapshot, &throttling),
            "cpu.throttle",
            Severity::Warn
        ));
    }

    #[test]
    fn zombie_finding_lists_busiest_parents() {
        let (inventory, snapshot) = fixture_data();
        let processes = (0..5)
            .map(|pid| ProcessInfo {
                pid,
                ppid: if pid < 3 { 10 } else { 20 },
                name: "zombie".into(),
                cmdline: String::new(),
                state: 'Z',
                uid: None,
                threads: 1,
                nice: 0,
                rss_kib: 0,
                cpu_percent: 0.0,
                start_ticks: 0,
            })
            .collect::<Vec<_>>();
        let verdict = assess(
            &inventory,
            &snapshot,
            &Context {
                peak_freq_khz: None,
                processes: Some(&processes),
            },
        );
        let finding = verdict
            .findings
            .iter()
            .find(|finding| finding.code == "processes.zombies")
            .unwrap();
        assert_eq!(finding.severity, Severity::Info);
        assert!(finding.detail.contains("10 (3), 20 (2)"));
    }
}
