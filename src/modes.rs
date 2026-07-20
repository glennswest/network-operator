//! Mode resolution: `NetworkSpec` (sparse, mode-driven) -> [`EffectiveConfig`]
//! (dense, every knob concrete).
//!
//! This is the whole of the operator's policy and it is deliberately pure — no
//! client, no I/O — so the mode table is unit-testable and the render layer
//! below it never has to ask "what did the mode mean?".
//!
//! The rule from README.md: **`mode` sets defaults; any explicitly set
//! `spec.cilium.*` field overrides its mode default.** Modes are presets, not a
//! straitjacket.

use std::fmt;

use ipnet::IpNet;

use crate::crd::{
    Announce, EncryptionType, HostRouting, IpamMode, Mode, Network, NetworkSpec, RoutingMode,
};

/// Cilium version installed when the CR does not pin one.
pub const DEFAULT_CILIUM_VERSION: &str = "1.19.6";
/// Image registry prefix used when the CR does not override it.
pub const DEFAULT_REGISTRY: &str = "quay.io/cilium";
/// `cilium-envoy` tag paired with the Cilium 1.19 series.
///
/// Envoy is versioned independently of Cilium and the tag encodes an Envoy
/// version, a build number and a commit sha — none of it derivable from
/// `spec.cilium.version`. Taken from a known-good 1.19.6 install. Bump this
/// alongside [`DEFAULT_CILIUM_VERSION`], or override per-cluster with
/// `spec.cilium.envoy.image`.
pub const DEFAULT_ENVOY_TAG: &str =
    "v1.36.9-1782267392-edeb3f2af56c37c407efa1f63f0b32f595399bbc";
/// Namespace every rendered object lands in.
pub const NAMESPACE: &str = "kube-system";

/// A fully resolved cluster network. Every field the render layer needs, with
/// no `Option` left to interpret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveConfig {
    /// Name of the owning `Network` object (for owner references and labels).
    pub network_name: String,
    /// `metadata.uid` of the owning `Network`, when reconciling a live object.
    pub network_uid: Option<String>,

    pub mode: Mode,
    pub cluster_network: Vec<String>,
    pub service_network: Vec<String>,

    pub version: String,
    pub registry: String,

    pub ipam: IpamMode,
    pub cluster_pool_ipv4_mask_size: u8,
    pub routing: RoutingMode,
    /// Native routing only: program per-node routes for directly-reachable nodes.
    pub auto_direct_node_routes: bool,
    /// 0 means auto-detect.
    pub mtu: i32,
    pub kube_proxy_replacement: bool,
    pub host_routing: HostRouting,
    pub encryption: EncryptionType,

    pub k8s_service_host: String,
    pub k8s_service_port: u16,

    pub lb_ipam: bool,
    pub lb_pools: Vec<String>,
    pub announce: Announce,
    pub bgp_local_asn: i64,
    pub bgp_peers: Vec<(String, i64)>,

    pub envoy: bool,
    pub envoy_image: String,

    pub cluster_name: String,
    pub cluster_id: u32,
}

impl EffectiveConfig {
    /// The agent/operator image tag family — `v1.19.6`, Cilium's own convention.
    pub fn image_tag(&self) -> String {
        if self.version.starts_with('v') {
            self.version.clone()
        } else {
            format!("v{}", self.version)
        }
    }

    pub fn agent_image(&self) -> String {
        format!("{}/cilium:{}", self.registry, self.image_tag())
    }

    pub fn operator_image(&self) -> String {
        // The generic operator — no cloud IPAM, which is what cluster-pool and
        // kubernetes IPAM both use.
        format!("{}/operator-generic:{}", self.registry, self.image_tag())
    }

    /// Whether any BGP object should be rendered.
    pub fn bgp_enabled(&self) -> bool {
        self.announce == Announce::Bgp
    }
}

