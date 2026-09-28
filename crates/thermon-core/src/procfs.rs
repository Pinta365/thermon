//! Parsers for `/proc/stat`, `/proc/meminfo`, `/proc/pressure/*` and cpufreq.

use std::{
    collections::HashSet,
    fs,
    io::{self, ErrorKind},
    path::Path,
};

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimes {
    pub fn total(&self) -> u64 {
        self.user
            .saturating_add(self.nice)
            .saturating_add(self.system)
            .saturating_add(self.idle)
            .saturating_add(self.iowait)
            .saturating_add(self.irq)
            .saturating_add(self.softirq)
            .saturating_add(self.steal)
    }

    pub fn idle_all(&self) -> u64 {
        self.idle.saturating_add(self.iowait)
    }

    /// Busy percentage between two samples. Deltas saturate at zero, since
    /// per-CPU `iowait` can go backwards.
    pub fn usage_since(&self, prev: &CpuTimes) -> f32 {
        let elapsed = self.total().saturating_sub(prev.total());
        if elapsed == 0 {
            return 0.0;
        }
        let idle = self.idle_all().saturating_sub(prev.idle_all()).min(elapsed);
        (100.0 * (elapsed - idle) as f32 / elapsed as f32).clamp(0.0, 100.0)
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CpuStat {
    pub total: CpuTimes,
    /// Online CPUs only: offline ones have no `cpuN` line, so positions don't
    /// match CPU numbers. Match samples by `cpu`.
    pub cores: Vec<CoreTimes>,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CoreTimes {
    /// N from `cpuN`.
    pub cpu: u32,
    pub times: CpuTimes,
}

fn parse_cpu_times(fields: &str) -> io::Result<CpuTimes> {
    let values = fields
        .split_whitespace()
        .map(|field| {
            field
                .parse::<u64>()
                .map_err(|_| invalid_data(format!("invalid CPU counter: {field}")))
        })
        .collect::<io::Result<Vec<_>>>()?;
    if values.is_empty() {
        return Err(invalid_data("CPU line has no counters"));
    }

    Ok(CpuTimes {
        user: values.first().copied().unwrap_or_default(),
        nice: values.get(1).copied().unwrap_or_default(),
        system: values.get(2).copied().unwrap_or_default(),
        idle: values.get(3).copied().unwrap_or_default(),
        iowait: values.get(4).copied().unwrap_or_default(),
        irq: values.get(5).copied().unwrap_or_default(),
        softirq: values.get(6).copied().unwrap_or_default(),
        steal: values.get(7).copied().unwrap_or_default(),
    })
}

pub fn parse_stat(s: &str) -> io::Result<CpuStat> {
    let mut total = None;
    let mut cores = Vec::new();

    for line in s.lines() {
        let Some((name, fields)) = line.split_once(char::is_whitespace) else {
            continue;
        };

        if name == "cpu" {
            if total.is_some() {
                return Err(invalid_data("duplicate aggregate CPU line"));
            }
            total = Some(parse_cpu_times(fields)?);
        } else if let Some(cpu) = name
            .strip_prefix("cpu")
            .filter(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|index| index.parse::<u32>().ok())
        {
            cores.push(CoreTimes {
                cpu,
                times: parse_cpu_times(fields)?,
            });
        }
    }

    total
        .map(|total| CpuStat { total, cores })
        .ok_or_else(|| invalid_data("missing aggregate CPU line"))
}

pub fn read_stat(root: &Path) -> io::Result<CpuStat> {
    parse_stat(&fs::read_to_string(root.join("proc/stat"))?)
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct MemInfo {
    pub total_kib: u64,
    pub available_kib: u64,
    pub free_kib: u64,
    pub buffers_kib: u64,
    pub cached_kib: u64,
    pub swap_total_kib: u64,
    pub swap_free_kib: u64,
}

impl MemInfo {
    pub fn used_kib(&self) -> u64 {
        self.total_kib.saturating_sub(self.available_kib)
    }

    pub fn swap_used_kib(&self) -> u64 {
        self.swap_total_kib.saturating_sub(self.swap_free_kib)
    }
}

pub fn parse_meminfo(s: &str) -> io::Result<MemInfo> {
    let mut meminfo = MemInfo::default();
    let mut found_total = false;
    let mut found_available = false;

    for line in s.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let Some(value) = value.split_whitespace().next() else {
            if matches!(
                name,
                "MemTotal"
                    | "MemAvailable"
                    | "MemFree"
                    | "Buffers"
                    | "Cached"
                    | "SwapTotal"
                    | "SwapFree"
            ) {
                return Err(invalid_data(format!("missing value for {name}")));
            }
            continue;
        };
        let value = value
            .parse::<u64>()
            .map_err(|_| invalid_data(format!("invalid value for {name}")))?;

        match name {
            "MemTotal" => {
                meminfo.total_kib = value;
                found_total = true;
            }
            "MemAvailable" => {
                meminfo.available_kib = value;
                found_available = true;
            }
            "MemFree" => meminfo.free_kib = value,
            "Buffers" => meminfo.buffers_kib = value,
            "Cached" => meminfo.cached_kib = value,
            "SwapTotal" => meminfo.swap_total_kib = value,
            "SwapFree" => meminfo.swap_free_kib = value,
            _ => {}
        }
    }

    if !found_total || !found_available {
        return Err(invalid_data("missing MemTotal or MemAvailable"));
    }
    Ok(meminfo)
}

pub fn read_meminfo(root: &Path) -> io::Result<MemInfo> {
    parse_meminfo(&fs::read_to_string(root.join("proc/meminfo"))?)
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PsiResource {
    Cpu,
    Memory,
    Io,
}

impl PsiResource {
    fn file_name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Io => "io",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PsiLine {
    pub avg10: f32,
    pub avg60: f32,
    pub avg300: f32,
    pub total_us: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pressure {
    pub some: PsiLine,
    pub full: Option<PsiLine>,
}

fn parse_psi_line(fields: &str) -> io::Result<PsiLine> {
    let mut avg = [None; 3];
    let mut total_us = None;

    for field in fields.split_whitespace() {
        let Some((name, value)) = field.split_once('=') else {
            return Err(invalid_data(format!("invalid PSI field: {field}")));
        };
        let slot = match name {
            "avg10" => 0,
            "avg60" => 1,
            "avg300" => 2,
            "total" => {
                total_us = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| invalid_data("invalid PSI total"))?,
                );
                continue;
            }
            // Tolerate fields added by future kernels.
            _ => continue,
        };
        avg[slot] = Some(
            value
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| invalid_data(format!("invalid PSI {name}")))?,
        );
    }

    let [Some(avg10), Some(avg60), Some(avg300)] = avg else {
        return Err(invalid_data("missing PSI average"));
    };
    Ok(PsiLine {
        avg10,
        avg60,
        avg300,
        total_us: total_us.ok_or_else(|| invalid_data("missing PSI total"))?,
    })
}

pub fn parse_pressure(s: &str) -> io::Result<Pressure> {
    let mut some = None;
    let mut full = None;

    for line in s.lines() {
        let Some((kind, fields)) = line.split_once(char::is_whitespace) else {
            return Err(invalid_data("invalid PSI line"));
        };
        match kind {
            "some" if some.is_none() => some = Some(parse_psi_line(fields)?),
            "full" if full.is_none() => full = Some(parse_psi_line(fields)?),
            "some" | "full" => return Err(invalid_data(format!("duplicate PSI {kind} line"))),
            _ => return Err(invalid_data(format!("unknown PSI line: {kind}"))),
        }
    }

    Ok(Pressure {
        some: some.ok_or_else(|| invalid_data("missing PSI some line"))?,
        full,
    })
}

pub fn read_pressure(root: &Path, res: PsiResource) -> io::Result<Pressure> {
    parse_pressure(&fs::read_to_string(
        root.join("proc/pressure").join(res.file_name()),
    )?)
}

/// Current frequency per CPU in kHz, ordered by CPU number; `None` where
/// cpufreq is unavailable or unreadable.
pub fn read_cpufreq_khz(root: &Path) -> io::Result<Vec<Option<u64>>> {
    let cpus = root.join("sys/devices/system/cpu");
    let entries = match fs::read_dir(cpus) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut cpu_dirs = Vec::new();

    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }

        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(index) = name.strip_prefix("cpu") else {
            continue;
        };
        let Ok(index) = index.parse::<u64>() else {
            continue;
        };
        cpu_dirs.push((index, entry.path()));
    }
    cpu_dirs.sort_unstable_by_key(|(index, _)| *index);

    Ok(cpu_dirs
        .into_iter()
        .map(|(_, path)| {
            fs::read_to_string(path.join("cpufreq/scaling_cur_freq"))
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ThrottleCounts {
    pub core: u64,
    pub package: u64,
}

/// Aggregate Intel thermal-throttling counters. Package counters are shared by
/// all CPUs in a physical package, so each package contributes once.
pub fn read_throttle_counts(root: &Path) -> io::Result<Option<ThrottleCounts>> {
    let cpus = root.join("sys/devices/system/cpu");
    let entries = match fs::read_dir(cpus) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut counts = ThrottleCounts::default();
    let mut packages = HashSet::new();
    let mut found = false;

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(index) = name.strip_prefix("cpu") else {
            continue;
        };
        if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }

        let path = entry.path();
        let throttle = path.join("thermal_throttle");
        if !throttle.is_dir() {
            continue;
        }
        found = true;
        if let Some(core) = read_counter(&throttle.join("core_throttle_count")) {
            counts.core = counts.core.saturating_add(core);
        }

        let package = read_counter(&path.join("topology/physical_package_id"));
        let package_count = read_counter(&throttle.join("package_throttle_count"));
        if let (Some(package), Some(package_count)) = (package, package_count)
            && packages.insert(package)
        {
            counts.package = counts.package.saturating_add(package_count);
        }
    }

    Ok(found.then_some(counts))
}

fn read_counter(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::ErrorKind,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::test_support::fixture;

    static TEMP_DIR_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn reads_fixture_samples() {
        let root = fixture("ryzen-rx9070");
        let stat = read_stat(&root).unwrap();
        assert!(stat.total.total() > 0);
        assert_eq!(stat.cores.len(), 8);

        let meminfo = read_meminfo(&root).unwrap();
        assert!(meminfo.total_kib > 0);
        assert!(meminfo.used_kib() < meminfo.total_kib);

        for resource in [PsiResource::Cpu, PsiResource::Memory, PsiResource::Io] {
            let pressure = read_pressure(&root, resource).unwrap();
            assert!(pressure.some.avg10 >= 0.0);
            assert!(pressure.full.is_some());
        }

        let freqs = read_cpufreq_khz(&root).unwrap();
        assert_eq!(freqs.len(), 8);
        assert!(freqs.iter().all(Option::is_some));
    }

    #[test]
    fn ignores_unknown_psi_fields() {
        let p =
            parse_pressure("some avg10=0.10 avg60=0.20 avg300=0.30 total=7 avg1=9.0\n").unwrap();
        assert_eq!(p.some.avg300, 0.3);
        assert!(parse_pressure("some avg10=0.10 avg60=0.20 total=7\n").is_err());
    }

    #[test]
    fn parses_short_cpu_lines() {
        let stat = parse_stat("cpu 10 2\ncpu0 4\ncpu1 5 1 2 3\n").unwrap();
        assert_eq!(
            stat.total,
            CpuTimes {
                user: 10,
                nice: 2,
                ..CpuTimes::default()
            }
        );
        assert_eq!(stat.cores[0].times.user, 4);
        assert_eq!(stat.cores[1].times.idle, 3);
    }

    #[test]
    fn parses_pressure_without_full_line() {
        let pressure = parse_pressure("some avg10=1.00 avg60=0.50 avg300=0.25 total=42\n").unwrap();
        assert_eq!(pressure.some.total_us, 42);
        assert_eq!(pressure.full, None);
    }

    #[test]
    fn rejects_garbage_as_invalid_data() {
        for result in [
            parse_stat("not a stat file").map(|_| ()),
            parse_meminfo("not a meminfo file").map(|_| ()),
            parse_pressure("not a pressure file").map(|_| ()),
        ] {
            assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData);
        }
    }

    #[test]
    fn usage_saturates_backwards_counters() {
        let prev = CpuTimes {
            user: 10,
            idle: 10,
            iowait: 5,
            ..CpuTimes::default()
        };
        // iowait dipped by 1 while user advanced 10: still ~100% busy, not 0.
        let cur = CpuTimes {
            user: 20,
            idle: 10,
            iowait: 4,
            ..CpuTimes::default()
        };
        assert_eq!(cur.usage_since(&prev), 100.0);
        // Half busy.
        let cur = CpuTimes {
            user: 15,
            idle: 15,
            iowait: 5,
            ..CpuTimes::default()
        };
        assert_eq!(cur.usage_since(&prev), 50.0);
        // No time elapsed, or a counter reset.
        assert_eq!(prev.usage_since(&prev), 0.0);
        assert_eq!(CpuTimes::default().usage_since(&prev), 0.0);
    }

    #[test]
    fn reads_cpufreq_in_numeric_cpu_order() {
        let root = std::env::temp_dir().join(format!(
            "thermon-procfs-test-{}-{}",
            std::process::id(),
            TEMP_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let cpu_root = root.join("sys/devices/system/cpu");
        for (cpu, frequency) in [
            ("cpu0", Some("1000\n")),
            ("cpu1", None),
            ("cpu3", Some("garbage\n")),
            ("cpu10", Some("2000\n")),
            ("cpu2", Some("1500\n")),
            ("cpufreq", Some("9999\n")),
        ] {
            let path = cpu_root.join(cpu);
            fs::create_dir_all(&path).unwrap();
            if let Some(frequency) = frequency {
                fs::create_dir_all(path.join("cpufreq")).unwrap();
                fs::write(path.join("cpufreq/scaling_cur_freq"), frequency).unwrap();
            }
        }

        assert_eq!(
            read_cpufreq_khz(&root).unwrap(),
            vec![Some(1000), None, Some(1500), None, Some(2000)]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sums_throttle_counts_and_deduplicates_packages() {
        let root = std::env::temp_dir().join(format!(
            "thermon-procfs-test-{}-{}",
            std::process::id(),
            TEMP_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        for (cpu, package, core, package_count) in
            [(0, 0, 1, 10), (1, 0, 2, 10), (2, 1, 3, 20), (3, 1, 4, 20)]
        {
            let path = root.join(format!("sys/devices/system/cpu/cpu{cpu}"));
            fs::create_dir_all(path.join("thermal_throttle")).unwrap();
            fs::create_dir_all(path.join("topology")).unwrap();
            fs::write(
                path.join("topology/physical_package_id"),
                package.to_string(),
            )
            .unwrap();
            fs::write(
                path.join("thermal_throttle/core_throttle_count"),
                core.to_string(),
            )
            .unwrap();
            fs::write(
                path.join("thermal_throttle/package_throttle_count"),
                package_count.to_string(),
            )
            .unwrap();
        }

        assert_eq!(
            read_throttle_counts(&root).unwrap(),
            Some(ThrottleCounts {
                core: 10,
                package: 30,
            })
        );
        fs::remove_dir_all(root).unwrap();
    }
}
