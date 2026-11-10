//! Emits a CRD's YAML from the Rust types, so the manifests can never drift
//! from the code. Regenerate with `make crds`; never hand-edit the output.
//! Each is the CRD exactly as the operator registers it, version stamp
//! included. `crdgen` (or `crdgen network`) prints `Network`'s; `crdgen vlan`
//! prints `VLAN`'s (#32).

fn main() -> anyhow::Result<()> {
    let crd = match std::env::args().nth(1).as_deref() {
        None | Some("network") => network_operator::register::crd(),
        Some("vlan") => network_operator::register::vlan_crd(),
        Some(other) => anyhow::bail!("unknown CRD {other:?}: network or vlan"),
    };
    print!("{}", serde_yaml::to_string(&crd)?);
    Ok(())
}
