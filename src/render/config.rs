//! The `cilium-config` ConfigMap — the agent's and operator's entire runtime
//! configuration. This is where a `Network` mode actually becomes Cilium
//! behaviour, so it is the most load-bearing part of the render.

use k8s_openapi::api::core::v1::ConfigMap;
use std::collections::BTreeMap;

use crate::crd::{Announce, EncryptionType, HostRouting, IpamMode, RoutingMode};
use crate::modes::EffectiveConfig;

use super::{meta, typed, Rendered, AGENT_HEALTH_PORT, CONFIG_MAP};

/// Default VXLAN port. Cilium's own default; the OpenShift analogue is
/// `genevePort`.
const TUNNEL_PORT: &str = "8472";

/// Dual-stack is not supported yet. Kept as a named constant so the keys that
/// depend on it move together the day it is.
const IPV6_ENABLED: bool = false;

pub fn render(cfg: &EffectiveConfig) -> Rendered {
    typed(ConfigMap {
        metadata: meta(cfg, CONFIG_MAP, &[]),
        data: Some(data(cfg)),
        ..Default::default()
    })
}

/// The config keys themselves. Split out so tests can assert on the map without
/// digging through an object.
pub fn data(cfg: &EffectiveConfig) -> BTreeMap<String, String> {
    let mut d: BTreeMap<String, String> = BTreeMap::new();
    let mut set = |k: &str, v: &str| {
        d.insert(k.to_string(), v.to_string());
    };

    // Identity/cluster identity. CRD-backed identities are the only mode that
    // works without an external kvstore, which we deliberately do not run.
    set("identity-allocation-mode", "crd");
    set("cluster-name", &cfg.cluster_name);
    set("cluster-id", &cfg.cluster_id.to_string());

    // --- datapath ---
    set("routing-mode", cfg.routing.as_str());
    match cfg.routing {
        RoutingMode::Tunnel => {
            set("tunnel-protocol", "vxlan");
            set("tunnel-port", TUNNEL_PORT);
        }
        RoutingMode::Native => {
            // Traffic to these prefixes is routed, not masqueraded — without it
            // native routing NATs pod-to-pod traffic and breaks policy identity.
            set("ipv4-native-routing-cidr", &cfg.cluster_network.join(","));
            set("auto-direct-node-routes", bool_str(cfg.auto_direct_node_routes));
        }
    }
    set("enable-ipv4", "true");
    set("enable-ipv6", bool_str(IPV6_ENABLED));
    set("enable-ipv4-masquerade", "true");
    // Follows the IPv6 setting rather than being pinned independently. IPv6 is
    // not supported yet, so this is always false today — but the shape is right
    // for when it is.
    set("enable-ipv6-masquerade", bool_str(IPV6_ENABLED));
    set("enable-bpf-masquerade", "true");
    // bpf host routing is the fast path; `legacy` pushes packets back up through
    // the host stack (the OVN local-gateway analogue).
    set(
        "enable-host-legacy-routing",
        bool_str(cfg.host_routing == HostRouting::Legacy),
    );

    // --- IPAM ---
    set("ipam", cfg.ipam.as_str());
    if cfg.ipam == IpamMode::ClusterPool {
        set("cluster-pool-ipv4-cidr", &cfg.cluster_network.join(","));
        set(
            "cluster-pool-ipv4-mask-size",
            &cfg.cluster_pool_ipv4_mask_size.to_string(),
        );
    }

    // --- services ---
    set("kube-proxy-replacement", bool_str(cfg.kube_proxy_replacement));
    set("k8s-service-host", &cfg.k8s_service_host);
    set("k8s-service-port", &cfg.k8s_service_port.to_string());
    set("bpf-lb-external-clusterip", "false");
    set("enable-session-affinity", "true");
    set("enable-health-check-nodeport", "true");

    // --- MTU ---
    // 0 means "let the agent probe the node", which is what CNO's mtu: 0 does.
    if cfg.mtu != 0 {
        set("mtu", &cfg.mtu.to_string());
    }

    // --- encryption ---
    match cfg.encryption {
        EncryptionType::None => {
            set("enable-wireguard", "false");
            set("enable-ipsec", "false");
        }
        EncryptionType::Wireguard => {
            set("enable-wireguard", "true");
            set("enable-ipsec", "false");
        }
        // Rejected in validation; kept exhaustive so adding it forces a decision.
        EncryptionType::Ipsec => {
            set("enable-wireguard", "false");
            set("enable-ipsec", "true");
        }
    }

    // --- load balancing / announcements ---
    set("enable-lb-ipam", bool_str(cfg.lb_ipam));
    set(
        "enable-l2-announcements",
        bool_str(cfg.announce == Announce::L2),
    );
    set("enable-bgp-control-plane", bool_str(cfg.bgp_enabled()));

    // --- policy / proxy ---
    set("enable-policy", "default");
    set("enable-k8s-networkpolicy", "true");
    set("enable-l7-proxy", "true");
    // When false the proxy stays embedded in the agent; when true the
    // standalone cilium-envoy DaemonSet carries it.
    set("external-envoy-proxy", bool_str(cfg.envoy));
    set("envoy-base-id", "0");
    set("envoy-access-log-buffer-size", "4096");
    set("envoy-keep-cap-netbindservice", "false");

    // --- CNI plumbing ---
    set("cni-exclusive", "true");
    set("cni-log-file", "/var/run/cilium/cilium-cni.log");
    set("custom-cni-conf", "false");
    set(
        "write-cni-conf-when-ready",
        "/host/etc/cni/net.d/05-cilium.conflist",
    );

    // --- health / operations ---
    set("agent-health-port", &AGENT_HEALTH_PORT.to_string());
    set("enable-health-checking", "true");
    set("enable-endpoint-health-checking", "true");
    set("operator-api-serve-addr", "127.0.0.1:9234");
    set("debug", "false");
    set("monitor-aggregation", "medium");
    set("monitor-aggregation-interval", "5s");
    set("monitor-aggregation-flags", "all");
    set("preallocate-bpf-maps", "false");
    set("bpf-map-dynamic-size-ratio", "0.0025");

    d
}

