//! Turns the individual readers into periodic [`Snapshot`]s.
//!
//! Discovery (which chips/sensors/GPUs exist) is kept in an [`Inventory`] and
//! refreshed occasionally; each [`Sampler::sample`] only reads values.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::gpu::{self, Gpu, GpuStats};
use crate::hwmon::{self, Category, Chip};
use crate::procfs::{self, CpuStat, MemInfo, Pressure, PsiResource};

/// How often hardware is rediscovered (hotplug, driver reloads).
const REDISCOVER_EVERY: Duration = Duration::from_secs(60);

/// The hardware that exists. Changes rarely; clients fetch it once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    pub chips: Vec<Chip>,
    pub gpus: Vec<Gpu>,
    pub cpu_count: usize,
    /// Sensors the config hides, so UIs can offer to show them again.
    #[serde(default)]
    pub hidden: Vec<HiddenSensor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HiddenSensor {
    pub id: String,
    pub label: String,
    /// Title of the chip it belongs to ("ACPI thermal zone", ...).
    pub chip: String,
}

impl Inventory {
    /// The DRM device a GPU chip belongs to.
    pub fn gpu_for(&self, chip: &Chip) -> Option<&Gpu> {
        let pci = chip.pci.as_deref()?;
        self.gpus.iter().find(|g| g.pci.as_deref() == Some(pci))
    }

