//! The `cilium-config` ConfigMap — the agent's and operator's entire runtime
//! configuration. This is where a `Network` mode actually becomes Cilium
//! behaviour, so it is the most load-bearing part of the render.

use k8s_openapi::api::core::v1::ConfigMap;
use std::collections::BTreeMap;

use crate::crd::{Announce, EncryptionType, HostRouting, IpamMode, RoutingMode};
use crate::modes::EffectiveConfig;

use super::{
    meta, typed, Rendered, AGENT_HEALTH_PORT, AGENT_PROMETHEUS_PORT, CONFIG_MAP,
    ENVOY_METRICS_PORT, HUBBLE_METRICS_PORT, HUBBLE_PEER_PORT, OPERATOR_PROMETHEUS_PORT,
    SECRETS_NAMESPACE,
};

/// Default VXLAN port. Cilium's own default; the OpenShift analogue is
/// `genevePort`.
const TUNNEL_PORT: &str = "8472";


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
            // One key per family (#45).
            if cfg.ipv4() {
                set("ipv4-native-routing-cidr", &cfg.pod_cidrs(false).join(","));
            }
            if cfg.ipv6() {
                set("ipv6-native-routing-cidr", &cfg.pod_cidrs(true).join(","));
            }
            set("auto-direct-node-routes", bool_str(cfg.auto_direct_node_routes));
        }
    }
    // The families are the ones clusterNetwork has (#45): IPv4, IPv6, or both.
    // Masquerade follows each family, so pods reach off-cluster addresses
    // whichever they are using.
    set("enable-ipv4", bool_str(cfg.ipv4()));
    set("enable-ipv6", bool_str(cfg.ipv6()));
    set("enable-ipv4-masquerade", bool_str(cfg.ipv4()));
    set("enable-ipv6-masquerade", bool_str(cfg.ipv6()));
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
        if cfg.ipv4() {
            set("cluster-pool-ipv4-cidr", &cfg.pod_cidrs(false).join(","));
            set(
                "cluster-pool-ipv4-mask-size",
                &cfg.cluster_pool_ipv4_mask_size.to_string(),
            );
        }
        if cfg.ipv6() {
            set("cluster-pool-ipv6-cidr", &cfg.pod_cidrs(true).join(","));
            set(
                "cluster-pool-ipv6-mask-size",
                &cfg.cluster_pool_ipv6_mask_size.to_string(),
            );
        }
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
        // The keyfile is the Secret src/ipsec.rs keeps, mounted by the agent
        // (render/agent.rs). The key watcher reloads it on rotation and keeps
        // the old key for the rotation window so in-flight packets still
        // decrypt; values are the Cilium chart's defaults (#44).
        EncryptionType::Ipsec => {
            set("enable-wireguard", "false");
            set("enable-ipsec", "true");
            set("ipsec-key-file", crate::ipsec::KEY_FILE);
            set("enable-ipsec-key-watcher", "true");
            set("ipsec-key-rotation-duration", crate::ipsec::ROTATION_DURATION);
            set("enable-ipsec-encrypted-overlay", "false");
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
    // The L7 proxy reads TLS secrets for policy (TLS interception, SNI) only
    // from this namespace, which rbac.rs creates and grants. Always on because
    // the L7 proxy is; the values are the 1.20 chart's.
    set("enable-policy-secrets-sync", "true");
    set("policy-secrets-only-from-secrets-namespace", "true");
    set("policy-secrets-namespace", SECRETS_NAMESPACE);
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

    // --- metrics ---
    // The ports are named on the pods and fronted by Services (services.rs).
    set("enable-metrics", "true");
    set("prometheus-serve-addr", &format!(":{AGENT_PROMETHEUS_PORT}"));
    set("operator-prometheus-serve-addr", &format!(":{OPERATOR_PROMETHEUS_PORT}"));
    // The embedded proxy's metrics; a standalone cilium-envoy serves its own
    // on the same port (envoy.rs), so the agent must not also bind it.
    if !cfg.envoy {
        set("proxy-prometheus-port", &ENVOY_METRICS_PORT.to_string());
    }

    // --- Hubble ---
    set("enable-hubble", bool_str(cfg.hubble));
    if cfg.hubble {
        // The relay dials this socket over a hostPath (hubble.rs); the TCP
        // listener is for the hubble-peer Service. TLS is off end to end, as
        // in stormcos: a flow API locked down harder than the apiserver it
        // describes protects nothing.
        set("hubble-socket-path", "/var/run/cilium/hubble.sock");
        set("hubble-listen-address", &format!(":{HUBBLE_PEER_PORT}"));
        set("hubble-disable-tls", "true");
        set("hubble-metrics-server", &format!(":{HUBBLE_METRICS_PORT}"));
        set("hubble-metrics-server-enable-tls", "false");
        set("hubble-metrics", "drop dns tcp flow");
        set("hubble-network-policy-correlation-enabled", "true");
    }

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
    fn ipsec_turns_on_ipsec_with_the_mounted_keyfile() {
        let mut cfg = cfg_for(Mode::Overlay);
        cfg.encryption = EncryptionType::Ipsec;
        let d = data(&cfg);
        assert_eq!(d["enable-ipsec"], "true");
        assert_eq!(d["enable-wireguard"], "false");
        assert_eq!(d["ipsec-key-file"], "/etc/ipsec/keys");
        assert_eq!(d["enable-ipsec-key-watcher"], "true");
        assert_eq!(d["ipsec-key-rotation-duration"], "5m");
        assert_eq!(d["enable-ipsec-encrypted-overlay"], "false");

        // And none of it leaks into the other encryption types.
        for mode in [Mode::Overlay, Mode::Encrypted] {
            assert!(!data(&cfg_for(mode)).keys().any(|k| k.starts_with("ipsec-")));
        }
    }

    fn dual_stack(mode: Mode) -> EffectiveConfig {
        let mut cfg = cfg_for(mode);
        cfg.cluster_network.push("fd00:10:244::/104".into());
        cfg.service_network.push("fd00:10:96::/112".into());
        cfg
    }

    #[test]
    fn ipv4_only_is_what_it_always_was() {
        let d = data(&cfg_for(Mode::Native));
        assert_eq!(d["enable-ipv4"], "true");
        assert_eq!(d["enable-ipv6"], "false");
        assert_eq!(d["enable-ipv4-masquerade"], "true");
        assert_eq!(d["enable-ipv6-masquerade"], "false");
        assert!(!d.keys().any(|k| k.starts_with("ipv6-") || k.starts_with("cluster-pool-ipv6")));
    }

    #[test]
    fn dual_stack_splits_every_cidr_key_by_family() {
        let d = data(&dual_stack(Mode::Native));
        assert_eq!(d["enable-ipv4"], "true");
        assert_eq!(d["enable-ipv6"], "true");
        assert_eq!(d["enable-ipv4-masquerade"], "true");
        assert_eq!(d["enable-ipv6-masquerade"], "true");
        assert_eq!(d["cluster-pool-ipv4-cidr"], "10.244.0.0/16");
        assert_eq!(d["cluster-pool-ipv4-mask-size"], "24");
        assert_eq!(d["cluster-pool-ipv6-cidr"], "fd00:10:244::/104");
        assert_eq!(d["cluster-pool-ipv6-mask-size"], "120");
        assert_eq!(d["ipv4-native-routing-cidr"], "10.244.0.0/16");
        assert_eq!(d["ipv6-native-routing-cidr"], "fd00:10:244::/104");

        // Over a tunnel there is no native-routing CIDR of either family.
        let t = data(&dual_stack(Mode::Overlay));
        assert!(!t.keys().any(|k| k.ends_with("native-routing-cidr")));
    }

    #[test]
    fn ipv6_only_turns_ipv4_off() {
        let mut cfg = cfg_for(Mode::Native);
        cfg.cluster_network = vec!["fd00:10:244::/104".into()];
        cfg.service_network = vec!["fd00:10:96::/112".into()];
        let d = data(&cfg);
        assert_eq!(d["enable-ipv4"], "false");
        assert_eq!(d["enable-ipv4-masquerade"], "false");
        assert_eq!(d["enable-ipv6"], "true");
        assert!(!d.contains_key("cluster-pool-ipv4-cidr"));
        assert!(!d.contains_key("ipv4-native-routing-cidr"));
        assert_eq!(d["cluster-pool-ipv6-cidr"], "fd00:10:244::/104");
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
    fn hubble_keys_follow_the_switch() {
        let mut cfg = cfg_for(Mode::Overlay);
        let on = data(&cfg);
        assert_eq!(on["enable-hubble"], "true");
        assert_eq!(on["hubble-socket-path"], "/var/run/cilium/hubble.sock");
        assert_eq!(on["hubble-listen-address"], ":4244");
        cfg.hubble = false;
        let off = data(&cfg);
        assert_eq!(off["enable-hubble"], "false");
        assert!(!off.keys().any(|k| k.starts_with("hubble-")));
    }

    #[test]
    fn policy_secrets_live_in_the_namespace_rbac_grants() {
        let d = data(&cfg_for(Mode::Overlay));
        assert_eq!(d["enable-l7-proxy"], "true");
        assert_eq!(d["policy-secrets-namespace"], SECRETS_NAMESPACE);
        assert_eq!(d["policy-secrets-only-from-secrets-namespace"], "true");
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
