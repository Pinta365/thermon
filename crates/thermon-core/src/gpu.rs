//! GPU utilisation and VRAM from `/sys/class/drm/card*/device`.
//!
//! Temperatures, fan and power for GPUs come from hwmon; this module adds what
//! only the DRM device exposes. Match the two by [`Gpu::pci`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::{pci_address, read_trimmed, relative_device_path};

/// Fallback only: APUs usually report a small BIOS carve-out as VRAM.
const INTEGRATED_VRAM_MAX: u64 = 2 * 1024 * 1024 * 1024;

/// A heuristic: AMD APUs sit behind the internal bridge at `0000:00:08.x`,
/// however much VRAM the BIOS carves out. Otherwise fall back to VRAM size.
fn amd_integrated(device: Option<&str>, vram_total: impl FnOnce() -> Option<u64>) -> Option<bool> {
    if let Some(dev) = device {
        let parts: Vec<&str> = dev.split('/').collect();
        let at = parts.iter().rposition(|p| crate::util::is_pci_address(p))?;
        if at >= 1 && parts[at - 1].starts_with("0000:00:08.") {
            return Some(true);
        }
        // Behind any other bridge on the root bus: a discrete card in a slot.
        if at >= 1 && crate::util::is_pci_address(parts[at - 1]) {
            return Some(false);
        }
    }
    vram_total().map(|t| t <= INTEGRATED_VRAM_MAX)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    /// Stable id: the PCI address, or the card name if not on PCI.
    pub id: String,
    pub pci: Option<String>,
    /// Kernel driver (`amdgpu`, `i915`, `xe`, `nouveau`, `nvidia`, ...).
    pub driver: Option<String>,
    /// `cardN`; unstable across boots, for display/debugging only.
    pub card: String,
    /// amdgpu only, and a heuristic (see [`amd_integrated`]); `None` otherwise.
    pub integrated: Option<bool>,
    #[serde(skip)]
    pub device_dir: PathBuf,
}

impl Gpu {
    /// True while runtime-suspended; reading DRM stats could wake it.
    pub fn is_suspended(&self) -> bool {
        crate::util::is_runtime_suspended(&self.device_dir.join("power/runtime_status"))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuStats {
    pub busy_percent: Option<u8>,
    pub vram_used_bytes: Option<u64>,
    pub vram_total_bytes: Option<u64>,
}

impl Gpu {
    pub fn read_stats(&self) -> GpuStats {
        let num = |f: &str| {
            read_trimmed(&self.device_dir.join(f))
                .ok()?
                .parse::<u64>()
                .ok()
        };
        GpuStats {
            busy_percent: num("gpu_busy_percent").map(|v| v.min(100) as u8),
            vram_used_bytes: num("mem_info_vram_used"),
            vram_total_bytes: num("mem_info_vram_total"),
        }
    }
}

/// Enumerate DRM cards (not connectors such as `card0-DP-1`), sorted by id.
pub fn discover(root: &Path) -> io::Result<Vec<Gpu>> {
    let class = root.join("sys/class/drm");
    let entries = match fs::read_dir(&class) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut gpus = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(card) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let is_card = card
            .strip_prefix("card")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !is_card {
            continue;
        }

        let device_link = entry.path().join("device");
        let device = relative_device_path(root, &device_link);
        let pci = device.as_deref().and_then(pci_address);
        let driver = fs::read_link(device_link.join("driver"))
            .ok()
            .and_then(|p| p.file_name()?.to_str().map(str::to_string));
        let device_dir = device.as_ref().map_or(device_link, |d| root.join(d));

        let mut gpu = Gpu {
            id: pci.clone().unwrap_or_else(|| card.clone()),
            pci,
            driver,
            card,
            integrated: None,
            device_dir,
        };
        if gpu.driver.as_deref() == Some("amdgpu") {
            gpu.integrated = amd_integrated(device.as_deref(), || {
                // Don't wake a sleeping dGPU just to classify it.
                (!gpu.is_suspended())
                    .then(|| gpu.read_stats().vram_total_bytes)
                    .flatten()
            });
        }
        gpus.push(gpu);
    }
    gpus.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(gpus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fixture;

    #[test]
    fn discovers_both_amdgpus() {
        let gpus = discover(&fixture("ryzen-rx9070")).unwrap();
        assert_eq!(gpus.len(), 2);

        let dgpu = &gpus[0];
        assert_eq!(dgpu.id, "0000:03:00.0");
        assert_eq!(dgpu.driver.as_deref(), Some("amdgpu"));
        assert_eq!(dgpu.integrated, Some(false));
        let stats = dgpu.read_stats();
        assert!(stats.busy_percent.is_some());
        assert_eq!(stats.vram_total_bytes, Some(17_095_983_104));

        let igpu = &gpus[1];
        assert_eq!(igpu.id, "0000:12:00.0");
        assert_eq!(igpu.integrated, Some(true));
    }

    #[test]
    fn integrated_detection() {
        // Strix Halo-like: huge carve-out, but behind the internal bridge.
        let apu = "sys/devices/pci0000:00/0000:00:08.1/0000:c3:00.0";
        assert_eq!(amd_integrated(Some(apu), || Some(96 << 30)), Some(true));
        // Small dGPU in a slot: behind a regular bridge.
        let dgpu = "sys/devices/pci0000:00/0000:00:01.1/0000:01:00.0/0000:02:00.0/0000:03:00.0";
        assert_eq!(amd_integrated(Some(dgpu), || Some(2 << 30)), Some(false));
        // Directly on the root bus (older APUs): VRAM decides.
        assert_eq!(
            amd_integrated(Some("sys/devices/pci0000:00/0000:00:01.0"), || Some(
                512 << 20
            )),
            Some(true)
        );
        assert_eq!(amd_integrated(None, || None), None);
    }

    #[test]
    fn missing_drm_is_empty() {
        assert!(
            discover(Path::new("/nonexistent-thermon"))
                .unwrap()
                .is_empty()
        );
    }
}
