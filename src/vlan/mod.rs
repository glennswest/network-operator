//! The VLAN layer (#32): `VLAN` CRs -> Multus NetworkAttachmentDefinitions in
//! the namespaces allowed to use them, a namespace default, and the VLAN's
//! entry on each node's VLAN-aware bridge.
//!
//! ```text
//!   VLAN CRs ──accept──▶ valid, conflict-free set ──plan──▶ NADs + namespace defaults
//!      ▲                        │                               │ (controller.rs)
//!      │                        └──▶ node agent (agent.rs) ──▶ bridge VLAN entries
//!      └── status ◀── NADs, node reports (node annotation), pods' network-status
//! ```
//!
//! Two processes share this module. The controller ([`controller`], `run
//! --controllers vlan`) owns the NADs, the namespace default annotation and
//! `VLAN.status`. The node agent ([`agent`], `node-agent`, a DaemonSet) owns
//! the bridge: it turns `vlan_filtering` on and adds/removes the VLAN entries
//! it put on the bridge's uplink ports, and reports what the node carries in
//! the node's [`NODE_STATE_ANNOTATION`]. Both use [`accept`], so they agree on
//! which VLANs exist.
//!
//! Everything in this file is pure; the controller and agent do the I/O.
//! The Cilium controller is not involved: on stormcos the operator runs with
//! `--controllers vlan` and never touches `kube-system`'s Cilium objects (#30).

use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::api::{ApiResource, DynamicObject};
use kube::ResourceExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::render::Rendered;

pub mod agent;
pub mod controller;
pub mod crd;
pub mod netlink;

use crd::{AddressType, Vlan, VlanSpec, VlanStatus, DEFAULT_BRIDGE};

/// Label on every NAD the operator keeps: the VLAN it belongs to.
pub const VLAN_LABEL: &str = "network.storm.io/vlan";
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY: &str = "network-operator";

/// Namespace annotation naming its default VLAN's NAD. The operator owns this
/// key: it writes it from `VLAN.spec.defaultFor` and removes it when no VLAN
/// claims the namespace. The kubelet reads it for a pod that names no networks.
pub const DEFAULT_NETWORKS_ANNOTATION: &str = "network.storm.io/default-networks";

/// Node annotation where the node agent reports its bridge ([`NodeState`]).
pub const NODE_STATE_ANNOTATION: &str = "network.storm.io/vlan-agent";

/// What the kubelet writes back on a pod (the Multus standard).
pub const NETWORK_STATUS_ANNOTATION: &str = "k8s.v1.cni.cncf.io/network-status";

/// How many pod names a VLAN's status lists; `podCount` is the full count.
pub const MAX_LISTED_PODS: usize = 50;

/// VLAN 1 is the bridge's default PVID: the node's own untagged traffic. The
/// agent never adds, removes or claims it.
pub const DEFAULT_PVID: u16 = 1;

/// `k8s.cni.cncf.io/v1` NetworkAttachmentDefinition.
pub fn nad_api() -> ApiResource {
    ApiResource {
        group: "k8s.cni.cncf.io".into(),
        version: "v1".into(),
        api_version: "k8s.cni.cncf.io/v1".into(),
        kind: "NetworkAttachmentDefinition".into(),
        plural: "network-attachment-definitions".into(),
    }
}

/// The bridge a VLAN uses.
pub fn bridge_of(spec: &VlanSpec) -> &str {
    spec.bridge.as_deref().filter(|b| !b.is_empty()).unwrap_or(DEFAULT_BRIDGE)
}

/// Every namespace a VLAN's NAD goes in: `namespaces` plus `defaultFor`.
pub fn allowed_namespaces(spec: &VlanSpec) -> BTreeSet<String> {
    spec.namespaces.iter().chain(&spec.default_for).cloned().collect()
}

/// Check a spec on its own. Cross-VLAN conflicts are [`accept`]'s.
pub fn validate(spec: &VlanSpec) -> Result<(), String> {
    if !(1..=4094).contains(&spec.id) {
        return Err(format!("id {} is outside 1-4094", spec.id));
    }
    if let Some(b) = &spec.bridge {
        check_ifname("bridge", b)?;
    }
    if let Some(u) = &spec.uplink {
        check_ifname("uplink", u)?;
    }
    if let Some(m) = spec.mtu {
        if !(576..=9216).contains(&m) {
            return Err(format!("mtu {m} is outside 576-9216"));
        }
    }
    for ns in spec.namespaces.iter().chain(&spec.default_for) {
        if !is_dns_label(ns) {
            return Err(format!("namespace {ns:?} is not a valid namespace name"));
        }
    }
    let addresses = spec.addresses.clone().unwrap_or_default();
    if !addresses.routes.is_empty() && addresses.kind != AddressType::Static {
        return Err("addresses.routes needs addresses.type static".into());
    }
    for r in &addresses.routes {
        r.dst.parse::<ipnet::IpNet>().map_err(|_| format!("route dst {:?} is not a CIDR", r.dst))?;
        if let Some(gw) = &r.gw {
            gw.parse::<std::net::IpAddr>().map_err(|_| format!("route gw {gw:?} is not an IP"))?;
        }
    }
    Ok(())
}

