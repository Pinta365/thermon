use std::path::PathBuf;

/// Path to `fixtures/<name>` at the workspace root.
pub(crate) fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}
