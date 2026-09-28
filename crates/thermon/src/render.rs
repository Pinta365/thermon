//! Human-readable output.

use thermon_core::alerts::AlertState;
use thermon_core::health::{Severity, Verdict};
use thermon_core::history::HistoryResponse;
use thermon_core::hwmon::{Category, Sensor};
use thermon_core::protocol::AlertLog;
use thermon_core::protocol::ProcessList;
use thermon_core::sampler::{Inventory, Snapshot};

pub fn alerts(log: &AlertLog) -> String {
    let mut out = String::new();
    if log.active.is_empty() {
        out.push_str("No alerts firing.\n");
    } else {
        out.push_str("Firing\n");
        for a in &log.active {
            out.push_str(&format!(
                "  {:<5} {}  {} — {}\n",
                severity_name(a.finding.severity),
                clock(a.ts_ms),
                a.finding.title,
                a.finding.detail
            ));
        }
    }
    if !log.recent.is_empty() {
        out.push_str("\nRecent\n");
        for a in log.recent.iter().rev().take(20) {
            let line = match a.state {
                AlertState::Firing => format!(
                    "  {:<5} {}  {}\n",
                    severity_name(a.finding.severity),
                    clock(a.ts_ms),
                    a.finding.title
                ),
                AlertState::Resolved => format!(
                    "  {:<5} {}  {} (resolved)\n",
                    "OK",
                    clock(a.ts_ms),
                    a.finding.title
                ),
            };
            out.push_str(&line);
        }
    }
    out
}

/// Local time for a unix-ms timestamp: `HH:MM:SS` today, else `Mon DD HH:MM`.
fn clock(ts_ms: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let local = |secs: i64| {
        let t: libc::time_t = secs as libc::time_t;
        // SAFETY: zeroed tm is a valid out-parameter for localtime_r.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: valid pointers to a time_t and a tm.
        unsafe { libc::localtime_r(&t, &mut tm) };
        tm
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let tm = local((ts_ms / 1000) as i64);
    let today = local(now);
    if (tm.tm_year, tm.tm_yday) == (today.tm_year, today.tm_yday) {
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    } else {
        format!(
            "{} {:2} {:02}:{:02}",
            MONTHS[tm.tm_mon.clamp(0, 11) as usize],
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min
        )
    }
}

pub fn health(verdict: &Verdict) -> String {
    if verdict.severity == Severity::Ok {
        return "Health  OK\n".into();
    }

    let count = verdict.findings.len();
    let noun = if count == 1 { "finding" } else { "findings" };
    let mut output = format!(
        "Health  {} · {count} {noun}\n",
        severity_name(verdict.severity)
    );
    for finding in &verdict.findings {
        output.push_str(&format!(
            "  {:<4}  {} — {}\n",
            severity_name(finding.severity),
            finding.title,
            finding.detail
        ));
    }
    output
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Ok => "OK",
        Severity::Info => "INFO",
        Severity::Warn => "WARN",
        Severity::Crit => "CRIT",
    }
}

