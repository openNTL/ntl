//! `ntl` — the command-line interface for the Neural Transfer Layer.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use colored::Colorize as _;
use ntl_core::config::StorageConfig;
use ntl_core::learning::DeploymentClass;
use ntl_core::signal::{Signal, SignalType};
use ntl_core::{NodeConfig, NodeStore};
use ntl_net::{Event, Runtime, RuntimeConfig};

#[derive(Parser)]
#[command(name = "ntl")]
#[command(about = "Neural Transfer Layer — signal transport that learns its routes")]
#[command(version)]
struct Cli {
    /// Node directory. Defaults to $`NTL_HOME`, then ~/.ntl
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a node identity, config, and storage
    Init {
        /// Deployment class, which selects coherent defaults
        #[arg(long, value_parser = ["edge", "full-node", "high-traffic"], default_value = "edge")]
        class: String,
        /// Overwrite an existing configuration
        #[arg(long)]
        force: bool,
    },
    /// Run the node
    Start {
        /// Development mode: loopback only, no bootstrap peers
        #[arg(long)]
        dev: bool,
        /// Address to listen on
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// Peer to dial, repeatable
        #[arg(long = "peer")]
        peers: Vec<SocketAddr>,
    },
    /// Emit a signal
    Emit {
        /// Signal type
        #[arg(long, short = 't', default_value = "data")]
        r#type: String,
        /// JSON payload
        #[arg(long, short = 'p')]
        payload: Option<String>,
        /// Signal weight, 0.0 to 1.0
        #[arg(long, short = 'w', default_value = "0.5")]
        weight: f32,
        /// Comma-separated tags
        #[arg(long)]
        tags: Option<String>,
        /// Request at-least-once delivery with failure reporting
        #[arg(long)]
        acknowledged: bool,
        /// Peer to connect to before emitting
        #[arg(long = "peer")]
        peers: Vec<SocketAddr>,
        /// How long to wait for receipts, in seconds
        #[arg(long, default_value = "5")]
        wait: u64,
    },
    /// Listen for signals and print them as they arrive
    Listen {
        /// Filter by signal type
        #[arg(long, short = 't')]
        r#type: Option<String>,
        /// Address to listen on
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// Peer to dial, repeatable
        #[arg(long = "peer")]
        peers: Vec<SocketAddr>,
    },
    /// Show synapses and their learned weights
    Synapses,
    /// Show node status
    Status,
    /// Show known peers
    Topology,
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {e}", "error:".red().bold());
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let home = resolve_home(cli.home.as_ref());

    match cli.command {
        Commands::Init { class, force } => cmd_init(&home, &class, force),
        Commands::Start { dev, listen, peers } => block_on(cmd_start(home, dev, listen, peers)),
        Commands::Emit {
            r#type,
            payload,
            weight,
            tags,
            acknowledged,
            peers,
            wait,
        } => block_on(cmd_emit(
            home,
            &r#type,
            payload.as_deref(),
            weight,
            tags.as_deref(),
            acknowledged,
            peers,
            wait,
        )),
        Commands::Listen {
            r#type,
            listen,
            peers,
        } => block_on(cmd_listen(home, r#type.as_deref(), listen, peers)),
        Commands::Synapses => cmd_synapses(&home),
        Commands::Status => cmd_status(&home),
        Commands::Topology => cmd_topology(&home),
    }
}

/// One place that builds the async runtime, so each command need not.
fn block_on<F: std::future::Future<Output = Result<(), String>>>(f: F) -> Result<(), String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?
        .block_on(f)
}

fn resolve_home(explicit: Option<&PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path.clone();
    }
    if let Some(env) = std::env::var_os("NTL_HOME") {
        return PathBuf::from(env);
    }
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from(".ntl"), |h| PathBuf::from(h).join(".ntl"))
}

fn config_path(home: &std::path::Path) -> PathBuf {
    home.join("config.toml")
}

fn load_config(home: &std::path::Path) -> Result<NodeConfig, String> {
    let path = config_path(home);
    if !path.exists() {
        return Err(format!(
            "no node at {}. Run `ntl init` first.",
            home.display()
        ));
    }
    NodeConfig::from_file(path.to_str().ok_or("node path is not valid UTF-8")?)
        .map_err(|e| e.to_string())
}