fn bool_str(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::Mode;
    use crate::testutil::cfg_for;

    #[test]
    fn overlay_sets_vxlan_and_no_native_cidr() {
        let d = data(&cfg_for(Mode::Overlay));
        assert_eq!(d["routing-mode"], "tunnel");
        assert_eq!(d["tunnel-protocol"], "vxlan");
        assert_eq!(d["tunnel-port"], TUNNEL_PORT);
        assert!(!d.contains_key("ipv4-native-routing-cidr"));
        assert!(!d.contains_key("auto-direct-node-routes"));
    }

    #[test]
    fn native_sets_the_routing_cidr_and_direct_routes() {
        let d = data(&cfg_for(Mode::Native));
        assert_eq!(d["routing-mode"], "native");
        assert_eq!(d["ipv4-native-routing-cidr"], "10.244.0.0/16");
        assert_eq!(d["auto-direct-node-routes"], "true");
        assert!(!d.contains_key("tunnel-protocol"));
    }

    #[test]
    fn cluster_pool_carries_the_pod_cidr_and_node_mask() {
        let d = data(&cfg_for(Mode::Overlay));
        assert_eq!(d["ipam"], "cluster-pool");
        assert_eq!(d["cluster-pool-ipv4-cidr"], "10.244.0.0/16");
        assert_eq!(d["cluster-pool-ipv4-mask-size"], "24");
    }

    #[test]
    fn kubernetes_ipam_omits_the_cluster_pool_keys() {
        let mut cfg = cfg_for(Mode::Overlay);
        cfg.ipam = IpamMode::Kubernetes;
        let d = data(&cfg);
        assert_eq!(d["ipam"], "kubernetes");
        assert!(!d.contains_key("cluster-pool-ipv4-cidr"));
        assert!(!d.contains_key("cluster-pool-ipv4-mask-size"));
    }

    #[test]
    fn encrypted_mode_turns_on_wireguard_only() {
        let d = data(&cfg_for(Mode::Encrypted));
        assert_eq!(d["enable-wireguard"], "true");
        assert_eq!(d["enable-ipsec"], "false");
    }

    #[test]
    fn announcements_are_mutually_exclusive() {
        let bgp = data(&cfg_for(Mode::Bgp));
        assert_eq!(bgp["enable-bgp-control-plane"], "true");
        assert_eq!(bgp["enable-l2-announcements"], "false");
        assert_eq!(bgp["enable-lb-ipam"], "true");

        let bm = data(&cfg_for(Mode::BareMetal));
        assert_eq!(bm["enable-l2-announcements"], "true");
        assert_eq!(bm["enable-bgp-control-plane"], "false");

        let overlay = data(&cfg_for(Mode::Overlay));
        assert_eq!(overlay["enable-lb-ipam"], "false");
        assert_eq!(overlay["enable-l2-announcements"], "false");
        assert_eq!(overlay["enable-bgp-control-plane"], "false");
    }

    #[test]
    fn mtu_zero_means_auto_and_is_omitted() {
        let mut cfg = cfg_for(Mode::Overlay);
        assert_eq!(cfg.mtu, 0);
        assert!(!data(&cfg).contains_key("mtu"));
        cfg.mtu = 9000;
        assert_eq!(data(&cfg)["mtu"], "9000");
    }

    #[test]
    fn legacy_host_routing_flips_the_bpf_fast_path_off() {
        let mut cfg = cfg_for(Mode::Overlay);
        assert_eq!(data(&cfg)["enable-host-legacy-routing"], "false");
        cfg.host_routing = HostRouting::Legacy;
        assert_eq!(data(&cfg)["enable-host-legacy-routing"], "true");
    }

    #[test]
    fn apiserver_endpoint_is_propagated_for_kube_proxy_replacement() {
        let d = data(&cfg_for(Mode::Overlay));
        assert_eq!(d["kube-proxy-replacement"], "true");
        assert_eq!(d["k8s-service-host"], "192.168.8.98");
        assert_eq!(d["k8s-service-port"], "6443");
    }
}
