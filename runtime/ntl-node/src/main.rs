//! `ntl-node` — a full NTL node.
//!
//! A thin binary over [`ntl_net::Runtime`]. Full nodes participate in
//! propagation, maintain synapse state, and persist through a storage
//! backend.

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    match runtime.block_on(ntl_net::bin_support::run_node(
        ntl_core::learning::DeploymentClass::FullNode,
        4433,
    )) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
