use std::sync::Arc;

use crate::{host::NotifyHost, store::FamilyStore};

pub const PLUGIN_ID: &str = "@ryu/family";
pub const DEFAULT_PORT: u16 = 8025;
pub const DEFAULT_TICK_SECS: u64 = 300;
pub const MIN_TICK_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    /// Inbound bearer for `/api/family/*`; also the outbound bearer for the
    /// `notify.deliver` callback (held by [`NotifyHost`]).
    pub token: Option<String>,
    /// `RYU_MANAGED_NODE`: requests with no caller headers fail closed.
    pub managed_node: bool,
    /// Reminder sweep period in seconds (never below [`MIN_TICK_SECS`]).
    pub tick_secs: u64,
}

impl Config {
    pub fn from_env(port: u16) -> Self {
        Self {
            port,
            token: std::env::var("RYU_EXT_TOKEN")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
            managed_node: crate::paths::is_managed_node(),
            tick_secs: DEFAULT_TICK_SECS,
        }
    }

    /// A configuration for tests and embedding: a fixed bearer, not managed.
    pub fn for_tests(token: &str) -> Self {
        Self {
            port: 0,
            token: Some(token.to_owned()),
            managed_node: false,
            tick_secs: DEFAULT_TICK_SECS,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub store: FamilyStore,
    pub config: Arc<Config>,
    /// The Core callback for `notify.deliver`. `None` when Core did not inject
    /// `RYU_CORE_PORT` and `RYU_EXT_TOKEN`; the sweep then claims nothing.
    pub host: Option<NotifyHost>,
}

impl AppState {
    pub fn new(store: FamilyStore, config: Config, host: Option<NotifyHost>) -> Self {
        Self {
            store,
            config: Arc::new(config),
            host,
        }
    }
}
