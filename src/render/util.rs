//! Small constructors for the k8s-openapi pod-spec types. These exist only to
//! keep the workload renders readable — every one is a plain struct literal.

use k8s_openapi::api::core::v1::{
    ConfigMapKeySelector, ConfigMapVolumeSource, EmptyDirVolumeSource, EnvVar, EnvVarSource,
    HTTPGetAction, HostPathVolumeSource, ObjectFieldSelector, Probe, SELinuxOptions,
    SecurityContext, Toleration, Volume, VolumeMount,
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

/// Privileged security context with the SELinux label Cilium needs on an
/// SELinux-enforcing host (`spc_t` — a super-privileged container).
pub fn privileged() -> SecurityContext {
    SecurityContext {
        privileged: Some(true),
        se_linux_options: Some(SELinuxOptions {
            level: Some("s0".to_string()),
            type_: Some("spc_t".to_string()),
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
