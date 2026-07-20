//! network-operator — the rustkube/stormcos Cluster Network Operator.
//!
//! Reconciles a `Network` CR into a Cilium install. See README.md for the
//! design; `dry-run` prints what would be applied without touching a cluster.

use clap::{Parser, Subcommand};
use kube::Client;
use network_operator::{crd::Network, modes, render};
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

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Watch the Network CR and reconcile Cilium (the default).
    Run,
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
        None | Some(Command::Run) => {
            let client = Client::try_default().await?;
            network_operator::controller::run(client).await
        }
        Some(Command::DryRun { file }) => dry_run(&file),
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