/// Why a `Network` spec could not be resolved into a config we would install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl ValidationError {
    fn new(field: &str, message: impl Into<String>) -> Self {
        Self { field: field.into(), message: message.into() }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ValidationError {}

/// Resolve a live `Network` object, carrying its name/uid through for owner refs.
pub fn resolve_network(net: &Network) -> Result<EffectiveConfig, ValidationError> {
    let mut cfg = resolve(&net.spec)?;
    cfg.network_name = net
        .metadata
        .name
        .clone()
        .unwrap_or_else(|| crate::crd::NETWORK_NAME.to_string());
    cfg.network_uid = net.metadata.uid.clone();
    Ok(cfg)
}

/// Resolve a bare spec. Applies the mode defaults, then every explicit override,
/// then validates the result as a whole.
pub fn resolve(spec: &NetworkSpec) -> Result<EffectiveConfig, ValidationError> {
    let d = defaults_for(spec.mode);
    let c = spec.cilium.as_ref();

    let ipam = c
        .and_then(|c| c.ipam.as_ref())
        .and_then(|i| i.mode)
        .unwrap_or(d.ipam);
    let routing = c
        .and_then(|c| c.routing.as_ref())
        .and_then(|r| r.mode)
        .unwrap_or(d.routing);
    let announce = c
        .and_then(|c| c.load_balancer.as_ref())
        .and_then(|lb| lb.announce)
        .unwrap_or(d.announce);

    let bgp = c.and_then(|c| c.load_balancer.as_ref()).and_then(|lb| lb.bgp.as_ref());

    let cfg = EffectiveConfig {
        network_name: crate::crd::NETWORK_NAME.to_string(),
        network_uid: None,

        mode: spec.mode,
        cluster_network: spec.cluster_network.clone(),
        service_network: spec.service_network.clone(),

        version: c
            .and_then(|c| c.version.clone())
            .unwrap_or_else(|| DEFAULT_CILIUM_VERSION.to_string()),
        registry: c
            .and_then(|c| c.registry.clone())
            .unwrap_or_else(|| DEFAULT_REGISTRY.to_string()),

        ipam,
        cluster_pool_ipv4_mask_size: c
            .and_then(|c| c.ipam.as_ref())
            .and_then(|i| i.cluster_pool_ipv4_mask_size)
            .unwrap_or(d.cluster_pool_ipv4_mask_size),
        routing,
        // Direct node routes only mean anything without an overlay, and only
        // help when the nodes actually share a segment — which is what every
        // native-routing mode in the table assumes.
        auto_direct_node_routes: routing == RoutingMode::Native,
        mtu: c.and_then(|c| c.mtu).unwrap_or(d.mtu),
        kube_proxy_replacement: c
            .and_then(|c| c.kube_proxy_replacement)
            .unwrap_or(d.kube_proxy_replacement),
        host_routing: c.and_then(|c| c.host_routing).unwrap_or(d.host_routing),
        encryption: c
            .and_then(|c| c.encryption.as_ref())
            .and_then(|e| e.kind)
            .unwrap_or(d.encryption),

        k8s_service_host: c
            .and_then(|c| c.k8s_service_host.clone())
            .unwrap_or_default(),
        k8s_service_port: c.and_then(|c| c.k8s_service_port).unwrap_or(6443),

        lb_ipam: c
            .and_then(|c| c.load_balancer.as_ref())
            .and_then(|lb| lb.ipam)
            .unwrap_or(d.lb_ipam),
        lb_pools: c
            .and_then(|c| c.load_balancer.as_ref())
            .and_then(|lb| lb.pools.clone())
            .unwrap_or_default(),
        announce,
        bgp_local_asn: bgp.and_then(|b| b.local_asn).unwrap_or(0),
        bgp_peers: bgp
            .map(|b| b.peers.iter().map(|p| (p.address.clone(), p.asn)).collect())
            .unwrap_or_default(),

        envoy: c
            .and_then(|c| c.envoy.as_ref())
            .and_then(|e| e.enabled)
            .unwrap_or(false),
        envoy_image: c
            .and_then(|c| c.envoy.as_ref())
            .and_then(|e| e.image.clone())
            .unwrap_or_else(|| {
                let registry = c
                    .and_then(|c| c.registry.clone())
                    .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());
                format!("{registry}/cilium-envoy:{DEFAULT_ENVOY_TAG}")
            }),

        cluster_name: c
            .and_then(|c| c.cluster_name.clone())
            .unwrap_or_else(|| "default".to_string()),
        cluster_id: c.and_then(|c| c.cluster_id).unwrap_or(0),
    };

    validate(&cfg)?;
    Ok(cfg)
}

/// The mode table from README.md, as data.
struct ModeDefaults {
    ipam: IpamMode,
    cluster_pool_ipv4_mask_size: u8,
    routing: RoutingMode,
    mtu: i32,
    kube_proxy_replacement: bool,
    host_routing: HostRouting,
    encryption: EncryptionType,
    lb_ipam: bool,
    announce: Announce,
}

