//! The standalone `cilium-envoy` DaemonSet — Cilium's L7 proxy, split out of
//! the agent.
//!
//! Off by default: Cilium embeds the proxy in the agent unless you split it
//! out, and the embedded proxy serves L7 policy perfectly well. Splitting it
//! out decouples proxy restarts from agent restarts, which matters once L7
//! policy or Ingress is carrying real traffic.
//!
//! The bootstrap config in `assets/envoy-bootstrap.json` is taken **verbatim
//! from a known-good running install** rather than being generated. It is
//! cluster-agnostic — the only CIDRs in it are Envoy's stock RFC1918
//! internal-address ranges, not the cluster's pod or service networks — so it
//! needs no templating. Generating it from scratch would be inventing an Envoy
//! xDS bootstrap, which is a much worse idea than copying one that works.
//!
//! Envoy talks to the agent over a unix socket, not the network: the agent
//! serves xDS on `/var/run/cilium/envoy/sockets/xds.sock`. That is why enabling
//! this also adds a mount to the agent DaemonSet (see `agent.rs`).

use k8s_openapi::api::apps::v1::{
    DaemonSet, DaemonSetSpec, DaemonSetUpdateStrategy, RollingUpdateDaemonSet,
};
use k8s_openapi::api::core::v1::{
    Affinity, ConfigMap, Container, ContainerPort, KeyToPath, NodeAffinity, NodeSelector,
    NodeSelectorRequirement, NodeSelectorTerm, PodAffinity, PodAffinityTerm, PodAntiAffinity,
    PodSpec, PodTemplateSpec, Probe, SELinuxOptions, SecurityContext, Service, ServicePort,
    ServiceSpec, ServiceAccount, Volume, VolumeMount,
};
use k8s_openapi::api::core::v1::{Capabilities, ConfigMapVolumeSource};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use std::collections::BTreeMap;

use crate::modes::EffectiveConfig;

use super::util::*;
use super::{common_labels, meta, typed, Rendered};

pub const ENVOY_DS: &str = "cilium-envoy";
pub const ENVOY_SA: &str = "cilium-envoy";
pub const ENVOY_CONFIG_MAP: &str = "cilium-envoy-config";

/// Health endpoint, on loopback because the pod is host-networked.
const HEALTH_PORT: i32 = 9878;
/// Prometheus listener. A host port, again because of host networking.
const METRICS_PORT: i32 = 9964;

/// Directory the agent and Envoy share unix sockets through. The agent serves
/// xDS here; Envoy dials it. Mounted by both.
pub const SOCKET_DIR: &str = "/var/run/cilium/envoy/sockets";

const APP_LABEL: (&str, &str) = ("k8s-app", "cilium-envoy");
const NAME_LABEL: (&str, &str) = ("name", "cilium-envoy");

/// The bootstrap Envoy reads at startup. Verbatim from a working install.
const BOOTSTRAP: &str = include_str!("assets/envoy-bootstrap.json");

pub fn render(cfg: &EffectiveConfig) -> Vec<Rendered> {
    if !cfg.envoy {
        return Vec::new();
    }
    vec![
        typed(service_account(cfg)),
        typed(config_map(cfg)),
        typed(daemon_set(cfg)),
        typed(service(cfg)),
    ]
}

/// Envoy needs no API access — it only talks to the agent over a unix socket —
/// so this ServiceAccount deliberately has no ClusterRole bound to it.
fn service_account(cfg: &EffectiveConfig) -> ServiceAccount {
    ServiceAccount { metadata: meta(cfg, ENVOY_SA, &[]), ..Default::default() }
}

fn config_map(cfg: &EffectiveConfig) -> ConfigMap {
    ConfigMap {
        metadata: meta(cfg, ENVOY_CONFIG_MAP, &[]),
        data: Some(BTreeMap::from([(
            "bootstrap-config.json".to_string(),
            BOOTSTRAP.to_string(),
        )])),
        ..Default::default()
    }
}

