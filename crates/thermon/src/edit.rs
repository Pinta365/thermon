use std::{
    fs,
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use thermon_core::config::{Config, wildcard_matches};
use toml_edit::{DocumentMut, Item, Table, value};

static TEMP_FILE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

pub struct Outcome {
    pub changed: bool,
    action: Action,
}

enum Action {
    Hid,
    Unhid,
    AlreadyHidden,
    Unchanged,
}

impl Outcome {
    pub fn message(&self, id: &str) -> String {
        match self.action {
            Action::Hid => format!("hid {id}"),
            Action::Unhid => format!("showing {id} again"),
            Action::AlreadyHidden => format!("{id} is already hidden"),
            Action::Unchanged => format!("{id} isn't hidden"),
        }
    }
}

/// Hide a sensor. Undoes an earlier `unhide` override instead of stacking a
/// second entry on it, so hide and unhide are exact inverses.
pub fn hide(path: &Path, id: &str) -> Result<Outcome, String> {
    validate_id(id)?;
    let (mut document, _) = read_document(path)?;
    let sensors = sensors_table(&mut document)?;
    let exact = exact_hide(sensors, id);
    let by_wildcard = wildcard_hides(sensors, id);
    let action = match (exact, by_wildcard) {
        (Some(true), _) | (None, true) => Action::AlreadyHidden,
        (Some(false), true) => {
            remove_hide(sensors, id);
            Action::Hid
        }
        _ => {
            sensor_table(sensors, id)?.insert("hide", value(true));
            Action::Hid
        }
    };
    let changed = matches!(action, Action::Hid);
    if changed {
        write_validated(path, &document)?;
    }
    Ok(Outcome { changed, action })
}

/// Show a sensor again: removes an exact `hide`, or overrides a wildcard one
/// with an exact `hide = false`.
pub fn unhide(path: &Path, id: &str) -> Result<Outcome, String> {
    validate_id(id)?;
    let (mut document, _) = read_document(path)?;
    let sensors = sensors_table(&mut document)?;
    let action = match (exact_hide(sensors, id), wildcard_hides(sensors, id)) {
        (Some(true), wildcard) => {
            remove_hide(sensors, id);
            // With the exact entry gone, a wildcard would hide it again.
            if wildcard {
                sensor_table(sensors, id)?.insert("hide", value(false));
            }
            Action::Unhid
        }
        (None, true) => {
            sensor_table(sensors, id)?.insert("hide", value(false));
            Action::Unhid
        }
        _ => Action::Unchanged,
    };
    let changed = matches!(action, Action::Unhid);
    if changed {
        write_validated(path, &document)?;
    }
    Ok(Outcome { changed, action })
}

/// The exact entry's `hide` value, if it sets one.
fn exact_hide(sensors: &Table, id: &str) -> Option<bool> {
    sensors
        .get(id)
        .and_then(Item::as_table)
        .and_then(|table| table.get("hide"))
        .and_then(Item::as_bool)
}

fn wildcard_hides(sensors: &Table, id: &str) -> bool {
    sensors.iter().any(|(key, item)| {
        key.contains('*')
            && wildcard_matches(key, id)
            && item
                .as_table()
                .and_then(|table| table.get("hide"))
                .and_then(Item::as_bool)
                == Some(true)
    })
}

/// Remove `hide` from the exact entry, and the entry if nothing is left.
fn remove_hide(sensors: &mut Table, id: &str) {
    if let Some(sensor) = sensors.get_mut(id).and_then(Item::as_table_mut) {
        sensor.remove("hide");
        if sensor.is_empty() {
            sensors.remove(id);
        }
    }
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.contains('"') || id.contains(['\n', '\r']) {
        Err("sensor id must not be empty or contain quotes or newlines".into())
    } else {
        Ok(())
    }
}

fn read_document(path: &Path) -> Result<(DocumentMut, bool), String> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            Config::parse(&contents)?;
            contents
                .parse()
                .map(|document| (document, true))
                .map_err(|error: toml_edit::TomlError| error.to_string())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok((DocumentMut::new(), false)),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn sensors_table(document: &mut DocumentMut) -> Result<&mut Table, String> {
    if document.get("sensors").is_none() {
        // Implicit: written only as `[sensors."<id>"]` headers, no bare `[sensors]`.
        let mut table = Table::new();
        table.set_implicit(true);
        document["sensors"] = Item::Table(table);
    }
    document["sensors"]
        .as_table_mut()
        .ok_or_else(|| "[sensors] must be a table".into())
}

fn sensor_table<'a>(sensors: &'a mut Table, id: &str) -> Result<&'a mut Table, String> {
    if !sensors.contains_key(id) {
        sensors.insert(id, Item::Table(Table::new()));
    }
    sensors
        .get_mut(id)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| format!("sensors.{id:?} must be a table"))
}

