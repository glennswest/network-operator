//! `hubble-relay` — the flow API (gRPC, :4245) in front of the agents' Hubble
//! servers. Rendered only while `spec.cilium.hubble.enabled` is true, and
//! reaped when it is turned off.
//!
//! The shape is stormcos's, not the chart's: host-networked, tolerating the
//! boot taints, and dialling the agent's own unix socket over a hostPath. That
//! needs no CNI, no TLS, no peer Service and no DNS, so the relay comes up with
//! the node instead of after it. The cost is that it sees the flows of the
//! node it lands on; aggregating a multi-node cluster means pointing
//! `peer-service` at `hubble-peer` instead, which is not wired yet.

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{ConfigMap, Container, PodSpec, PodTemplateSpec, Toleration};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use std::collections::BTreeMap;

use crate::modes::EffectiveConfig;

use super::util::*;
use super::{common_labels, meta, typed, Rendered, HUBBLE_RELAY, HUBBLE_RELAY_CONFIG, HUBBLE_RELAY_PORT};

/// Pod selector label. Immutable on a Deployment, so it is a constant.
const APP_LABEL: (&str, &str) = ("k8s-app", "hubble-relay");

pub fn render(cfg: &EffectiveConfig) -> Vec<Rendered> {
    if cfg.hubble {
        all(cfg)
    } else {
        Vec::new()
    }
}

/// The relay's objects, rendered or not. For [`super::reapable`].
pub fn all(cfg: &EffectiveConfig) -> Vec<Rendered> {
    vec![typed(config(cfg)), typed(deployment(cfg))]
}

fn config(cfg: &EffectiveConfig) -> ConfigMap {
    let yaml = format!(
        "cluster-name: {}\npeer-service: \"unix:///var/run/cilium/hubble.sock\"\nlisten-address: :{HUBBLE_RELAY_PORT}\ndisable-server-tls: true\ndisable-client-tls: true\n",
        cfg.cluster_name
    );
    ConfigMap {
        metadata: meta(cfg, HUBBLE_RELAY_CONFIG, &[]),
        data: Some(BTreeMap::from([("config.yaml".to_string(), yaml)])),
        ..Default::default()
    }
}

/// The operator's set: up while the node still carries its boot taints, since
/// it is host-networked and dials a socket, so it needs neither CNI nor Ready.
fn tolerations() -> Vec<Toleration> {
    [
        "node-role.kubernetes.io/control-plane",
        "node-role.kubernetes.io/master",
        "node.kubernetes.io/not-ready",
        "node.cilium.io/agent-not-ready",
    ]
    .iter()
    .map(|k| Toleration {
        key: Some(k.to_string()),
        operator: Some("Exists".to_string()),
        ..Default::default()
    })
    .collect()
}

fn deployment(cfg: &EffectiveConfig) -> Deployment {
    let mut pod_labels = common_labels(cfg);
    pod_labels.insert(APP_LABEL.0.to_string(), APP_LABEL.1.to_string());

    Deployment {
        metadata: meta(cfg, HUBBLE_RELAY, &[APP_LABEL]),
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    APP_LABEL.0.to_string(),
                    APP_LABEL.1.to_string(),
                )])),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta { labels: Some(pod_labels), ..Default::default() }),
                spec: Some(PodSpec {
                    host_network: Some(true),
                    tolerations: Some(tolerations()),
                    containers: vec![Container {
                        name: HUBBLE_RELAY.to_string(),
                        image: Some(cfg.hubble_relay_image.clone()),
                        image_pull_policy: Some("IfNotPresent".to_string()),
                        command: Some(vec!["hubble-relay".to_string()]),
                        // It reads /etc/hubble-relay/config.yaml by default,
                        // which is where the ConfigMap is mounted. There is no
                        // --config flag: passing one crash-loops it.
                        args: Some(vec!["serve".to_string()]),
                        volume_mounts: Some(vec![
                            // Its root filesystem may be read-only.
                            mount("tmp", "/tmp"),
                            mount_ro("config", "/etc/hubble-relay"),
                            mount_ro("hubble-sock", "/var/run/cilium"),
                        ]),
                        termination_message_policy: Some("FallbackToLogsOnError".to_string()),
                        ..Default::default()
                    }],
                    volumes: Some(vec![
                        empty_dir("tmp"),
                        config_map_volume("config", HUBBLE_RELAY_CONFIG),
                        host_path("hubble-sock", "/var/run/cilium", "Directory"),
                    ]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::Mode;
    use crate::testutil::cfg_for;

    #[test]
    fn rendered_only_while_hubble_is_on() {
        let mut cfg = cfg_for(Mode::Overlay);
        let ids: Vec<_> = render(&cfg).iter().map(|r| r.id()).collect();
        assert_eq!(
            ids,
            vec!["ConfigMap/kube-system/hubble-relay-config", "Deployment/kube-system/hubble-relay"]
        );
        cfg.hubble = false;
        assert!(render(&cfg).is_empty());
    }

    #[test]
    fn dials_the_socket_the_agent_config_names() {
        let cfg = cfg_for(Mode::Overlay);
        let c = config(&cfg);
        let yaml = &c.data.unwrap()["config.yaml"];
        let sock = super::super::config::data(&cfg)["hubble-socket-path"].clone();
        assert!(yaml.contains(&format!("unix://{sock}")), "{yaml}");
        assert!(yaml.contains("listen-address: :4245"));
    }

    #[test]
    fn runs_the_pinned_relay_image_with_a_static_selector() {
        let cfg = cfg_for(Mode::Overlay);
        let d = deployment(&cfg);
        let spec = d.spec.unwrap();
        assert_eq!(
            spec.selector.match_labels.unwrap(),
            BTreeMap::from([("k8s-app".to_string(), "hubble-relay".to_string())])
        );
        let pod = spec.template.spec.unwrap();
        assert_eq!(pod.containers[0].image.as_deref(), Some(cfg.hubble_relay_image.as_str()));
        assert!(cfg.hubble_relay_image.contains("@sha256:"));
        assert_eq!(pod.containers[0].args.as_deref(), Some(&["serve".to_string()][..]));
    }
}
