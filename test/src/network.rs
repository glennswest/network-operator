//! The cluster's `Network` CR, read with the operator's own types, and what it
//! should say. [`verdict`] and [`consistency`] are pure; [`find`] is the one
//! read.

use ipnet::IpNet;
use network_operator::crd::{conditions, Network, NetworkStatus, NETWORK_NAME};
use network_operator::immutable::applied_from;
use network_operator::modes::{resolve_network, EffectiveConfig};

use crate::api::{Api, Error};
use crate::report::Outcome;

pub const NETWORKS: &str = "/apis/network.storm.io/v1/networks";

/// The cluster's network, as far as the test could read it.
#[derive(Debug, Clone)]
pub enum Found {
    Read(Box<Cluster>),
    /// No `networks.network.storm.io` served: network-operator is not
    /// deployed here. Its tests do not apply.
    NotDeployed(String),
    /// The API refused or failed: the test could not run.
    Unreadable(Error),
    /// Deployed, and wrong: no `Network`, or several and none named `cluster`,
    /// or one that does not parse.
    Broken(String),
}

#[derive(Debug, Clone)]
pub struct Cluster {
    pub net: Network,
    /// The spec resolved with the operator's own policy, when it resolves.
    pub cfg: Result<EffectiveConfig, String>,
}

impl Cluster {
    pub fn new(net: Network) -> Cluster {
        let cfg = resolve_network(&net).map_err(|e| e.to_string());
        Cluster { net, cfg }
    }

    pub fn name(&self) -> &str {
        self.net.metadata.name.as_deref().unwrap_or(NETWORK_NAME)
    }

    fn status(&self) -> NetworkStatus {
        self.net.status.clone().unwrap_or_default()
    }

    /// Pod CIDRs: what was applied, else what the spec asks for.
    pub fn pod_cidrs(&self) -> Vec<IpNet> {
        let s = self.status();
        cidrs(if s.applied_cluster_network.is_empty() { &self.net.spec.cluster_network } else { &s.applied_cluster_network })
    }

    pub fn service_cidrs(&self) -> Vec<IpNet> {
        let s = self.status();
        cidrs(if s.applied_service_network.is_empty() { &self.net.spec.service_network } else { &s.applied_service_network })
    }
}

impl Found {
    pub fn cluster(&self) -> Option<&Cluster> {
        match self {
            Found::Read(c) => Some(c),
            _ => None,
        }
    }

    /// The outcome for a test that needs the `Network` and cannot have it.
    pub fn without(&self) -> Option<Outcome> {
        match self {
            Found::Read(_) => None,
            Found::NotDeployed(w) => Some(Outcome::Skip(w.clone())),
            Found::Unreadable(e) => Some(e.outcome()),
            Found::Broken(w) => Some(Outcome::Fail(w.clone())),
        }
    }
}

pub async fn find(api: &Api) -> Found {
    match api.list(NETWORKS).await {
        Err(e) => Found::Unreadable(e),
        Ok(None) => Found::NotDeployed("requires network-operator: no networks.network.storm.io served on this cluster".into()),
        Ok(Some(items)) => pick(items),
    }
}

/// The `Network` to test: the one named `cluster`, else the only one.
pub fn pick(items: Vec<serde_json::Value>) -> Found {
    let named = items.iter().position(|i| i["metadata"]["name"] == NETWORK_NAME);
    let item = match (named, items.len()) {
        (Some(i), _) => items[i].clone(),
        (None, 1) => items[0].clone(),
        (None, 0) => return Found::Broken("the Network CRD is served but there is no Network object: nothing tells network-operator what to install".into()),
        (None, n) => return Found::Broken(format!("{n} Network objects and none named {NETWORK_NAME:?}")),
    };
    match serde_json::from_value::<Network>(item) {
        Ok(net) => Found::Read(Box::new(Cluster::new(net))),
        Err(e) => Found::Broken(format!("the Network object does not parse as network-operator's type: {e}")),
    }
}

