use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use thermon_core::{
    config::Config,
    gpu,
    health::{self, Context, Severity},
    hwmon::{self, Category},
    procfs::{self, ThrottleCounts},
    sampler::Sampler,
};

static TEMP_DIR_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/intel-laptop-synthetic")
}

fn chip<'a>(
    inventory: &'a thermon_core::sampler::Inventory,
    id: &str,
) -> &'a thermon_core::hwmon::Chip {
    inventory
        .chips
        .iter()
        .find(|chip| chip.id == id)
        .unwrap_or_else(|| panic!("missing chip {id}"))
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type().unwrap();
        if file_type.is_dir() {
            copy_tree(&source_path, &destination_path);
        } else if file_type.is_symlink() {
            symlink(fs::read_link(source_path).unwrap(), destination_path).unwrap();
        } else {
            fs::copy(source_path, destination_path).unwrap();
        }
    }
}

#[test]
fn discovers_intel_laptop_chip_topology_and_coretemp() {
    let chips = hwmon::discover(&root()).unwrap();
    assert_eq!(
        chips
            .iter()
            .map(|chip| (chip.id.as_str(), chip.category))
            .collect::<Vec<_>>(),
        vec![
            ("coretemp", Category::Cpu),
            ("nvme@0000:02:00.0", Category::Storage),
            ("acpitz", Category::Board),
            ("pch_cannonlake@0000:00:12.0", Category::Board),
            ("thinkpad", Category::Board),
            ("iwlwifi_1", Category::Other),
        ]
    );

    let coretemp = chips.iter().find(|chip| chip.id == "coretemp").unwrap();
    assert_eq!(
        coretemp
            .sensors
            .iter()
            .map(|sensor| (sensor.id.as_str(), sensor.label.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("coretemp/package-id-0", "Package id 0"),
            ("coretemp/core-0", "Core 0"),
            ("coretemp/core-1", "Core 1"),
            ("coretemp/core-2", "Core 2"),
            ("coretemp/core-3", "Core 3"),
        ]
    );
    assert_eq!(coretemp.sensors[0].max, Some(100.0));
    assert_eq!(coretemp.sensors[0].crit, Some(100.0));
}

#[test]
fn discovers_i915_without_hwmon_or_stats() {
    let gpus = gpu::discover(&root()).unwrap();
    assert_eq!(gpus.len(), 1);
    let gpu = &gpus[0];
    assert_eq!(gpu.id, "0000:00:02.0");
    assert_eq!(gpu.driver.as_deref(), Some("i915"));
    assert_eq!(gpu.integrated, None);
    assert_eq!(gpu.read_stats(), Default::default());
    assert!(
        !hwmon::discover(&root())
            .unwrap()
            .iter()
            .any(|chip| chip.pci.as_deref() == Some("0000:00:02.0"))
    );
}

#[test]
fn sampler_applies_default_warnings_and_current_chip_titles() {
    let sampler = Sampler::new(root(), Config::default()).unwrap();
    let inventory = sampler.inventory();
    assert!(
        chip(inventory, "coretemp")
            .sensors
            .iter()
            .all(|sensor| sensor.warn == Some(95.0))
    );
    assert_eq!(
        inventory.chip_title(chip(inventory, "coretemp")),
        "Processor"
    );
    assert_eq!(
        inventory.chip_title(chip(inventory, "thinkpad")),
        "Motherboard"
    );
    assert_eq!(
        inventory.chip_title(chip(inventory, "pch_cannonlake@0000:00:12.0")),
        "Motherboard"
    );
    assert_eq!(
        inventory.chip_title(chip(inventory, "iwlwifi_1")),
        "Network adapter"
    );
}

#[test]
fn intel_throttle_counts_and_sampler_delta_are_reported() {
    assert_eq!(
        procfs::read_throttle_counts(&root()).unwrap(),
        Some(ThrottleCounts {
            core: 96,
            package: 3,
        })
    );

    let temp_root = std::env::temp_dir().join(format!(
        "thermon-intel-fixture-test-{}-{}",
        std::process::id(),
        TEMP_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    copy_tree(&root(), &temp_root);
    let mut sampler = Sampler::new(&temp_root, Config::default()).unwrap();
    assert_eq!(sampler.sample().cpu.throttle_events, None);
    fs::write(
        temp_root.join("sys/devices/system/cpu/cpu0/thermal_throttle/core_throttle_count"),
        "14\n",
    )
    .unwrap();
    assert_eq!(sampler.sample().cpu.throttle_events, Some(2));
    fs::remove_dir_all(temp_root).unwrap();
}

#[test]
fn health_is_ok_then_warns_for_hot_package() {
    let mut sampler = Sampler::new(root(), Config::default()).unwrap();
    sampler.sample();
    let mut snapshot = sampler.sample();
    let inventory = sampler.inventory();
    let context = Context {
        peak_freq_khz: None,
        processes: None,
    };
    assert_eq!(
        health::assess(inventory, &snapshot, &context).severity,
        Severity::Ok
    );

    snapshot
        .sensors
        .insert("coretemp/package-id-0".into(), 99.0);
    let verdict = health::assess(inventory, &snapshot, &context);
    let finding = verdict
        .findings
        .iter()
        .find(|finding| finding.code == "temp.warn")
        .unwrap();
    assert_eq!(finding.severity, Severity::Warn);
    assert_eq!(finding.title, "Processor Package id 0 is hot");
}