pub fn status(inv: &Inventory, snap: &Snapshot) {
    let usage = snap.cpu.usage.map_or("—".into(), |u| format!("{u:.1}%"));
    let freqs: Vec<u64> = snap.cpu.freq_khz.iter().flatten().copied().collect();
    let freq = match (freqs.iter().min(), freqs.iter().max()) {
        (Some(lo), Some(hi)) => format!(" · {:.2}–{:.2} GHz", *lo as f64 / 1e6, *hi as f64 / 1e6),
        _ => String::new(),
    };
    println!("CPU     {usage} over {} CPUs{freq}", inv.cpu_count);
    if !snap.cpu.cores.is_empty() {
        let cores: Vec<String> = snap
            .cpu
            .cores
            .iter()
            .map(|u| u.map_or("-".into(), |u| format!("{u:.0}")))
            .collect();
        println!("        per CPU %: {}", cores.join(" "));
    }
    if let Some(m) = &snap.memory {
        println!(
            "Memory  {} / {} used · swap {} / {}",
            gib(m.used_kib()),
            gib(m.total_kib),
            gib(m.swap_used_kib()),
            gib(m.swap_total_kib)
        );
    }
    let psi = |p: &Option<thermon_core::procfs::Pressure>| {
        p.as_ref().map_or("n/a".to_string(), |p| {
            let full = p
                .full
                .as_ref()
                .map_or(String::new(), |f| format!(" full {:.2}", f.avg10));
            format!("some {:.2}{full}", p.some.avg10)
        })
    };
    println!(
        "PSI10   cpu {} · memory {} · io {}",
        psi(&snap.pressure.cpu),
        psi(&snap.pressure.memory),
        psi(&snap.pressure.io)
    );

    let mut last: Option<Category> = None;
    for chip in &inv.chips {
        if last != Some(chip.category) {
            println!("\n{}", chip.category.title());
            last = Some(chip.category);
        }
        let gpu = inv
            .gpus
            .iter()
            .find(|g| g.pci.is_some() && g.pci == chip.pci);
        println!(
            "  {}{}",
            chip.id,
            gpu.map(|g| gpu_summary(g, snap)).unwrap_or_default()
        );
        for s in &chip.sensors {
            println!("    {}", sensor_line(s, snap.sensors.get(&s.id).copied()));
        }
    }
    // DRM cards with no hwmon chip.
    for g in &inv.gpus {
        if !inv.chips.iter().any(|c| c.pci.is_some() && c.pci == g.pci) {
            println!("\nGPU (no sensors)\n  {}{}", g.id, gpu_summary(g, snap));
        }
    }
}

fn gpu_summary(g: &thermon_core::gpu::Gpu, snap: &Snapshot) -> String {
    let mut parts = Vec::new();
    match g.integrated {
        Some(true) => parts.push("integrated".to_string()),
        Some(false) => parts.push("discrete".to_string()),
        None => {}
    }
    parts.push(g.card.clone());
    if let Some(stats) = snap.gpus.get(&g.id) {
        if let Some(b) = stats.busy_percent {
            parts.push(format!("busy {b}%"));
        }
        if let (Some(u), Some(t)) = (stats.vram_used_bytes, stats.vram_total_bytes) {
            parts.push(format!("VRAM {} / {}", gib(u / 1024), gib(t / 1024)));
        }
    }
    format!("  ({})", parts.join(" · "))
}

fn sensor_line(s: &Sensor, v: Option<f64>) -> String {
    let unit = s.kind.unit();
    let value = match v {
        Some(v) if unit == "RPM" => format!("{v:>7.0} {unit}"),
        Some(v) => format!("{v:>7.1} {unit}"),
        None => format!("{:>7} {unit}", "—"),
    };
    let mut limits = Vec::new();
    if let Some(m) = s.max {
        limits.push(format!("max {m:.0}"));
    }
    if let Some(c) = s.crit {
        limits.push(format!("crit {c:.0}"));
    }
    let limits = if limits.is_empty() {
        String::new()
    } else {
        format!("  ({})", limits.join(", "))
    };
    format!("{:<12} {value}{limits}   {}", s.label, s.id)
}

pub fn processes(list: &ProcessList) {
    println!(
        "{:>8} {:>6} {:>9} {:>4} {:1}  {:<16} COMMAND",
        "PID", "CPU%", "RSS", "NI", "S", "NAME"
    );
    for p in &list.processes {
        let cmd = if p.cmdline.is_empty() {
            format!("[{}]", p.name)
        } else {
            p.cmdline.clone()
        };
        println!(
            "{:>8} {:>6.1} {:>9} {:>4} {:1}  {:<16} {}",
            p.pid,
            p.cpu_percent,
            human_kib(p.rss_kib),
            p.nice,
            p.state,
            truncate(&p.name, 16),
            truncate(&cmd, 80)
        );
    }
    println!("({} of {} processes)", list.processes.len(), list.total);
}

