//! yamaha-osc — OSC to Yamaha RCP bridge.
//!
//! Wires the modules into a running process: an OSC listener feeds a router that turns each
//! message into console actions, which a single dispatcher paces onto one pooled RCP connection.
//! Optional health reporting mirrors the console link state back out over OSC.
//!
//! Some module items (extra `discover`/`osc` helpers, `Stats` accessors) exist for tests and a
//! future `monitor` subcommand and are not reached from `main`; hence the crate-level allow.
#![allow(dead_code)]

mod address;
mod config;
mod discover;
mod dispatch;
mod fade;
mod health;
mod osc;
mod router;
mod value;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use yamaha_rcp::{ConnectionConfig, Mixer};

use crate::address::Resolver;
use crate::config::Config;
use crate::dispatch::{DispatchConfig, Dispatcher};
use crate::router::{Action, Router};

const DEFAULT_CONFIG: &str = "yamaha-osc.toml";

#[derive(Parser)]
#[command(name = "yamaha-osc", version, about = "OSC to Yamaha RCP bridge for Yamaha TF consoles")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the bridge (the default when no subcommand is given).
    Run {
        /// Path to the configuration file.
        #[arg(short, long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Print an example configuration file to stdout.
    Init,
    /// Scan a CIDR range for a console and print what answered.
    Discover {
        /// Subnet to scan, e.g. 192.168.50.0/24.
        cidr: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run {
        config: PathBuf::from(DEFAULT_CONFIG),
    }) {
        Command::Run { config } => run(config).await,
        Command::Init => {
            print!("{}", Config::example());
            Ok(())
        }
        Command::Discover { cidr } => discover_cmd(&cidr).await,
    }
}

/// Load config, connect everything, and pump OSC into the console until Ctrl-C.
async fn run(config_path: PathBuf) -> Result<()> {
    let config = Config::load(&config_path)?;

    let console_addr = resolve_console(&config).await?;
    let bind_address = config
        .console
        .bind_address
        .as_ref()
        .map(|ip| ip.parse::<std::net::IpAddr>())
        .transpose()
        .with_context(|| format!("console.bind_address {:?}", config.console.bind_address))?;
    info!(%console_addr, ?bind_address, "connecting to console");
    let conn_config = ConnectionConfig {
        connect_timeout: Duration::from_millis(config.console.connect_timeout_ms),
        response_timeout: Duration::from_millis(config.console.response_timeout_ms),
        bind_address,
        ..Default::default()
    };
    let mixer = connect_with_retry(console_addr, conn_config).await;
    info!("console connection established");

    // Health reporting, if configured. Started before the listener so the first status goes out
    // as soon as the link is up.
    if let Some(dest) = &config.health.destination {
        let dest: SocketAddr = dest
            .parse()
            .with_context(|| format!("health.destination {dest:?}"))?;
        health::spawn(
            dest,
            mixer.state(),
            Duration::from_secs(config.health.heartbeat_s),
        )
        .await
        .context("starting health reporting")?;
        info!(%dest, "health reporting enabled");
    }

    let dispatcher = Dispatcher::spawn(
        mixer.clone(),
        DispatchConfig {
            min_command_interval: config.min_command_interval(),
            fade_tick: config.fade_tick(),
            coalesce: config.limits.coalesce,
            queue_depth: config.limits.queue_depth,
        },
    );

    let mut router = build_router(&config)?;

    let listen: SocketAddr = config
        .osc
        .listen
        .parse()
        .with_context(|| format!("osc.listen {:?}", config.osc.listen))?;
    let (tx, mut rx) = mpsc::channel(config.limits.queue_depth);
    osc::spawn_listener(listen, config.osc.transport, tx)
        .await
        .with_context(|| format!("binding the OSC listener on {listen}"))?;
    info!(%listen, transport = ?config.osc.transport, "listening for OSC");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown requested");
                dispatcher.shutdown();
                break;
            }
            received = rx.recv() => {
                let Some(incoming) = received else {
                    warn!("OSC listener stopped; shutting down");
                    dispatcher.shutdown();
                    break;
                };
                if dispatch_message(&incoming, &router, &dispatcher).await == Control::Reload {
                    match reload(&config_path) {
                        Ok(new_router) => {
                            router = new_router;
                            info!("configuration reloaded");
                        }
                        Err(e) => warn!(error = %e, "reload failed; keeping the running configuration"),
                    }
                }
            }
        }
    }

    Ok(())
}

/// Keep trying the initial console connection instead of exiting on a transient failure.
///
/// A show bridge may start before the console is powered on, or during a network blip on a
/// multi-homed laptop; either should resolve itself rather than take the bridge down. Once this
/// returns, the connection actor handles all later reconnects on its own.
async fn connect_with_retry(addr: SocketAddr, config: ConnectionConfig) -> Mixer {
    let max_backoff = Duration::from_secs(5);
    let mut backoff = Duration::from_millis(200);
    let mut attempt = 0u64;
    loop {
        attempt += 1;
        match Mixer::connect_with(addr, config.clone()).await {
            Ok(mixer) => return mixer,
            Err(e) => {
                warn!(attempt, error = %e, retry_in = ?backoff,
                      "console connect failed; retrying (is the console on and reachable?)");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

/// Whether the main loop needs to act after routing a message.
#[derive(PartialEq, Eq)]
enum Control {
    Continue,
    Reload,
}

/// Route one OSC message and submit its actions. `/bridge/reload` is handled by the supervisor,
/// so it is signalled back rather than submitted.
async fn dispatch_message(
    incoming: &osc::Incoming,
    router: &Router,
    dispatcher: &Dispatcher,
) -> Control {
    let actions = match router.route(&incoming.message) {
        Ok(actions) => actions,
        Err(e) => {
            // A cue with a typo must never take the bridge down; log and move on.
            warn!(address = %incoming.message.addr, error = %e, "ignoring OSC message");
            return Control::Continue;
        }
    };

    let mut control = Control::Continue;
    for action in actions {
        if action == Action::Reload {
            control = Control::Reload;
            continue;
        }
        debug!(?action, "dispatching");
        if !dispatcher.submit(action).await {
            warn!("dispatcher queue full or stopped; an action was dropped");
        }
    }
    control
}

/// Re-read the config file and rebuild the router. Only the name table and channel base are
/// applied live; console/listener/limit changes need a restart.
fn reload(config_path: &PathBuf) -> Result<Router> {
    let config = Config::load(config_path)?;
    build_router(&config)
}

fn build_router(config: &Config) -> Result<Router> {
    let names = config.resolved_names()?;
    Ok(Router::new(Resolver::new(names, config.osc.channel_base)))
}

/// Work out the console address from an explicit `address` or by scanning `discover`.
async fn resolve_console(config: &Config) -> Result<SocketAddr> {
    if let Some(addr) = &config.console.address {
        return config::resolve_console_addr(addr);
    }
    if let Some(cidr) = &config.console.discover {
        info!(%cidr, "no console address set; scanning");
        let timeout = Duration::from_millis(config.console.connect_timeout_ms);
        let found = discover::scan(cidr, timeout).await?;
        let console = found
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no console answered on {cidr}"))?;
        info!(%console, "using discovered console");
        return Ok(console.addr);
    }
    bail!("neither console.address nor console.discover is set");
}

/// The `discover` subcommand: scan and print, without connecting.
async fn discover_cmd(cidr: &str) -> Result<()> {
    let found = discover::scan(cidr, Duration::from_millis(500)).await?;
    if found.is_empty() {
        println!("no consoles found on {cidr}");
    } else {
        for console in found {
            println!("{console}");
        }
    }
    Ok(())
}
