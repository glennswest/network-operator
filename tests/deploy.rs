//! The shipped manifests (`deploy/`): what may run where, and that the image
//! tags agree with the crate version.

use serde_yaml::Value;

fn docs(text: &str) -> Vec<Value> {
    use serde::Deserialize;
    serde_yaml::Deserializer::from_str(text)
        .map(|d| Value::deserialize(d).unwrap())
        .filter(|v| !v.is_null())
        .collect()
}

/// Every container of every workload in a manifest stream.
fn containers(docs: &[Value]) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for d in docs {
        let kind = d["kind"].as_str().unwrap_or_default();
        if !matches!(kind, "Deployment" | "DaemonSet") {
            continue;
        }
        let name = format!("{kind}/{}", d["metadata"]["name"].as_str().unwrap_or("?"));
        for c in d["spec"]["template"]["spec"]["containers"].as_sequence().unwrap() {
            out.push((name.clone(), c.clone()));
        }
    }
    out
}

fn args(c: &Value) -> Vec<String> {
    c["args"]
        .as_sequence()
        .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

/// #30: on stormcos only the VLAN layer may run. Every workload there is the
/// VLAN controller alone (`run --controllers vlan`) or the node agent; the
/// default `run` would start the Cilium controller over stormcos's own.
#[test]
fn the_stormcos_manifest_never_runs_the_cilium_controller() {
    let d = docs(include_str!("../deploy/stormcos/vlan.yaml"));
    let cs = containers(&d);
    assert_eq!(cs.len(), 2, "the VLAN controller and its agent");
    for (owner, c) in &cs {
        let a = args(c);
        let ok = a == ["run", "--controllers", "vlan"] || a.first().map(String::as_str) == Some("node-agent");
        assert!(ok, "{owner} runs {a:?}: on stormcos only `run --controllers vlan` or `node-agent`");
        // The golden by name, run from where the golden puts the binary.
        assert_eq!(c["image"].as_str(), Some("network-operator"), "{owner}");
        assert_eq!(c["command"][0].as_str(), Some("/usr/sbin/network-operator"), "{owner}");
    }
    for doc in &d {
        if let Some(ns) = doc["metadata"]["namespace"].as_str() {
            assert_eq!(ns, "network-operator", "{:?} must not land in {ns}", doc["metadata"]["name"]);
        }
    }
}

/// The stormcos manifest is deploy/vlan.yaml with only the image, command,
/// namespace and priority changed: same objects, same RBAC.
#[test]
fn the_stormcos_manifest_is_the_vlan_manifest() {
    let ids = |text: &str| -> Vec<String> {
        docs(text)
            .iter()
            .map(|d| format!("{}/{}", d["kind"].as_str().unwrap(), d["metadata"]["name"].as_str().unwrap()))
            .collect()
    };
    assert_eq!(ids(include_str!("../deploy/stormcos/vlan.yaml")), ids(include_str!("../deploy/vlan.yaml")));
    let rules = |text: &str| -> Vec<Value> {
        docs(text).into_iter().filter(|d| d["kind"] == "ClusterRole").map(|d| d["rules"].clone()).collect()
    };
    assert_eq!(rules(include_str!("../deploy/stormcos/vlan.yaml")), rules(include_str!("../deploy/vlan.yaml")));
}

/// Every `localhost/network-operator:<tag>` names this crate's version, so a
/// release's manifests load the archive it ships.
#[test]
fn image_tags_are_the_crate_version() {
    let want = format!("localhost/network-operator:{}", env!("CARGO_PKG_VERSION"));
    for (file, text) in [
        ("deploy/operator.yaml", include_str!("../deploy/operator.yaml")),
        ("deploy/vlan.yaml", include_str!("../deploy/vlan.yaml")),
    ] {
        let cs = containers(&docs(text));
        assert!(!cs.is_empty(), "{file}");
        for (owner, c) in cs {
            assert_eq!(c["image"].as_str(), Some(want.as_str()), "{file}: {owner}");
        }
    }
}
