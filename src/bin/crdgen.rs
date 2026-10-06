//! Emits the `Network` CRD YAML from the Rust types, so the manifest can never
//! drift from the code. Regenerate with `make crds`; never hand-edit the output.
//! It is the CRD exactly as the operator registers it, version stamp included.

fn main() -> anyhow::Result<()> {
    print!("{}", serde_yaml::to_string(&network_operator::register::crd())?);
    Ok(())
}
