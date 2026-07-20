//! The `cilium` agent DaemonSet — the dataplane itself.
//!
//! Shape follows the upstream Cilium install: a chain of privileged init
//! containers that prepare the host (cgroupv2 mount, sysctls, bpffs, state
//! cleanup, CNI binary install), then the long-running `cilium-agent`.
//!
//! Two things here are load-bearing for this stack in particular:
//!
//! * `KUBERNETES_SERVICE_HOST`/`PORT` are set explicitly. With kube-proxy
//!   replacement there is no iptables rule for the kubernetes Service, so the
//!   agent must be told where the apiserver is or it cannot bootstrap.
//! * A `startupProbe` guards the long first boot (BPF compile + map init) so a
//!   slow start is not liveness-killed — rustkube-node only gained startupProbe
//!   support for exactly this reason.

use k8s_openapi::api::apps::v1::{
    DaemonSet, DaemonSetSpec, DaemonSetUpdateStrategy, RollingUpdateDaemonSet,
};
use k8s_openapi::api::core::v1::{Container, EnvVar, PodSpec, PodTemplateSpec, Probe, Volume};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use std::collections::BTreeMap;

use crate::modes::EffectiveConfig;

use super::util::*;
use super::{common_labels, meta, typed, Rendered, AGENT_DS, AGENT_HEALTH_PORT, AGENT_SA, CONFIG_MAP};

/// Pod selector label. Immutable on a DaemonSet, so it must never be derived
/// from anything in the spec.
const APP_LABEL: (&str, &str) = ("k8s-app", "cilium");

/// Capabilities the agent needs to program the datapath. Taken from a known-good
/// upstream install — narrower than blanket `privileged: true`, which is what
/// this used to grant.
const AGENT_CAPS: &[&str] = &[
    "CHOWN", "KILL", "NET_ADMIN", "NET_RAW", "IPC_LOCK", "SYS_MODULE", "SYS_ADMIN",
    "SYS_RESOURCE", "DAC_OVERRIDE", "FOWNER", "SETGID", "SETUID", "SYSLOG",
];

/// Entering PID 1's namespaces needs these three and nothing else.
const NSENTER_CAPS: &[&str] = &["SYS_ADMIN", "SYS_CHROOT", "SYS_PTRACE"];

pub fn render(cfg: &EffectiveConfig) -> Rendered {
    let mut pod_labels = common_labels(cfg);
    pod_labels.insert(APP_LABEL.0.to_string(), APP_LABEL.1.to_string());

    typed(DaemonSet {
        metadata: meta(cfg, AGENT_DS, &[APP_LABEL]),
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
                    // Two nodes at a time: an upgrade should not take a large
                    // cluster all afternoon, but the blast radius stays bounded.
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
    })
}

fn pod_spec(cfg: &EffectiveConfig) -> PodSpec {
    PodSpec {
        service_account_name: Some(AGENT_SA.to_string()),
        // Host network and PID: the agent programs the host datapath and enters
        // PID 1's mount/cgroup namespaces from its init containers.
        host_network: Some(true),
        host_pid: Some(true),
        dns_policy: Some("ClusterFirstWithHostNet".to_string()),
        priority_class_name: Some("system-node-critical".to_string()),
        restart_policy: Some("Always".to_string()),
        termination_grace_period_seconds: Some(1),
        tolerations: Some(tolerate_all()),
        // Without this the init containers fail with EPERM — see
        // util::unconfined_pod_security.
        security_context: Some(unconfined_pod_security(false)),
        init_containers: Some(init_containers(cfg)),
        containers: vec![agent_container(cfg)],
        volumes: Some(volumes(cfg)),
        ..Default::default()
    }
}

