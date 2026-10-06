//! Render parity with the Cilium objects stormcos ships (#9).
//!
//! `tests/fixtures/stormcos/` is a verbatim copy of stormcos's
//! `deploy/manifests/10…75` (see the README there). A `Network` in the default
//! overlay mode, Hubble on, must render every object those files hold under the
//! same kind, namespace and name, and its `cilium-config` must agree with
//! stormcos's on every key both set — except the few listed below, each for a
//! stated reason.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use network_operator::crd::Network;
use serde::Deserialize;
use network_operator::modes::resolve_network;
use network_operator::render;

/// Keys both set to different values on purpose.
const EXPECTED_DIFFERENCES: &[(&str, &str)] = &[
    ("cluster-pool-ipv4-cidr", "from spec.clusterNetwork; stormcos's cluster uses 10.0.0.0/8"),
    ("enable-lb-ipam", "from the mode; stormcos turns LB-IPAM on without pools, which validation refuses"),
    (
        "enable-ipv6-masquerade",
        "follows IPv6, which is off; the chart writes its default `true`, inert without IPv6",
    ),
];

fn fixtures() -> Vec<serde_yaml::Value> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stormcos");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("yaml"))
        .collect();
    files.sort();
    assert!(files.len() >= 9, "stormcos fixtures missing from {}", dir.display());
    let mut docs = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for doc in serde_yaml::Deserializer::from_str(&text) {
            let v = serde_yaml::Value::deserialize(doc).unwrap();
            if !v.is_null() {
                docs.push(v);
            }
        }
    }
    docs
}

fn id(v: &serde_yaml::Value) -> String {
    let kind = v["kind"].as_str().unwrap();
    let name = v["metadata"]["name"].as_str().unwrap();
    match v["metadata"]["namespace"].as_str() {
        Some(ns) => format!("{kind}/{ns}/{name}"),
        None => format!("{kind}/{name}"),
    }
}

fn network() -> Network {
    serde_yaml::from_str(
        r#"apiVersion: network.storm.io/v1
kind: Network
metadata:
  name: cluster
spec:
  mode: overlay
  clusterNetwork: ["10.244.0.0/16"]
  serviceNetwork: ["10.96.0.0/12"]
  cilium:
    k8sServiceHost: "192.168.8.98"
    hubble:
      enabled: true
"#,
    )
    .unwrap()
}

#[test]
fn renders_every_object_stormcos_ships_under_the_same_name() {
    let theirs: BTreeSet<String> = fixtures().iter().map(id).collect();
    assert_eq!(theirs.len(), 23, "stormcos's object count moved: {theirs:#?}");

    let cfg = resolve_network(&network()).unwrap();
    let ours: BTreeSet<String> = render::render(&cfg).iter().map(|r| r.id()).collect();

    let missing: Vec<_> = theirs.difference(&ours).collect();
    assert!(missing.is_empty(), "stormcos ships these and we do not render them: {missing:#?}");
    let extra: Vec<_> = ours.difference(&theirs).collect();
    assert!(extra.is_empty(), "we render these and stormcos does not ship them: {extra:#?}");
}

#[test]
fn cilium_config_agrees_with_stormcos_where_both_set_a_key() {
    let docs = fixtures();
    let theirs = docs
        .iter()
        .find(|v| id(v) == "ConfigMap/kube-system/cilium-config")
        .expect("stormcos ships cilium-config");
    let theirs: BTreeMap<String, String> = serde_yaml::from_value(theirs["data"].clone()).unwrap();

    let cfg = resolve_network(&network()).unwrap();
    let ours = render::render(&cfg)
        .into_iter()
        .find(|r| r.id() == "ConfigMap/kube-system/cilium-config")
        .unwrap();
    let ours: BTreeMap<String, String> =
        serde_json::from_value(ours.obj.data["data"].clone()).unwrap();

    let allowed: BTreeSet<&str> = EXPECTED_DIFFERENCES.iter().map(|(k, _)| *k).collect();
    let mut differ = Vec::new();
    for (k, v) in &ours {
        if let Some(t) = theirs.get(k) {
            if t != v && !allowed.contains(k.as_str()) {
                differ.push(format!("{k}: ours {v:?}, stormcos {t:?}"));
            }
        }
    }
    assert!(differ.is_empty(), "cilium-config disagrees with stormcos:\n{}", differ.join("\n"));

    // An allowance for a key that no longer differs is stale; drop it.
    for k in &allowed {
        assert_ne!(ours.get(*k), theirs.get(*k), "{k} now agrees; remove it from EXPECTED_DIFFERENCES");
    }
}

#[test]
fn images_are_the_digests_stormcos_runs() {
    let docs = fixtures();
    let cfg = resolve_network(&network()).unwrap();
    let mut theirs = BTreeSet::new();
    for d in &docs {
        let spec = &d["spec"]["template"]["spec"];
        for group in ["initContainers", "containers"] {
            if let Some(cs) = spec[group].as_sequence() {
                for c in cs {
                    theirs.insert(c["image"].as_str().unwrap().to_string());
                }
            }
        }
    }
    let ours: BTreeSet<String> =
        [cfg.agent_image, cfg.operator_image, cfg.hubble_relay_image].into_iter().collect();
    assert_eq!(ours, theirs);
}
