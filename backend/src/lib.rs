//! Family hub sidecar library: one SQLite store served over HTTP (the companion
//! and the ext-proxy) and stdio MCP (the family agent), plus the daily reminder
//! sweep. Every visibility and edit decision lives in [`authz`].

pub mod api;
pub mod authz;
pub mod host;
pub mod mcp;
pub mod models;
pub mod paths;
pub mod seal_keys;
pub mod seal_sql;
pub mod sidecar_link;
pub mod state;
pub mod store;
pub mod tick;
pub mod validation;
