//! Small constructors for the k8s-openapi pod-spec types. These exist only to
//! keep the workload renders readable — every one is a plain struct literal.

use k8s_openapi::api::core::v1::{
    AppArmorProfile, Capabilities, ConfigMapKeySelector, ConfigMapVolumeSource,
    ContainerPort, EmptyDirVolumeSource, EnvVar, EnvVarSource, HTTPGetAction,
    HostPathVolumeSource, ObjectFieldSelector, PodSecurityContext, Probe, SELinuxOptions,
    SeccompProfile, SecretVolumeSource, SecurityContext, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

pub fn env(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..Default::default()
    }
}

/// An env var read from the pod's own object via the downward API.
pub fn env_field(name: &str, field_path: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector {
                api_version: Some("v1".to_string()),
                field_path: field_path.to_string(),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// An optional env var from `cilium-config`. Optional because these keys are
/// operator escape hatches that are normally absent.
pub fn env_config(name: &str, key: &str, config_map: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value_from: Some(EnvVarSource {
            config_map_key_ref: Some(ConfigMapKeySelector {
                name: config_map.to_string(),
                key: key.to_string(),
                optional: Some(true),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn mount(name: &str, path: &str) -> VolumeMount {
    VolumeMount {
        name: name.to_string(),
        mount_path: path.to_string(),
        ..Default::default()
    }
}

pub fn mount_ro(name: &str, path: &str) -> VolumeMount {
    VolumeMount { read_only: Some(true), ..mount(name, path) }
}

/// A mount whose sub-mounts propagate both ways — needed wherever the container
/// mounts something the host must also see (bpffs, cgroupv2).
pub fn mount_bidirectional(name: &str, path: &str) -> VolumeMount {
    VolumeMount {
        mount_propagation: Some("Bidirectional".to_string()),
        ..mount(name, path)
    }
}

/// A mount that sees mounts the host makes under it later (network namespaces
/// bind-mounted under /var/run/netns), without propagating anything back.
pub fn mount_host_to_container(name: &str, path: &str) -> VolumeMount {
    VolumeMount {
        mount_propagation: Some("HostToContainer".to_string()),
        ..mount(name, path)
    }
}

/// A named TCP port on a host-networked pod, so `hostPort` equals the
/// container port. The name is what a Service's `targetPort` refers to.
pub fn host_port(name: &str, port: i32) -> ContainerPort {
    ContainerPort {
        name: Some(name.to_string()),
        container_port: port,
        host_port: Some(port),
        protocol: Some("TCP".to_string()),
        ..Default::default()
    }
}

pub fn host_path(name: &str, path: &str, kind: &str) -> Volume {
    Volume {
        name: name.to_string(),
        host_path: Some(HostPathVolumeSource {
            path: path.to_string(),
            type_: Some(kind.to_string()),
        }),
        ..Default::default()
    }
}

pub fn empty_dir(name: &str) -> Volume {
    Volume {
        name: name.to_string(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    }
}

pub fn config_map_volume(name: &str, config_map: &str) -> Volume {
    Volume {
        name: name.to_string(),
        config_map: Some(ConfigMapVolumeSource {
            name: config_map.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn secret_volume(name: &str, secret: &str) -> Volume {
    Volume {
        name: name.to_string(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(secret.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Fully privileged. Only `mount-bpf-fs` needs this; everything else uses an
/// explicit capability set via [`caps`].
pub fn privileged() -> SecurityContext {
    SecurityContext { privileged: Some(true), ..Default::default() }
}

/// Drop every capability, then add back exactly the listed ones, with the
/// SELinux label Cilium needs on an enforcing host (`spc_t` — a
/// super-privileged container).
///
/// Preferred over blanket `privileged: true`: it is what upstream Cilium ships,
/// and it keeps each container to the privileges it actually uses.
pub fn caps(add: &[&str]) -> SecurityContext {
    SecurityContext {
        capabilities: Some(Capabilities {
            add: Some(add.iter().map(|c| c.to_string()).collect()),
            drop: Some(vec!["ALL".to_string()]),
        }),
        se_linux_options: Some(SELinuxOptions {
            level: Some("s0".to_string()),
            type_: Some("spc_t".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Capabilities without the SELinux label — for containers that only talk to
/// the apiserver and never touch host paths.
pub fn caps_no_selinux(add: &[&str]) -> SecurityContext {
    SecurityContext {
        se_linux_options: None,
        ..caps(add)
    }
}

/// The SELinux label alone, no added capabilities.
pub fn selinux_only() -> SecurityContext {
    SecurityContext {
        capabilities: None,
        ..caps(&[])
    }
}

/// Pod-level security context.
///
/// **Both profiles must be unconfined.** A default seccomp profile answers a
/// blocked syscall with `EPERM`, which surfaces as the agent's init containers
/// failing with "operation not permitted" — including `config`, which only
/// wants to reach the apiserver. Cilium programs the datapath and cannot run
/// under the default profiles.
pub fn unconfined_pod_security(apparmor_only: bool) -> PodSecurityContext {
    PodSecurityContext {
        app_armor_profile: Some(AppArmorProfile {
            type_: "Unconfined".to_string(),
            ..Default::default()
        }),
        seccomp_profile: (!apparmor_only).then(|| SeccompProfile {
            type_: "Unconfined".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// An HTTP probe against the agent's own health server on localhost. The agent
/// is host-networked, so `host` must be pinned to loopback rather than left to
/// resolve to the pod IP.
pub fn health_probe(port: i32, path: &str) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            host: Some("127.0.0.1".to_string()),
            path: Some(path.to_string()),
            port: IntOrString::Int(port),
            scheme: Some("HTTP".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Tolerate everything: the CNI has to run before a node can become Ready, so
/// no taint may keep it off.
pub fn tolerate_all() -> Vec<Toleration> {
    vec![Toleration { operator: Some("Exists".to_string()), ..Default::default() }]
}

/// Pod-template annotation carrying [`config_hash`] of the ConfigMap the pods
/// read their configuration from (#48, finding `4337dd4c99d7`). Cilium reads
/// that ConfigMap at start only, so a ConfigMap-only change must change the
/// template, or the running pods keep their startup configuration. An
/// annotation, never a label: selectors stay independent of the spec.
pub const CONFIG_HASH_ANNOTATION: &str = "network.storm.io/config-hash";

/// A deterministic digest of a ConfigMap's `data`: FNV-1a 64 over each
/// key and value, each prefixed by its length (u64, little-endian), in key
/// order, as 16 hex digits. Not a security boundary, only a change detector;
/// written out (not `DefaultHasher`) so it is the same on every toolchain and
/// the goldens stay stable.
pub fn config_hash(data: &std::collections::BTreeMap<String, String>) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    let mut eat = |bytes: &[u8]| {
        for b in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(PRIME);
        }
    };
    for (k, v) in data {
        eat(k.as_bytes());
        eat(v.as_bytes());
    }
    format!("{h:016x}")
}

/// The annotations for a pod template whose pods read `data` at start.
pub fn config_hash_annotations(
    data: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([(CONFIG_HASH_ANNOTATION.to_string(), config_hash(data))])
}

#[cfg(test)]
mod hash_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn m(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Pinned values: the hash is part of every golden, so it must not move
    /// with the toolchain. FNV-1a 64 of the empty input is its offset basis.
    #[test]
    fn hash_is_pinned() {
        assert_eq!(config_hash(&BTreeMap::new()), "cbf29ce484222325");
        assert_eq!(config_hash(&m(&[("mtu", "1450")])), "821c422a87553c8a");
    }

    #[test]
    fn any_key_or_value_change_moves_the_hash() {
        let base = config_hash(&m(&[("mtu", "1450"), ("debug", "false")]));
        assert_ne!(base, config_hash(&m(&[("mtu", "1500"), ("debug", "false")])));
        assert_ne!(base, config_hash(&m(&[("mtu", "1450")])));
        assert_ne!(base, config_hash(&m(&[("mtu", "1450"), ("debug", "false"), ("x", "")])));
        // Length prefixes: moving a boundary between key and value is a change.
        assert_ne!(config_hash(&m(&[("ab", "c")])), config_hash(&m(&[("a", "bc")])));
    }
}