impl ModeDefaults {
    /// The shared baseline every mode starts from; each mode then changes only
    /// the cells where its row differs.
    fn base() -> Self {
        Self {
            ipam: IpamMode::ClusterPool,
            cluster_pool_ipv4_mask_size: 24,
            routing: RoutingMode::Tunnel,
            mtu: 0,
            kube_proxy_replacement: true,
            host_routing: HostRouting::Bpf,
            encryption: EncryptionType::None,
            lb_ipam: false,
            announce: Announce::None,
        }
    }
}

fn defaults_for(mode: Mode) -> ModeDefaults {
    let mut d = ModeDefaults::base();
    match mode {
        Mode::Overlay => {}
        Mode::Native => {
            d.routing = RoutingMode::Native;
        }
        Mode::Bgp => {
            d.routing = RoutingMode::Native;
            d.lb_ipam = true;
            d.announce = Announce::Bgp;
        }
        Mode::Encrypted => {
            d.encryption = EncryptionType::Wireguard;
        }
        Mode::BareMetal => {
            d.lb_ipam = true;
            d.announce = Announce::L2;
        }
    }
    d
}

fn validate(cfg: &EffectiveConfig) -> Result<(), ValidationError> {
    if cfg.cluster_network.is_empty() {
        return Err(ValidationError::new(
            "spec.clusterNetwork",
            "at least one pod CIDR is required",
        ));
    }
    for cidr in &cfg.cluster_network {
        parse_cidr("spec.clusterNetwork", cidr)?;
    }
    if cfg.service_network.is_empty() {
        return Err(ValidationError::new(
            "spec.serviceNetwork",
            "at least one service CIDR is required",
        ));
    }
    for cidr in &cfg.service_network {
        parse_cidr("spec.serviceNetwork", cidr)?;
    }

    // cluster-pool carves per-node /N blocks out of the pod CIDR; a mask that is
    // not strictly longer than the pool leaves room for exactly zero nodes.
    if cfg.ipam == IpamMode::ClusterPool {
        if !(1..=32).contains(&cfg.cluster_pool_ipv4_mask_size) {
            return Err(ValidationError::new(
                "spec.cilium.ipam.clusterPoolIPv4MaskSize",
                "must be between 1 and 32",
            ));
        }
        for cidr in &cfg.cluster_network {
            let net = parse_cidr("spec.clusterNetwork", cidr)?;
            if net.prefix_len() >= cfg.cluster_pool_ipv4_mask_size {
                return Err(ValidationError::new(
                    "spec.cilium.ipam.clusterPoolIPv4MaskSize",
                    format!(
                        "must be longer than the clusterNetwork prefix /{} to carve per-node blocks (got /{})",
                        net.prefix_len(),
                        cfg.cluster_pool_ipv4_mask_size
                    ),
                ));
            }
        }
    }

    if cfg.k8s_service_host.is_empty() {
        return Err(ValidationError::new(
            "spec.cilium.k8sServiceHost",
            "required — the agent must reach the apiserver before Services exist",
        ));
    }
    if cfg.k8s_service_port == 0 {
        return Err(ValidationError::new(
            "spec.cilium.k8sServicePort",
            "must be non-zero",
        ));
    }

    if cfg.version.trim().is_empty() {
        return Err(ValidationError::new("spec.cilium.version", "must not be empty"));
    }

    if cfg.mtu != 0 && !(576..=9216).contains(&cfg.mtu) {
        return Err(ValidationError::new(
            "spec.cilium.mtu",
            "must be 0 (auto) or between 576 and 9216",
        ));
    }

    if cfg.lb_ipam {
        if cfg.lb_pools.is_empty() {
            return Err(ValidationError::new(
                "spec.cilium.loadBalancer.pools",
                "at least one pool CIDR is required when LB-IPAM is enabled",
            ));
        }
        for cidr in &cfg.lb_pools {
            parse_cidr("spec.cilium.loadBalancer.pools", cidr)?;
        }
    }
    if cfg.announce != Announce::None && !cfg.lb_ipam {
        return Err(ValidationError::new(
            "spec.cilium.loadBalancer.announce",
            "announcing requires loadBalancer.ipam to be enabled",
        ));
    }

    if cfg.bgp_enabled() {
        // Cilium's BGP control plane needs native routing to advertise pod CIDRs
        // that the fabric can actually route to; behind an overlay the advertised
        // prefixes are unreachable.
        if cfg.routing != RoutingMode::Native {
            return Err(ValidationError::new(
                "spec.cilium.routing.mode",
                "BGP announcements require native routing",
            ));
        }
        if !(1..=4_294_967_295).contains(&cfg.bgp_local_asn) {
            return Err(ValidationError::new(
                "spec.cilium.loadBalancer.bgp.localASN",
                "must be a valid ASN (1-4294967295)",
            ));
        }
        if cfg.bgp_peers.is_empty() {
            return Err(ValidationError::new(
                "spec.cilium.loadBalancer.bgp.peers",
                "at least one peer is required for BGP announcements",
            ));
        }
        for (addr, asn) in &cfg.bgp_peers {
            if addr.parse::<std::net::IpAddr>().is_err() {
                return Err(ValidationError::new(
                    "spec.cilium.loadBalancer.bgp.peers",
                    format!("peer address {addr:?} is not an IP address"),
                ));
            }
            if !(1..=4_294_967_295).contains(asn) {
                return Err(ValidationError::new(
                    "spec.cilium.loadBalancer.bgp.peers",
                    format!("peer {addr} has an invalid ASN {asn}"),
                ));
            }
        }
    }

    // IPsec needs a pre-shared keyfile Secret we do not manage yet; failing here
    // beats installing an agent that crash-loops on a missing key.
    if cfg.encryption == EncryptionType::Ipsec {
        return Err(ValidationError::new(
            "spec.cilium.encryption.type",
            "ipsec is not supported yet — use wireguard",
        ));
    }

    // Cluster identity: ClusterMesh requires a distinct id per cluster, and
    // Cilium caps it at 255.
    if cfg.cluster_name.trim().is_empty() {
        return Err(ValidationError::new(
            "spec.cilium.clusterName",
            "must not be empty",
        ));
    }
    if cfg.cluster_id > 255 {
        return Err(ValidationError::new(
            "spec.cilium.clusterID",
            "must be between 0 and 255",
        ));
    }

    Ok(())
}