fn daemon_set(cfg: &EffectiveConfig) -> DaemonSet {
    let mut pod_labels = common_labels(cfg);
    pod_labels.insert(APP_LABEL.0.to_string(), APP_LABEL.1.to_string());
    pod_labels.insert(NAME_LABEL.0.to_string(), NAME_LABEL.1.to_string());

    DaemonSet {
        metadata: meta(cfg, ENVOY_DS, &[APP_LABEL, NAME_LABEL]),
        spec: Some(DaemonSetSpec {
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    APP_LABEL.0.to_string(),
                    APP_LABEL.1.to_string(),
                )])),
                ..Default::default()
            },
            update_strategy: Some(DaemonSetUpdateStrategy {
                type_: Some("RollingUpdate".to_string()),
                rolling_update: Some(RollingUpdateDaemonSet {
                    max_unavailable: Some(IntOrString::Int(2)),
                    ..Default::default()
                }),
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(pod_labels),
                    ..Default::default()
                }),
                spec: Some(pod_spec(cfg)),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn pod_spec(cfg: &EffectiveConfig) -> PodSpec {
    PodSpec {
        service_account_name: Some(ENVOY_SA.to_string()),
        host_network: Some(true),
        priority_class_name: Some("system-node-critical".to_string()),
        restart_policy: Some("Always".to_string()),
        termination_grace_period_seconds: Some(1),
        node_selector: Some(BTreeMap::from([(
            "kubernetes.io/os".to_string(),
            "linux".to_string(),
        )])),
        tolerations: Some(tolerate_all()),
        security_context: Some(unconfined_pod_security(true)),
        affinity: Some(affinity()),
        containers: vec![container(cfg)],
        volumes: Some(volumes()),
        ..Default::default()
    }
}

