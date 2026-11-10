//! The node agent (`network-operator node-agent`, a DaemonSet; #32 decision
//! 2a): puts the accepted VLANs on one bridge's uplink ports and reports what
//! the node carries.
//!
//! What it may touch, and nothing else:
//!
//! - `vlan_filtering` on the bridge it was started for: turned **on** when a
//!   VLAN names that bridge, never turned off (a bridge with filtering on and
//!   no VLANs behaves as before: every port keeps PVID 1 untagged).
//! - VLAN entries on that bridge's uplink ports (NICs and bonds), added
//!   *tagged*, never as PVID. It removes only entries it added itself (the
//!   `owned` list in its node annotation); an entry already there is used,
//!   not claimed. VLAN 1, the ports' PVID, is never added, removed or claimed.
//!
//! It never creates, deletes, enslaves or re-addresses a link. The pods'
//! ports are the CNI bridge plugin's (the NAD's `vlan`).
//!
//! The pass is [`actions`] (pure) then [`apply`] (netlink), and the report
//! goes to the node's [`NODE_STATE_ANNOTATION`], where the controller reads
//! it into each VLAN's status.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, Patch, PatchParams};
use kube::Client;
use serde_json::json;
use tracing::{info, warn};

use super::crd::Vlan;
use super::netlink::{self, Netlink};
use super::{accept, NodeState, DEFAULT_PVID, NODE_STATE_ANNOTATION};
use crate::metrics::{FailureReason, Metrics};

/// sysfs's view of the links (the agent is host-networked).
pub const SYS_CLASS_NET: &str = "/sys/class/net";

/// A VLAN this node's bridge should carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub vid: u16,
    /// The one port it goes on; `None`: every uplink.
    pub uplink: Option<String>,
}

/// The accepted VLANs on `bridge`.
pub fn wanted(vlans: &[Vlan], bridge: &str) -> Vec<Wanted> {
    let (accepted, _) = accept(vlans);
    accepted
        .into_iter()
        .filter(|a| a.bridge == bridge)
        .map(|a| Wanted { vid: a.spec.id, uplink: a.spec.uplink.filter(|u| !u.is_empty()) })
        .collect()
}

/// One port of the bridge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Port {
    pub index: i32,
    /// A NIC or a bond, i.e. it leaves the node.
    pub uplink: bool,
    pub vids: BTreeSet<u16>,
}

/// What a pass will do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Actions {
    pub add: Vec<(String, u16)>,
    pub del: Vec<(String, u16)>,
    /// VLANs that cannot be placed, and why.
    pub problems: BTreeMap<u16, String>,
}

/// The ports each wanted VLAN goes on (`Err`: why it cannot be placed).
fn targets<'a>(w: &Wanted, ports: &'a BTreeMap<String, Port>, bridge: &str) -> Result<Vec<&'a String>, String> {
    match &w.uplink {
        Some(u) => match ports.get_key_value(u) {
            Some((name, _)) => Ok(vec![name]),
            None => Err(format!("uplink {u} is not a port of bridge {bridge}")),
        },
        None => {
            let ups: Vec<&String> = ports.iter().filter(|(_, p)| p.uplink).map(|(n, _)| n).collect();
            if ups.is_empty() {
                Err(format!("no uplink port (NIC or bond) on bridge {bridge}"))
            } else {
                Ok(ups)
            }
        }
    }
}

/// Decide the adds and removes for one pass.
pub fn actions(
    wanted: &[Wanted],
    ports: &BTreeMap<String, Port>,
    owned: &BTreeMap<String, BTreeSet<u16>>,
    bridge: &str,
) -> Actions {
    let mut out = Actions::default();
    let mut keep: BTreeSet<(String, u16)> = BTreeSet::new();
    for w in wanted {
        if w.vid == DEFAULT_PVID {
            continue;
        }
        match targets(w, ports, bridge) {
            Err(why) => {
                out.problems.insert(w.vid, why);
            }
            Ok(ps) => {
                for p in ps {
                    keep.insert((p.clone(), w.vid));
                    if !ports[p].vids.contains(&w.vid) {
                        out.add.push((p.clone(), w.vid));
                    }
                }
            }
        }
    }
    for (port, vids) in owned {
        let Some(current) = ports.get(port) else { continue };
        for v in vids {
            if *v != DEFAULT_PVID && current.vids.contains(v) && !keep.contains(&(port.clone(), *v)) {
                out.del.push((port.clone(), *v));
            }
        }
    }
    out
}

