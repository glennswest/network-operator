//! LoadBalancer support: LB-IPAM pools plus the announcement mechanism that
//! makes the allocated VIPs reachable — L2/ARP or BGP.
//!
//! These are Cilium's *own* CRs, so their CRDs only exist once `cilium-operator`
//! has installed them. The reconciler applies this group last and treats a
//! missing CRD as "not yet", not as a failure.

use serde_json::json;

use crate::crd::Announce;
use crate::modes::EffectiveConfig;

use super::{cluster_meta, custom, Rendered};

/// Cilium promoted the LB-IPAM and BGP v2 CRs from `v2alpha1` to `v2` in 1.17.
/// Anything older still has to be addressed at the alpha group version.
const V2_SINCE_MINOR: u32 = 17;

const POOL_NAME: &str = "storm-default";
const BGP_CLUSTER_CONFIG: &str = "storm";
const BGP_PEER_CONFIG: &str = "storm-peers";
const BGP_ADVERTISEMENT: &str = "storm-advertisements";
/// Ties `CiliumBGPPeerConfig` to `CiliumBGPAdvertisement` by label.
const ADVERTISE_LABEL: (&str, &str) = ("advertise", "storm");

pub fn render(cfg: &EffectiveConfig) -> Vec<Rendered> {
    let mut out = Vec::new();
    if !cfg.lb_ipam {
        return out;
    }
    let v = api_version(cfg);

    out.push(custom(
        "cilium.io",
        v,
        "CiliumLoadBalancerIPPool",
        "ciliumloadbalancerippools",
        cluster_meta(cfg, POOL_NAME, &[]),
        json!({
            "blocks": cfg.lb_pools.iter().map(|c| json!({ "cidr": c })).collect::<Vec<_>>(),
        }),
    ));

    match cfg.announce {
        Announce::None => {}
        Announce::L2 => out.push(l2_policy(cfg)),
        Announce::Bgp => out.extend(bgp(cfg, v)),
    }
    out
}

/// Every LB/BGP object *any* config would render, whether or not this one does.
/// Only the kind and name are meaningful — the caller uses these to find and
/// delete objects a previous config left behind.
pub fn all_variants(cfg: &EffectiveConfig) -> Vec<Rendered> {
    let mut cfg = cfg.clone();
    cfg.lb_ipam = true;
    cfg.lb_pools = vec!["0.0.0.0/32".to_string()];

    let mut out = Vec::new();
    for announce in [Announce::L2, Announce::Bgp] {
        cfg.announce = announce;
        out.extend(render(&cfg));
    }
    out.sort_by_key(|r| r.id());
    out.dedup_by_key(|r| r.id());
    out
}

/// The `cilium.io` group version to address these CRs at, for the Cilium
/// version being installed.
fn api_version(cfg: &EffectiveConfig) -> &'static str {
    if minor(&cfg.version).is_some_and(|m| m < V2_SINCE_MINOR) {
        "v2alpha1"
    } else {
        "v2"
    }
}

/// Minor version out of `1.19.6` / `v1.19.6`. `None` for anything unparseable,
/// which resolves to the modern group version.
fn minor(version: &str) -> Option<u32> {
    version
        .trim_start_matches('v')
        .split('.')
        .nth(1)?
        .parse()
        .ok()
}

/// L2 announcements are still an alpha API — they were not part of the v2
/// promotion that took the LB pool and BGP CRs, so this one ignores
/// [`api_version`] and pins itself.
fn l2_policy(cfg: &EffectiveConfig) -> Rendered {
    custom(
        "cilium.io",
        "v2alpha1",
        "CiliumL2AnnouncementPolicy",
        "ciliuml2announcementpolicies",
        cluster_meta(cfg, POOL_NAME, &[]),
        json!({
            // Empty selectors mean every node and every LoadBalancer Service.
            "nodeSelector": { "matchLabels": {} },
            "serviceSelector": { "matchLabels": {} },
            "externalIPs": true,
            "loadBalancerIPs": true,
        }),
    )
}

