//! Golden-file tests for the render.
//!
//! The render is the operator's whole contract with the cluster, so every mode's
//! full manifest set is checked in under `tests/golden/`. A diff here is a
//! deliberate change to what gets installed and should be reviewed as such.
//!
//! Regenerate after an intended change:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test --test golden
//! ```

use std::path::PathBuf;

use network_operator::crd::{Mode, Network};
use network_operator::modes::{resolve, resolve_network};
use network_operator::render;

const MODES: [(&str, Mode); 5] = [
    ("overlay", Mode::Overlay),
    ("native", Mode::Native),
    ("bgp", Mode::Bgp),
    ("encrypted", Mode::Encrypted),
    ("bare-metal", Mode::BareMetal),
];

/// A `Network` covering every knob a mode might set, so the goldens exercise the
/// LB/BGP paths even for the modes that do not turn them on by default.
fn manifest(mode: &str) -> String {
    let load_balancer = match mode {
        "bgp" => {
            r#"
    loadBalancer:
      pools: ["192.168.8.240/28"]
      bgp:
        localASN: 64512
        peers:
        - address: "192.168.8.1"
          asn: 64512
        - address: "192.168.8.2"
          asn: 64512"#
        }
        "bare-metal" => {
            r#"
    loadBalancer:
      pools: ["192.168.8.240/28"]"#
        }
        _ => "",
    };
    let envoy = if mode == "overlay-envoy" {
        "\n    envoy:\n      enabled: true"
    } else {
        ""
    };
    let mode = if mode == "overlay-envoy" { "overlay" } else { mode };

    format!(
        r#"apiVersion: network.storm.io/v1
kind: Network
metadata:
  name: cluster
spec:
  cni: cilium
  mode: {mode}
  clusterNetwork: ["10.244.0.0/16"]
  serviceNetwork: ["10.96.0.0/12"]
  cilium:
    version: "1.19.6"
    kubeProxyReplacement: true
    k8sServiceHost: "192.168.8.98"
    k8sServicePort: 6443{load_balancer}{envoy}
"#
    )
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/golden/{name}.yaml"))
}

fn render_to_yaml(net: &Network) -> String {
    let cfg = resolve_network(net).expect("fixture must resolve");
    let mut out = String::new();
    for object in render::render(&cfg) {
        out.push_str("---\n");
        out.push_str(&serde_yaml::to_string(&object.obj).unwrap());
    }
    out
}

#[test]
fn every_mode_matches_its_golden_manifest() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut stale = Vec::new();

    for name in MODES.iter().map(|(n, _)| *n).chain(["overlay-envoy"]) {
        let net: Network = serde_yaml::from_str(&manifest(name)).unwrap();
        let rendered = render_to_yaml(&net);
        let path = golden_path(name);

        if update {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &rendered).unwrap();
            continue;
        }

        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e} — run UPDATE_GOLDEN=1 cargo test", path.display()));
        if expected != rendered {
            stale.push(name);
        }
    }

    assert!(
        stale.is_empty(),
        "render changed for {stale:?} — review the diff, then re-run with UPDATE_GOLDEN=1"
    );
}

/// The manifests shipped in `examples/` are the documented entry point, so a
/// field rename that breaks them has to fail the build.
#[test]
fn shipped_examples_parse_and_resolve() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut checked = 0;

    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let net: Network = serde_yaml::from_str(&text)
            .unwrap_or_else(|e| panic!("{} does not parse as a Network: {e}", path.display()));
        resolve_network(&net)
            .unwrap_or_else(|e| panic!("{} does not resolve: {e}", path.display()));
        checked += 1;
    }

    assert!(checked >= 2, "expected the example manifests to be present");
}

/// Field names that appear in README.md and the examples. Serde's camelCase
/// mangles acronyms, so these are pinned by hand and worth guarding.
#[test]
fn acronym_field_names_survive_a_round_trip() {
    let yaml = r#"apiVersion: network.storm.io/v1
kind: Network
metadata:
  name: cluster
spec:
  mode: bgp
  clusterNetwork: ["10.244.0.0/16"]
  serviceNetwork: ["10.96.0.0/12"]
  cilium:
    k8sServiceHost: "192.168.8.98"
    ipam:
      clusterPoolIPv4MaskSize: 25
    loadBalancer:
      pools: ["192.168.8.240/28"]
      bgp:
        localASN: 64512
        peers:
        - address: "192.168.8.1"
          asn: 64512
"#;
    let net: Network = serde_yaml::from_str(yaml).unwrap();
    let cfg = resolve_network(&net).unwrap();
    assert_eq!(cfg.cluster_pool_ipv4_mask_size, 25);
    assert_eq!(cfg.bgp_local_asn, 64512);

    // And back out again under the same names.
    let round_tripped = serde_yaml::to_string(&net.spec).unwrap();
    assert!(round_tripped.contains("clusterPoolIPv4MaskSize"));
    assert!(round_tripped.contains("localASN"));
}

/// An upgrade must change image tags and nothing else — the property that makes
/// `spec.cilium.version` a safe knob to turn.
#[test]
fn a_version_bump_only_moves_image_tags() {
    let base: Network = serde_yaml::from_str(&manifest("overlay")).unwrap();
    let mut bumped = base.clone();
    bumped.spec.cilium.as_mut().unwrap().version = Some("1.20.0".into());

    let before = render_to_yaml(&base);
    let after = render_to_yaml(&bumped);
    assert_ne!(before, after);
    assert_eq!(before.replace("v1.19.6", "v1.20.0"), after);
}

/// Two clusters that differ only in a runtime-changeable field must not differ
/// in anything structural (selectors, names, kinds).
#[test]
fn runtime_changeable_fields_do_not_move_object_identities() {
    for (name, mode) in MODES {
        let net: Network = serde_yaml::from_str(&manifest(name)).unwrap();
        let cfg = resolve_network(&net).unwrap();
        let ids: Vec<String> = render::render(&cfg).iter().map(|r| r.id()).collect();

        let mut spec = net.spec.clone();
        spec.cilium.as_mut().unwrap().mtu = Some(9000);
        let mutated = resolve(&spec).unwrap();
        let mutated_ids: Vec<String> = render::render(&mutated).iter().map(|r| r.id()).collect();

        assert_eq!(ids, mutated_ids, "mode {name} ({mode:?}) changed object identities on an mtu change");
    }
}