fn check_ifname(what: &str, name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 15
        && name != "."
        && name != ".."
        && name.chars().all(|c| c.is_ascii_graphic() && c != '/' && c != ':');
    if ok {
        Ok(())
    } else {
        Err(format!("{what} {name:?} is not a Linux interface name"))
    }
}

fn is_dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// A VLAN that passed validation and holds its (bridge, id).
#[derive(Debug, Clone)]
pub struct Accepted {
    pub name: String,
    pub spec: VlanSpec,
    pub bridge: String,
}

/// The VLANs, split into those that stand and those rejected (with why).
/// Two VLANs on one bridge with one ID: the older (then the lower name) keeps
/// it. The controller and every node agent run this same function, so they
/// always agree on what exists.
pub fn accept(vlans: &[Vlan]) -> (Vec<Accepted>, BTreeMap<String, String>) {
    let mut ordered: Vec<&Vlan> = vlans.iter().filter(|v| v.metadata.deletion_timestamp.is_none()).collect();
    ordered.sort_by(|a, b| {
        let t = |v: &Vlan| v.metadata.creation_timestamp.as_ref().map(|t| t.0);
        t(a).cmp(&t(b)).then_with(|| a.name_any().cmp(&b.name_any()))
    });

    let mut taken: BTreeMap<(String, u16), String> = BTreeMap::new();
    let mut accepted = Vec::new();
    let mut rejected = BTreeMap::new();
    for v in ordered {
        let name = v.name_any();
        if let Err(e) = validate(&v.spec) {
            rejected.insert(name, e);
            continue;
        }
        let bridge = bridge_of(&v.spec).to_string();
        let key = (bridge.clone(), v.spec.id);
        if let Some(holder) = taken.get(&key) {
            rejected.insert(name, format!("VLAN {} on bridge {} is already VLAN {holder}", v.spec.id, bridge));
            continue;
        }
        taken.insert(key, name.clone());
        accepted.push(Accepted { name, spec: v.spec.clone(), bridge });
    }
    (accepted, rejected)
}

/// The CNI config of a VLAN's NAD: the bridge plugin, tagging the pod's port
/// with the VLAN as its untagged PVID.
///
/// `preserveDefaultVlan: false` is what isolates it: otherwise the pod's port
/// stays a member of VLAN 1 too, which is the node's own untagged LAN.
pub fn nad_config(name: &str, spec: &VlanSpec) -> String {
    let mut c = json!({
        "cniVersion": "1.0.0",
        "name": name,
        "type": "bridge",
        "bridge": bridge_of(spec),
        "vlan": spec.id,
        "preserveDefaultVlan": false,
        "isGateway": false,
        "ipMasq": false,
    });
    if let Some(m) = spec.mtu {
        c["mtu"] = json!(m);
    }
    let addresses = spec.addresses.clone().unwrap_or_default();
    match addresses.kind {
        AddressType::None => {}
        AddressType::Dhcp => c["ipam"] = json!({"type": "dhcp"}),
        AddressType::Static => {
            let routes: Vec<Value> = addresses
                .routes
                .iter()
                .map(|r| match &r.gw {
                    Some(gw) => json!({"dst": r.dst, "gw": gw}),
                    None => json!({"dst": r.dst}),
                })
                .collect();
            c["ipam"] = if routes.is_empty() {
                json!({"type": "static"})
            } else {
                json!({"type": "static", "routes": routes})
            };
            // The pod's `ips` reach the plugin as runtimeConfig only if it asks.
            c["capabilities"] = json!({"ips": true});
        }
    }
    c.to_string()
}

/// A VLAN's NAD in one namespace.
pub fn nad(name: &str, namespace: &str, spec: &VlanSpec) -> Rendered {
    let api = nad_api();
    let mut obj = DynamicObject::new(name, &api).within(namespace);
    obj.metadata.labels = Some(
        [
            (VLAN_LABEL.to_string(), name.to_string()),
            (MANAGED_BY_LABEL.to_string(), MANAGED_BY.to_string()),
        ]
        .into(),
    );
    obj.data = json!({"spec": {"config": nad_config(name, spec)}});
    Rendered { api, namespace: Some(namespace.to_string()), obj }
}