fn agent_container(cfg: &EffectiveConfig) -> Container {
    Container {
        name: "cilium-agent".to_string(),
        image: Some(cfg.agent_image()),
        image_pull_policy: Some("IfNotPresent".to_string()),
        command: Some(vec!["cilium-agent".to_string()]),
        args: Some(vec!["--config-dir=/tmp/cilium/config-map".to_string()]),
        env: Some(agent_env(cfg)),
        // A cold agent compiles BPF and initialises maps; allow 2 minutes
        // (24 x 5s) before liveness is even consulted.
        startup_probe: Some(Probe {
            failure_threshold: Some(24),
            period_seconds: Some(5),
            initial_delay_seconds: Some(5),
            ..health_probe(AGENT_HEALTH_PORT, "/healthz")
        }),
        liveness_probe: Some(Probe {
            period_seconds: Some(30),
            timeout_seconds: Some(5),
            failure_threshold: Some(10),
            ..health_probe(AGENT_HEALTH_PORT, "/healthz")
        }),
        readiness_probe: Some(Probe {
            period_seconds: Some(30),
            timeout_seconds: Some(5),
            failure_threshold: Some(3),
            ..health_probe(AGENT_HEALTH_PORT, "/healthz")
        }),
        security_context: Some(caps(AGENT_CAPS)),
        termination_message_policy: Some("FallbackToLogsOnError".to_string()),
        volume_mounts: Some(vec![
            mount_ro("cilium-config-path", "/tmp/cilium/config-map"),
            mount("cilium-run", "/var/run/cilium"),
            mount_bidirectional("bpf-maps", "/sys/fs/bpf"),
            mount("etc-cni-netd", "/host/etc/cni/net.d"),
            mount("cni-path", "/host/opt/cni/bin"),
            mount_ro("lib-modules", "/lib/modules"),
            mount("xtables-lock", "/run/xtables.lock"),
            mount("host-proc-sys-net", "/host/proc/sys/net"),
            mount("host-proc-sys-kernel", "/host/proc/sys/kernel"),
            mount("tmp", "/tmp"),
        ]
        .into_iter()
        // A standalone Envoy reaches xDS through this directory, so the agent
        // has to serve it from the same host path Envoy mounts.
        .chain(cfg.envoy.then(super::envoy::agent_socket_mount))
        .collect()),
        ..Default::default()
    }
}

fn agent_env(cfg: &EffectiveConfig) -> Vec<EnvVar> {
    vec![
        env_field("K8S_NODE_NAME", "spec.nodeName"),
        env_field("CILIUM_K8S_NAMESPACE", "metadata.namespace"),
        // Without these the agent cannot find the apiserver before the Service
        // network exists — see the module comment.
        env("KUBERNETES_SERVICE_HOST", &cfg.k8s_service_host),
        env("KUBERNETES_SERVICE_PORT", &cfg.k8s_service_port.to_string()),
        env_config("CILIUM_CLUSTERMESH_CONFIG", "clustermesh-config", CONFIG_MAP),
        env("GOMEMLIMIT", "1GiB"),
    ]
}

