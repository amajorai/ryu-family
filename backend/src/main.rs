use std::net::{Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use ryu_family::{
    api,
    host::NotifyHost,
    mcp, paths,
    seal_keys::PluginKeys,
    sidecar_link,
    state::{AppState, Config, DEFAULT_PORT, PLUGIN_ID},
    store::FamilyStore,
    tick,
};

fn main() -> Result<()> {
    // Take the sealing key Core delivered out of the environment before any
    // thread starts (the async runtime spawns workers).
    ryu_family::seal_keys::capture()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let port = std::env::var("RYU_FAMILY_PORT")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let data_dir = paths::data_dir();
    let db_path = paths::db_path();
    let mcp_mode = std::env::args().nth(1).as_deref() == Some("mcp");
    // The sidecar resolves its key normally. The stdio MCP process gets no key
    // from Core and never generates a standalone root (it would seal under a
    // key the sidecar does not hold); it runs its tools through the sidecar.
    let keys = if mcp_mode {
        PluginKeys::resolve_existing(PLUGIN_ID, &data_dir)?
    } else {
        PluginKeys::resolve(PLUGIN_ID, &data_dir)?
    };
    let store = FamilyStore::open_with_keys(db_path.clone(), &keys)
        .with_context(|| format!("opening {}", db_path.display()))?;

    if mcp_mode {
        // The MCP process never runs the reminder sweep and never calls Core.
        return mcp::serve(
            AppState::new(store, Config::from_env(port), None),
            Some(data_dir),
        )
        .await;
    }

    let state = AppState::new(store, Config::from_env(port), NotifyHost::from_env());
    tokio::spawn(tick::run(state.clone()));

    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding {address}"))?;
    // Publish the loopback handoff for the keyless stdio `mcp` process.
    let handoff = sidecar_link::publish(&data_dir, listener.local_addr()?.port())?;
    tracing::info!(port, mount = api::MOUNT, db = %db_path.display(), "ryu-family: listening");
    let served = axum::serve(
        listener,
        api::app_with_handoff(state, handoff.token.clone()),
    )
    .with_graceful_shutdown(sidecar_link::shutdown_signal())
    .await;
    sidecar_link::withdraw(&data_dir, &handoff);
    served.context("family server stopped")?;
    Ok(())
}