/// Is the operator reporting a healthy, current install? `Available=True`,
/// `Degraded` not `True`, and the latest generation observed.
pub fn verdict(c: &Cluster) -> Outcome {
    let s = c.status();
    let cond = |t: &str| s.conditions.iter().find(|x| x.type_ == t);
    let say = |t: &str| cond(t).map(|x| format!("{t}={} ({}: {})", x.status, x.reason, x.message)).unwrap_or_else(|| format!("no {t} condition"));
    let mut bad = Vec::new();
    if cond(conditions::AVAILABLE).map(|x| x.status.as_str()) != Some("True") {
        bad.push(say(conditions::AVAILABLE));
    }
    if cond(conditions::DEGRADED).map(|x| x.status.as_str()) == Some("True") {
        bad.push(say(conditions::DEGRADED));
    }
    if let (Some(g), o) = (c.net.metadata.generation, s.observed_generation) {
        if o != Some(g) {
            bad.push(format!("generation {g} not yet observed (observedGeneration {o:?})"));
        }
    }
    if bad.is_empty() {
        Outcome::Pass(format!(
            "Network {} Available: mode {}, Cilium {}, generation {} observed",
            c.name(),
            s.applied_mode.as_deref().unwrap_or("?"),
            s.applied_version.as_deref().unwrap_or("?"),
            s.observed_generation.map(|g| g.to_string()).unwrap_or_else(|| "?".into())
        ))
    } else {
        Outcome::Fail(format!("Network {}: {}", c.name(), bad.join("; ")))
    }
}

/// Does `status.applied*` say what the operator's own code says it should
/// for this spec? That is the immutability baseline; a mismatch means the
/// last change was rejected (then `Degraded` says why) or never applied.
pub fn consistency(c: &Cluster) -> Outcome {
    let cfg = match &c.cfg {
        Ok(cfg) => cfg,
        Err(e) => return Outcome::Fail(format!("the spec does not resolve with network-operator's own rules: {e}")),
    };
    let mut want = NetworkStatus::default();
    applied_from(cfg, &mut want);
    let got = c.status();
    let mut diff = Vec::new();
    let mut cmp = |field: &str, g: String, w: String| {
        if g != w {
            diff.push(format!("{field} is {g}, the spec resolves to {w}"));
        }
    };
    cmp("appliedMode", format!("{:?}", got.applied_mode), format!("{:?}", want.applied_mode));
    cmp("appliedVersion", format!("{:?}", got.applied_version), format!("{:?}", want.applied_version));
    cmp("appliedDatapath", format!("{:?}", got.applied_datapath), format!("{:?}", want.applied_datapath));
    cmp("appliedIpam", format!("{:?}", got.applied_ipam), format!("{:?}", want.applied_ipam));
    cmp("appliedClusterNetwork", format!("{:?}", got.applied_cluster_network), format!("{:?}", want.applied_cluster_network));
    cmp("appliedServiceNetwork", format!("{:?}", got.applied_service_network), format!("{:?}", want.applied_service_network));
    if diff.is_empty() {
        Outcome::Pass(format!(
            "status.applied* matches the spec: {} / {} / {} IPAM, pods {:?}, services {:?}",
            cfg.mode.as_str(),
            cfg.routing.as_str(),
            cfg.ipam.as_str(),
            cfg.cluster_network,
            cfg.service_network
        ))
    } else {
        Outcome::Fail(diff.join("; "))
    }
}

pub fn cidrs(v: &[String]) -> Vec<IpNet> {
    v.iter().filter_map(|s| s.parse().ok()).collect()
}

