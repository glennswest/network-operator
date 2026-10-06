//! The agent's Services: metrics endpoints for scraping and `hubble-peer`, the
//! name a multi-node Hubble relay finds every node's flow server by.
//!
//! All three select the agent's pods (`k8s-app: cilium`) and target its named
//! ports (agent.rs), the way the upstream chart does. The Hubble two exist only
//! while Hubble is on; `cilium-agent` (the embedded proxy's metrics) only while
//! Envoy is not split out, since `cilium-envoy` then has its own Service. Each
//! is reaped when its condition goes away.

use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use std::collections::BTreeMap;

use crate::modes::EffectiveConfig;

use super::{meta, typed, Rendered, ENVOY_METRICS_PORT, HUBBLE_METRICS_PORT, HUBBLE_PEER_PORT};

pub const AGENT_SERVICE: &str = "cilium-agent";
pub const HUBBLE_METRICS: &str = "hubble-metrics";
pub const HUBBLE_PEER: &str = "hubble-peer";

pub fn render(cfg: &EffectiveConfig) -> Vec<Rendered> {
    let mut out = Vec::new();
    if !cfg.envoy {
        out.push(agent_metrics(cfg));
    }
    if cfg.hubble {
        out.extend(hubble(cfg));
    }
    out
}

/// Every Service here, rendered or not. For [`super::reapable`].
pub fn all(cfg: &EffectiveConfig) -> Vec<Rendered> {
    vec![agent_metrics(cfg), hubble_metrics(cfg), hubble_peer(cfg)]
}

fn selector() -> Option<BTreeMap<String, String>> {
    Some(BTreeMap::from([("k8s-app".to_string(), "cilium".to_string())]))
}

fn port(name: &str, port: i32, target: IntOrString) -> ServicePort {
    ServicePort {
        name: Some(name.to_string()),
        port,
        protocol: Some("TCP".to_string()),
        target_port: Some(target),
        ..Default::default()
    }
}

/// Headless: a scraper wants every agent, not one picked by a VIP.
fn headless(cfg: &EffectiveConfig, name: &str, app: &str, p: ServicePort, scrape: i32) -> Rendered {
    let mut m = meta(cfg, name, &[("k8s-app", app)]);
    m.annotations = Some(BTreeMap::from([
        ("prometheus.io/scrape".to_string(), "true".to_string()),
        ("prometheus.io/port".to_string(), scrape.to_string()),
    ]));
    typed(Service {
        metadata: m,
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".to_string()),
            cluster_ip: Some("None".to_string()),
            selector: selector(),
            ports: Some(vec![p]),
            ..Default::default()
        }),
        ..Default::default()
    })
}

fn agent_metrics(cfg: &EffectiveConfig) -> Rendered {
    headless(
        cfg,
        AGENT_SERVICE,
        "cilium",
        port("envoy-metrics", ENVOY_METRICS_PORT, IntOrString::String("envoy-metrics".into())),
        ENVOY_METRICS_PORT,
    )
}

fn hubble_metrics(cfg: &EffectiveConfig) -> Rendered {
    headless(
        cfg,
        HUBBLE_METRICS,
        "hubble",
        port("hubble-metrics", HUBBLE_METRICS_PORT, IntOrString::String("hubble-metrics".into())),
        HUBBLE_METRICS_PORT,
    )
}

/// `internalTrafficPolicy: Local`: a client asking for its peers reaches the
/// agent on its own node, which answers with the full peer list.
fn hubble_peer(cfg: &EffectiveConfig) -> Rendered {
    typed(Service {
        metadata: meta(cfg, HUBBLE_PEER, &[("k8s-app", "cilium")]),
        spec: Some(ServiceSpec {
            selector: selector(),
            // Port 80 because TLS is off; the chart uses 443 when it is on.
            ports: Some(vec![port("peer-service", 80, IntOrString::Int(HUBBLE_PEER_PORT))]),
            internal_traffic_policy: Some("Local".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::Mode;
    use crate::testutil::cfg_for;

    fn ids(cfg: &EffectiveConfig) -> Vec<String> {
        render(cfg).iter().map(|r| r.id()).collect()
    }

    #[test]
    fn hubble_services_follow_the_switch_and_the_metrics_one_does_not() {
        let mut cfg = cfg_for(Mode::Overlay);
        assert_eq!(
            ids(&cfg),
            vec![
                "Service/kube-system/cilium-agent",
                "Service/kube-system/hubble-metrics",
                "Service/kube-system/hubble-peer",
            ]
        );
        cfg.hubble = false;
        assert_eq!(ids(&cfg), vec!["Service/kube-system/cilium-agent"]);
    }

    /// A standalone cilium-envoy binds the proxy-metrics host port itself; the
    /// agent declaring it too would make the two pods unschedulable together.
    #[test]
    fn standalone_envoy_takes_the_proxy_metrics_port_from_the_agent() {
        let mut cfg = cfg_for(Mode::Overlay);
        cfg.envoy = true;
        assert!(!ids(&cfg).contains(&"Service/kube-system/cilium-agent".to_string()));
        assert!(!super::super::config::data(&cfg).contains_key("proxy-prometheus-port"));

        let ds = super::super::agent::render(&cfg);
        let ds: k8s_openapi::api::apps::v1::DaemonSet =
            serde_json::from_value(serde_json::to_value(&ds.obj).unwrap()).unwrap();
        let ports = ds.spec.unwrap().template.spec.unwrap().containers[0].ports.clone().unwrap();
        assert!(!ports.iter().any(|p| p.host_port == Some(ENVOY_METRICS_PORT)));
    }

    /// A named targetPort that no container declares routes nowhere.
    #[test]
    fn every_named_target_port_exists_on_the_agent() {
        let cfg = cfg_for(Mode::Overlay);
        let ds = super::super::agent::render(&cfg);
        let ds: k8s_openapi::api::apps::v1::DaemonSet =
            serde_json::from_value(serde_json::to_value(&ds.obj).unwrap()).unwrap();
        let agent = &ds.spec.unwrap().template.spec.unwrap().containers[0];
        let names: Vec<_> = agent.ports.as_ref().unwrap().iter().filter_map(|p| p.name.clone()).collect();
        let nums: Vec<_> = agent.ports.as_ref().unwrap().iter().map(|p| p.container_port).collect();
        for r in render(&cfg) {
            let svc: Service = serde_json::from_value(serde_json::to_value(&r.obj).unwrap()).unwrap();
            for p in svc.spec.unwrap().ports.unwrap() {
                match p.target_port.unwrap() {
                    IntOrString::String(n) => assert!(names.contains(&n), "{}: {n}", r.id()),
                    IntOrString::Int(n) => assert!(nums.contains(&n), "{}: {n}", r.id()),
                }
            }
        }
    }
}
