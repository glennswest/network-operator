//! network-operator — the rustkube/stormcos Cluster Network Operator.
//!
//! Reconciles a `Network` CR into a Cilium install. See README.md for the
//! design; `dry-run` prints what would be applied without touching a cluster.

use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use kube::Client;
use network_operator::{crd::Network, metrics, modes, register, render, vlan};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser)]
#[command(name = "network-operator", version, about, long_about = None)]
struct Cli {
    /// Log filter, e.g. `info`, `network_operator=debug`.
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    log: String,

    /// Emit logs as JSON, for a cluster log pipeline.
    #[arg(long, env = "LOG_JSON")]
    log_json: bool,

    /// Address of the `/healthz`, `/readyz` and `/metrics` listener. The pod
    /// is host-networked, so this is a host port.
    #[arg(long, env = "HEALTH_ADDR", default_value = metrics::DEFAULT_ADDR)]
    health_addr: std::net::SocketAddr,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the controllers (the default: the Network/Cilium one).
    Run {
        /// Which controllers: `network` (Cilium from the Network CR) and/or
        /// `vlan` (VLAN CRs -> NADs, #32). On stormcos run `vlan` alone: the
        /// network controller must never run there (#30), and with it off the
        /// Network CRD is not even registered.
        #[arg(long, env = "CONTROLLERS", value_delimiter = ',', default_value = "network")]
        controllers: Vec<ControllerKind>,
    },
    /// The VLAN node agent (a DaemonSet, #32): puts the VLANs on one bridge's
    /// uplink ports and reports them on the node.
    NodeAgent {
        /// The only bridge it touches.
        #[arg(long, env = "BRIDGE", default_value = vlan::crd::DEFAULT_BRIDGE)]
        bridge: String,
        /// This node's name (the downward API's `spec.nodeName`).
        #[arg(long, env = "NODE_NAME")]
        node_name: String,
        /// Where sysfs's network class is.
        #[arg(long, env = "SYS_CLASS_NET", default_value = vlan::agent::SYS_CLASS_NET)]
        sys_class_net: std::path::PathBuf,
        /// Pass at least this often, in seconds, besides on every VLAN change.
        #[arg(long, env = "RESYNC_SECS", default_value_t = 30)]
        resync_secs: u64,
    },
    /// Render the manifests a Network YAML would produce, without a cluster.
    DryRun {
        /// Path to a Network manifest, or `-` for stdin.
        #[arg(default_value = "-")]
        file: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli);

    match cli.command {
        None => run(cli.health_addr, &[ControllerKind::Network]).await,
        Some(Command::Run { controllers }) => run(cli.health_addr, &controllers).await,
        Some(Command::NodeAgent { bridge, node_name, sys_class_net, resync_secs }) => {
            let metrics = metrics::Metrics::new();
            metrics::serve(cli.health_addr, metrics.clone()).await?;
            let client = Client::try_default().await?;
            let settings = vlan::agent::Settings {
                bridge,
                node: node_name,
                sys: sys_class_net,
                resync: Duration::from_secs(resync_secs.max(1)),
            };
            vlan::agent::run(client, settings, metrics).await
        }
        Some(Command::DryRun { file }) => dry_run(&file),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ControllerKind {
    Network,
    Vlan,
}

async fn run(health_addr: std::net::SocketAddr, controllers: &[ControllerKind]) -> anyhow::Result<()> {
    let network = controllers.contains(&ControllerKind::Network);
    let vlans = controllers.contains(&ControllerKind::Vlan);
    if !network && !vlans {
        anyhow::bail!("--controllers names none: network, vlan or both");
    }
    // Up first, so liveness answers while the CRDs are registered; readiness
    // waits for the controllers.
    let metrics = metrics::Metrics::new();
    metrics::serve(health_addr, metrics.clone()).await?;
    let client = Client::try_default().await?;
    // The operator owns its own CRDs — self-register them (and update their
    // schema on upgrade) so deploying the operator is all that's needed; the
    // admin only supplies the CRs. Only the running controllers' CRDs.
    if network {
        register::ensure_crd(&client).await?;
    }
    if vlans {
        register::ensure_vlan_crd(&client).await?;
    }
    match (network, vlans) {
        (true, true) => {
            tokio::try_join!(
                network_operator::controller::run(client.clone(), metrics.clone()),
                vlan::controller::run(client, metrics),
            )?;
            Ok(())
        }
        (true, false) => network_operator::controller::run(client, metrics).await,
        _ => vlan::controller::run(client, metrics).await,
    }
}

fn init_tracing(cli: &Cli) {
    let filter = EnvFilter::new(&cli.log);
    let registry = tracing_subscriber::registry().with(filter);
    if cli.log_json {
        registry.with(fmt::layer().json()).init();
    } else {
        registry.with(fmt::layer()).init();
    }
}

/// Resolve + render a `Network` manifest to a YAML stream. Same code path the
/// reconciler uses, so what this prints is what would be applied.
fn dry_run(file: &str) -> anyhow::Result<()> {
    let input = if file == "-" {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(file)?
    };

    let net: Network = serde_yaml::from_str(&input)?;
    let cfg = modes::resolve_network(&net)?;

    for object in render::render(&cfg) {
        println!("---");
        print!("{}", serde_yaml::to_string(&object.obj)?);
    }
    Ok(())
}
