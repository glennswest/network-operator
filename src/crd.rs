//! The `Network` custom resource — the single source of truth for the cluster
//! network. Cluster-scoped, conventionally named `cluster`, with a `/status`
//! subresource carrying cluster-operator-style conditions.
//!
//! Everything under `spec.cilium` is optional: `spec.mode` supplies the default
//! for each field and an explicitly set field overrides its mode default. The
//! resolution lives in [`crate::modes`]; this module is only the wire shape.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Conventional name of the singleton `Network` object.
pub const NETWORK_NAME: &str = "cluster";

#[derive(CustomResource, Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[kube(
    group = "network.storm.io",
    version = "v1",
    kind = "Network",
    plural = "networks",
    shortname = "net",
    status = "NetworkStatus",
    derive = "Default",
    derive = "PartialEq"
)]
#[kube(printcolumn = r#"{"name":"Mode","type":"string","jsonPath":".status.appliedMode"}"#)]
#[kube(printcolumn = r#"{"name":"Version","type":"string","jsonPath":".status.appliedVersion"}"#)]
#[kube(
    printcolumn = r#"{"name":"Available","type":"string","jsonPath":".status.conditions[?(@.type==\"Available\")].status"}"#
)]
#[kube(
    printcolumn = r#"{"name":"Progressing","type":"string","jsonPath":".status.conditions[?(@.type==\"Progressing\")].status"}"#
)]
#[kube(
    printcolumn = r#"{"name":"Degraded","type":"string","jsonPath":".status.conditions[?(@.type==\"Degraded\")].status"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSpec {
    /// The CNI to install. Cilium is the only supported value.
    #[serde(default)]
    pub cni: Cni,

    /// Named install-time profile. Sets defaults for every `cilium` field below;
    /// anything set explicitly wins.
    #[serde(default)]
    pub mode: Mode,

    /// Pod CIDR(s) (OpenShift: `networking.clusterNetwork`). Immutable.
    #[serde(default)]
    pub cluster_network: Vec<String>,

    /// Service CIDR(s) (OpenShift: `networking.serviceNetwork`). Immutable.
    #[serde(default)]
    pub service_network: Vec<String>,

    /// Cilium knobs. Every field is an override of the mode's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cilium: Option<CiliumSpec>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Cni {
    #[default]
    Cilium,
}

/// Install-time profile. See the mode table in README.md.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// VXLAN overlay, cluster-pool IPAM, eBPF kube-proxy replacement.
    #[default]
    Overlay,
    /// Native routing with `autoDirectNodeRoutes`; nodes share an L2.
    Native,
    /// Native routing plus the Cilium BGP control plane; LB-IPAM + BGP announce.
    Bgp,
    /// Overlay plus transparent WireGuard pod-to-pod encryption.
    Encrypted,
    /// Overlay plus LB-IPAM with L2/ARP announcements — bare metal, no cloud LB.
    BareMetal,
}

impl Mode {
    /// The datapath family. Crossing this boundary is a disruptive re-plumb, so
    /// it is the part of `mode` that is immutable after install.
    pub fn datapath(self) -> RoutingMode {
        match self {
            Mode::Overlay | Mode::Encrypted | Mode::BareMetal => RoutingMode::Tunnel,
            Mode::Native | Mode::Bgp => RoutingMode::Native,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Overlay => "overlay",
            Mode::Native => "native",
            Mode::Bgp => "bgp",
            Mode::Encrypted => "encrypted",
            Mode::BareMetal => "bare-metal",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CiliumSpec {
    /// Image tag. Bumping it triggers a rolling upgrade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Image registry/repository prefix, for mirrored or air-gapped installs.
    /// Default `quay.io/cilium`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipam: Option<IpamSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingSpec>,

    /// MTU; 0 (the default) means auto-detect from the node, as CNO does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<i32>,

    /// eBPF ClusterIP/NodePort/LoadBalancer/HostPort in place of kube-proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kube_proxy_replacement: Option<bool>,

    /// `bpf` (≈ OVN shared gateway) or `legacy` (≈ local gateway).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_routing: Option<HostRouting>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<EncryptionSpec>,

    /// The apiserver the agent dials before Services exist. Required — there is
    /// no in-cluster Service to fall back on while the CNI is coming up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k8s_service_host: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k8s_service_port: Option<u16>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_balancer: Option<LoadBalancerSpec>,

    /// The standalone `cilium-envoy` DaemonSet. Off by default — Cilium embeds
    /// the proxy in the agent unless you split it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envoy: Option<EnvoySpec>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IpamSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<IpamMode>,
    /// Per-node pod CIDR mask carved out of `clusterNetwork`.
    // camelCase would give `clusterPoolIpv4MaskSize`; the acronym is capitalised
    // in Cilium's own config and in our README, so pin the wire name.
    #[serde(
        default,
        rename = "clusterPoolIPv4MaskSize",
        skip_serializing_if = "Option::is_none"
    )]
    pub cluster_pool_ipv4_mask_size: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum IpamMode {
    /// Cilium carves per-node CIDRs out of `clusterNetwork` itself.
    #[default]
    ClusterPool,
    /// Per-node CIDRs come from `Node.spec.podCIDR` (kube-controller-manager).
    Kubernetes,
}

impl IpamMode {
    pub fn as_str(self) -> &'static str {
        match self {
            IpamMode::ClusterPool => "cluster-pool",
            IpamMode::Kubernetes => "kubernetes",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RoutingSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RoutingMode>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingMode {
    /// VXLAN/Geneve encapsulation.
    #[default]
    Tunnel,
    /// Native routing — the underlay carries pod traffic.
    Native,
}

impl RoutingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RoutingMode::Tunnel => "tunnel",
            RoutingMode::Native => "native",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HostRouting {
    #[default]
    Bpf,
    Legacy,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EncryptionSpec {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<EncryptionType>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EncryptionType {
    #[default]
    None,
    Wireguard,
    Ipsec,
}

impl EncryptionType {
    pub fn as_str(self) -> &'static str {
        match self {
            EncryptionType::None => "none",
            EncryptionType::Wireguard => "wireguard",
            EncryptionType::Ipsec => "ipsec",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LoadBalancerSpec {
    /// Enable LB-IPAM: allocate VIPs for `type: LoadBalancer` Services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipam: Option<bool>,
    /// CIDRs LB-IPAM allocates from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pools: Option<Vec<String>>,
    /// How allocated VIPs reach the network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub announce: Option<Announce>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bgp: Option<BgpSpec>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Announce {
    #[default]
    None,
    /// Gratuitous ARP / NDP from the elected node.
    L2,
    /// Advertised to peers by the Cilium BGP control plane.
    Bgp,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BgpSpec {
    // camelCase would give `localAsn`; BGP spells it ASN.
    #[serde(default, rename = "localASN", skip_serializing_if = "Option::is_none")]
    pub local_asn: Option<i64>,
    #[serde(default)]
    pub peers: Vec<BgpPeer>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BgpPeer {
    pub address: String,
    pub asn: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EnvoySpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Cluster-operator-style status. Written only by the operator.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkStatus {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,

    /// The mode that is actually installed. Set on the first successful apply
    /// and thereafter the baseline for immutability checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_mode: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_version: Option<String>,

    /// Datapath family (`tunnel`/`native`) of the installed mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_datapath: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_ipam: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_cluster_network: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied_service_network: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

/// Condition types, matching cluster-operator semantics.
pub mod conditions {
    pub const AVAILABLE: &str = "Available";
    pub const PROGRESSING: &str = "Progressing";
    pub const DEGRADED: &str = "Degraded";
}