pub fn history(inv: &Inventory, h: &HistoryResponse) {
    let points = h.series.values().map(Vec::len).max().unwrap_or(0);
    println!(
        "{} points at {} intervals",
        points,
        human_duration(h.interval_ms)
    );
    let width = h.series.keys().map(|k| k.len()).max().unwrap_or(0);
    let spark_width = points.clamp(1, 60);
    for (name, values) in &h.series {
        let (unit, scale) = series_unit(inv, name);
        let present: Vec<f32> = values.iter().flatten().map(|v| v * scale).collect();
        if present.is_empty() {
            println!("{name:<width$}  (no data)");
            continue;
        }
        let min = present.iter().copied().fold(f32::INFINITY, f32::min);
        let max = present.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let avg = present.iter().sum::<f32>() / present.len() as f32;
        let last = *present.last().unwrap();
        println!(
            "{name:<width$}  {}  last {last:.1}{unit}  min {min:.1}  avg {avg:.1}  max {max:.1}",
            sparkline(&padded(values, points), spark_width),
        );
    }
}

/// Unit label and multiplier for a history series.
fn series_unit(inv: &Inventory, name: &str) -> (&'static str, f32) {
    if let Some(s) = inv
        .chips
        .iter()
        .flat_map(|c| &c.sensors)
        .find(|s| s.id == name)
    {
        return (s.kind.unit(), 1.0);
    }
    if name.ends_with("_kib") {
        (" GiB", 1.0 / (1024.0 * 1024.0))
    } else if name.ends_with("_mib") {
        (" MiB", 1.0)
    } else if name.ends_with("_mhz") {
        (" MHz", 1.0)
    } else if name == "cpu.usage" || name.starts_with("psi.") || name.ends_with(".busy") {
        ("%", 1.0)
    } else {
        ("", 1.0)
    }
}

/// Left-pad with gaps to `len`: series that started later stay time-aligned.
fn padded(values: &[Option<f32>], len: usize) -> Vec<Option<f32>> {
    let mut v = vec![None; len.saturating_sub(values.len())];
    v.extend_from_slice(values);
    v
}

/// Average the whole series into `width` buckets, so a spike anywhere in the
/// range still shows.
fn bucket(values: &[Option<f32>], width: usize) -> Vec<Option<f32>> {
    let n = values.len();
    if n <= width {
        return values.to_vec();
    }
    (0..width)
        .map(|i| {
            let slice = &values[i * n / width..(i + 1) * n / width];
            let present: Vec<f32> = slice.iter().flatten().copied().collect();
            (!present.is_empty()).then(|| present.iter().sum::<f32>() / present.len() as f32)
        })
        .collect()
}

