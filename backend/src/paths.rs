//! Shared Ryu sidecar data-directory seam and server-owned process wiring.

use std::path::PathBuf;

const MANAGED_NODE_ENV: &str = "RYU_MANAGED_NODE";

/// This app's own data dir: `<data dir>/plugin-data/@ryu+family` (`RYU_PLUGIN_DATA_DIR`
/// when Core spawned the process). Every file the app persists lives under it.
pub fn data_dir() -> PathBuf {
    ryu_sidecar_runtime::plugin_data_dir("@ryu/family")
}

/// `<data dir>/plugin-data/@ryu+family/family.db`.
pub fn db_path() -> PathBuf {
    data_dir().join("family.db")
}

/// Match Core's managed-node truthy values so a request with no verified caller
/// fails closed on a managed node instead of falling back to the local tenant.
pub fn is_managed_node() -> bool {
    std::env::var(MANAGED_NODE_ENV)
        .ok()
        .is_some_and(|value| is_truthy(&value))
}

pub fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}