fn parse_cidr(field: &str, cidr: &str) -> Result<IpNet, ValidationError> {
    cidr.parse::<IpNet>()
        .map_err(|e| ValidationError::new(field, format!("{cidr:?} is not a valid CIDR: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{
        BgpPeer, BgpSpec, CiliumSpec, EncryptionSpec, IpamSpec, LoadBalancerSpec, RoutingSpec,
    };

    fn spec(mode: Mode) -> NetworkSpec {
        NetworkSpec {
            mode,
            cluster_network: vec!["10.244.0.0/16".into()],
            service_network: vec!["10.96.0.0/12".into()],
            cilium: Some(CiliumSpec {
                k8s_service_host: Some("192.168.8.98".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn overlay_is_the_documented_default() {
        let cfg = resolve(&spec(Mode::Overlay)).unwrap();
        assert_eq!(cfg.routing, RoutingMode::Tunnel);
        assert_eq!(cfg.ipam, IpamMode::ClusterPool);
        assert!(cfg.kube_proxy_replacement);
        assert_eq!(cfg.encryption, EncryptionType::None);
        assert!(!cfg.lb_ipam);
        assert_eq!(cfg.announce, Announce::None);
        assert!(!cfg.auto_direct_node_routes);
        assert_eq!(cfg.version, DEFAULT_CILIUM_VERSION);
    }

    #[test]
    fn every_mode_matches_the_readme_table() {
        let native = resolve(&spec(Mode::Native)).unwrap();
        assert_eq!(native.routing, RoutingMode::Native);
        assert!(native.auto_direct_node_routes);
        assert!(!native.lb_ipam);

        let encrypted = resolve(&spec(Mode::Encrypted)).unwrap();
        assert_eq!(encrypted.encryption, EncryptionType::Wireguard);
        assert_eq!(encrypted.routing, RoutingMode::Tunnel);

        let mut bm = spec(Mode::BareMetal);
        bm.cilium.as_mut().unwrap().load_balancer = Some(LoadBalancerSpec {
            pools: Some(vec!["192.168.8.240/28".into()]),
            ..Default::default()
        });
        let bm = resolve(&bm).unwrap();
        assert!(bm.lb_ipam);
        assert_eq!(bm.announce, Announce::L2);
        assert_eq!(bm.routing, RoutingMode::Tunnel);
    }

    #[test]
    fn bgp_mode_defaults_to_native_lb_ipam_and_bgp_announce() {
        let mut s = spec(Mode::Bgp);
        s.cilium.as_mut().unwrap().load_balancer = Some(LoadBalancerSpec {
            pools: Some(vec!["192.168.8.240/28".into()]),
            bgp: Some(BgpSpec {
                local_asn: Some(64512),
                peers: vec![BgpPeer { address: "192.168.8.1".into(), asn: 64512 }],
            }),
            ..Default::default()
        });
        let cfg = resolve(&s).unwrap();
        assert_eq!(cfg.routing, RoutingMode::Native);
        assert!(cfg.lb_ipam);
        assert_eq!(cfg.announce, Announce::Bgp);
        assert_eq!(cfg.bgp_local_asn, 64512);
        assert_eq!(cfg.bgp_peers, vec![("192.168.8.1".to_string(), 64512)]);
    }

    #[test]
    fn explicit_fields_override_mode_defaults() {
        let mut s = spec(Mode::Overlay);
        let c = s.cilium.as_mut().unwrap();
        c.routing = Some(RoutingSpec { mode: Some(RoutingMode::Native) });
        c.kube_proxy_replacement = Some(false);
        c.encryption = Some(EncryptionSpec { kind: Some(EncryptionType::Wireguard) });
        c.ipam = Some(IpamSpec {
            mode: Some(IpamMode::Kubernetes),
            cluster_pool_ipv4_mask_size: None,
        });
        c.version = Some("1.18.0".into());

        let cfg = resolve(&s).unwrap();
        assert_eq!(cfg.mode, Mode::Overlay);
        assert_eq!(cfg.routing, RoutingMode::Native);
        assert!(!cfg.kube_proxy_replacement);
        assert_eq!(cfg.encryption, EncryptionType::Wireguard);
        assert_eq!(cfg.ipam, IpamMode::Kubernetes);
        assert_eq!(cfg.version, "1.18.0");
    }

    #[test]
    fn image_tag_is_normalized_to_cilium_convention() {
        let cfg = resolve(&spec(Mode::Overlay)).unwrap();
        assert_eq!(cfg.agent_image(), "quay.io/cilium/cilium:v1.19.6");
        assert_eq!(cfg.operator_image(), "quay.io/cilium/operator-generic:v1.19.6");

        let mut s = spec(Mode::Overlay);
        s.cilium.as_mut().unwrap().version = Some("v1.18.1".into());
        s.cilium.as_mut().unwrap().registry = Some("mirror.local/cilium".into());
        let cfg = resolve(&s).unwrap();
        assert_eq!(cfg.agent_image(), "mirror.local/cilium/cilium:v1.18.1");
    }

    fn err_field(s: &NetworkSpec) -> String {
        resolve(s).unwrap_err().field
    }

    #[test]
    fn rejects_missing_or_malformed_networks() {
        let mut s = spec(Mode::Overlay);
        s.cluster_network.clear();
        assert_eq!(err_field(&s), "spec.clusterNetwork");

        let mut s = spec(Mode::Overlay);
        s.cluster_network = vec!["not-a-cidr".into()];
        assert_eq!(err_field(&s), "spec.clusterNetwork");

        let mut s = spec(Mode::Overlay);
        s.service_network.clear();
        assert_eq!(err_field(&s), "spec.serviceNetwork");
    }

    #[test]
    fn rejects_apiserver_endpoint_omission() {
        let mut s = spec(Mode::Overlay);
        s.cilium.as_mut().unwrap().k8s_service_host = None;
        assert_eq!(err_field(&s), "spec.cilium.k8sServiceHost");
    }

    #[test]
    fn rejects_node_mask_that_cannot_carve_the_pod_cidr() {
        let mut s = spec(Mode::Overlay);
        s.cilium.as_mut().unwrap().ipam = Some(IpamSpec {
            mode: None,
            cluster_pool_ipv4_mask_size: Some(16),
        });
        assert_eq!(err_field(&s), "spec.cilium.ipam.clusterPoolIPv4MaskSize");
    }

    #[test]
    fn rejects_announcing_without_lb_ipam_and_bgp_over_tunnel() {
        let mut s = spec(Mode::Overlay);
        s.cilium.as_mut().unwrap().load_balancer = Some(LoadBalancerSpec {
            ipam: Some(false),
            announce: Some(Announce::L2),
            ..Default::default()
        });
        assert_eq!(err_field(&s), "spec.cilium.loadBalancer.announce");

        // bgp announce forced on top of the overlay mode's tunnel datapath
        let mut s = spec(Mode::Overlay);
        s.cilium.as_mut().unwrap().load_balancer = Some(LoadBalancerSpec {
            ipam: Some(true),
            pools: Some(vec!["192.168.8.240/28".into()]),
            announce: Some(Announce::Bgp),
            bgp: Some(BgpSpec {
                local_asn: Some(64512),
                peers: vec![BgpPeer { address: "192.168.8.1".into(), asn: 64512 }],
            }),
        });
        assert_eq!(err_field(&s), "spec.cilium.routing.mode");
    }

    #[test]
    fn rejects_bgp_without_peers_and_lb_ipam_without_pools() {
        let mut s = spec(Mode::Bgp);
        s.cilium.as_mut().unwrap().load_balancer = Some(LoadBalancerSpec {
            pools: Some(vec!["192.168.8.240/28".into()]),
            bgp: Some(BgpSpec { local_asn: Some(64512), peers: vec![] }),
            ..Default::default()
        });
        assert_eq!(err_field(&s), "spec.cilium.loadBalancer.bgp.peers");

        // bgp mode turns LB-IPAM on by default, so omitting pools must fail
        let s = spec(Mode::Bgp);
        assert_eq!(err_field(&s), "spec.cilium.loadBalancer.pools");
    }

    #[test]
    fn rejects_unimplemented_features_rather_than_installing_something_broken() {
        let mut s = spec(Mode::Overlay);
        s.cilium.as_mut().unwrap().encryption =
            Some(EncryptionSpec { kind: Some(EncryptionType::Ipsec) });
        assert_eq!(err_field(&s), "spec.cilium.encryption.type");
    }

    #[test]
    fn envoy_is_off_by_default_and_versioned_independently_of_cilium() {
        let cfg = resolve(&spec(Mode::Overlay)).unwrap();
        assert!(!cfg.envoy);

        let mut s = spec(Mode::Overlay);
        s.cilium.as_mut().unwrap().envoy = Some(crate::crd::EnvoySpec {
            enabled: Some(true),
            image: None,
        });
        let cfg = resolve(&s).unwrap();
        assert!(cfg.envoy);
        // The envoy tag is its own thing — deriving it from spec.cilium.version
        // would produce a tag that does not exist.
        assert_eq!(
            cfg.envoy_image,
            format!("{DEFAULT_REGISTRY}/cilium-envoy:{DEFAULT_ENVOY_TAG}")
        );
        assert!(!cfg.envoy_image.contains(&cfg.version));

        // A mirrored registry carries the envoy image too.
        let mut s2 = s.clone();
        s2.cilium.as_mut().unwrap().registry = Some("mirror.local/cilium".into());
        assert!(resolve(&s2).unwrap().envoy_image.starts_with("mirror.local/cilium/cilium-envoy:"));

        // ...and a full override wins outright.
        s.cilium.as_mut().unwrap().envoy = Some(crate::crd::EnvoySpec {
            enabled: Some(true),
            image: Some("registry.internal/envoy:pinned".into()),
        });
        assert_eq!(resolve(&s).unwrap().envoy_image, "registry.internal/envoy:pinned");
    }

    #[test]
    fn cluster_identity_defaults_and_is_bounded() {
        let cfg = resolve(&spec(Mode::Overlay)).unwrap();
        assert_eq!(cfg.cluster_name, "default");
        assert_eq!(cfg.cluster_id, 0);

        let mut s = spec(Mode::Overlay);
        s.cilium.as_mut().unwrap().cluster_name = Some("g8".into());
        s.cilium.as_mut().unwrap().cluster_id = Some(7);
        let cfg = resolve(&s).unwrap();
        assert_eq!(cfg.cluster_name, "g8");
        assert_eq!(cfg.cluster_id, 7);

        // ClusterMesh caps the id at 255.
        let mut s = spec(Mode::Overlay);
        s.cilium.as_mut().unwrap().cluster_id = Some(256);
        assert_eq!(err_field(&s), "spec.cilium.clusterID");

        let mut s = spec(Mode::Overlay);
        s.cilium.as_mut().unwrap().cluster_name = Some("  ".into());
        assert_eq!(err_field(&s), "spec.cilium.clusterName");
    }
}
