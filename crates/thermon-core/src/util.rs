use std::fs;
use std::io;
use std::path::Path;

/// Read a small sysfs/procfs attribute with surrounding whitespace removed.
pub(crate) fn read_trimmed(path: &Path) -> io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}

/// Resolve a `device` symlink and return its path relative to `root`
/// (e.g. `sys/devices/pci0000:00/0000:00:18.3`). `None` if it doesn't resolve
/// or escapes the root.
pub(crate) fn relative_device_path(root: &Path, link: &Path) -> Option<String> {
    let real = fs::canonicalize(link).ok()?;
    let root = fs::canonicalize(root).ok()?;
    let rel = real.strip_prefix(&root).ok()?;
    rel.to_str().map(str::to_string)
}

/// The last PCI address (`dddd:bb:dd.f`) in a device path: the device itself,
/// or the PCI device a child (nvme controller, mdio bus) belongs to.
pub(crate) fn pci_address(device_path: &str) -> Option<String> {
    device_path
        .rsplit('/')
        .find(|c| is_pci_address(c))
        .map(str::to_string)
}

/// The device path up to and including its last PCI component: the PCI
/// device itself, where `power/runtime_status` lives.
pub(crate) fn pci_device_path(device_path: &str) -> Option<String> {
    let parts: Vec<&str> = device_path.split('/').collect();
    let at = parts.iter().rposition(|p| is_pci_address(p))?;
    Some(parts[..=at].join("/"))
}

pub(crate) fn is_runtime_suspended(status_file: &Path) -> bool {
    fs::read_to_string(status_file).is_ok_and(|s| s.trim() == "suspended")
}

pub(crate) fn is_pci_address(s: &str) -> bool {
    let b = s.as_bytes();
    let hex = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_hexdigit);
    b.len() == 12
        && b[4] == b':'
        && b[7] == b':'
        && b[10] == b'.'
        && hex(0..4)
        && hex(5..7)
        && hex(8..10)
        && hex(11..12)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_addresses() {
        assert_eq!(
            pci_address("sys/devices/pci0000:00/0000:00:01.2/0000:04:00.0/nvme/nvme0").as_deref(),
            Some("0000:04:00.0")
        );
        assert_eq!(
            pci_address("sys/devices/pci0000:00/0000:00:18.3").as_deref(),
            Some("0000:00:18.3")
        );
        assert_eq!(
            pci_address("sys/devices/virtual/thermal/thermal_zone0"),
            None
        );
        assert_eq!(pci_address("sys/devices/pci0000:00"), None);
    }
}