/// Unicode block sparkline of the whole series in `width` columns, scaled to
/// its own range. Shorter series are right-aligned.
pub fn sparkline(values: &[Option<f32>], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let values = bucket(values, width);
    let present = values.iter().flatten();
    let min = present.clone().copied().fold(f32::INFINITY, f32::min);
    let max = present.copied().fold(f32::NEG_INFINITY, f32::max);
    let span = max - min;
    let mut out: String = values
        .iter()
        .map(|v| match v {
            None => ' ',
            Some(_) if span <= f32::EPSILON => BARS[0],
            Some(v) => BARS[(((v - min) / span) * 7.0).round() as usize],
        })
        .collect();
    // Keep columns aligned when a series is shorter than the rest.
    while out.chars().count() < width {
        out.insert(0, ' ');
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

pub fn gib(kib: u64) -> String {
    format!("{:.1} GiB", kib as f64 / (1024.0 * 1024.0))
}

fn human_kib(kib: u64) -> String {
    if kib >= 1024 * 1024 {
        format!("{:.1}G", kib as f64 / (1024.0 * 1024.0))
    } else if kib >= 1024 {
        format!("{:.0}M", kib as f64 / 1024.0)
    } else {
        format!("{kib}K")
    }
}

fn human_duration(ms: u64) -> String {
    match ms {
        ms if ms % 60_000 == 0 => format!("{} min", ms / 60_000),
        ms if ms % 1000 == 0 => format!("{} s", ms / 1000),
        ms => format!("{ms} ms"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thermon_core::health::Finding;

    #[test]
    fn alert_log_formatting() {
        use thermon_core::alerts::Alert;
        let alert = |state, severity| Alert {
            ts_ms: 0,
            state,
            key: "temp:x".into(),
            finding: Finding {
                severity,
                code: "temp.warn".into(),
                title: "Processor Tctl is hot".into(),
                detail: "97 °C; the warning level is 95 °C.".into(),
                sensor: Some("x".into()),
            },
        };
        let empty = alerts(&AlertLog {
            active: vec![],
            recent: vec![],
        });
        assert_eq!(empty, "No alerts firing.\n");

        let firing = alert(AlertState::Firing, Severity::Warn);
        let out = alerts(&AlertLog {
            active: vec![firing.clone()],
            recent: vec![firing, alert(AlertState::Resolved, Severity::Warn)],
        });
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "Firing");
        assert!(
            lines[1].starts_with("  WARN ")
                && lines[1].ends_with("Processor Tctl is hot — 97 °C; the warning level is 95 °C.")
        );
        assert_eq!(lines[3], "Recent");
        // Newest first.
        assert!(
            lines[4].starts_with("  OK ") && lines[4].ends_with("Processor Tctl is hot (resolved)")
        );
        assert!(lines[5].ends_with("  Processor Tctl is hot"));
    }

    #[test]
    fn health_formatting() {
        assert_eq!(
            health(&Verdict {
                severity: Severity::Ok,
                findings: vec![],
            }),
            "Health  OK\n"
        );
        assert_eq!(
            health(&Verdict {
                severity: Severity::Crit,
                findings: vec![
                    Finding {
                        severity: Severity::Crit,
                        code: "temp.crit".into(),
                        title: "junction is critical".into(),
                        detail: "Graphics junction is at 110 °C; critical is 110 °C.".into(),
                        sensor: Some("amdgpu/junction".into()),
                    },
                    Finding {
                        severity: Severity::Warn,
                        code: "io.pressure".into(),
                        title: "IO pressure is high".into(),
                        detail: "Tasks were stalled waiting for IO 20.0% of the last 10 s.".into(),
                        sensor: None,
                    },
                ],
            }),
            concat!(
                "Health  CRIT · 2 findings\n",
                "  CRIT  junction is critical — Graphics junction is at 110 °C; critical is 110 °C.\n",
                "  WARN  IO pressure is high — Tasks were stalled waiting for IO 20.0% of the last 10 s.\n"
            )
        );
    }

    #[test]
    fn sparklines() {
        assert_eq!(
            sparkline(&[Some(0.0), Some(7.0), None, Some(3.5)], 4),
            "▁█ ▅"
        );
        assert_eq!(sparkline(&[Some(5.0), Some(5.0)], 4), "  ▁▁");
        assert_eq!(sparkline(&[None, None], 2), "  ");
        assert_eq!(sparkline(&[Some(1.0), Some(2.0), Some(3.0)], 2), "▁█");
    }

    #[test]
    fn sparkline_covers_the_whole_range() {
        // A spike at the start of 300 points must still show in 60 columns.
        let mut v = vec![Some(1.0); 300];
        v[3] = Some(100.0);
        let s = sparkline(&v, 60);
        assert_eq!(s.chars().count(), 60);
        assert_eq!(s.chars().next(), Some('█'), "{s}");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello!", 5), "hell…");
    }
}