fn init_containers(cfg: &EffectiveConfig) -> Vec<Container> {
    let image = cfg.agent_image();
    vec![
        // Reconciles per-node config overrides (CiliumNodeConfig) into the
        // config dir the agent reads.
        Container {
            name: "config".to_string(),
            image: Some(image.clone()),
            image_pull_policy: Some("IfNotPresent".to_string()),
            command: Some(vec!["cilium-dbg".to_string(), "build-config".to_string()]),
            env: Some(vec![
                env_field("K8S_NODE_NAME", "spec.nodeName"),
                env_field("CILIUM_K8S_NAMESPACE", "metadata.namespace"),
                env("KUBERNETES_SERVICE_HOST", &cfg.k8s_service_host),
                env("KUBERNETES_SERVICE_PORT", &cfg.k8s_service_port.to_string()),
            ]),
            volume_mounts: Some(vec![mount("tmp", "/tmp")]),
            security_context: Some(caps_no_selinux(&["NET_ADMIN"])),
            termination_message_policy: Some("FallbackToLogsOnError".to_string()),
            ..Default::default()
        },
        // cgroupv2 has to be mounted in the *host's* namespaces, hence nsenter
        // into PID 1 — the container's own mount namespace is not where the
        // kubelet and the agent will look for it.
        Container {
            name: "mount-cgroup".to_string(),
            image: Some(image.clone()),
            image_pull_policy: Some("IfNotPresent".to_string()),
            env: Some(vec![
                env("CGROUP_ROOT", "/run/cilium/cgroupv2"),
                env("BIN_PATH", "/opt/cni/bin"),
            ]),
            command: Some(vec!["sh".to_string(), "-ec".to_string()]),
            args: Some(vec![concat!(
                "cp /usr/bin/cilium-mount /hostbin/cilium-mount; ",
                "nsenter --cgroup=/hostproc/1/ns/cgroup --mount=/hostproc/1/ns/mnt ",
                "\"${BIN_PATH}/cilium-mount\" $CGROUP_ROOT; ",
                "rm /hostbin/cilium-mount"
            )
            .to_string()]),
            volume_mounts: Some(vec![mount("hostproc", "/hostproc"), mount("cni-path", "/hostbin")]),
            security_context: Some(caps(NSENTER_CAPS)),
            termination_message_policy: Some("FallbackToLogsOnError".to_string()),
            ..Default::default()
        },
        Container {
            name: "apply-sysctl-overwrites".to_string(),
            image: Some(image.clone()),
            image_pull_policy: Some("IfNotPresent".to_string()),
            env: Some(vec![env("BIN_PATH", "/opt/cni/bin")]),
            command: Some(vec!["sh".to_string(), "-ec".to_string()]),
            args: Some(vec![concat!(
                "cp /usr/bin/cilium-sysctlfix /hostbin/cilium-sysctlfix; ",
                "nsenter --mount=/hostproc/1/ns/mnt \"${BIN_PATH}/cilium-sysctlfix\"; ",
                "rm /hostbin/cilium-sysctlfix"
            )
            .to_string()]),
            volume_mounts: Some(vec![mount("hostproc", "/hostproc"), mount("cni-path", "/hostbin")]),
            security_context: Some(caps(NSENTER_CAPS)),
            termination_message_policy: Some("FallbackToLogsOnError".to_string()),
            ..Default::default()
        },
        Container {
            name: "mount-bpf-fs".to_string(),
            image: Some(image.clone()),
            image_pull_policy: Some("IfNotPresent".to_string()),
            command: Some(vec!["/bin/bash".to_string(), "-c".to_string(), "--".to_string()]),
            args: Some(vec![
                "mount | grep \"/sys/fs/bpf type bpf\" || mount -t bpf bpf /sys/fs/bpf".to_string(),
            ]),
            volume_mounts: Some(vec![mount_bidirectional("bpf-maps", "/sys/fs/bpf")]),
            security_context: Some(privileged()),
            termination_message_policy: Some("FallbackToLogsOnError".to_string()),
            ..Default::default()
        },
        // No-op unless the escape-hatch config keys are set; when they are, it
        // wipes BPF state so a broken datapath can be recovered by config alone.
        Container {
            name: "clean-cilium-state".to_string(),
            image: Some(image.clone()),
            image_pull_policy: Some("IfNotPresent".to_string()),
            command: Some(vec!["/init-container.sh".to_string()]),
            env: Some(vec![
                env_config("CILIUM_ALL_STATE", "clean-cilium-state", CONFIG_MAP),
                env_config("CILIUM_BPF_STATE", "clean-cilium-bpf-state", CONFIG_MAP),
            ]),
            volume_mounts: Some(vec![
                mount_bidirectional("bpf-maps", "/sys/fs/bpf"),
                mount("cilium-cgroup", "/run/cilium/cgroupv2"),
                mount("cilium-run", "/var/run/cilium"),
            ]),
            security_context: Some(caps(&["NET_ADMIN", "SYS_MODULE", "SYS_ADMIN", "SYS_RESOURCE"])),
            termination_message_policy: Some("FallbackToLogsOnError".to_string()),
            ..Default::default()
        },
        Container {
            name: "install-cni-binaries".to_string(),
            image: Some(image),
            image_pull_policy: Some("IfNotPresent".to_string()),
            command: Some(vec!["/install-plugin.sh".to_string()]),
            volume_mounts: Some(vec![mount("cni-path", "/host/opt/cni/bin")]),
            security_context: Some(selinux_only()),
            termination_message_policy: Some("FallbackToLogsOnError".to_string()),
            ..Default::default()
        },
    ]
}

