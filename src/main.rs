//! network-operator — the rustkube/stormcos Cluster Network Operator.
//!
//! Scaffold. See README.md for the full design: a Rust operator that reconciles
//! a `Network` CR into the Cilium CNI install (render -> server-side apply ->
//! drift-heal -> status), with install-time modes (overlay/native/bgp/encrypted/
//! bare-metal) that each render a different Cilium config.

fn main() {
    println!("network-operator: scaffold — see README.md for the design and roadmap");
}