/// Three constraints, all load-bearing:
///
/// * **node affinity** — respect `cilium.io/no-schedule`, the same opt-out the
///   agent honours.
/// * **pod affinity on `k8s-app=cilium`** — Envoy is useless on a node with no
///   agent, since it reaches xDS through the agent's socket.
/// * **pod anti-affinity on itself** — host-networked, so two Envoys on one
///   node would collide on the metrics host port.
fn affinity() -> Affinity {
    Affinity {
        node_affinity: Some(NodeAffinity {
            required_during_scheduling_ignored_during_execution: Some(NodeSelector {
                node_selector_terms: vec![NodeSelectorTerm {
                    match_expressions: Some(vec![NodeSelectorRequirement {
                        key: "cilium.io/no-schedule".to_string(),
                        operator: "NotIn".to_string(),
                        values: Some(vec!["true".to_string()]),
                    }]),
                    ..Default::default()
                }],
            }),
            ..Default::default()
        }),
        pod_affinity: Some(PodAffinity {
            required_during_scheduling_ignored_during_execution: Some(vec![PodAffinityTerm {
                label_selector: Some(LabelSelector {
                    match_labels: Some(BTreeMap::from([(
                        "k8s-app".to_string(),
                        "cilium".to_string(),
                    )])),
                    ..Default::default()
                }),
                topology_key: "kubernetes.io/hostname".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        pod_anti_affinity: Some(PodAntiAffinity {
            required_during_scheduling_ignored_during_execution: Some(vec![PodAffinityTerm {
                label_selector: Some(LabelSelector {
                    match_labels: Some(BTreeMap::from([(
                        APP_LABEL.0.to_string(),
                        APP_LABEL.1.to_string(),
                    )])),
                    ..Default::default()
                }),
                topology_key: "kubernetes.io/hostname".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }),
    }
}

fn container(cfg: &EffectiveConfig) -> Container {
    Container {
        name: "cilium-envoy".to_string(),
        image: Some(cfg.envoy_image.clone()),
        image_pull_policy: Some("IfNotPresent".to_string()),
        command: Some(vec!["/usr/bin/cilium-envoy-starter".to_string()]),
        // The starter passes everything after `--` through to Envoy itself.
        args: Some(vec![
            "--".to_string(),
            "-c /var/run/cilium/envoy/bootstrap-config.json".to_string(),
            "--base-id 0".to_string(),
            "--log-level info".to_string(),
        ]),
        env: Some(vec![
            env_field("K8S_NODE_NAME", "spec.nodeName"),
            env_field("CILIUM_K8S_NAMESPACE", "metadata.namespace"),
            env("KUBERNETES_SERVICE_HOST", &cfg.k8s_service_host),
            env("KUBERNETES_SERVICE_PORT", &cfg.k8s_service_port.to_string()),
        ]),
        ports: Some(vec![ContainerPort {
            name: Some("envoy-metrics".to_string()),
            container_port: METRICS_PORT,
            host_port: Some(METRICS_PORT),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }]),
        // Not fully privileged, unlike the agent: Envoy needs to program
        // transparent sockets and enter namespaces, nothing more.
        security_context: Some(SecurityContext {
            capabilities: Some(Capabilities {
                add: Some(vec!["NET_ADMIN".to_string(), "SYS_ADMIN".to_string()]),
                drop: Some(vec!["ALL".to_string()]),
            }),
            se_linux_options: Some(SELinuxOptions {
                level: Some("s0".to_string()),
                type_: Some("spc_t".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        // Envoy comes up fast but must wait for the agent's xDS socket to
        // exist, so the startup budget is generous (105 x 2s).
        startup_probe: Some(Probe {
            failure_threshold: Some(105),
            period_seconds: Some(2),
            initial_delay_seconds: Some(5),
            success_threshold: Some(1),
            ..health_probe(HEALTH_PORT, "/healthz")
        }),
        liveness_probe: Some(Probe {
            period_seconds: Some(30),
            timeout_seconds: Some(5),
            failure_threshold: Some(10),
            success_threshold: Some(1),
            ..health_probe(HEALTH_PORT, "/healthz")
        }),
        readiness_probe: Some(Probe {
            period_seconds: Some(30),
            timeout_seconds: Some(5),
            failure_threshold: Some(3),
            success_threshold: Some(1),
            ..health_probe(HEALTH_PORT, "/healthz")
        }),
        termination_message_policy: Some("FallbackToLogsOnError".to_string()),
        volume_mounts: Some(vec![
            mount("envoy-sockets", SOCKET_DIR),
            mount("envoy-artifacts", "/var/run/cilium/envoy/artifacts"),
            mount("envoy-config", "/var/run/cilium/envoy/"),
            mount_bidirectional("bpf-maps", "/sys/fs/bpf"),
        ]),
        ..Default::default()
    }
}

fn volumes() -> Vec<Volume> {
    vec![
        host_path("envoy-sockets", SOCKET_DIR, "DirectoryOrCreate"),
        host_path(
            "envoy-artifacts",
            "/var/run/cilium/envoy/artifacts",
            "DirectoryOrCreate",
        ),
        Volume {
            name: "envoy-config".to_string(),
            config_map: Some(ConfigMapVolumeSource {
                name: ENVOY_CONFIG_MAP.to_string(),
                // 0400 — the bootstrap is read once, by Envoy, at startup.
                default_mode: Some(256),
                items: Some(vec![KeyToPath {
                    key: "bootstrap-config.json".to_string(),
                    path: "bootstrap-config.json".to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        },
        host_path("bpf-maps", "/sys/fs/bpf", "DirectoryOrCreate"),
    ]
}

/// Headless service, purely so Prometheus can discover the metrics port.
fn service(cfg: &EffectiveConfig) -> Service {
    Service {
        metadata: meta(cfg, ENVOY_DS, &[APP_LABEL, NAME_LABEL]),
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".to_string()),
            cluster_ip: Some("None".to_string()),
            selector: Some(BTreeMap::from([(
                APP_LABEL.0.to_string(),
                APP_LABEL.1.to_string(),
            )])),
            ports: Some(vec![ServicePort {
                name: Some("envoy-metrics".to_string()),
                port: METRICS_PORT,
                target_port: Some(IntOrString::Int(METRICS_PORT)),
                protocol: Some("TCP".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The socket-directory mount the agent needs so it can serve xDS to a
/// standalone Envoy. Only added to the agent when Envoy is split out.
pub fn agent_socket_volume() -> Volume {
    host_path("envoy-sockets", SOCKET_DIR, "DirectoryOrCreate")
}

pub fn agent_socket_mount() -> VolumeMount {
    mount("envoy-sockets", SOCKET_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::Mode;
    use crate::testutil::cfg_for;

    fn enabled() -> EffectiveConfig {
        let mut cfg = cfg_for(Mode::Overlay);
        cfg.envoy = true;
        cfg
    }

    #[test]
    fn renders_nothing_unless_enabled() {
        assert!(render(&cfg_for(Mode::Overlay)).is_empty());
    }

    #[test]
    fn renders_the_full_object_set_when_enabled() {
        let ids: Vec<_> = render(&enabled()).iter().map(|r| r.id()).collect();
        assert_eq!(
            ids,
            vec![
                "ServiceAccount/kube-system/cilium-envoy",
                "ConfigMap/kube-system/cilium-envoy-config",
                "DaemonSet/kube-system/cilium-envoy",
                "Service/kube-system/cilium-envoy",
            ]
        );
    }

    #[test]
    fn bootstrap_asset_is_valid_json_with_the_xds_socket_and_both_listeners() {
        let v: serde_json::Value = serde_json::from_str(BOOTSTRAP).expect("bootstrap must be JSON");

        // xDS is reached over the agent's unix socket, not the network.
        let clusters = v["staticResources"]["clusters"].as_array().unwrap();
        let xds = clusters
            .iter()
            .find(|c| c["name"] == "xds-grpc-cilium")
            .expect("xds cluster");
        assert_eq!(
            xds["loadAssignment"]["endpoints"][0]["lbEndpoints"][0]["endpoint"]["address"]["pipe"]
                ["path"],
            format!("{SOCKET_DIR}/xds.sock")
        );

        // The health listener is what our probes hit; the metrics listener is
        // what the Service exposes. Both ports must line up with the DaemonSet.
        let listeners = v["staticResources"]["listeners"].as_array().unwrap();
        let ports: Vec<i64> = listeners
            .iter()
            .map(|l| l["address"]["socketAddress"]["portValue"].as_i64().unwrap())
            .collect();
        assert!(ports.contains(&(HEALTH_PORT as i64)), "health port missing: {ports:?}");
        assert!(ports.contains(&(METRICS_PORT as i64)), "metrics port missing: {ports:?}");
    }

    #[test]
    fn bootstrap_carries_no_cluster_specific_values() {
        // If this ever fails, the asset has been replaced with one captured
        // from a cluster whose addressing leaked in, and it needs templating.
        for needle in ["192.168.8.", "10.244.", "10.96.", ".g8.lo"] {
            assert!(
                !BOOTSTRAP.contains(needle),
                "bootstrap contains cluster-specific value {needle:?}"
            );
        }
    }

    #[test]
    fn envoy_image_is_independent_of_the_cilium_version() {
        let mut cfg = enabled();
        cfg.version = "1.20.0".into();
        let ds = daemon_set(&cfg);
        let image = ds.spec.unwrap().template.spec.unwrap().containers[0]
            .image
            .clone()
            .unwrap();
        // Bumping Cilium must not invent an envoy tag that does not exist.
        assert!(!image.contains("1.20.0"), "envoy image tracked the cilium version: {image}");
        assert!(image.starts_with("quay.io/cilium/cilium-envoy:v"));
    }

    #[test]
    fn image_can_be_overridden_wholesale() {
        let mut cfg = enabled();
        cfg.envoy_image = "mirror.local/cilium-envoy:v1.36.9-custom".into();
        let ds = daemon_set(&cfg);
        assert_eq!(
            ds.spec.unwrap().template.spec.unwrap().containers[0].image.as_deref(),
            Some("mirror.local/cilium-envoy:v1.36.9-custom")
        );
    }

    #[test]
    fn scheduled_only_where_an_agent_is_and_never_twice_per_node() {
        let ds = daemon_set(&enabled());
        let a = ds.spec.unwrap().template.spec.unwrap().affinity.unwrap();

        let with_agent = &a.pod_affinity.unwrap().required_during_scheduling_ignored_during_execution.unwrap()[0];
        assert_eq!(
            with_agent.label_selector.as_ref().unwrap().match_labels.as_ref().unwrap()["k8s-app"],
            "cilium"
        );

        let not_itself = &a.pod_anti_affinity.unwrap().required_during_scheduling_ignored_during_execution.unwrap()[0];
        assert_eq!(not_itself.topology_key, "kubernetes.io/hostname");

        let no_schedule = &a.node_affinity.unwrap().required_during_scheduling_ignored_during_execution.unwrap()
            .node_selector_terms[0].match_expressions.clone().unwrap()[0];
        assert_eq!(no_schedule.key, "cilium.io/no-schedule");
        assert_eq!(no_schedule.operator, "NotIn");
    }

    /// #5: envoy needs AppArmor unconfined, but — unlike the agent — the
    /// known-good install leaves seccomp at the default.
    #[test]
    fn pod_is_apparmor_unconfined_but_keeps_the_default_seccomp() {
        let ds = daemon_set(&enabled());
        let sc = ds
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .security_context
            .expect("pod-level securityContext is required");
        assert_eq!(sc.app_armor_profile.unwrap().type_, "Unconfined");
        assert!(sc.seccomp_profile.is_none(), "envoy does not unconfine seccomp");
    }

    #[test]
    fn every_mount_resolves_to_a_declared_volume() {
        let ds = daemon_set(&enabled());
        let pod = ds.spec.unwrap().template.spec.unwrap();
        let declared: Vec<_> = pod.volumes.unwrap().into_iter().map(|v| v.name).collect();
        for m in pod.containers[0].volume_mounts.iter().flatten() {
            assert!(declared.contains(&m.name), "undeclared volume {}", m.name);
        }
    }

    #[test]
    fn probes_and_metrics_ports_agree_with_the_service() {
        let ds = daemon_set(&enabled());
        let c = &ds.spec.unwrap().template.spec.unwrap().containers[0];
        assert_eq!(
            c.liveness_probe.as_ref().unwrap().http_get.as_ref().unwrap().port,
            IntOrString::Int(HEALTH_PORT)
        );
        assert_eq!(c.ports.as_ref().unwrap()[0].container_port, METRICS_PORT);

        let svc = service(&enabled());
        let port = &svc.spec.unwrap().ports.unwrap()[0];
        assert_eq!(port.port, METRICS_PORT);
        assert_eq!(port.target_port, Some(IntOrString::Int(METRICS_PORT)));
    }
}