fn open_store(config: &NodeConfig) -> Result<std::sync::Arc<dyn NodeStore>, String> {
    let store = ntl_net::open_store(&config.storage)?;
    store.migrate().map_err(|e| e.to_string())?;
    Ok(store)
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

fn cmd_init(home: &std::path::Path, class: &str, force: bool) -> Result<(), String> {
    let path = config_path(home);
    if path.exists() && !force {
        return Err(format!(
            "{} already exists. Pass --force to overwrite.",
            path.display()
        ));
    }

    let deployment = match class {
        "edge" => DeploymentClass::Edge,
        "full-node" => DeploymentClass::FullNode,
        "high-traffic" => DeploymentClass::HighTraffic,
        other => return Err(format!("unknown deployment class {other:?}")),
    };

    std::fs::create_dir_all(home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;

    let db_path = home.join("node.db");
    let mut config = NodeConfig::for_class(deployment);
    config.storage = StorageConfig::Sqlite {
        path: db_path.to_string_lossy().into_owned(),
        retain_signal_history: false,
    };
    config
        .validate()
        .map_err(|e| format!("bad defaults: {e}"))?;

    config
        .to_file(path.to_str().ok_or("config path is not valid UTF-8")?)
        .map_err(|e| e.to_string())?;

    // Create the store and identity now, so `init` either fully succeeds or
    // reports why. Discovering a permissions problem at first `start` would be
    // worse.
    let store = open_store(&config)?;
    let identity = ntl_net::Identity::load_or_create(store.as_ref()).map_err(|e| e.to_string())?;

    println!("{} node initialized", "✓".green().bold());
    println!("  home      {}", home.display());
    println!("  config    {}", path.display());
    println!("  store     {} (sqlite)", db_path.display());
    println!("  class     {class}");
    println!("  identity  {}", identity.short().cyan());
    println!();
    println!("Next: {}", "ntl start --dev".bold());
    Ok(())
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

async fn cmd_start(
    home: PathBuf,
    dev: bool,
    listen: Option<SocketAddr>,
    peers: Vec<SocketAddr>,
) -> Result<(), String> {
    let config = load_config(&home)?;
    let store = open_store(&config)?;

    let bind = listen.unwrap_or_else(|| {
        if dev {
            // Loopback in dev mode: a development node should not be reachable
            // from the network by accident.
            "127.0.0.1:4433".parse().expect("valid literal")
        } else {
            format!("{}:{}", config.network.bind_address, config.network.port)
                .parse()
                .unwrap_or_else(|_| "0.0.0.0:4433".parse().expect("valid literal"))
        }
    });

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

    println!(
        "{} node {} listening on {}",
        "✓".green().bold(),
        runtime.identity().short().cyan(),
        addr.to_string().bold()
    );
    if dev {
        println!("  {} development mode — loopback only", "•".dimmed());
    }

    for peer in &peers {
        match runtime.dial(*peer).await {
            Ok(()) => println!("  {} dialed {peer}", "→".green()),
            Err(e) => eprintln!("  {} {peer}: {e}", "✗".red()),
        }
    }

    println!("  {} ctrl-c to stop", "•".dimmed());

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("\n{} checkpointing and shutting down", "•".dimmed());
                runtime.node().checkpoint().map_err(|e| e.to_string())?;
                return Ok(());
            }
            event = events.recv() => {
                match event {
                    Some(e) => print_event(&e),
                    None => return Ok(()),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// emit
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn cmd_emit(
    home: PathBuf,
    signal_type: &str,
    payload: Option<&str>,
    weight: f32,
    tags: Option<&str>,
    acknowledged: bool,
    peers: Vec<SocketAddr>,
    wait_secs: u64,
) -> Result<(), String> {
    let config = load_config(&home)?;
    let store = open_store(&config)?;

    let parsed_payload = match payload {
        Some(raw) => {
            serde_json::from_str(raw).map_err(|e| format!("--payload is not valid JSON: {e}"))?
        }
        None => serde_json::Value::Null,
    };

    let (runtime, mut events) = Runtime::new(
        store,
        RuntimeConfig {
            // Port 0: an emitting node needs a return path for receipts, but
            // does not need a predictable address.
            bind: "127.0.0.1:0".parse().expect("valid literal"),
            bootstrap: peers.clone(),
            node: config,
        },
    )
    .map_err(|e| e.to_string())?;

    let (_addr, _accept) = runtime.listen().await.map_err(|e| e.to_string())?;

    if peers.is_empty() {
        return Err(
            "no peers to emit to. Pass --peer <addr>, or run `ntl start` and \
             emit from another terminal."
                .to_string(),
        );
    }
    for peer in &peers {
        runtime.dial(*peer).await.map_err(|e| e.to_string())?;
    }

    // Wait for the handshake, rather than sleeping and hoping. `dial` returns
    // when the TCP connection opens, but a peer cannot carry a signal until the
    // signed Discovery exchange has completed and its synapse is in the store.
    // A fixed sleep here raced: it passed locally and failed on a slower CI
    // runner, where the signal was routed to zero peers and the sender then
    // reported a delivery failure for a network that was about to be fine.
    let connected = runtime
        .wait_for_peers(peers.len(), std::time::Duration::from_secs(10))
        .await;
    if connected == 0 {
        return Err(format!(
            "no peer completed a handshake within 10s (dialed {}). \
             Check the address, and that the remote node is running.",
            peers.len()
        ));
    }
    if connected < peers.len() {
        println!(
            "  {} {connected} of {} peers connected; emitting anyway",
            "!".yellow(),
            peers.len()
        );
    }

    let mut builder = parse_type(signal_type)?
        .with_payload(parsed_payload)
        .with_weight(weight);
    if let Some(raw) = tags {
        builder = builder.with_tags(raw.split(',').map(str::trim).collect());
    }
    if acknowledged {
        builder = builder.acknowledged();
    }

    let signal = runtime.emit(builder).await.map_err(|e| e.to_string())?;
    println!(
        "{} emitted {} ({}, weight {weight})",
        "→".green().bold(),
        signal.id.to_string().bold(),
        signal_type
    );
    if acknowledged {
        println!("  {} awaiting receipts for {wait_secs}s", "•".dimmed());
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
    let mut receipts = 0;
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => break,
            event = events.recv() => match event {
                Some(e) => {
                    if matches!(e, Event::ReceiptApplied { .. }) {
                        receipts += 1;
                    }
                    print_event(&e);
                }
                None => break,
            },
        }
    }

    if acknowledged && receipts == 0 {
        // Silence is a real outcome for an acknowledged signal, and the
        // operator needs to know they did not get a guarantee.
        println!(
            "{} no receipt within {wait_secs}s — the signal may not have been delivered",
            "!".yellow().bold()
        );
    }
    runtime.node().checkpoint().map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// listen
// ---------------------------------------------------------------------------

async fn cmd_listen(
    home: PathBuf,
    filter: Option<&str>,
    listen: Option<SocketAddr>,
    peers: Vec<SocketAddr>,
) -> Result<(), String> {
    let config = load_config(&home)?;
    let store = open_store(&config)?;
    let wanted = filter.map(parse_type_name);

    let bind = listen.unwrap_or_else(|| "127.0.0.1:4433".parse().expect("valid literal"));
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

    println!(
        "{} listening on {} for {} signals",
        "✓".green().bold(),
        addr.to_string().bold(),
        filter.unwrap_or("all")
    );
    for peer in &peers {
        let _ = runtime.dial(*peer).await;
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                runtime.node().checkpoint().map_err(|e| e.to_string())?;
                return Ok(());
            }
            event = events.recv() => match event {
                Some(Event::Handled { signal }) => {
                    show_signal(&signal, wanted.as_ref(), None);
                }
                // A signal released by the latency guard was received just as
                // genuinely as one that crossed the threshold; the note says
                // which so an operator can tell the node is backpressured.
                Some(Event::Released { signal: Some(signal), .. }) => {
                    show_signal(&signal, wanted.as_ref(), Some("below threshold; released by guard"));
                }
                Some(other) => print_event(&other),
                None => return Ok(()),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// inspection
// ---------------------------------------------------------------------------

fn cmd_synapses(home: &std::path::Path) -> Result<(), String> {
    let config = load_config(home)?;
    let store = open_store(&config)?;
    let synapses = store
        .list_synapses(&ntl_core::store::SynapseFilter::default())
        .map_err(|e| e.to_string())?;

    if synapses.is_empty() {
        println!("no synapses yet — start the node and connect a peer");
        return Ok(());
    }

    println!(
        "{:<14} {:>8} {:>10} {:<10} {:>6}  {}",
        "PEER".bold(),
        "WEIGHT".bold(),
        "STATE".bold(),
        "SENT/RECV".bold(),
        "ERR".bold(),
        "AFFINITY".bold()
    );
    for s in &synapses {
        let affinity = if s.type_affinity.is_empty() {
            "-".to_string()
        } else {
            let mut pairs: Vec<_> = s.type_affinity.iter().collect();
            pairs.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
            pairs
                .iter()
                .take(3)
                .map(|(k, v)| format!("{k}:{v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let peer: String = s
            .peer
            .0
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect();
        println!(
            "{peer:<14} {:>8.4} {:>10} {:<10} {:>6.2}  {affinity}",
            s.weight,
            format!("{:?}", s.state).to_lowercase(),
            format!("{}/{}", s.signals_transmitted, s.signals_received),
            s.error_rate,
        );
    }

    let total: f32 = synapses.iter().map(|s| s.weight).sum();
    println!();
    println!(
        "total outbound weight {:.4} of {:.1} budget",
        total, config.learning.max_total_outbound_weight
    );
    Ok(())
}

fn cmd_status(home: &std::path::Path) -> Result<(), String> {
    let config = load_config(home)?;
    let store = open_store(&config)?;
    let identity = ntl_net::Identity::load_or_create(store.as_ref()).map_err(|e| e.to_string())?;

    let synapses = store
        .list_synapses(&ntl_core::store::SynapseFilter::default())
        .map_err(|e| e.to_string())?;
    let recent = store
        .recent_decisions(None, 500)
        .map_err(|e| e.to_string())?;

    let pending = recent.iter().filter(|d| !d.outcome.is_resolved()).count();
    let explored = recent.iter().filter(|d| d.explored).count();
    let delivered = recent
        .iter()
        .filter(|d| d.outcome == ntl_core::Outcome::Delivered)
        .count();

    println!("{}", "node".bold());
    println!("  identity   {}", identity.short().cyan());
    println!("  home       {}", home.display());
    println!("  durability {:?}", store.durability());
    println!("  class      {:?}", config.activation.node_class);
    println!();
    println!("{}", "topology".bold());
    println!("  synapses   {}", synapses.len());
    println!(
        "  eligible   {}",
        synapses.iter().filter(|s| s.state.can_carry()).count()
    );
    println!();
    println!("{}", "routing model".bold());
    if recent.is_empty() {
        println!("  no decisions recorded yet");
    } else {
        let total = recent.len();
        #[allow(clippy::cast_precision_loss)]
        let pct = |n: usize| (n as f32 / total as f32) * 100.0;
        println!("  decisions   {total} sampled");
        println!("  delivered   {:.1}%", pct(delivered));
        println!("  exploration {:.1}%", pct(explored));
        println!("  pending     {:.1}%", pct(pending));

        // These two are the model's health check, so say what they mean rather
        // than leaving the operator to infer it.
        //
        // Only warn when there was actually a choice to make: with one
        // eligible synapse there is nothing to explore, and flagging that
        // would train the operator to ignore the warning.
        let eligible = synapses.iter().filter(|s| s.state.can_carry()).count();
        if explored == 0 && eligible > 1 {
            println!(
                "  {} exploration is at zero across {eligible} eligible \
                 synapses — this node has stopped learning",
                "!".yellow().bold()
            );
        }
        if pct(pending) > 80.0 {
            println!(
                "  {} almost every decision is unresolved — no receipts are \
                 arriving, so the weights reflect nothing",
                "!".yellow().bold()
            );
        }
    }
    Ok(())
}

fn cmd_topology(home: &std::path::Path) -> Result<(), String> {
    let config = load_config(home)?;
    let store = open_store(&config)?;
    let peers = store.list_peers(None, 100).map_err(|e| e.to_string())?;

    if peers.is_empty() {
        println!("no known peers");
        return Ok(());
    }
    println!(
        "{:<14} {:<12} {:<10} {}",
        "PEER".bold(),
        "SOURCE".bold(),
        "REGION".bold(),
        "ADDRESSES".bold()
    );
    for p in &peers {
        let id: String = p.id.0.iter().take(6).map(|b| format!("{b:02x}")).collect();
        println!(
            "{id:<14} {:<12} {:<10} {}",
            format!("{:?}", p.source).to_lowercase(),
            p.region.as_deref().unwrap_or("-"),
            if p.addresses.is_empty() {
                "-".to_string()
            } else {
                p.addresses.join(", ")
            }
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn parse_type(name: &str) -> Result<ntl_core::signal::SignalBuilder, String> {
    Ok(match name.to_lowercase().as_str() {
        "data" => Signal::data("cli"),
        "query" => Signal::query("cli"),
        "event" => Signal::event("cli"),
        "command" => Signal::command("cli"),
        "discovery" => Signal::discovery(),
        "heartbeat" => Signal::heartbeat(),
        "receipt" => {
            return Err(
                "receipts are emitted by the protocol, not by hand — they must \
                 reference a real decision"
                    .to_string(),
            );
        }
        other => {
            return Err(format!(
                "unknown signal type {other:?}. Try data, query, event, command, \
             discovery, or heartbeat."
            ));
        }
    })
}

fn parse_type_name(name: &str) -> SignalType {
    match name.to_lowercase().as_str() {
        "data" => SignalType::Data,
        "query" => SignalType::Query,
        "event" => SignalType::Event,
        "command" => SignalType::Command,
        "discovery" => SignalType::Discovery,
        "heartbeat" => SignalType::Heartbeat,
        "receipt" => SignalType::Receipt,
        other => SignalType::Custom(other.to_string()),
    }
}

/// Render an arriving signal, honouring the type filter.
fn show_signal(signal: &Signal, wanted: Option<&SignalType>, note: Option<&str>) {
    if !wanted.is_none_or(|w| &signal.signal_type == w) {
        return;
    }
    println!(
        "{} {} {:?} weight {:.3} from {}",
        "←".green().bold(),
        signal.id.to_string().bold(),
        signal.signal_type,
        signal.weight,
        signal.origin
    );
    if !signal.payload.is_null() {
        println!("  {}", signal.payload);
    }
    if let Some(note) = note {
        println!("  {}", note.dimmed());
    }
}

fn print_event(event: &Event) {
    match event {
        Event::Handled { signal } => println!(
            "{} handled {} {:?}",
            "←".green(),
            signal.id,
            signal.signal_type
        ),
        Event::Forwarded {
            signal_id,
            peers,
            explored,
        } => {
            let tag = if *explored {
                " (exploring)".yellow()
            } else {
                "".normal()
            };
            println!(
                "{} forwarded {signal_id} to {peers} peer(s){tag}",
                "→".dimmed()
            );
        }
        Event::Released { signal_id, .. } => println!(
            "{} released {signal_id} (held past the latency guard)",
            "←".green()
        ),
        Event::Refused { signal_id, reason } => {
            println!("{} refused {signal_id}: {reason:?}", "✗".red());
        }
        Event::ReceiptApplied {
            delivered,
            weight_delta,
            ..
        } => println!(
            "{} receipt {} weight {weight_delta:+.4}",
            "✓".green(),
            if *delivered { "delivered" } else { "rejected" }
        ),
        Event::PeerConnected { peer } => println!("{} peer connected {peer}", "+".green()),
        Event::PeerDisconnected { peer } => println!("{} peer disconnected {peer}", "-".dimmed()),
        Event::SignatureFailed { peer, pruned } => {
            if *pruned {
                println!(
                    "{} signature-failure threshold exceeded by {peer} — synapse pruned, \
                     re-formation in cooldown",
                    "!!".red().bold()
                );
            } else {
                println!(
                    "{} signature verification failed from {peer} — synapse penalized",
                    "!".red().bold()
                );
            }
        }
        Event::HeadersClamped {
            peer, weight, ttl, ..
        } => {
            let which = match (weight, ttl) {
                (true, true) => "weight and ttl",
                (true, false) => "weight",
                _ => "ttl",
            };
            println!(
                "{} clamped inflated {which} on a signal from {peer}",
                "~".yellow()
            );
        }
        Event::OriginKeyUnknown { peer, origin } => println!(
            "{} dropped a signal relayed by {peer}: no public key known for its \
             claimed origin {origin}, so its signature could not be checked",
            "!".yellow()
        ),
        Event::Malformed { peer, reason } => println!(
            "{} dropped a malformed signal from {peer}: {reason}",
            "!".yellow()
        ),
    }
}