/// One address family per IP family that has something to advertise: the pod
/// CIDRs, or LoadBalancer pools, of that family (#45). An IPv4-only cluster is
/// exactly the single ipv4 family it always was.
fn bgp_families(cfg: &EffectiveConfig) -> Vec<serde_json::Value> {
    [("ipv4", false), ("ipv6", true)]
        .into_iter()
        .filter(|(_, v6)| !cfg.pod_cidrs(*v6).is_empty() || !cfg.lb_pool_cidrs(*v6).is_empty())
        .map(|(afi, _)| {
            json!({
                "afi": afi,
                "safi": "unicast",
                "advertisements": { "matchLabels": { ADVERTISE_LABEL.0: ADVERTISE_LABEL.1 } },
            })
        })
        .collect()
}

fn bgp(cfg: &EffectiveConfig, v: &str) -> Vec<Rendered> {
    let instance = format!("asn{}", cfg.bgp_local_asn);
    let peers: Vec<_> = cfg
        .bgp_peers
        .iter()
        .map(|(address, asn)| {
            json!({
                "name": format!("peer-{}", address.replace(['.', ':'], "-")),
                "peerAddress": address,
                "peerASN": asn,
                "peerConfigRef": { "name": BGP_PEER_CONFIG },
            })
        })
        .collect();

    vec![
        custom(
            "cilium.io",
            v,
            "CiliumBGPClusterConfig",
            "ciliumbgpclusterconfigs",
            cluster_meta(cfg, BGP_CLUSTER_CONFIG, &[]),
            json!({
                // No nodeSelector: every node peers, which is what gives the
                // fabric ECMP paths to the pod CIDRs.
                "bgpInstances": [{
                    "name": instance,
                    "localASN": cfg.bgp_local_asn,
                    "peers": peers,
                }],
            }),
        ),
        custom(
            "cilium.io",
            v,
            "CiliumBGPPeerConfig",
            "ciliumbgppeerconfigs",
            cluster_meta(cfg, BGP_PEER_CONFIG, &[]),
            json!({ "families": bgp_families(cfg) }),
        ),
        custom(
            "cilium.io",
            v,
            "CiliumBGPAdvertisement",
            "ciliumbgpadvertisements",
            cluster_meta(cfg, BGP_ADVERTISEMENT, &[ADVERTISE_LABEL]),
            json!({
                "advertisements": [
                    { "advertisementType": "PodCIDR" },
                    {
                        "advertisementType": "Service",
                        "service": { "addresses": ["LoadBalancerIP"] },
                        "selector": { "matchLabels": {} },
                    },
                ],
            }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::Mode;
    use crate::testutil::cfg_for;

    fn spec_of(r: &Rendered) -> serde_json::Value {
        r.obj.data["spec"].clone()
    }

    /// The rendered object of `kind`. (A helper, not `r.api.kind == "…"`
    /// inline: gitleaks' generic-api-key rule reads that as a key, #55.)
    fn by_kind<'a>(objs: &'a [Rendered], kind: &str) -> &'a Rendered {
        objs.iter()
            .find(|r| r.api.kind == kind)
            .unwrap_or_else(|| panic!("no {kind} rendered"))
    }

    #[test]
    fn nothing_is_rendered_without_lb_ipam() {
        assert!(render(&cfg_for(Mode::Overlay)).is_empty());
        assert!(render(&cfg_for(Mode::Native)).is_empty());
        assert!(render(&cfg_for(Mode::Encrypted)).is_empty());
    }

    #[test]
    fn pool_blocks_come_from_the_configured_cidrs() {
        let mut cfg = cfg_for(Mode::BareMetal);
        cfg.lb_pools = vec!["192.168.8.240/28".into(), "10.0.0.0/24".into()];
        let objs = render(&cfg);
        let pool = by_kind(&objs, "CiliumLoadBalancerIPPool");
        assert_eq!(
            spec_of(pool)["blocks"],
            serde_json::json!([{ "cidr": "192.168.8.240/28" }, { "cidr": "10.0.0.0/24" }])
        );
    }

    #[test]
    fn bgp_renders_a_peer_per_configured_neighbour() {
        let mut cfg = cfg_for(Mode::Bgp);
        cfg.bgp_peers = vec![("192.168.8.1".into(), 64512), ("192.168.8.2".into(), 64513)];
        let objs = render(&cfg);
        let cluster = by_kind(&objs, "CiliumBGPClusterConfig");
        let instances = spec_of(cluster)["bgpInstances"].clone();
        assert_eq!(instances[0]["localASN"], 64512);

        let peers = instances[0]["peers"].as_array().unwrap().clone();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0]["peerAddress"], "192.168.8.1");
        assert_eq!(peers[0]["peerASN"], 64512);
        // Names must be DNS-safe, so dots cannot survive into the peer name.
        assert_eq!(peers[0]["name"], "peer-192-168-8-1");
        assert_eq!(peers[1]["peerASN"], 64513);
    }

    #[test]
    fn bgp_families_follow_what_there_is_to_advertise() {
        let afis = |cfg: &EffectiveConfig| -> Vec<String> {
            let objs = render(cfg);
            let peer_cfg = by_kind(&objs, "CiliumBGPPeerConfig");
            spec_of(peer_cfg)["families"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["afi"].as_str().unwrap().to_string())
                .collect()
        };
        // IPv4-only: exactly the one family it always had.
        let mut cfg = cfg_for(Mode::Bgp);
        assert_eq!(afis(&cfg), ["ipv4"]);

        // Dual-stack pods: both.
        cfg.cluster_network.push("fd00:10:244::/104".into());
        assert_eq!(afis(&cfg), ["ipv4", "ipv6"]);

        // IPv6-only pods, but an IPv4 LoadBalancer pool still needs ipv4.
        cfg.cluster_network = vec!["fd00:10:244::/104".into()];
        assert_eq!(afis(&cfg), ["ipv4", "ipv6"]);
        cfg.lb_pools = vec!["fd00:8::/120".into()];
        assert_eq!(afis(&cfg), ["ipv6"]);
    }

    #[test]
    fn peer_config_and_advertisement_are_linked_by_label() {
        let objs = render(&cfg_for(Mode::Bgp));
        let peer_cfg = by_kind(&objs, "CiliumBGPPeerConfig");
        let selector = spec_of(peer_cfg)["families"][0]["advertisements"]["matchLabels"].clone();

        let adv = by_kind(&objs, "CiliumBGPAdvertisement");
        let labels = adv.obj.metadata.labels.as_ref().unwrap();
        assert_eq!(
            selector[ADVERTISE_LABEL.0],
            serde_json::json!(labels[ADVERTISE_LABEL.0])
        );

        // Every peer points at the peer config we actually rendered.
        let cluster = by_kind(&objs, "CiliumBGPClusterConfig");
        let peers = spec_of(cluster)["bgpInstances"][0]["peers"].clone();
        assert_eq!(peers[0]["peerConfigRef"]["name"], peer_cfg.name());
    }

    #[test]
    fn bgp_advertises_both_pod_cidrs_and_lb_vips() {
        let objs = render(&cfg_for(Mode::Bgp));
        let adv = by_kind(&objs, "CiliumBGPAdvertisement");
        let kinds: Vec<_> = spec_of(adv)["advertisements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["advertisementType"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, vec!["PodCIDR", "Service"]);
    }

    #[test]
    fn group_version_tracks_the_cilium_release() {
        let mut cfg = cfg_for(Mode::Bgp);
        assert_eq!(api_version(&cfg), "v2");

        cfg.version = "1.16.5".into();
        assert_eq!(api_version(&cfg), "v2alpha1");
        assert!(render(&cfg).iter().all(|r| r.api.version == "v2alpha1"));

        cfg.version = "v1.17.0".into();
        assert_eq!(api_version(&cfg), "v2");
    }

    #[test]
    fn l2_policy_stays_on_the_alpha_group_even_on_new_cilium() {
        let cfg = cfg_for(Mode::BareMetal);
        assert_eq!(api_version(&cfg), "v2");
        let objs = render(&cfg);
        let policy = by_kind(&objs, "CiliumL2AnnouncementPolicy");
        assert_eq!(policy.api.version, "v2alpha1");
    }
}
