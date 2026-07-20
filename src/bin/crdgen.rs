//! Emits the `Network` CRD YAML from the Rust types, so the manifest can never
//! drift from the code. Regenerate with `make crds`; never hand-edit the output.

use kube::CustomResourceExt;
use network_operator::crd::Network;

fn main() -> anyhow::Result<()> {
    print!("{}", serde_yaml::to_string(&Network::crd())?);
    Ok(())
}