fn volumes(cfg: &EffectiveConfig) -> Vec<Volume> {
    let mut v = vec![
        empty_dir("tmp"),
        host_path("cilium-run", "/var/run/cilium", "DirectoryOrCreate"),
        host_path("bpf-maps", "/sys/fs/bpf", "DirectoryOrCreate"),
        host_path("hostproc", "/proc", "Directory"),
        host_path("cilium-cgroup", "/run/cilium/cgroupv2", "DirectoryOrCreate"),
        host_path("cni-path", "/opt/cni/bin", "DirectoryOrCreate"),
        host_path("etc-cni-netd", "/etc/cni/net.d", "DirectoryOrCreate"),
        host_path("lib-modules", "/lib/modules", "Directory"),
        host_path("xtables-lock", "/run/xtables.lock", "FileOrCreate"),
        host_path("host-proc-sys-net", "/proc/sys/net", "Directory"),
        host_path("host-proc-sys-kernel", "/proc/sys/kernel", "Directory"),
        config_map_volume("cilium-config-path", CONFIG_MAP),
    ];
    if cfg.envoy {
        v.push(super::envoy::agent_socket_volume());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::Mode;
    use crate::testutil::cfg_for;

    fn ds(cfg: &EffectiveConfig) -> DaemonSet {
        serde_json::from_value(serde_json::to_value(render(cfg).obj).unwrap()).unwrap()
    }

    #[test]
    fn selector_matches_the_pod_template_labels() {
        let ds = ds(&cfg_for(Mode::Overlay));
        let spec = ds.spec.unwrap();
        let selector = spec.selector.match_labels.unwrap();
        let labels = spec.template.metadata.unwrap().labels.unwrap();
        for (k, v) in &selector {
            assert_eq!(labels.get(k), Some(v), "pod template must match selector on {k}");
        }
    }

    #[test]
    fn selector_does_not_depend_on_spec_and_so_stays_immutable() {
        let a = ds(&cfg_for(Mode::Overlay)).spec.unwrap().selector;
        let b = ds(&cfg_for(Mode::Bgp)).spec.unwrap().selector;
        assert_eq!(a, b);
    }

    #[test]
    fn agent_is_told_where_the_apiserver_is() {
        let ds = ds(&cfg_for(Mode::Overlay));
        let spec = ds.spec.unwrap();
        let pod = spec.template.spec.unwrap();
        for c in pod.containers.iter().chain(pod.init_containers.as_ref().unwrap()) {
            // Only the containers that talk to the apiserver need it.
            if c.name != "cilium-agent" && c.name != "config" {
                continue;
            }
            let env = c.env.as_ref().unwrap();
            let host = env.iter().find(|e| e.name == "KUBERNETES_SERVICE_HOST").unwrap();
            assert_eq!(host.value.as_deref(), Some("192.168.8.98"));
            let port = env.iter().find(|e| e.name == "KUBERNETES_SERVICE_PORT").unwrap();
            assert_eq!(port.value.as_deref(), Some("6443"));
        }
    }

    #[test]
    fn startup_probe_gives_the_first_boot_room_before_liveness() {
        let ds = ds(&cfg_for(Mode::Overlay));
        let pod = ds.spec.unwrap().template.spec.unwrap();
        let agent = &pod.containers[0];
        let startup = agent.startup_probe.as_ref().expect("startupProbe is required");
        let budget = startup.failure_threshold.unwrap() * startup.period_seconds.unwrap();
        assert!(budget >= 60, "first-boot budget is only {budget}s");
        assert!(agent.liveness_probe.is_some());
        assert!(agent.readiness_probe.is_some());
    }

    #[test]
    fn probes_target_loopback_because_the_pod_is_host_networked() {
        let ds = ds(&cfg_for(Mode::Overlay));
        let pod = ds.spec.clone().unwrap().template.spec.unwrap();
        assert_eq!(pod.host_network, Some(true));
        assert_eq!(pod.dns_policy.as_deref(), Some("ClusterFirstWithHostNet"));
        let get = pod.containers[0]
            .liveness_probe
            .as_ref()
            .unwrap()
            .http_get
            .as_ref()
            .unwrap();
        assert_eq!(get.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(get.port, IntOrString::Int(AGENT_HEALTH_PORT));
    }

    #[test]
    fn tolerates_every_taint_so_it_can_make_nodes_ready() {
        let ds = ds(&cfg_for(Mode::Overlay));
        let tolerations = ds.spec.unwrap().template.spec.unwrap().tolerations.unwrap();
        assert_eq!(tolerations.len(), 1);
        assert_eq!(tolerations[0].operator.as_deref(), Some("Exists"));
        assert!(tolerations[0].key.is_none());
        assert!(tolerations[0].effect.is_none());
    }

    /// Regression guard for #5. A default seccomp profile answers blocked
    /// syscalls with EPERM, which stalled the `config` init container on nothing
    /// more than an HTTPS call to the apiserver.
    #[test]
    fn pod_runs_unconfined_or_the_datapath_cannot_be_programmed() {
        let ds = ds(&cfg_for(Mode::Overlay));
        let sc = ds
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .security_context
            .expect("pod-level securityContext is required");
        assert_eq!(sc.seccomp_profile.unwrap().type_, "Unconfined");
        assert_eq!(sc.app_armor_profile.unwrap().type_, "Unconfined");
    }

    /// Also #5: an absent securityContext silently inherits the runtime default,
    /// which is how the `config` container ended up with no NET_ADMIN.
    #[test]
    fn every_container_states_its_privileges_explicitly() {
        let ds = ds(&cfg_for(Mode::Overlay));
        let pod = ds.spec.unwrap().template.spec.unwrap();
        for c in pod.containers.iter().chain(pod.init_containers.as_ref().unwrap()) {
            let sc = c
                .security_context
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no securityContext", c.name));
            assert!(
                sc.privileged == Some(true) || sc.capabilities.is_some() || sc.se_linux_options.is_some(),
                "{} has an empty securityContext",
                c.name
            );
        }
    }

    #[test]
    fn privileges_match_the_known_good_install() {
        let ds = ds(&cfg_for(Mode::Overlay));
        let pod = ds.spec.unwrap().template.spec.unwrap();
        let find = |name: &str| {
            pod.containers
                .iter()
                .chain(pod.init_containers.as_ref().unwrap())
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("no container {name}"))
                .security_context
                .clone()
                .unwrap()
        };
        let added = |name: &str| find(name).capabilities.and_then(|c| c.add).unwrap_or_default();

        // config only talks to the apiserver: one capability, and no SELinux
        // label because it touches no host path.
        assert_eq!(added("config"), vec!["NET_ADMIN"]);
        assert!(find("config").se_linux_options.is_none());

        assert_eq!(added("mount-cgroup"), NSENTER_CAPS);
        assert_eq!(added("apply-sysctl-overwrites"), NSENTER_CAPS);
        assert_eq!(added("cilium-agent"), AGENT_CAPS);

        // Blanket privilege is reserved for the one container that truly needs
        // it; everything else names its capabilities.
        assert_eq!(find("mount-bpf-fs").privileged, Some(true));
        for name in [
            "config",
            "mount-cgroup",
            "apply-sysctl-overwrites",
            "clean-cilium-state",
            "install-cni-binaries",
            "cilium-agent",
        ] {
            assert_ne!(find(name).privileged, Some(true), "{name} should not be privileged");
        }

        // Anything writing host paths needs the spc_t label under enforcing
        // SELinux (rustkube-node#26).
        for name in ["mount-cgroup", "clean-cilium-state", "install-cni-binaries", "cilium-agent"] {
            assert_eq!(
                find(name).se_linux_options.unwrap().type_.as_deref(),
                Some("spc_t"),
                "{name} needs the spc_t label"
            );
        }
    }

    #[test]
    fn every_mount_resolves_to_a_declared_volume() {
        let ds = ds(&cfg_for(Mode::Overlay));
        let pod = ds.spec.unwrap().template.spec.unwrap();
        let declared: Vec<_> = pod.volumes.unwrap().into_iter().map(|v| v.name).collect();
        for c in pod.containers.iter().chain(pod.init_containers.as_ref().unwrap()) {
            for m in c.volume_mounts.iter().flatten() {
                assert!(
                    declared.contains(&m.name),
                    "container {} mounts undeclared volume {}",
                    c.name,
                    m.name
                );
            }
        }
    }

    #[test]
    fn version_bump_changes_every_image_and_nothing_else() {
        let base = cfg_for(Mode::Overlay);
        let mut bumped = base.clone();
        bumped.version = "1.20.0".into();

        let before = ds(&base).spec.unwrap().template.spec.unwrap();
        let after = ds(&bumped).spec.unwrap().template.spec.unwrap();
        for c in after.containers.iter().chain(after.init_containers.as_ref().unwrap()) {
            assert_eq!(c.image.as_deref(), Some("quay.io/cilium/cilium:v1.20.0"));
        }
        assert_ne!(before.containers[0].image, after.containers[0].image);
    }
}
