//! Shared entry point for the `ntl-node` and `ntl-edge` binaries.
//!
//! Both are thin wrappers over [`Runtime`]; the only difference is which
//! deployment class they default to. Keeping the body here means they cannot
//! drift apart.

use std::net::SocketAddr;
use std::path::PathBuf;

use ntl_core::NodeConfig;
use ntl_core::config::StorageConfig;
use ntl_core::learning::DeploymentClass;

use crate::{Event, Runtime, RuntimeConfig};

/// Run a node to completion, until ctrl-c.
///
/// Configuration is read from `$NTL_HOME/config.toml` when present, and
/// otherwise synthesised from `class` so a fresh container starts without a
/// separate init step.
///
/// # Errors
/// Returns a message describing why the node could not start.
pub async fn run_node(class: DeploymentClass, default_port: u16) -> Result<(), String> {
    let home = resolve_home();
    let config = load_or_default(&home, class)?;

    let bind: SocketAddr = std::env::var("NTL_LISTEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            format!("{}:{}", config.network.bind_address, default_port)
                .parse()
                .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], default_port)))
        });

    let peers: Vec<SocketAddr> = std::env::var("NTL_PEERS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let store = crate::open_store(&config.storage)?;
    let (runtime, mut events) = Runtime::new(
        store,
        RuntimeConfig {
            bind,
            bootstrap: peers.clone(),
            node: config,
        },
    )
    .map_err(|e| e.to_string())?;

    let (addr, _accept) = runtime.listen().await.map_err(|e| e.to_string())?;
    let _maintenance = runtime.spawn_maintenance();

    tracing::info!(
        node = %runtime.identity().short(),
        %addr,
        ?class,
        "node started"
    );

    for peer in &peers {
        match runtime.dial(*peer).await {
            Ok(()) => tracing::info!(%peer, "dialed bootstrap peer"),
            Err(e) => tracing::warn!(%peer, error = %e, "bootstrap dial failed"),
        }
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down; checkpointing state");
                runtime.node().checkpoint().map_err(|e| e.to_string())?;
                return Ok(());
            }
            event = events.recv() => match event {
                Some(Event::SignatureFailed { peer, pruned }) => {
                    // Either an attack or a serious defect; both warrant a
                    // louder level than the rest.
                    if pruned {
                        tracing::error!(
                            %peer,
                            "signature-failure threshold exceeded; synapse pruned and \
                             re-formation is in cooldown"
                        );
                    } else {
                        tracing::warn!(%peer, "signature verification failed; synapse penalized");
                    }
                }
                Some(Event::HeadersClamped { peer, weight, ttl, .. }) => {
                    // `weight` and `ttl` sit outside the origin signature, so a
                    // clamp means this peer inflated them.
                    tracing::warn!(%peer, weight, ttl, "clamped inflated signal headers");
                }
                Some(Event::OriginKeyUnknown { peer, origin }) => {
                    // Not necessarily the relaying peer's fault, but an
                    // operator needs to see it: these signals are being
                    // dropped, so a topology that produces them steadily is a
                    // topology that is losing traffic.
                    tracing::warn!(%peer, %origin, "dropped: no key for claimed origin");
                }
                Some(Event::Malformed { peer, reason }) => {
                    tracing::warn!(%peer, %reason, "dropped: malformed signal");
                }
                Some(other) => tracing::debug!(?other, "event"),
                None => return Ok(()),
            },
        }
    }
}

fn resolve_home() -> PathBuf {
    if let Some(env) = std::env::var_os("NTL_HOME") {
        return PathBuf::from(env);
    }
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from(".ntl"), |h| PathBuf::from(h).join(".ntl"))
}

fn load_or_default(home: &std::path::Path, class: DeploymentClass) -> Result<NodeConfig, String> {
    let path = home.join("config.toml");
    if path.exists() {
        return NodeConfig::from_file(path.to_str().ok_or("home path is not valid UTF-8")?)
            .map_err(|e| e.to_string());
    }

    std::fs::create_dir_all(home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
    let mut config = NodeConfig::for_class(class);
    config.storage = StorageConfig::Sqlite {
        path: home.join("node.db").to_string_lossy().into_owned(),
        retain_signal_history: false,
    };
    config
        .validate()
        .map_err(|e| format!("bad defaults: {e}"))?;
    Ok(config)
}