/// What a node agent reports, as JSON in [`NODE_STATE_ANNOTATION`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeState {
    /// The bridge this agent manages.
    pub bridge: String,
    #[serde(default)]
    pub vlan_filtering: bool,
    /// VLANs present on every uplink port they belong on.
    #[serde(default)]
    pub present: Vec<u16>,
    /// VLAN entries this agent added, by port: the only ones it may remove.
    #[serde(default)]
    pub owned: BTreeMap<String, Vec<u16>>,
    /// Per-VLAN reasons a VLAN is not present (`id -> why`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub problems: BTreeMap<u16, String>,
    /// A failure of the whole pass (the bridge is missing, netlink refused).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A NAD found on the cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExistingNad {
    pub namespace: String,
    pub name: String,
    /// Our [`VLAN_LABEL`], if it carries one.
    pub vlan: Option<String>,
    pub config: Option<String>,
}

impl ExistingNad {
    pub fn from_object(o: &DynamicObject) -> ExistingNad {
        ExistingNad {
            namespace: o.metadata.namespace.clone().unwrap_or_default(),
            name: o.metadata.name.clone().unwrap_or_default(),
            vlan: o.metadata.labels.as_ref().and_then(|l| l.get(VLAN_LABEL)).cloned(),
            config: o.data["spec"]["config"].as_str().map(str::to_string),
        }
    }
}

/// Everything one controller pass looks at.
#[derive(Debug, Clone, Default)]
pub struct World {
    pub vlans: Vec<Vlan>,
    /// Existing namespaces, with their current [`DEFAULT_NETWORKS_ANNOTATION`].
    pub namespaces: BTreeMap<String, Option<String>>,
    pub nads: Vec<ExistingNad>,
    /// Nodes, with their agent's report (`None`: no agent there).
    pub nodes: BTreeMap<String, Option<NodeState>>,
    /// `namespace/name` of each pod -> the `namespace/name` NADs its
    /// network-status says it is attached to.
    pub pods: BTreeMap<String, Vec<String>>,
}

/// What one VLAN's status says, before conditions get timestamps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub nads: Vec<String>,
    pub missing_namespaces: Vec<String>,
    pub nodes: Vec<String>,
    pub pending_nodes: Vec<String>,
    pub pods: Vec<String>,
    pub pod_count: usize,
    /// Degraded: things retrying will not fix.
    pub problems: Vec<String>,
    /// Not Ready yet: things that should settle.
    pub waiting: Vec<String>,
}

/// One controller pass's work.
#[derive(Debug, Default)]
pub struct Plan {
    /// NADs to create or update.
    pub apply: Vec<Rendered>,
    /// Our NADs (`namespace`, `name`) no VLAN wants any more.
    pub delete: Vec<(String, String)>,
    /// Namespaces whose default annotation changes (`None`: remove it).
    pub annotate: Vec<(String, Option<String>)>,
    /// By VLAN name, every VLAN on the cluster (rejected ones included).
    pub reports: BTreeMap<String, Report>,
}

