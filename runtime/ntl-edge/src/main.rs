//! `ntl-edge` — a lightweight NTL node for constrained devices.
//!
//! Same runtime as `ntl-node`, with edge-class defaults: `SQLite` storage, a
//! smaller queue, a shorter fanout, and a 10 ms refractory period that
//! protects a battery-powered device.

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // A single worker thread: an edge node should not spawn a thread per core
    // on hardware that has better uses for them.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    match runtime.block_on(ntl_net::bin_support::run_node(
        ntl_core::learning::DeploymentClass::Edge,
        4434,
    )) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
