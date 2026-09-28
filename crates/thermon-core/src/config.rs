//! User configuration for sensor presentation and warning thresholds.

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::{
    health::Severity,
    hwmon::{Category, SensorKind},
    sampler::{HiddenSensor, Inventory},
};

fn default_storage_warn() -> Option<f64> {
    Some(65.0)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarnDefaults {
    #[serde(default)]
    pub cpu: Option<f64>,
    #[serde(default)]
    pub gpu: Option<f64>,
    #[serde(default = "default_storage_warn")]
    pub storage: Option<f64>,
    #[serde(default)]
    pub board: Option<f64>,
    #[serde(default)]
    pub other: Option<f64>,
}

impl Default for WarnDefaults {
    fn default() -> Self {
        Self {
            cpu: None,
            gpu: None,
            storage: default_storage_warn(),
            board: None,
            other: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorConfig {
    pub label: Option<String>,
    #[serde(default)]
    pub hide: bool,
    pub warn: Option<f64>,
    pub crit: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertConfig {
    #[serde(default = "default_alerts_enabled")]
    pub enabled: bool,
    #[serde(default = "default_alerts_min_severity")]
    pub min_severity: Severity,
    #[serde(default = "default_alerts_hold_s")]
    pub hold_s: u64,
    #[serde(default = "default_alerts_notify")]
    pub notify: bool,
}

fn default_alerts_enabled() -> bool {
    true
}

fn default_alerts_min_severity() -> Severity {
    Severity::Warn
}

fn default_alerts_hold_s() -> u64 {
    10
}

fn default_alerts_notify() -> bool {
    true
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            enabled: default_alerts_enabled(),
            min_severity: default_alerts_min_severity(),
            hold_s: default_alerts_hold_s(),
            notify: default_alerts_notify(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub interval_ms: Option<u64>,
    #[serde(default)]
    pub warn: WarnDefaults,
    #[serde(default)]
    pub sensors: BTreeMap<String, SensorConfig>,
    #[serde(default)]
    pub alerts: AlertConfig,
}

impl Config {
    /// Parse and validate a config file.
    pub fn parse(s: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(s).map_err(|error| error.to_string())?;
        config.validate()?;
        Ok(config)
    }

    /// Load a config file, treating a missing file as an empty configuration.
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(contents) => {
                Self::parse(&contents).map_err(|error| format!("{}: {error}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    pub fn default_path() -> Option<PathBuf> {
        env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .map(|home| PathBuf::from(home).join(".config"))
            })
            .map(|base| base.join("thermon/config.toml"))
    }

    /// Apply presentation and threshold settings to a discovered inventory.
    pub fn apply(&self, inv: &mut Inventory) -> Vec<String> {
        let mut matched = BTreeMap::<String, bool>::new();
        for key in self.sensors.keys() {
            matched.insert(key.clone(), false);
        }

        let titles: BTreeMap<String, String> = inv
            .chips
            .iter()
            .map(|c| (c.id.clone(), inv.chip_title(c)))
            .collect();
        let mut hidden = Vec::new();
        for chip in &mut inv.chips {
            let chip_title = &titles[&chip.id];
            chip.sensors.retain_mut(|sensor| {
                for key in self.sensors.keys() {
                    if key == &sensor.id || (key.contains('*') && wildcard_matches(key, &sensor.id))
                    {
                        matched.insert(key.clone(), true);
                    }
                }
                let matching_key = self.matching_key(&sensor.id);
                let entry = matching_key.and_then(|key| self.sensors.get(key));
                if entry.is_some_and(|entry| entry.hide) {
                    hidden.push(HiddenSensor {
                        id: sensor.id.clone(),
                        label: sensor.label.clone(),
                        chip: chip_title.clone(),
                    });
                    return false;
                }

                if let Some(label) = entry.and_then(|entry| entry.label.as_ref()) {
                    sensor.label = label.clone();
                }
                if let Some(crit) = entry.and_then(|entry| entry.crit) {
                    sensor.crit = Some(crit);
                }
                sensor.warn = match entry.and_then(|entry| entry.warn) {
                    Some(warn) => (warn > 0.0).then_some(warn),
                    None => self.category_warn(chip.category, sensor.kind, sensor.crit),
                };
                true
            });
        }
        inv.chips.retain(|chip| !chip.sensors.is_empty());
        inv.hidden = hidden;

        matched
            .into_iter()
            .filter_map(|(key, matched)| (!matched).then_some(key))
            .collect()
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(interval_ms) = self.interval_ms
            && !(200..=60_000).contains(&interval_ms)
        {
            return Err("interval_ms must be between 200 and 60000".into());
        }
        if self.alerts.min_severity == Severity::Ok {
            return Err("alerts.min_severity must be info, warn, or crit".into());
        }
        if self.alerts.hold_s > 3_600 {
            return Err("alerts.hold_s must be at most 3600".into());
        }

        for (name, threshold) in [
            ("warn.cpu", self.warn.cpu),
            ("warn.gpu", self.warn.gpu),
            ("warn.storage", self.warn.storage),
            ("warn.board", self.warn.board),
            ("warn.other", self.warn.other),
        ] {
            validate_threshold(name, threshold)?;
        }
        for (key, sensor) in &self.sensors {
            if key.is_empty() {
                return Err("sensor key must not be empty".into());
            }
            if sensor
                .label
                .as_deref()
                .is_some_and(|label| label.trim().is_empty())
            {
                return Err(format!("sensor {key:?} label must not be empty"));
            }
            validate_threshold(&format!("sensor {key:?} warn"), sensor.warn)?;
            validate_threshold(&format!("sensor {key:?} crit"), sensor.crit)?;
            if sensor.crit.is_some_and(|crit| crit < 0.0) {
                return Err(format!("sensor {key:?} crit must not be negative"));
            }
            if let (Some(warn), Some(crit)) = (sensor.warn, sensor.crit)
                && warn > 0.0
                && crit > 0.0
                && warn > crit
            {
                return Err(format!(
                    "sensor {key:?} warn must not exceed its critical threshold"
                ));
            }
        }
        Ok(())
    }

    fn category_warn(
        &self,
        category: Category,
        kind: SensorKind,
        crit: Option<f64>,
    ) -> Option<f64> {
        if kind != SensorKind::Temp {
            return None;
        }
        match category {
            Category::Cpu => self.warn.cpu.or(Some(crit.map_or(95.0, |crit| crit - 5.0))),
            // GPU memory and junction run hot by design; warn well below
            // the sensor's own limit rather than at a fixed number.
            Category::Gpu => self
                .warn
                .gpu
                .or(Some(crit.map_or(90.0, |crit| crit - 10.0))),
            Category::Storage => self.warn.storage,
            Category::Board => self.warn.board,
            Category::Other => self.warn.other,
        }
        .filter(|threshold| *threshold > 0.0)
    }

    fn matching_key<'a>(&'a self, sensor_id: &str) -> Option<&'a str> {
        if let Some((key, _)) = self.sensors.get_key_value(sensor_id) {
            return Some(key);
        }

        self.sensors
            .keys()
            .filter(|key| key.contains('*') && wildcard_matches(key, sensor_id))
            .max_by_key(|key| key.bytes().filter(|byte| *byte != b'*').count())
            .map(String::as_str)
    }
}

fn validate_threshold(name: &str, threshold: Option<f64>) -> Result<(), String> {
    if threshold.is_some_and(|threshold| !threshold.is_finite()) {
        Err(format!("{name} must be finite"))
    } else {
        Ok(())
    }
}

/// Whether a `[sensors."..."]` key with `*` wildcards matches a sensor id.
pub fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut remainder) = value.strip_prefix(first) else {
        return false;
    };
    let mut remaining_parts = parts.peekable();
    while let Some(part) = remaining_parts.next() {
        if remaining_parts.peek().is_none() {
            return remainder.ends_with(part);
        }
        let Some(index) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[index + part.len()..];
    }
    true
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{sampler::Sampler, test_support::fixture};

    #[test]
    fn parses_documented_example() {
        let config = Config::parse(
            r#"
                interval_ms = 1000
                [warn]
                cpu = 85
                gpu = 90
                storage = 65
                board = 60

                [sensors."gigabyte_wmi/temp1"]
                label = "System"
                [sensors."acpitz/temp1"]
                hide = true
                [sensors."amdgpu@0000:03:00.0/junction"]
                warn = 95
                crit = 105
                [sensors."r8169*"]
                hide = true
            "#,
        )
        .unwrap();
        assert_eq!(config.interval_ms, Some(1000));
        assert_eq!(config.warn.board, Some(60.0));
        assert!(config.sensors["r8169*"].hide);
    }

    #[test]
    fn empty_config_has_warning_defaults() {
        assert_eq!(
            Config::parse("").unwrap(),
            Config {
                interval_ms: None,
                warn: WarnDefaults::default(),
                sensors: BTreeMap::new(),
                alerts: AlertConfig::default(),
            }
        );
    }

    #[test]
    fn parses_alert_defaults_and_validation() {
        assert_eq!(Config::default().alerts, AlertConfig::default());
        let config = Config::parse(
            r#"
                [alerts]
                enabled = false
                min_severity = "crit"
                hold_s = 30
                notify = false
            "#,
        )
        .unwrap();
        assert_eq!(
            config.alerts,
            AlertConfig {
                enabled: false,
                min_severity: Severity::Crit,
                hold_s: 30,
                notify: false,
            }
        );
        assert!(Config::parse("[alerts]\nmin_severity = \"ok\"").is_err());
        assert!(Config::parse("[alerts]\nhold_s = 3601").is_err());
    }

    #[test]
    fn rejects_invalid_configuration() {
        for input in [
            "interval_ms = 199",
            "[warn]\ncpu = nan",
            "[sensors.\"x\"]\nlabel = \"  \"",
            "[sensors.\"\"]\nhide = true",
        ] {
            assert!(Config::parse(input).is_err(), "{input}");
        }
        assert!(Config::parse("[sensors.\"x\"]\nlable = \"typo\"").is_err());
        assert!(Config::parse("intervul_ms = 1000").is_err());
        assert!(Config::parse("[sensors.\"x\"]\ncrit = -1").is_err());
        assert!(Config::parse("[sensors.\"x\"]\nwarn = 100\ncrit = 90").is_err());
        let syntax_error = Config::parse("interval_ms = [").unwrap_err();
        assert!(syntax_error.contains("line 1"), "{syntax_error}");
    }

    #[test]
    fn applies_precedence_defaults_and_overrides() {
        let config = Config::parse(
            r#"
                [sensors."*tctl"]
                label = "short"
                [sensors."k10temp*"]
                label = "long"
                [sensors."k10temp@0000:00:18.3/tctl"]
                label = "exact"
                [sensors."acpitz/temp1"]
                hide = true
                [sensors."amdgpu@0000:03:00.0/junction"]
                crit = 105
                [sensors."missing/*"]
                hide = true
            "#,
        )
        .unwrap();
        let mut inventory = Sampler::new(fixture("ryzen-rx9070"), Config::default())
            .unwrap()
            .inventory()
            .clone();
        let unmatched = config.apply(&mut inventory);

        let chip = |id: &str| inventory.chips.iter().find(|chip| chip.id == id).unwrap();
        let sensor = |chip_id: &str, sensor_id: &str| {
            chip(chip_id)
                .sensors
                .iter()
                .find(|sensor| sensor.id == sensor_id)
                .unwrap()
        };
        assert_eq!(
            sensor("k10temp@0000:00:18.3", "k10temp@0000:00:18.3/tctl").label,
            "exact"
        );
        assert_eq!(
            sensor("k10temp@0000:00:18.3", "k10temp@0000:00:18.3/tctl").warn,
            Some(95.0)
        );
        assert_eq!(
            sensor("nvme@0000:04:00.0", "nvme@0000:04:00.0/composite").warn,
            Some(65.0)
        );
        assert_eq!(sensor("gigabyte_wmi", "gigabyte_wmi/temp1").warn, None);
        assert_eq!(
            sensor("amdgpu@0000:03:00.0", "amdgpu@0000:03:00.0/junction").crit,
            Some(105.0)
        );
        assert!(
            chip("amdgpu@0000:03:00.0")
                .sensors
                .iter()
                .filter(|sensor| sensor.kind != SensorKind::Temp)
                .all(|sensor| sensor.warn.is_none())
        );
        assert!(!inventory.chips.iter().any(|chip| chip.id == "acpitz"));
        assert_eq!(unmatched, vec!["missing/*"]);
    }

    #[test]
    fn gpu_warning_defaults_to_ten_below_critical() {
        let mut inventory = Sampler::new(fixture("ryzen-rx9070"), Config::default())
            .unwrap()
            .inventory()
            .clone();
        Config::default().apply(&mut inventory);
        let warn = |id: &str| {
            inventory
                .chips
                .iter()
                .flat_map(|c| &c.sensors)
                .find(|s| s.id == id)
                .unwrap()
                .warn
        };
        assert_eq!(warn("amdgpu@0000:03:00.0/junction"), Some(100.0));
        assert_eq!(warn("amdgpu@0000:03:00.0/mem"), Some(98.0));
        // The iGPU reports no crit.
        assert_eq!(warn("amdgpu@0000:12:00.0/edge"), Some(90.0));
        // An explicit value still wins.
        let mut inventory2 = inventory.clone();
        Config::parse("[warn]\ngpu = 80")
            .unwrap()
            .apply(&mut inventory2);
        let mem = inventory2
            .chips
            .iter()
            .flat_map(|c| &c.sensors)
            .find(|s| s.id.ends_with("/mem"))
            .unwrap();
        assert_eq!(mem.warn, Some(80.0));
    }

    #[test]
    fn hidden_sensors_are_listed_with_their_chip() {
        let mut inventory = Sampler::new(fixture("ryzen-rx9070"), Config::default())
            .unwrap()
            .inventory()
            .clone();
        Config::parse("[sensors.\"acpitz/temp1\"]\nhide = true\n[sensors.\"r8169*\"]\nhide = true")
            .unwrap()
            .apply(&mut inventory);
        let hidden: Vec<(&str, &str)> = inventory
            .hidden
            .iter()
            .map(|h| (h.id.as_str(), h.chip.as_str()))
            .collect();
        assert_eq!(
            hidden,
            [
                ("acpitz/temp1", "ACPI thermal zone"),
                ("r8169_0_a00:00@0000:0a:00.0/temp1", "Network adapter")
            ]
        );
    }

    #[test]
    fn missing_config_loads_defaults() {
        assert_eq!(
            Config::load(Path::new("/nonexistent-thermon-config.toml")).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn wildcard_matching_allows_slashes() {
        assert!(wildcard_matches("a*/c", "a/b/c"));
        assert!(!wildcard_matches("a*/c", "a/b/d"));
    }

    #[test]
    fn cpu_warning_defaults_to_critical_margin_and_sensor_zero_disables_it() {
        let config = Config::parse(
            r#"
                [sensors."k10temp@0000:00:18.3/tctl"]
                crit = 100
                [sensors."k10temp@0000:00:18.3/tccd1"]
                warn = 0
            "#,
        )
        .unwrap();
        let mut inventory = Sampler::new(fixture("ryzen-rx9070"), Config::default())
            .unwrap()
            .inventory()
            .clone();
        config.apply(&mut inventory);
        let sensors = &inventory
            .chips
            .iter()
            .find(|chip| chip.id == "k10temp@0000:00:18.3")
            .unwrap()
            .sensors;
        assert_eq!(
            sensors
                .iter()
                .find(|sensor| sensor.id.ends_with("/tctl"))
                .unwrap()
                .warn,
            Some(95.0)
        );
        assert_eq!(
            sensors
                .iter()
                .find(|sensor| sensor.id.ends_with("/tccd1"))
                .unwrap()
                .warn,
            None
        );
    }
}
