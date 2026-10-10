//! network-operator's test container (stormcentral `docs/test-standard.md`).
//!
//! `/test short|medium|long|drift-heal` runs a suite against the cluster the Job is in
//! and prints one JSON line per test, then a summary; exit 0 all passed, 1 a
//! test failed, 2 a test could not run. `/test workload serve <port>` is the
//! workload the suites start in pods — this same image.
//!
//! Everything goes through the apiserver, in the run's namespace; the only
//! cluster-scoped calls are reads of the `Network` CR and `nodes`. The one
//! exception is `drift-heal`, the declared disruptive suite: it writes
//! `ConfigMap/cilium-config` and `DaemonSet/cilium` in `kube-system`
//! (`src/drift.rs`).

mod api;
mod ctx;
mod drift;
mod env;
mod long;
mod medium;
mod network;
mod objects;
mod report;
mod short;
mod trend;
mod workload;

use report::{Outcome, Report};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("workload") {
        let port = match args.get(1).map(String::as_str) {
            Some("serve") => args.get(2).and_then(|p| p.parse().ok()).unwrap_or(objects::POD_PORT),
            other => {
                eprintln!("usage: /test workload serve [port] (got {other:?})");
                std::process::exit(2);
            }
        };
        std::process::exit(workload::serve(port).await);
    }

    let env = env::Env::read(args.first().cloned());
    let mut r = Report::new();
    match env.suite.as_str() {
        "short" => short::run(&env, &mut r).await,
        "medium" => medium::run(&env, &mut r).await,
        "long" => long::run(&env, &mut r).await,
        "drift-heal" => drift::run(&env, &mut r).await,
        other => {
            r.record("suite", Outcome::Infra(format!("unknown suite {other:?}: short, medium, long or drift-heal")), 0, None);
        }
    }
    std::process::exit(r.finish());
}