fn write_validated(path: &Path, document: &DocumentMut) -> Result<(), String> {
    let contents = document.to_string();
    Config::parse(&contents)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config"),
        TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<(), String> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        file.write_all(contents.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        fs::rename(&temporary, path).map_err(|error| format!("{}: {error}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use super::*;
    use thermon_core::sampler::Sampler;

    /// `<tmp>/thermon-edit-test-<pid>-<n>/<name>`: every test gets its own
    /// directory, which `cleanup` removes.
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "thermon-edit-test-{}-{}",
                std::process::id(),
                TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ))
            .join(name)
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures")
            .join(name)
    }

    fn cleanup(path: &Path) {
        let dir = path
            .ancestors()
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("thermon-edit-test-"))
            })
            .expect("test path outside its own directory");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn new_file_has_no_bare_sensors_header() {
        let path = temp_path("fresh.toml");
        hide(&path, "acpitz/temp1").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[sensors.\"acpitz/temp1\"]\nhide = true\n"
        );
        cleanup(&path);
    }

    #[test]
    fn hide_and_unhide_are_inverses() {
        let path = temp_path("roundtrip.toml");
        let original = "# mine\ninterval_ms = 1000  # keep\n\n[sensors.\"r8169*\"]\nhide = true\n";
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, original).unwrap();
        let id = "r8169_0_a00:00@0000:0a:00.0/temp1";
        // Already hidden by the wildcard: nothing to do.
        assert!(!hide(&path, id).unwrap().changed);
        assert!(unhide(&path, id).unwrap().changed);
        // Visible now: unhide again is a no-op, hide removes the override.
        assert!(!unhide(&path, id).unwrap().changed);
        assert!(hide(&path, id).unwrap().changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);

        assert!(hide(&path, "acpitz/temp1").unwrap().changed);
        assert!(unhide(&path, "acpitz/temp1").unwrap().changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        cleanup(&path);
    }

    #[test]
    fn hide_creates_missing_file() {
        let path = temp_path("missing/config.toml");
        let outcome = hide(&path, "acpitz/temp1").unwrap();
        assert!(outcome.changed);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("[sensors.\"acpitz/temp1\"]\nhide = true\n")
        );
        cleanup(&path);
    }

    #[test]
    fn hide_preserves_existing_content_and_is_idempotent() {
        let path = temp_path("preserve/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "# keep this comment\n[warn]\ngpu = 91\n\n[sensors.\"acpitz/temp1\"]\nlabel = \"ACPI\"\n",
        )
        .unwrap();
        assert!(hide(&path, "acpitz/temp1").unwrap().changed);
        let once = fs::read_to_string(&path).unwrap();
        assert!(once.starts_with("# keep this comment\n[warn]\ngpu = 91\n\n"));
        assert!(once.contains("label = \"ACPI\"\nhide = true\n"));
        assert!(!hide(&path, "acpitz/temp1").unwrap().changed);
        assert_eq!(fs::read_to_string(&path).unwrap(), once);
        cleanup(&path);
    }

    #[test]
    fn unhide_exact_removes_only_hide() {
        let path = temp_path("exact/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "[sensors.\"acpitz/temp1\"]\nhide = true\n\n[sensors.\"coretemp/package\"]\nhide = true\nlabel = \"Package\"\n",
        )
        .unwrap();
        assert!(unhide(&path, "acpitz/temp1").unwrap().changed);
        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("acpitz/temp1"));
        assert!(contents.contains("coretemp/package\"]\nhide = true\nlabel = \"Package\""));
        assert!(unhide(&path, "coretemp/package").unwrap().changed);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("label = \"Package\"")
        );
        cleanup(&path);
    }

    #[test]
    fn unhide_wildcard_adds_exact_override() {
        let path = temp_path("wildcard/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[sensors.\"r8169*\"]\nhide = true\n").unwrap();
        let mut inventory = Sampler::new(fixture("ryzen-rx9070"), Config::default())
            .unwrap()
            .inventory()
            .clone();
        let id = inventory
            .chips
            .iter()
            .flat_map(|chip| &chip.sensors)
            .find(|sensor| sensor.id.starts_with("r8169"))
            .unwrap()
            .id
            .clone();
        assert!(unhide(&path, &id).unwrap().changed);
        let config = Config::load(&path).unwrap();
        config.apply(&mut inventory);
        assert!(
            inventory
                .chips
                .iter()
                .flat_map(|chip| &chip.sensors)
                .any(|sensor| sensor.id == id)
        );
        cleanup(&path);
    }

    #[test]
    fn invalid_existing_file_is_not_replaced() {
        let path = temp_path("invalid/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let contents = "[warn\ncpu = 90\n";
        fs::write(&path, contents).unwrap();
        assert!(hide(&path, "acpitz/temp1").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        cleanup(&path);
    }
}