/// Decide a pass from what is on the cluster.
pub fn plan(world: &World) -> Plan {
    let (accepted, rejected) = accept(&world.vlans);
    let mut plan = Plan::default();

    for (name, why) in &rejected {
        plan.reports.insert(name.clone(), Report { problems: vec![why.clone()], ..Default::default() });
    }

    // Namespace defaults: first claim (in accept order) wins.
    let mut default_of: BTreeMap<String, String> = BTreeMap::new();
    for a in &accepted {
        let report = plan.reports.entry(a.name.clone()).or_default();
        for ns in &a.spec.default_for {
            match default_of.get(ns) {
                Some(holder) => report.problems.push(format!("namespace {ns}'s default is already VLAN {holder}")),
                None => {
                    default_of.insert(ns.clone(), a.name.clone());
                }
            }
        }
    }
    for (ns, current) in &world.namespaces {
        let want = default_of.get(ns).cloned();
        if *current != want {
            plan.annotate.push((ns.clone(), want));
        }
    }

    // NADs.
    let existing: BTreeMap<(String, String), &ExistingNad> =
        world.nads.iter().map(|n| ((n.namespace.clone(), n.name.clone()), n)).collect();
    let mut wanted: BTreeSet<(String, String)> = BTreeSet::new();
    for a in &accepted {
        let report = plan.reports.entry(a.name.clone()).or_default();
        let config = nad_config(&a.name, &a.spec);
        for ns in allowed_namespaces(&a.spec) {
            if !world.namespaces.contains_key(&ns) {
                report.missing_namespaces.push(ns);
                continue;
            }
            let key = (ns.clone(), a.name.clone());
            match existing.get(&key) {
                Some(e) if e.vlan.as_deref() != Some(a.name.as_str()) => {
                    report.problems.push(format!(
                        "NetworkAttachmentDefinition {ns}/{} exists and is not this VLAN's; not overwriting it",
                        a.name
                    ));
                    continue;
                }
                Some(e) if e.config.as_deref() == Some(config.as_str()) => {}
                _ => plan.apply.push(nad(&a.name, &ns, &a.spec)),
            }
            wanted.insert(key);
            report.nads.push(format!("{ns}/{}", a.name));
        }
    }
    for n in &world.nads {
        if n.vlan.is_some() && !wanted.contains(&(n.namespace.clone(), n.name.clone())) {
            plan.delete.push((n.namespace.clone(), n.name.clone()));
        }
    }

    // Nodes and pods.
    for a in &accepted {
        let report = plan.reports.entry(a.name.clone()).or_default();
        for (node, state) in &world.nodes {
            let Some(state) = state else { continue };
            if state.bridge != a.bridge {
                report.pending_nodes.push(format!("{node}: its agent manages bridge {}, not {}", state.bridge, a.bridge));
            } else if state.present.contains(&a.spec.id) {
                report.nodes.push(node.clone());
            } else {
                let why = state
                    .problems
                    .get(&a.spec.id)
                    .cloned()
                    .or_else(|| state.error.clone())
                    .unwrap_or_else(|| "not on the uplinks yet".into());
                report.pending_nodes.push(format!("{node}: {why}"));
            }
        }
        if report.nodes.is_empty() && report.pending_nodes.is_empty() {
            report.waiting.push("no node agent reports this bridge".into());
        } else if !report.pending_nodes.is_empty() {
            report.waiting.push(format!("{} node(s) do not carry it yet", report.pending_nodes.len()));
        }

        let nads: BTreeSet<&String> = report.nads.iter().collect();
        let mut on: Vec<String> = world
            .pods
            .iter()
            .filter(|(_, attached)| attached.iter().any(|n| nads.contains(n)))
            .map(|(pod, _)| pod.clone())
            .collect();
        on.sort();
        report.pod_count = on.len();
        on.truncate(MAX_LISTED_PODS);
        report.pods = on;
    }
    plan
}

/// The NADs a pod's network-status annotation says it is attached to, as
/// `namespace/name` (an unqualified name is in the pod's namespace).
pub fn attached_nads(pod_namespace: &str, network_status: &str) -> Vec<String> {
    let Ok(entries) = serde_json::from_str::<Vec<Value>>(network_status) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| e["name"].as_str())
        .map(|n| if n.contains('/') { n.to_string() } else { format!("{pod_namespace}/{n}") })
        .collect()
}

/// The status a report becomes, carrying over unchanged conditions' times.
pub fn status(previous: &VlanStatus, report: &Report, generation: i64, now: &Time) -> VlanStatus {
    let ready = report.problems.is_empty() && report.waiting.is_empty();
    let ready_cond = if ready {
        cond(crd::conditions::READY, "True", "Ready", "NADs in place and every node with an agent carries the VLAN")
    } else if !report.problems.is_empty() {
        cond(crd::conditions::READY, "False", "Rejected", &report.problems.join("; "))
    } else {
        cond(crd::conditions::READY, "False", "Waiting", &report.waiting.join("; "))
    };
    let degraded_cond = if report.problems.is_empty() {
        cond(crd::conditions::DEGRADED, "False", "AsExpected", "")
    } else {
        cond(crd::conditions::DEGRADED, "True", "Rejected", &report.problems.join("; "))
    };

    let carry = |mut c: Condition| {
        c.observed_generation = Some(generation);
        c.last_transition_time = previous
            .conditions
            .iter()
            .find(|p| p.type_ == c.type_ && p.status == c.status)
            .map(|p| p.last_transition_time.clone())
            .unwrap_or_else(|| now.clone());
        c
    };

    VlanStatus {
        conditions: vec![carry(ready_cond), carry(degraded_cond)],
        nads: report.nads.clone(),
        missing_namespaces: report.missing_namespaces.clone(),
        nodes: report.nodes.clone(),
        pending_nodes: report.pending_nodes.clone(),
        node_count: report.nodes.len() as i64,
        pods: report.pods.clone(),
        pod_count: report.pod_count as i64,
        observed_generation: Some(generation),
    }
}

fn cond(type_: &str, status: &str, reason: &str, message: &str) -> Condition {
    Condition {
        type_: type_.into(),
        status: status.into(),
        reason: reason.into(),
        message: message.into(),
        last_transition_time: Time(chrono::DateTime::from_timestamp(0, 0).expect("the epoch is a valid time")),
        observed_generation: None,
    }
}

#[cfg(test)]
mod tests;