/// Whether `ip` is inside one of `nets`; `None` when there are none to
/// check against (the `Network` could not be read).
pub fn within(nets: &[IpNet], ip: &str) -> Option<bool> {
    if nets.is_empty() {
        return None;
    }
    let ip: std::net::IpAddr = ip.parse().ok()?;
    Some(nets.iter().any(|n| n.contains(&ip)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn network(status: serde_json::Value) -> serde_json::Value {
        json!({
            "apiVersion": "network.storm.io/v1", "kind": "Network",
            "metadata": {"name": "cluster", "generation": 2},
            "spec": {"mode": "overlay", "clusterNetwork": ["10.244.0.0/16"], "serviceNetwork": ["10.96.0.0/12"],
                     "cilium": {"k8sServiceHost": "192.168.8.98"}},
            "status": status,
        })
    }

    fn healthy() -> serde_json::Value {
        json!({
            "observedGeneration": 2,
            "appliedMode": "overlay", "appliedVersion": network_operator::modes::DEFAULT_CILIUM_VERSION, "appliedDatapath": "tunnel", "appliedIpam": "cluster-pool",
            "appliedClusterNetwork": ["10.244.0.0/16"], "appliedServiceNetwork": ["10.96.0.0/12"],
            "conditions": [
                {"type": "Available", "status": "True", "reason": "AsExpected", "message": "", "lastTransitionTime": "2026-09-27T00:00:00Z"},
                {"type": "Progressing", "status": "False", "reason": "AsExpected", "message": "", "lastTransitionTime": "2026-09-27T00:00:00Z"},
                {"type": "Degraded", "status": "False", "reason": "AsExpected", "message": "", "lastTransitionTime": "2026-09-27T00:00:00Z"},
            ],
        })
    }

    fn cluster(status: serde_json::Value) -> Cluster {
        match pick(vec![network(status)]) {
            Found::Read(c) => *c,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_healthy_current_network_passes() {
        let c = cluster(healthy());
        assert!(matches!(verdict(&c), Outcome::Pass(_)), "{:?}", verdict(&c));
        assert!(matches!(consistency(&c), Outcome::Pass(_)), "{:?}", consistency(&c));
    }

    #[test]
    fn degraded_unavailable_or_stale_fails_and_says_why() {
        let mut s = healthy();
        s["conditions"][2] = json!({"type": "Degraded", "status": "True", "reason": "ReconcileFailed", "message": "boom", "lastTransitionTime": "2026-09-27T00:00:00Z"});
        s["observedGeneration"] = json!(1);
        match verdict(&cluster(s)) {
            Outcome::Fail(m) => {
                assert!(m.contains("ReconcileFailed: boom"), "{m}");
                assert!(m.contains("generation 2 not yet observed"), "{m}");
            }
            o => panic!("{o:?}"),
        }
        let mut s = healthy();
        s["conditions"][0]["status"] = json!("False");
        assert!(matches!(verdict(&cluster(s)), Outcome::Fail(_)));
    }

    #[test]
    fn applied_star_must_match_what_the_operator_would_write() {
        let mut s = healthy();
        s["appliedDatapath"] = json!("native");
        match consistency(&cluster(s)) {
            Outcome::Fail(m) => assert!(m.contains("appliedDatapath"), "{m}"),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn the_network_named_cluster_wins_and_absence_is_named() {
        let mut other = network(healthy());
        other["metadata"]["name"] = json!("other");
        assert!(matches!(pick(vec![other.clone(), network(healthy())]), Found::Read(c) if c.name() == "cluster"));
        assert!(matches!(pick(vec![other.clone()]), Found::Read(c) if c.name() == "other"));
        assert!(matches!(pick(vec![other.clone(), other]), Found::Broken(_)));
        assert!(matches!(pick(vec![]), Found::Broken(_)));
    }

    #[test]
    fn cidrs_fall_back_to_the_spec_and_contain_addresses() {
        let c = cluster(json!({}));
        assert_eq!(within(&c.pod_cidrs(), "10.244.3.9"), Some(true));
        assert_eq!(within(&c.pod_cidrs(), "10.96.0.10"), Some(false));
        assert_eq!(within(&c.service_cidrs(), "10.96.0.10"), Some(true));
        assert_eq!(within(&[], "10.96.0.10"), None);
    }
}
