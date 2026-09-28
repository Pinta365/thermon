//! Discovery and reading of `/sys/class/hwmon` sensors.
//!
//! `hwmonN` numbering is not stable across boots, so chips are identified by
//! their driver name plus the PCI address of the device they hang off (when
//! there is one), e.g. `amdgpu@0000:03:00.0`. Sensor ids append the label
//! slug: `amdgpu@0000:03:00.0/junction`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::{pci_address, pci_device_path, read_trimmed, relative_device_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Cpu,
    Gpu,
    Storage,
    Board,
    Other,
}

impl Category {
    /// Classify a chip by its hwmon `name`.
    pub fn from_chip_name(name: &str) -> Category {
        match name {
            "k10temp" | "coretemp" | "zenpower" | "cpu_thermal" => Category::Cpu,
            "amdgpu" | "radeon" | "nouveau" | "i915" | "xe" => Category::Gpu,
            "nvme" | "drivetemp" => Category::Storage,
            "acpitz" | "dell_smm" | "thinkpad" | "asus" | "asus_ec_sensors" => Category::Board,
            n if n.ends_with("_wmi")
                || n.starts_with("it87")
                || n.starts_with("it8")
                || n.starts_with("nct")
                || n.starts_with("f71")
                || n.starts_with("w83")
                || n.starts_with("pch_") =>
            {
                Category::Board
            }
            _ => Category::Other,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Category::Cpu => "CPU",
            Category::Gpu => "GPU",
            Category::Storage => "Storage",
            Category::Board => "Board",
            Category::Other => "Other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SensorKind {
    Temp,
    Fan,
    Power,
}

impl SensorKind {
    fn prefix(self) -> &'static str {
        match self {
            SensorKind::Temp => "temp",
            SensorKind::Fan => "fan",
            SensorKind::Power => "power",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            SensorKind::Temp => "°C",
            SensorKind::Fan => "RPM",
            SensorKind::Power => "W",
        }
    }

    /// Divisor from the raw sysfs integer to [`Self::unit`].
    fn scale(self) -> f64 {
        match self {
            SensorKind::Temp => 1_000.0,
            SensorKind::Fan => 1.0,
            SensorKind::Power => 1_000_000.0,
        }
    }

    /// Plausible range for a value in [`Self::unit`]; anything outside is a
    /// driver sentinel (e.g. nvme reports a 65261 °C "max").
    fn sane(self, v: f64) -> bool {
        match self {
            SensorKind::Temp => (-60.0..=250.0).contains(&v),
            SensorKind::Fan => (0.0..=50_000.0).contains(&v),
            SensorKind::Power => (0.0..=5_000.0).contains(&v),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sensor {
    /// Stable id, `<chip id>/<label slug>`.
    pub id: String,
    pub kind: SensorKind,
    /// Label from `*_label`, or the channel name (`temp1`) when there is none.
    pub label: String,
    /// Critical threshold in the kind's unit.
    pub crit: Option<f64>,
    /// Max/high threshold in the kind's unit (fan: max RPM, power: cap).
    pub max: Option<f64>,
    /// Where thermon starts warning. Never set by discovery; comes from config.
    #[serde(default)]
    pub warn: Option<f64>,
    /// A firmware stub reporting a fixed value, not a real measurement.
    #[serde(default)]
    pub placeholder: bool,
    #[serde(skip)]
    pub input: PathBuf,
}

impl Sensor {
    /// Read the current value in the kind's unit.
    pub fn read(&self) -> io::Result<f64> {
        let raw = read_trimmed(&self.input)?;
        let v = raw
            .parse::<i64>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{raw:?}: {e}")))?
            as f64
            / self.kind.scale();
        if self.kind.sane(v) {
            Ok(v)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} out of range: {v}", self.id),
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chip {
    /// Stable id: `name@pci`, plus `-ataN`/`-i2cADDR` for devices on a
    /// shared controller; plain `name` off PCI.
    pub id: String,
    pub name: String,
    pub category: Category,
    pub pci: Option<String>,
    /// Device path relative to the root (`sys/devices/...`), if any.
    pub device: Option<String>,
    pub sensors: Vec<Sensor>,
    /// The `hwmonN` directory; unstable, for debugging only.
    #[serde(skip)]
    pub dir: PathBuf,
    /// `power/runtime_status` of the PCI device the chip belongs to.
    #[serde(skip)]
    pub power_status: Option<PathBuf>,
}

impl Chip {
    /// True while the device is runtime-suspended (e.g. an idle laptop dGPU);
    /// reading its sensors could wake it.
    pub fn is_suspended(&self) -> bool {
        self.power_status
            .as_deref()
            .is_some_and(crate::util::is_runtime_suspended)
    }
}

/// Enumerate all hwmon chips under `root`, sorted by category then id.
pub fn discover(root: &Path) -> io::Result<Vec<Chip>> {
    let class = root.join("sys/class/hwmon");
    let entries = match fs::read_dir(&class) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut chips = Vec::new();
    for entry in entries {
        let dir = entry?.path();
        if let Some(chip) = load_chip(root, &dir) {
            chips.push(chip);
        }
    }

    disambiguate(&mut chips);
    for chip in &mut chips {
        let chip_id = chip.id.clone();
        assign_sensor_ids(&chip_id, &mut chip.sensors);
    }
    chips.sort_by(|a, b| (a.category, &a.id).cmp(&(b.category, &b.id)));
    Ok(chips)
}

fn load_chip(root: &Path, dir: &Path) -> Option<Chip> {
    let name = read_trimmed(&dir.join("name")).ok()?;
    let device = relative_device_path(root, &dir.join("device"));
    let pci = device.as_deref().and_then(pci_address);
    let id = match (&pci, device.as_deref().and_then(bus_child_key)) {
        (Some(pci), Some(key)) => format!("{name}@{pci}-{key}"),
        (Some(pci), None) => format!("{name}@{pci}"),
        (None, _) => name.clone(),
    };
    let power_status = device
        .as_deref()
        .and_then(pci_device_path)
        .map(|p| root.join(p).join("power/runtime_status"));
    let mut sensors = load_sensors(dir);
    if sensors.is_empty() {
        return None;
    }
    if name == "acpitz"
        && device
            .as_deref()
            .is_some_and(|d| acpi_placeholder(&root.join(d)))
    {
        for s in &mut sensors {
            s.placeholder = true;
        }
    }
    Some(Chip {
        id,
        category: Category::from_chip_name(&name),
        name,
        pci,
        device,
        sensors,
        dir: dir.to_path_buf(),
        power_status,
    })
}

/// Some firmware defines an ACPI thermal zone with a hard-coded temperature.
/// Its trip points give it away: a "critical" point below 30 °C is impossible
/// for real hardware.
fn acpi_placeholder(zone: &Path) -> bool {
    (0..16).any(|i| {
        let kind = read_trimmed(&zone.join(format!("trip_point_{i}_type")));
        let temp = read_trimmed(&zone.join(format!("trip_point_{i}_temp")));
        matches!((kind, temp), (Ok(k), Ok(t))
            if k == "critical" && t.parse::<i64>().is_ok_and(|t| t > 0 && t < 30_000))
    })
}

/// SATA disks and DIMM sensors share one controller's PCI address; the ATA
/// port or I2C address tells them apart, and stays put when devices are added.
fn bus_child_key(device: &str) -> Option<String> {
    let parts: Vec<&str> = device.split('/').collect();
    let pci_at = parts.iter().rposition(|p| crate::util::is_pci_address(p))?;
    let below = &parts[pci_at + 1..];
    if let Some(ata) = below.iter().find(|p| {
        p.strip_prefix("ata")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }) {
        return Some((*ata).to_string());
    }
    // I2C client "<bus>-<4 hex addr>": the bus number is dynamic, the address isn't.
    below.iter().rev().find_map(|p| {
        let (bus, addr) = p.split_once('-')?;
        (bus.bytes().all(|b| b.is_ascii_digit())
            && !bus.is_empty()
            && addr.len() == 4
            && addr.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("i2c{addr}"))
    })
}

fn load_sensors(dir: &Path) -> Vec<Sensor> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    // (kind, channel) -> input file name
    let mut channels: Vec<(SensorKind, u32, String)> = Vec::new();
    for entry in entries.flatten() {
        let file = entry.file_name();
        let Some(file) = file.to_str() else { continue };
        for kind in [SensorKind::Temp, SensorKind::Fan, SensorKind::Power] {
            if let Some(n) = channel_number(file, kind.prefix()) {
                let base = format!("{}{n}", kind.prefix());
                // Power: `average` is the smoothed reading amdgpu dGPUs expose;
                // prefer it over `input` when both exist.
                let chosen =
                    if kind == SensorKind::Power && dir.join(format!("{base}_average")).exists() {
                        format!("{base}_average")
                    } else {
                        file.to_string()
                    };
                if !channels.iter().any(|(k, c, _)| *k == kind && *c == n) {
                    channels.push((kind, n, chosen));
                }
            }
        }
    }
    channels.sort();

    channels
        .into_iter()
        .map(|(kind, n, input)| {
            let base = format!("{}{n}", kind.prefix());
            let label = read_trimmed(&dir.join(format!("{base}_label")))
                .ok()
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| base.clone());
            let threshold = |suffix: &str| -> Option<f64> {
                let raw = read_trimmed(&dir.join(format!("{base}_{suffix}"))).ok()?;
                let v = raw.parse::<i64>().ok()? as f64 / kind.scale();
                (kind.sane(v) && v > 0.0).then_some(v)
            };
            let max = match kind {
                // The cap in effect; the settable ceiling only if there's none.
                SensorKind::Power => threshold("cap").or_else(|| threshold("cap_max")),
                _ => threshold("max"),
            };
            Sensor {
                id: String::new(),
                kind,
                label,
                crit: threshold("crit"),
                max,
                warn: None,
                placeholder: false,
                input: dir.join(input),
            }
        })
        .collect()
}

/// `temp3_input` / `power1_average` -> Some(3) / Some(1) for the given prefix.
fn channel_number(file: &str, prefix: &str) -> Option<u32> {
    let rest = file.strip_prefix(prefix)?;
    let (num, suffix) = rest.split_once('_')?;
    if !matches!(suffix, "input" | "average") {
        return None;
    }
    num.parse().ok()
}

/// Make chip ids unique: colliding ids get the device directory name appended,
/// and anything still colliding gets `#n` in device-path order.
fn disambiguate(chips: &mut [Chip]) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for c in chips.iter() {
        *counts.entry(c.id.clone()).or_default() += 1;
    }
    for c in chips.iter_mut() {
        if counts[&c.id] > 1
            && let Some(dev) = c.device.as_deref().and_then(|d| d.rsplit('/').next())
        {
            c.id = format!("{}@{dev}", c.name);
        }
    }

    chips.sort_by(|a, b| (&a.id, &a.device).cmp(&(&b.id, &b.device)));
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for c in chips.iter() {
        *counts.entry(c.id.clone()).or_default() += 1;
    }
    for c in chips.iter_mut() {
        if counts[&c.id] > 1 {
            let n = seen.entry(c.id.clone()).or_default();
            *n += 1;
            c.id = format!("{}#{n}", c.id);
        }
    }
}

fn assign_sensor_ids(chip_id: &str, sensors: &mut [Sensor]) {
    let mut used: HashSet<String> = HashSet::new();
    for s in sensors.iter_mut() {
        let mut base = slug(&s.label);
        if base.is_empty() {
            base = s.kind.prefix().to_string();
        }
        // Same label on two kinds (temp "PPT" and power "PPT") or two channels:
        // add the kind, then a number, until it's unused.
        let mut candidate = base.clone();
        let mut n = 1;
        while used.contains(&candidate) {
            candidate = if n == 1 {
                format!("{base}-{}", s.kind.prefix())
            } else {
                format!("{base}-{}{n}", s.kind.prefix())
            };
            n += 1;
        }
        used.insert(candidate.clone());
        s.id = format!("{chip_id}/{candidate}");
    }
}

/// Lowercase, alphanumerics kept, everything else collapsed to `-`.
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixture;

    fn chip<'a>(chips: &'a [Chip], id: &str) -> &'a Chip {
        chips.iter().find(|c| c.id == id).unwrap_or_else(|| {
            panic!(
                "no chip {id}; have {:?}",
                chips.iter().map(|c| &c.id).collect::<Vec<_>>()
            )
        })
    }