    /// A human name for a chip ("Processor", "Graphics card", ...), falling
    /// back to the driver name.
    pub fn chip_title(&self, chip: &Chip) -> String {
        let name = chip.name.as_str();
        match chip.category {
            Category::Cpu => "Processor".into(),
            Category::Gpu => match self.gpu_for(chip).and_then(|g| g.integrated) {
                Some(true) => "Integrated GPU".into(),
                Some(false) => "Graphics card".into(),
                None => "GPU".into(),
            },
            _ if name == "nvme" => format!("NVMe {}", chip.pci.as_deref().unwrap_or(""))
                .trim_end()
                .into(),
            _ if name == "drivetemp" => "Drive".into(),
            _ if name == "acpitz" => "ACPI thermal zone".into(),
            Category::Board => "Motherboard".into(),
            _ if name.contains("xhci") => "USB controller".into(),
            _ if [
                "r8169", "igb", "igc", "e1000", "ixgbe", "atlantic", "mt7", "iwlwifi", "ath1",
            ]
            .iter()
            .any(|p| name.starts_with(p)) =>
            {
                "Network adapter".into()
            }
            _ => name.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Unix time in milliseconds.
    pub ts_ms: u64,
    pub cpu: CpuSnapshot,
    pub memory: Option<MemInfo>,
    pub pressure: PressureSnapshot,
    /// Sensor id -> value in the sensor's unit. Sensors whose read failed are absent.
    pub sensors: BTreeMap<String, f64>,
    /// GPU id -> DRM stats.
    pub gpus: BTreeMap<String, GpuStats>,
    /// Chip and GPU ids whose device is runtime-suspended. They aren't read,
    /// so they're absent from `sensors` and `gpus`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asleep: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CpuSnapshot {
    /// Busy percentage over the last interval; `None` on the first sample.
    pub usage: Option<f32>,
    /// Indexed by CPU number; `None` for CPUs that are offline (or were
    /// not online in the previous sample).
    pub cores: Vec<Option<f32>>,
    pub freq_khz: Vec<Option<u64>>,
    /// Throttling events the CPU reported since the previous sample (Intel);
    /// `None` where the CPU has no counters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_events: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PressureSnapshot {
    pub cpu: Option<Pressure>,
    pub memory: Option<Pressure>,
    pub io: Option<Pressure>,
}

impl Snapshot {
    /// Flatten into `(series name, value)` pairs for history. Names:
    /// `cpu.usage`, `cpu.freq_max_mhz`, `mem.used_kib`, `swap.used_kib`,
    /// `psi.{cpu,memory,io}.some10`, `gpu.<id>.busy`, `gpu.<id>.vram_used_mib`,
    /// and each sensor id as-is.
    pub fn series(&self) -> Vec<(String, f32)> {
        let mut out = Vec::with_capacity(self.sensors.len() + 16);
        if let Some(u) = self.cpu.usage {
            out.push(("cpu.usage".into(), u));
        }
        if let Some(max) = self.cpu.freq_khz.iter().flatten().max() {
            out.push(("cpu.freq_max_mhz".into(), *max as f32 / 1000.0));
        }
        if let Some(m) = &self.memory {
            out.push(("mem.used_kib".into(), m.used_kib() as f32));
            out.push(("swap.used_kib".into(), m.swap_used_kib() as f32));
        }
        for (name, p) in [
            ("cpu", &self.pressure.cpu),
            ("memory", &self.pressure.memory),
            ("io", &self.pressure.io),
        ] {
            if let Some(p) = p {
                out.push((format!("psi.{name}.some10"), p.some.avg10));
            }
        }
        for (id, s) in &self.gpus {
            if let Some(b) = s.busy_percent {
                out.push((format!("gpu.{id}.busy"), b as f32));
            }
            if let Some(v) = s.vram_used_bytes {
                out.push((
                    format!("gpu.{id}.vram_used_mib"),
                    (v / (1024 * 1024)) as f32,
                ));
            }
        }
        for (id, v) in &self.sensors {
            out.push((id.clone(), *v as f32));
        }
        out
    }
}

pub struct Sampler {
    root: PathBuf,
    config: Config,
    inventory: Inventory,
    /// Config sensor keys that matched nothing at the last discovery.
    unmatched: Vec<String>,
    discovered_at: Instant,
    prev_stat: Option<CpuStat>,
    prev_throttle_counts: Option<procfs::ThrottleCounts>,
}

impl Sampler {
    pub fn new(root: impl Into<PathBuf>, config: Config) -> io::Result<Self> {
        let root = root.into();
        let (inventory, unmatched) = discover(&root, &config)?;
        Ok(Sampler {
            root,
            config,
            inventory,
            unmatched,
            discovered_at: Instant::now(),
            prev_stat: None,
            prev_throttle_counts: None,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Config sensor keys that matched no sensor (likely typos).
    pub fn unmatched(&self) -> &[String] {
        &self.unmatched
    }

    /// Swap in a new config and rediscover right away. Returns true if the
    /// inventory changed.
    pub fn set_config(&mut self, config: Config) -> bool {
        self.config = config;
        self.discovered_at = Instant::now();
        self.rediscover()
    }

    /// Re-run discovery if it's due. Returns true if the inventory changed.
    pub fn maybe_rediscover(&mut self) -> bool {
        if self.discovered_at.elapsed() < REDISCOVER_EVERY {
            return false;
        }
        self.discovered_at = Instant::now();
        self.rediscover()
    }

    fn rediscover(&mut self) -> bool {
        match discover(&self.root, &self.config) {
            Ok((inv, unmatched)) => {
                self.unmatched = unmatched;
                let changed = inv != self.inventory;
                self.inventory = inv;
                changed
            }
            Err(_) => false,
        }
    }

    pub fn sample(&mut self) -> Snapshot {
        let stat = procfs::read_stat(&self.root).ok();
        let (usage, cores) = match (&self.prev_stat, &stat) {
            (Some(prev), Some(cur)) => (
                Some(cur.total.usage_since(&prev.total)),
                core_usage(prev, cur),
            ),
            _ => (None, Vec::new()),
        };
        if stat.is_some() {
            self.prev_stat = stat;
        }
        let throttle_counts = procfs::read_throttle_counts(&self.root).ok().flatten();
        let throttle_events = match (&self.prev_throttle_counts, throttle_counts) {
            (Some(previous), Some(current)) => Some(
                current
                    .core
                    .saturating_sub(previous.core)
                    .saturating_add(current.package.saturating_sub(previous.package)),
            ),
            _ => None,
        };
        self.prev_throttle_counts = throttle_counts;

        let mut asleep = Vec::new();
        let mut sensors = BTreeMap::new();
        for chip in &self.inventory.chips {
            if chip.is_suspended() {
                asleep.push(chip.id.clone());
                continue;
            }
            for s in &chip.sensors {
                if let Ok(v) = s.read() {
                    sensors.insert(s.id.clone(), v);
                }
            }
        }
        let mut gpus = BTreeMap::new();
        for g in &self.inventory.gpus {
            if g.is_suspended() {
                asleep.push(g.id.clone());
            } else {
                gpus.insert(g.id.clone(), g.read_stats());
            }
        }

        Snapshot {
            ts_ms: now_ms(),
            cpu: CpuSnapshot {
                usage,
                cores,
                freq_khz: procfs::read_cpufreq_khz(&self.root).unwrap_or_default(),
                throttle_events,
            },
            memory: procfs::read_meminfo(&self.root).ok(),
            pressure: PressureSnapshot {
                cpu: procfs::read_pressure(&self.root, PsiResource::Cpu).ok(),
                memory: procfs::read_pressure(&self.root, PsiResource::Memory).ok(),
                io: procfs::read_pressure(&self.root, PsiResource::Io).ok(),
            },
            sensors,
            gpus,
            asleep,
        }
    }
}

/// Per-CPU usage matched by CPU number, so an offline CPU doesn't shift the
/// others.
fn core_usage(prev: &CpuStat, cur: &CpuStat) -> Vec<Option<f32>> {
    let len = cur
        .cores
        .iter()
        .map(|c| c.cpu as usize + 1)
        .max()
        .unwrap_or(0);
    let mut out = vec![None; len];
    for c in &cur.cores {
        if let Some(p) = prev.cores.iter().find(|p| p.cpu == c.cpu) {
            out[c.cpu as usize] = Some(c.times.usage_since(&p.times));
        }
    }
    out
}

fn discover(root: &Path, config: &Config) -> io::Result<(Inventory, Vec<String>)> {
    let mut inv = Inventory {
        chips: hwmon::discover(root)?,
        gpus: gpu::discover(root)?,
        cpu_count: procfs::read_stat(root).map(|s| s.cores.len()).unwrap_or(0),
        hidden: Vec::new(),
    };
    let unmatched = config.apply(&mut inv);
    Ok((inv, unmatched))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixture;

    #[test]
    fn samples_fixture() {
        let mut s = Sampler::new(fixture("ryzen-rx9070"), Config::default()).unwrap();
        assert_eq!(s.inventory().cpu_count, 8);

        let first = s.sample();
        assert_eq!(first.cpu.usage, None);
        assert!(first.sensors.contains_key("amdgpu@0000:03:00.0/junction"));
        assert_eq!(first.gpus.len(), 2);

        // Static fixture: second sample has usage, and it's 0.
        let second = s.sample();
        assert_eq!(second.cpu.usage, Some(0.0));
        assert_eq!(second.cpu.cores.len(), 8);
        assert!(second.cpu.cores.iter().all(Option::is_some));

        // Default config: CPU temperatures without a critical threshold warn at 95.
        let tctl = &s.inventory().chips[0].sensors[0];
        assert_eq!(tctl.warn, Some(95.0));

        let series = second.series();
        let names: Vec<&str> = series.iter().map(|(n, _)| n.as_str()).collect();
        for want in [
            "cpu.usage",
            "cpu.freq_max_mhz",
            "mem.used_kib",
            "psi.io.some10",
            "gpu.0000:03:00.0.busy",
            "gpu.0000:03:00.0.vram_used_mib",
            "k10temp@0000:00:18.3/tctl",
        ] {
            assert!(names.contains(&want), "missing {want} in {names:?}");
        }
    }

    #[test]
    fn cores_are_matched_by_cpu_number() {
        use crate::procfs::parse_stat;
        let prev =
            parse_stat("cpu 0 0 0 0\ncpu0 0 0 0 100\ncpu3 0 0 0 100\ncpu4 0 0 0 100\n").unwrap();
        // cpu3 went offline; cpu4 was fully busy.
        let cur = parse_stat("cpu 0 0 0 0\ncpu0 0 0 0 200\ncpu4 100 0 0 100\n").unwrap();
        let u = core_usage(&prev, &cur);
        assert_eq!(u, vec![Some(0.0), None, None, None, Some(100.0)]);
    }

    #[test]
    fn chip_titles() {
        let s = Sampler::new(fixture("ryzen-rx9070"), Config::default()).unwrap();
        let inv = s.inventory();
        let titles: Vec<String> = inv.chips.iter().map(|c| inv.chip_title(c)).collect();
        assert_eq!(
            titles,
            [
                "Processor",
                "Graphics card",
                "Integrated GPU",
                "NVMe 0000:04:00.0",
                "ACPI thermal zone",
                "Motherboard",
                "USB controller",
                "Network adapter",
            ]
        );
    }

    #[test]
    fn hidden_sensors_are_not_sampled_and_config_swaps_live() {
        let hide =
            Config::parse("[sensors.\"acpitz/*\"]\nhide = true\n[sensors.\"typo/*\"]\nhide = true")
                .unwrap();
        let mut s = Sampler::new(fixture("ryzen-rx9070"), hide).unwrap();
        assert!(!s.sample().sensors.contains_key("acpitz/temp1"));
        assert_eq!(s.unmatched(), ["typo/*"]);

        assert!(s.set_config(Config::default()));
        assert!(s.sample().sensors.contains_key("acpitz/temp1"));
        assert!(s.unmatched().is_empty());
        assert!(!s.set_config(Config::default()));
    }
}
