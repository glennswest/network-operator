//! The `VLAN` custom resource (#32): one VLAN on the nodes' VLAN-aware
//! bridge, offered to the namespaces allowed to use it as a Multus
//! `NetworkAttachmentDefinition`.
//!
//! Cluster-scoped; the object's name is also the NAD's name, so a pod names
//! the VLAN in `k8s.v1.cni.cncf.io/networks` by it.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The bridge a VLAN uses when `spec.bridge` is unset: stormcos's node
/// bridge, which carries the uplink.
pub const DEFAULT_BRIDGE: &str = "stormbr0";

#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[kube(
    group = "network.storm.io",
    version = "v1",
    kind = "VLAN",
    struct = "Vlan",
    plural = "vlans",
    shortname = "vl",
    status = "VlanStatus",
    derive = "Default",
    derive = "PartialEq"
)]
#[kube(printcolumn = r#"{"name":"ID","type":"integer","jsonPath":".spec.id"}"#)]
#[kube(printcolumn = r#"{"name":"Bridge","type":"string","jsonPath":".spec.bridge"}"#)]
#[kube(printcolumn = r#"{"name":"Nodes","type":"integer","jsonPath":".status.nodeCount"}"#)]
#[kube(printcolumn = r#"{"name":"Pods","type":"integer","jsonPath":".status.podCount"}"#)]
#[kube(
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct VlanSpec {
    /// The 802.1Q VLAN ID, 1–4094.
    #[schemars(range(min = 1, max = 4094))]
    pub id: u16,

    /// The node's VLAN-aware bridge (default `stormbr0`). The node agent only
    /// acts on the bridge it was started for; it turns `vlan_filtering` on
    /// and adds this VLAN to the bridge's uplink ports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,

    /// The bridge port that carries the VLAN off the node (a NIC or a bond).
    /// Unset: every bridge port that is a NIC or a bond.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uplink: Option<String>,

    /// MTU of the pods' interfaces on this VLAN. Unset: the bridge's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u32>,

    /// Namespaces allowed to use the VLAN. Its NAD exists only there; a pod
    /// anywhere else naming it waits (no NAD), it never falls back.
    #[serde(default)]
    pub namespaces: Vec<String>,

    /// Namespaces whose default VLAN this is (allowed implicitly). A pod there
    /// that names no networks gets this one; one default per namespace.
    #[serde(default)]
    pub default_for: Vec<String>,

    /// Where a pod's address on the VLAN comes from. Default: none (L2 only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addresses: Option<Addresses>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Addresses {
    #[serde(rename = "type", default)]
    pub kind: AddressType,

    /// `static` only: routes added with the pod's address.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<Route>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AddressType {
    /// No address: the interface is L2 only.
    #[default]
    None,
    /// DHCP from the VLAN's own server (the CNI `dhcp` IPAM plugin; its
    /// daemon must run on the node).
    Dhcp,
    /// The address the pod names in its annotation (`"ips": ["10.0.10.5/24"]`).
    Static,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    /// Destination CIDR.
    pub dst: String,
    /// Next hop; unset routes out of the interface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gw: Option<String>,
}

/// Written only by the operator.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VlanStatus {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,

    /// `namespace/name` of each NAD the operator keeps for this VLAN.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nads: Vec<String>,

    /// Allowed namespaces that do not exist (yet); their NAD follows them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_namespaces: Vec<String>,

    /// Nodes whose agent reports this VLAN on the bridge's uplinks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<String>,

    /// Nodes with an agent that does not (yet) have it, with the reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_nodes: Vec<String>,

    #[serde(default)]
    pub node_count: i64,

    /// `namespace/name` of pods attached to the VLAN (at most
    /// [`crate::vlan::MAX_LISTED_PODS`]; `podCount` is the full count).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pods: Vec<String>,

    #[serde(default)]
    pub pod_count: i64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

/// Condition types on a VLAN.
pub mod conditions {
    /// The spec is valid, its NADs are in place and every node with an agent
    /// carries the VLAN.
    pub const READY: &str = "Ready";
    /// Something is wrong that the operator cannot fix by retrying: an
    /// invalid spec, a conflict, a NAD it may not overwrite.
    pub const DEGRADED: &str = "Degraded";
}
