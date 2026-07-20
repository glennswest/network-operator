//! Immutability enforcement, matching OpenShift CNO semantics.
//!
//! Some choices cannot change under a running dataplane without a disruptive
//! re-plumb. CNO enforces these with a validating webhook; until we have one
//! (P3) the reconciler refuses the change instead — the install keeps running on
//! the applied config and the CR goes `Degraded` explaining what was rejected.
//!
//! The baseline is `status.applied*`, written on the first successful apply.
//! Before that there is nothing to protect, so anything goes.

use crate::crd::NetworkStatus;
use crate::modes::EffectiveConfig;

/// A field that changed but may not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub field: String,
    pub applied: String,
    pub requested: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is immutable after install (applied {}, requested {})",
            self.field, self.applied, self.requested
        )
    }
}

/// Check a resolved config against what is already installed.
///
/// Deliberately *not* checked: `mode` itself. Switching overlay -> encrypted is
/// a mode change but not a datapath change, and the runtime-changeable fields
/// (encryption, LB/BGP, mtu, version, kube-proxy replacement) come along with
/// it. Only the datapath family is pinned.
pub fn check(cfg: &EffectiveConfig, status: Option<&NetworkStatus>) -> Vec<Violation> {
    let Some(status) = status else { return Vec::new() };
    let mut out = Vec::new();

    let mut compare = |field: &str, applied: Option<&str>, requested: &str| {
        if let Some(applied) = applied {
            if applied != requested {
                out.push(Violation {
                    field: field.to_string(),
                    applied: applied.to_string(),
                    requested: requested.to_string(),
                });
            }
        }
    };

    compare(
        "spec.mode (datapath family)",
        status.applied_datapath.as_deref(),
        cfg.routing.as_str(),
    );
    compare("spec.cilium.ipam.mode", status.applied_ipam.as_deref(), cfg.ipam.as_str());

    compare_list(
        &mut out,
        "spec.clusterNetwork",
        &status.applied_cluster_network,
        &cfg.cluster_network,
    );
    compare_list(
        &mut out,
        "spec.serviceNetwork",
        &status.applied_service_network,
        &cfg.service_network,
    );

    out
}

fn compare_list(out: &mut Vec<Violation>, field: &str, applied: &[String], requested: &[String]) {
    if applied.is_empty() || applied == requested {
        return;
    }
    out.push(Violation {
        field: field.to_string(),
        applied: applied.join(","),
        requested: requested.join(","),
    });
}

/// The status stanza recording what is installed, written after a successful
/// apply. This is what [`check`] compares against next time.
pub fn applied_from(cfg: &EffectiveConfig, status: &mut NetworkStatus) {
    status.applied_mode = Some(cfg.mode.as_str().to_string());
    status.applied_version = Some(cfg.version.clone());
    status.applied_datapath = Some(cfg.routing.as_str().to_string());
    status.applied_ipam = Some(cfg.ipam.as_str().to_string());
    status.applied_cluster_network = cfg.cluster_network.clone();
    status.applied_service_network = cfg.service_network.clone();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{IpamMode, Mode, RoutingMode};
    use crate::testutil::cfg_for;

    fn installed(mode: Mode) -> NetworkStatus {
        let mut s = NetworkStatus::default();
        applied_from(&cfg_for(mode), &mut s);
        s
    }

    #[test]
    fn a_fresh_install_has_no_baseline_to_violate() {
        assert!(check(&cfg_for(Mode::Overlay), None).is_empty());
        assert!(check(&cfg_for(Mode::Bgp), Some(&NetworkStatus::default())).is_empty());
    }

    #[test]
    fn re_applying_the_same_config_is_clean() {
        let status = installed(Mode::Overlay);
        assert!(check(&cfg_for(Mode::Overlay), Some(&status)).is_empty());
    }

    #[test]
    fn crossing_the_datapath_family_is_rejected() {
        let status = installed(Mode::Overlay);
        let v = check(&cfg_for(Mode::Native), Some(&status));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].field, "spec.mode (datapath family)");
        assert_eq!(v[0].applied, "tunnel");
        assert_eq!(v[0].requested, "native");
    }

    #[test]
    fn runtime_changeable_fields_pass_even_across_modes() {
        // overlay -> encrypted: same tunnel datapath, only encryption differs.
        let status = installed(Mode::Overlay);
        assert!(check(&cfg_for(Mode::Encrypted), Some(&status)).is_empty());

        // native -> bgp: same native datapath, LB/BGP is runtime-changeable.
        let status = installed(Mode::Native);
        assert!(check(&cfg_for(Mode::Bgp), Some(&status)).is_empty());

        // a version bump is an upgrade, not a violation
        let status = installed(Mode::Overlay);
        let mut cfg = cfg_for(Mode::Overlay);
        cfg.version = "1.20.0".into();
        assert!(check(&cfg, Some(&status)).is_empty());
    }

    #[test]
    fn changing_the_pod_or_service_cidr_is_rejected() {
        let status = installed(Mode::Overlay);

        let mut cfg = cfg_for(Mode::Overlay);
        cfg.cluster_network = vec!["10.245.0.0/16".into()];
        let v = check(&cfg, Some(&status));
        assert_eq!(v[0].field, "spec.clusterNetwork");

        let mut cfg = cfg_for(Mode::Overlay);
        cfg.service_network = vec!["172.30.0.0/16".into()];
        let v = check(&cfg, Some(&status));
        assert_eq!(v[0].field, "spec.serviceNetwork");
    }

    #[test]
    fn changing_ipam_mode_is_rejected() {
        let status = installed(Mode::Overlay);
        let mut cfg = cfg_for(Mode::Overlay);
        cfg.ipam = IpamMode::Kubernetes;
        let v = check(&cfg, Some(&status));
        assert_eq!(v[0].field, "spec.cilium.ipam.mode");
    }

    #[test]
    fn several_violations_are_all_reported_not_just_the_first() {
        let status = installed(Mode::Overlay);
        let mut cfg = cfg_for(Mode::Overlay);
        cfg.routing = RoutingMode::Native;
        cfg.ipam = IpamMode::Kubernetes;
        cfg.cluster_network = vec!["10.245.0.0/16".into()];
        assert_eq!(check(&cfg, Some(&status)).len(), 3);
    }
}