    #[test]
    fn discovers_stable_ids_on_ryzen_fixture() {
        let chips = discover(&fixture("ryzen-rx9070")).unwrap();
        let ids: Vec<&str> = chips.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "k10temp@0000:00:18.3",
                "amdgpu@0000:03:00.0",
                "amdgpu@0000:12:00.0",
                "nvme@0000:04:00.0",
                "acpitz",
                "gigabyte_wmi",
                "prom21_xhci@0000:10:00.0",
                "r8169_0_a00:00@0000:0a:00.0",
            ]
        );
    }

    #[test]
    fn labels_units_and_thresholds() {
        let chips = discover(&fixture("ryzen-rx9070")).unwrap();

        let dgpu = chip(&chips, "amdgpu@0000:03:00.0");
        let ids: Vec<&str> = dgpu.sensors.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "amdgpu@0000:03:00.0/edge",
                "amdgpu@0000:03:00.0/junction",
                "amdgpu@0000:03:00.0/mem",
                "amdgpu@0000:03:00.0/fan1",
                "amdgpu@0000:03:00.0/ppt",
            ]
        );
        let junction = &dgpu.sensors[1];
        assert_eq!(junction.crit, Some(110.0));
        assert!(junction.input.ends_with("temp2_input"));
        let ppt = &dgpu.sensors[4];
        assert!(ppt.input.ends_with("power1_average"));
        assert_eq!(ppt.max, Some(340.0));

        // iGPU only has power1_input.
        let igpu = chip(&chips, "amdgpu@0000:12:00.0");
        assert!(
            igpu.sensors
                .iter()
                .any(|s| s.input.ends_with("power1_input"))
        );

        // nvme's 65261 °C sentinel max is dropped; the real one kept.
        let nvme = chip(&chips, "nvme@0000:04:00.0");
        assert_eq!(nvme.sensors[0].label, "Composite");
        assert_eq!(nvme.sensors[0].max, Some(80.85));
        assert_eq!(nvme.sensors[1].id, "nvme@0000:04:00.0/sensor-1");
        assert_eq!(nvme.sensors[1].max, None);

        // Unlabelled channels fall back to the channel name.
        let wmi = chip(&chips, "gigabyte_wmi");
        assert_eq!(wmi.category, Category::Board);
        assert_eq!(wmi.sensors.len(), 5);
        assert_eq!(wmi.sensors[0].label, "temp1");
    }

    #[test]
    fn reads_values_in_units() {
        let chips = discover(&fixture("ryzen-rx9070")).unwrap();
        for c in &chips {
            for s in &c.sensors {
                let v = s.read().unwrap_or_else(|e| panic!("{}: {e}", s.id));
                assert!(s.kind.sane(v), "{} = {v}", s.id);
            }
        }
        let cpu = chip(&chips, "k10temp@0000:00:18.3");
        assert_eq!(cpu.sensors[0].label, "Tctl");
        let t = cpu.sensors[0].read().unwrap();
        assert!((20.0..100.0).contains(&t), "{t}");
    }

    #[test]
    fn missing_class_dir_is_empty() {
        assert!(
            discover(Path::new("/nonexistent-thermon"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn categories() {
        assert_eq!(Category::from_chip_name("coretemp"), Category::Cpu);
        assert_eq!(Category::from_chip_name("it8688"), Category::Board);
        assert_eq!(Category::from_chip_name("nct6798"), Category::Board);
        assert_eq!(Category::from_chip_name("drivetemp"), Category::Storage);
        assert_eq!(Category::from_chip_name("r8169_0_a00:00"), Category::Other);
    }

    #[test]
    fn acpi_placeholder_zone_is_flagged() {
        let chips = discover(&fixture("ryzen-rx9070")).unwrap();
        let acpi = chip(&chips, "acpitz");
        assert!(acpi.sensors.iter().all(|s| s.placeholder));
        assert!(
            chips
                .iter()
                .filter(|c| c.id != "acpitz")
                .flat_map(|c| &c.sensors)
                .all(|s| !s.placeholder)
        );
    }

    #[test]
    fn generated_ids_never_collide() {
        let sensor = |label: &str| Sensor {
            id: String::new(),
            kind: SensorKind::Temp,
            label: label.into(),
            crit: None,
            max: None,
            warn: None,
            placeholder: false,
            input: PathBuf::new(),
        };
        let mut sensors: Vec<Sensor> = ["A B", "A-B", "a b-temp", "a b"].map(sensor).into();
        assign_sensor_ids("foo", &mut sensors);
        let ids: HashSet<&str> = sensors.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids.len(),
            4,
            "{:?}",
            sensors.iter().map(|s| &s.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn shared_controller_devices_get_stable_keys() {
        let ata = "sys/devices/pci0000:00/0000:00:17.0/ata3/host2/target2:0:0/2:0:0:0";
        assert_eq!(bus_child_key(ata).as_deref(), Some("ata3"));
        let i2c = "sys/devices/pci0000:00/0000:00:1f.4/i2c-0/0-0051";
        assert_eq!(bus_child_key(i2c).as_deref(), Some("i2c0051"));
        // Ordinary PCI devices and nvme controllers keep the plain id.
        assert_eq!(bus_child_key("sys/devices/pci0000:00/0000:00:18.3"), None);
        assert_eq!(
            bus_child_key("sys/devices/pci0000:00/0000:00:01.2/0000:04:00.0/nvme/nvme0"),
            None
        );
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Sensor 1"), "sensor-1");
        assert_eq!(slug("Package id 0"), "package-id-0");
        assert_eq!(slug("  --"), "");
    }

    #[test]
    fn channel_numbers() {
        assert_eq!(channel_number("temp12_input", "temp"), Some(12));
        assert_eq!(channel_number("power1_average", "power"), Some(1));
        assert_eq!(channel_number("temp1_label", "temp"), None);
        assert_eq!(channel_number("tempx_input", "temp"), None);
    }
}