/// What the agent owns after a pass: what it owned and still finds, plus
/// what it added, minus what it removed.
pub fn owned_after(
    owned: &BTreeMap<String, BTreeSet<u16>>,
    added: &[(String, u16)],
    removed: &[(String, u16)],
    ports: &BTreeMap<String, Port>,
) -> BTreeMap<String, BTreeSet<u16>> {
    let mut out: BTreeMap<String, BTreeSet<u16>> = BTreeMap::new();
    for (p, v) in owned.iter().flat_map(|(p, vs)| vs.iter().map(move |v| (p, *v))).chain(added.iter().map(|(p, v)| (p, *v))) {
        if v == DEFAULT_PVID || removed.contains(&(p.clone(), v)) {
            continue;
        }
        if ports.get(p).is_some_and(|port| port.vids.contains(&v)) {
            out.entry(p.clone()).or_default().insert(v);
        }
    }
    out
}

/// The wanted VLANs that are on every port they belong on.
pub fn present(wanted: &[Wanted], ports: &BTreeMap<String, Port>, bridge: &str) -> Vec<u16> {
    let mut out: Vec<u16> = wanted
        .iter()
        .filter(|w| {
            // VLAN 1 is the ports' PVID already.
            w.vid == DEFAULT_PVID
                || targets(w, ports, bridge).is_ok_and(|ps| ps.iter().all(|p| ports[*p].vids.contains(&w.vid)))
        })
        .map(|w| w.vid)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The bridge's ports, from the netlink dump and sysfs.
fn read_ports(nl: &mut Netlink, sys: &Path, bridge_index: i32) -> std::io::Result<BTreeMap<String, Port>> {
    Ok(nl
        .port_vlans()?
        .into_iter()
        .filter(|(index, p)| *index != bridge_index && p.master == Some(bridge_index) && !p.name.is_empty())
        .map(|(index, p)| {
            let uplink = netlink::is_uplink(sys, &p.name);
            (p.name, Port { index, uplink, vids: p.vids })
        })
        .collect())
}

/// One pass against the bridge. Blocking (netlink); run off the runtime.
pub fn apply(sys: &Path, bridge: &str, wanted: &[Wanted], owned: &BTreeMap<String, BTreeSet<u16>>) -> NodeState {
    let mut state = NodeState { bridge: bridge.to_string(), ..Default::default() };
    // Until the bridge is read, what we owned stays ours.
    state.owned = owned.iter().map(|(p, v)| (p.clone(), v.iter().copied().collect())).collect();

    let bridge_index = match netlink::ifindex(sys, bridge) {
        Ok(i) => i,
        Err(e) => {
            state.error = Some(format!("bridge {bridge}: {e}"));
            return state;
        }
    };
    let mut nl = match Netlink::open() {
        Ok(nl) => nl,
        Err(e) => {
            state.error = Some(format!("netlink: {e}"));
            return state;
        }
    };

    let mut filtering = netlink::vlan_filtering(sys, bridge).unwrap_or(false);
    if !filtering && wanted.iter().any(|w| w.vid != DEFAULT_PVID) {
        match nl.set_vlan_filtering(bridge_index) {
            Ok(()) => {
                info!(%bridge, "turned vlan_filtering on");
                filtering = true;
            }
            Err(e) => {
                state.error = Some(format!("vlan_filtering on {bridge}: {e}"));
                return state;
            }
        }
    }
    state.vlan_filtering = filtering;

    let ports = match read_ports(&mut nl, sys, bridge_index) {
        Ok(p) => p,
        Err(e) => {
            state.error = Some(format!("reading {bridge}'s ports: {e}"));
            return state;
        }
    };

    let plan = actions(wanted, &ports, owned, bridge);
    state.problems = plan.problems.clone();
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    for (port, vid) in &plan.add {
        match nl.add_vid(ports[port].index, *vid) {
            Ok(()) => {
                info!(%port, vid, "added VLAN");
                added.push((port.clone(), *vid));
            }
            Err(e) => {
                warn!(%port, vid, error = %e, "adding VLAN failed");
                state.problems.insert(*vid, format!("adding to {port}: {e}"));
            }
        }
    }
    for (port, vid) in &plan.del {
        match nl.del_vid(ports[port].index, *vid) {
            Ok(()) => {
                info!(%port, vid, "removed VLAN");
                removed.push((port.clone(), *vid));
            }
            Err(e) => warn!(%port, vid, error = %e, "removing VLAN failed"),
        }
    }

    // Report what is there now, not what was asked for.
    let after = match read_ports(&mut nl, sys, bridge_index) {
        Ok(p) => p,
        Err(e) => {
            state.error = Some(format!("re-reading {bridge}'s ports: {e}"));
            // Keep what we owned plus what we added, so nothing is orphaned.
            let mut owned_now = owned.clone();
            for (p, v) in &added {
                owned_now.entry(p.clone()).or_default().insert(*v);
            }
            state.owned = owned_now.into_iter().map(|(p, v)| (p, v.into_iter().collect())).collect();
            return state;
        }
    };
    state.present = present(wanted, &after, bridge);
    state.owned = owned_after(owned, &added, &removed, &after)
        .into_iter()
        .map(|(p, v)| (p, v.into_iter().collect()))
        .collect();
    state
}

/// The owned list from this node's last report, if it was for this bridge.
pub fn owned_from(annotation: Option<&str>, bridge: &str) -> BTreeMap<String, BTreeSet<u16>> {
    annotation
        .and_then(|a| serde_json::from_str::<NodeState>(a).ok())
        .filter(|s| s.bridge == bridge)
        .map(|s| s.owned.into_iter().map(|(p, v)| (p, v.into_iter().collect())).collect())
        .unwrap_or_default()
}

/// Agent settings.
#[derive(Debug, Clone)]
pub struct Settings {
    pub bridge: String,
    pub node: String,
    pub sys: PathBuf,
    pub resync: Duration,
}

/// Run the agent until the process is signalled.
pub async fn run(client: Client, settings: Settings, metrics: Arc<Metrics>) -> anyhow::Result<()> {
    let vlans: Api<Vlan> = Api::all(client.clone());
    let nodes: Api<Node> = Api::all(client.clone());
    let mut triggers = super::controller::changes(vec![super::controller::watch(vlans.clone())]);
    info!(bridge = %settings.bridge, node = %settings.node, "starting VLAN node agent");
    metrics.set_ready();

    loop {
        let started = Instant::now();
        match pass(&vlans, &nodes, &settings).await {
            Ok(()) => metrics.record_success(chrono::Utc::now().timestamp(), started.elapsed().as_micros() as u64),
            Err(e) => {
                warn!(error = %e, "agent pass failed");
                metrics.record_failure(FailureReason::Kube, started.elapsed().as_micros() as u64);
            }
        }
        super::controller::settle(&mut triggers, settings.resync).await;
    }
}

async fn pass(vlans: &Api<Vlan>, nodes: &Api<Node>, s: &Settings) -> Result<(), kube::Error> {
    let list = vlans.list(&Default::default()).await?.items;
    let want = wanted(&list, &s.bridge);
    let node = nodes.get(&s.node).await?;
    let previous = node.metadata.annotations.as_ref().and_then(|a| a.get(NODE_STATE_ANNOTATION)).cloned();
    let owned = owned_from(previous.as_deref(), &s.bridge);

    let (sys, bridge) = (s.sys.clone(), s.bridge.clone());
    let state = tokio::task::spawn_blocking(move || apply(&sys, &bridge, &want, &owned))
        .await
        .unwrap_or_else(|e| NodeState { bridge: s.bridge.clone(), error: Some(format!("agent pass panicked: {e}")), ..Default::default() });
    if let Some(e) = &state.error {
        warn!(error = %e, "bridge not in the wanted state");
    }

    let text = serde_json::to_string(&state).map_err(kube::Error::SerdeError)?;
    if previous.as_deref() != Some(text.as_str()) {
        let patch = json!({"metadata": {"annotations": {NODE_STATE_ANNOTATION: text}}});
        nodes.patch(&s.node, &PatchParams::default(), &Patch::Merge(&patch)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(index: i32, uplink: bool, vids: &[u16]) -> Port {
        Port { index, uplink, vids: vids.iter().copied().collect() }
    }

    fn w(vid: u16) -> Wanted {
        Wanted { vid, uplink: None }
    }

    fn ports() -> BTreeMap<String, Port> {
        [
            ("eth0".to_string(), port(2, true, &[1])),
            ("bond0".to_string(), port(3, true, &[1, 30])),
            ("veth1".to_string(), port(9, false, &[10])),
        ]
        .into()
    }

    fn owned(entries: &[(&str, &[u16])]) -> BTreeMap<String, BTreeSet<u16>> {
        entries.iter().map(|(p, v)| (p.to_string(), v.iter().copied().collect())).collect()
    }

    #[test]
    fn a_vlan_goes_on_every_uplink_and_never_on_a_pods_port() {
        let a = actions(&[w(10)], &ports(), &owned(&[]), "stormbr0");
        assert_eq!(a.add, vec![("bond0".to_string(), 10), ("eth0".to_string(), 10)]);
        assert!(a.del.is_empty() && a.problems.is_empty());
    }

    #[test]
    fn a_named_uplink_is_the_only_port_used() {
        let mut x = w(10);
        x.uplink = Some("eth0".into());
        assert_eq!(actions(&[x.clone()], &ports(), &owned(&[]), "br").add, vec![("eth0".to_string(), 10)]);
        x.uplink = Some("eth9".into());
        let a = actions(&[x], &ports(), &owned(&[]), "br");
        assert!(a.add.is_empty());
        assert_eq!(a.problems[&10], "uplink eth9 is not a port of bridge br");
    }

    #[test]
    fn no_uplink_is_a_problem_not_a_guess() {
        let only_veth: BTreeMap<String, Port> = [("veth1".to_string(), port(9, false, &[1]))].into();
        let a = actions(&[w(10)], &only_veth, &owned(&[]), "br");
        assert!(a.add.is_empty());
        assert!(a.problems[&10].contains("no uplink port"));
    }

    #[test]
    fn an_entry_already_there_is_used_not_claimed_and_never_removed() {
        // bond0 already carries 30 (the host's config): nothing to add there,
        // and since we do not own it, dropping VLAN 30 leaves it alone.
        let a = actions(&[w(30)], &ports(), &owned(&[]), "br");
        assert_eq!(a.add, vec![("eth0".to_string(), 30)]);
        let after: BTreeMap<String, Port> = [
            ("eth0".to_string(), port(2, true, &[1, 30])),
            ("bond0".to_string(), port(3, true, &[1, 30])),
        ]
        .into();
        let o = owned_after(&owned(&[]), &a.add, &[], &after);
        assert_eq!(o, owned(&[("eth0", &[30])]));
        let gone = actions(&[], &after, &o, "br");
        assert_eq!(gone.del, vec![("eth0".to_string(), 30)]);
    }

    #[test]
    fn deleting_a_vlan_removes_only_our_entries() {
        let p: BTreeMap<String, Port> = [
            ("eth0".to_string(), port(2, true, &[1, 10, 20])),
            ("bond0".to_string(), port(3, true, &[1, 10, 20])),
        ]
        .into();
        let mine = owned(&[("eth0", &[10, 20]), ("bond0", &[10])]);
        let a = actions(&[w(20)], &p, &mine, "br");
        assert!(a.add.is_empty());
        assert_eq!(a.del, vec![("bond0".to_string(), 10), ("eth0".to_string(), 10)]);
    }

    #[test]
    fn vlan_one_is_never_touched() {
        let a = actions(&[w(1)], &ports(), &owned(&[("eth0", &[1])]), "br");
        assert_eq!(a, Actions::default());
        assert_eq!(present(&[w(1)], &ports(), "br"), vec![1]);
        assert!(owned_after(&owned(&[("eth0", &[1])]), &[("eth0".into(), 1)], &[], &ports()).is_empty());
    }

    #[test]
    fn present_means_on_every_target_port() {
        let p: BTreeMap<String, Port> = [
            ("eth0".to_string(), port(2, true, &[1, 10, 20])),
            ("bond0".to_string(), port(3, true, &[1, 10])),
        ]
        .into();
        assert_eq!(present(&[w(10), w(20), w(30)], &p, "br"), vec![10]);
    }

    #[test]
    fn ownership_is_dropped_when_the_entry_or_port_is_gone() {
        let p: BTreeMap<String, Port> = [("eth0".to_string(), port(2, true, &[1]))].into();
        let o = owned_after(&owned(&[("eth0", &[10]), ("eth1", &[10])]), &[], &[], &p);
        assert!(o.is_empty());
    }

    #[test]
    fn the_owned_list_survives_a_restart_only_for_the_same_bridge() {
        let s = NodeState { bridge: "stormbr0".into(), owned: [("eth0".to_string(), vec![10])].into(), ..Default::default() };
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(owned_from(Some(&text), "stormbr0"), owned(&[("eth0", &[10])]));
        assert!(owned_from(Some(&text), "br1").is_empty());
        assert!(owned_from(Some("junk"), "stormbr0").is_empty());
        assert!(owned_from(None, "stormbr0").is_empty());
    }

    #[test]
    fn a_hundred_vlans_on_one_node_is_a_hundred_entries_per_uplink() {
        let want: Vec<Wanted> = (100..200).map(w).collect();
        let a = actions(&want, &ports(), &owned(&[]), "br");
        assert_eq!(a.add.len(), 200, "two uplinks, no new interfaces");
    }

    #[test]
    fn a_missing_bridge_is_reported_and_keeps_ownership() {
        let dir = std::env::temp_dir().join(format!("no-sysfs-{}", std::process::id()));
        let s = apply(&dir, "stormbr0", &[w(10)], &owned(&[("eth0", &[10])]));
        assert!(s.error.unwrap().starts_with("bridge stormbr0:"));
        assert_eq!(s.owned, [("eth0".to_string(), vec![10])].into());
    }
}
