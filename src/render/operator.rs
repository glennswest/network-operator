//! The `cilium-operator` Deployment — Cilium's own control plane (CRD
//! installation, cluster-pool IPAM allocation, identity/endpoint GC).
//!
//! Note the layering: *this* operator installs *that* one. `cilium-operator`
//! manages Cilium's data; `network-operator` manages Cilium's lifecycle.

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    Affinity, Container, PodAntiAffinity, PodSpec, PodTemplateSpec, Probe,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::api::core::v1::PodAffinityTerm;
use std::collections::BTreeMap;

use crate::modes::EffectiveConfig;

use super::util::*;
use super::{common_labels, meta, typed, Rendered, CONFIG_MAP, OPERATOR_DEPLOY, OPERATOR_SA};

const APP_LABEL: (&str, &str) = ("io.cilium/app", "operator");
const NAME_LABEL: (&str, &str) = ("name", "cilium-operator");

/// The operator's health server, on localhost because the pod is host-networked.
const HEALTH_PORT: i32 = 9234;

/// Single replica by design. The operator is not in the dataplane path — the
/// agents keep forwarding while it is down — and it is host-networked, so a
/// second replica cannot share a node without colliding on the health port.
/// Scaling up is safe (leader election + the anti-affinity below), but it buys
/// less than it costs on the cluster sizes this stack targets.
const REPLICAS: i32 = 1;

pub fn render(cfg: &EffectiveConfig) -> Rendered {
    let mut pod_labels = common_labels(cfg);
    pod_labels.insert(APP_LABEL.0.to_string(), APP_LABEL.1.to_string());
    pod_labels.insert(NAME_LABEL.0.to_string(), NAME_LABEL.1.to_string());

    typed(Deployment {
        metadata: meta(cfg, OPERATOR_DEPLOY, &[APP_LABEL, NAME_LABEL]),
        spec: Some(DeploymentSpec {
            replicas: Some(REPLICAS),
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([
                    (APP_LABEL.0.to_string(), APP_LABEL.1.to_string()),
                    (NAME_LABEL.0.to_string(), NAME_LABEL.1.to_string()),
                ])),
                ..Default::default()
            },
            strategy: Some(DeploymentStrategy {
                // Recreate, not RollingUpdate: two host-networked operators on
                // one node would fight over the health port during a rollout.
                type_: Some("Recreate".to_string()),
                ..Default::default()
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
    })
}

fn pod_spec(cfg: &EffectiveConfig) -> PodSpec {
    PodSpec {
        service_account_name: Some(OPERATOR_SA.to_string()),
        // Host network for the same reason as the agent: with kube-proxy
        // replacement there is no route to the apiserver Service until the
        // agents are running.
        host_network: Some(true),
        dns_policy: Some("ClusterFirstWithHostNet".to_string()),
        priority_class_name: Some("system-cluster-critical".to_string()),
        restart_policy: Some("Always".to_string()),
        tolerations: Some(tolerate_all()),
        affinity: Some(Affinity {
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
            ..Default::default()
        }),
        containers: vec![Container {
            name: "cilium-operator".to_string(),
            image: Some(cfg.operator_image()),
            image_pull_policy: Some("IfNotPresent".to_string()),
            command: Some(vec!["cilium-operator-generic".to_string()]),
            args: Some(vec![
                "--config-dir=/tmp/cilium/config-map".to_string(),
                "--debug=$(CILIUM_DEBUG)".to_string(),
            ]),
            env: Some(vec![
                env_field("K8S_NODE_NAME", "spec.nodeName"),
                env_field("CILIUM_K8S_NAMESPACE", "metadata.namespace"),
                env("KUBERNETES_SERVICE_HOST", &cfg.k8s_service_host),
                env("KUBERNETES_SERVICE_PORT", &cfg.k8s_service_port.to_string()),
                env_config("CILIUM_DEBUG", "debug", CONFIG_MAP),
            ]),
            liveness_probe: Some(Probe {
                initial_delay_seconds: Some(60),
                period_seconds: Some(10),
                timeout_seconds: Some(3),
                failure_threshold: Some(3),
                ..health_probe(HEALTH_PORT, "/healthz")
            }),
            readiness_probe: Some(Probe {
                initial_delay_seconds: Some(0),
                period_seconds: Some(5),
                timeout_seconds: Some(3),
                failure_threshold: Some(5),
                ..health_probe(HEALTH_PORT, "/healthz")
            }),
            volume_mounts: Some(vec![mount_ro("cilium-config-path", "/tmp/cilium/config-map")]),
            termination_message_policy: Some("FallbackToLogsOnError".to_string()),
            ..Default::default()
        }],
        volumes: Some(vec![config_map_volume("cilium-config-path", CONFIG_MAP)]),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::Mode;
    use crate::testutil::cfg_for;

    fn deploy(cfg: &EffectiveConfig) -> Deployment {
        serde_json::from_value(serde_json::to_value(render(cfg).obj).unwrap()).unwrap()
    }

    #[test]
    fn selector_matches_the_pod_template_labels() {
        let spec = deploy(&cfg_for(Mode::Overlay)).spec.unwrap();
        let selector = spec.selector.match_labels.unwrap();
        let labels = spec.template.metadata.unwrap().labels.unwrap();
        for (k, v) in &selector {
            assert_eq!(labels.get(k), Some(v), "pod template must match selector on {k}");
        }
    }

    #[test]
    fn host_networked_operator_never_rolls_two_pods_onto_one_node() {
        let spec = deploy(&cfg_for(Mode::Overlay)).spec.unwrap();
        assert_eq!(spec.replicas, Some(1));
        assert_eq!(spec.strategy.unwrap().type_.as_deref(), Some("Recreate"));

        let pod = spec.template.spec.unwrap();
        assert_eq!(pod.host_network, Some(true));
        let term = &pod
            .affinity
            .unwrap()
            .pod_anti_affinity
            .unwrap()
            .required_during_scheduling_ignored_during_execution
            .unwrap()[0];
        assert_eq!(term.topology_key, "kubernetes.io/hostname");
    }

    #[test]
    fn runs_the_generic_operator_because_ipam_is_never_cloud() {
        let d = deploy(&cfg_for(Mode::Overlay));
        let c = &d.spec.unwrap().template.spec.unwrap().containers[0];
        assert_eq!(c.image.as_deref(), Some("quay.io/cilium/operator-generic:v1.19.6"));
        assert_eq!(c.command.as_deref(), Some(&["cilium-operator-generic".to_string()][..]));
    }

    #[test]
    fn reads_the_same_config_map_as_the_agent() {
        let d = deploy(&cfg_for(Mode::Overlay));
        let pod = d.spec.unwrap().template.spec.unwrap();
        let vol = &pod.volumes.unwrap()[0];
        assert_eq!(vol.config_map.as_ref().unwrap().name, CONFIG_MAP);
        let m = &pod.containers[0].volume_mounts.as_ref().unwrap()[0];
        assert_eq!(m.mount_path, "/tmp/cilium/config-map");
        assert_eq!(m.read_only, Some(true));
    }
}
