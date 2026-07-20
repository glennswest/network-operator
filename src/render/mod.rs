//! Rendering: [`EffectiveConfig`] -> the concrete set of Kubernetes objects
//! that make up a Cilium install.
//!
//! The render is **pure and deterministic** — same config in, byte-identical
//! objects out — so it is covered by golden files rather than by a cluster.
//! Nothing here talks to an apiserver; [`crate::apply`] does that.
//!
//! Objects come back in dependency order (RBAC -> config -> workloads -> Cilium
//! CRs), because the Cilium CRDs that back the last group are created by
//! `cilium-operator` itself and therefore do not exist until it runs.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{ApiResource, DynamicObject};
use serde::Serialize;
use std::collections::BTreeMap;

use crate::modes::{EffectiveConfig, NAMESPACE};

mod agent;
mod config;
mod lb;
mod operator;
mod rbac;
mod util;

/// Field manager for server-side apply. Also how the operator recognises the
/// objects it owns.
pub const FIELD_MANAGER: &str = "network-operator";

pub const AGENT_DS: &str = "cilium";
pub const OPERATOR_DEPLOY: &str = "cilium-operator";
pub const CONFIG_MAP: &str = "cilium-config";
pub const AGENT_SA: &str = "cilium";
pub const OPERATOR_SA: &str = "cilium-operator";

/// Port the agent serves `/healthz` on (host-network, so it is a host port).
pub const AGENT_HEALTH_PORT: i32 = 9879;

/// One object to apply, plus the type information the dynamic client needs.
#[derive(Clone, Debug)]
pub struct Rendered {
    pub api: ApiResource,
    pub namespace: Option<String>,
    pub obj: DynamicObject,
}

impl Rendered {
    pub fn name(&self) -> &str {
        self.obj.metadata.name.as_deref().unwrap_or_default()
    }

    /// `Kind/name` (or `Kind/ns/name`), for logs and conditions.
    pub fn id(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{}/{}/{}", self.api.kind, ns, self.name()),
            None => format!("{}/{}", self.api.kind, self.name()),
        }
    }
}

/// Render the full object set for a resolved config, in apply order.
pub fn render(cfg: &EffectiveConfig) -> Vec<Rendered> {
    let mut out = Vec::new();
    out.extend(rbac::render(cfg));
    out.push(config::render(cfg));
    out.push(agent::render(cfg));
    out.push(operator::render(cfg));
    // Backed by CRDs that cilium-operator installs, so these are applied last
    // and tolerated as missing until it has.
    out.extend(lb::render(cfg));
    out
}

/// Every object that *some* config would render. The reconciler deletes any of
/// these that the current [`render`] does not include, so that turning a feature
/// off actually removes its objects instead of orphaning them.
///
/// Only the conditional objects need listing — the RBAC, config and workloads
/// are rendered unconditionally and are garbage-collected with the `Network`.
pub fn reapable(cfg: &EffectiveConfig) -> Vec<Rendered> {
    lb::all_variants(cfg)
}

/// The subset of [`render`] whose CRDs are owned by `cilium-operator` and so may
/// legitimately 404 on a fresh install.
pub fn is_cilium_cr(r: &Rendered) -> bool {
    r.api.group == "cilium.io"
}

// --- shared metadata -------------------------------------------------------

/// Labels stamped on every rendered object. `managed-by` is what makes drift
/// attributable and lets an operator find its own objects.
pub fn common_labels(cfg: &EffectiveConfig) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/managed-by".to_string(), FIELD_MANAGER.to_string()),
        ("app.kubernetes.io/part-of".to_string(), "cilium".to_string()),
        ("network.storm.io/owner".to_string(), cfg.network_name.clone()),
    ])
}

/// Owner reference back to the `Network`, so deleting the CR garbage-collects
/// the install. Cluster-scoped owners may own namespaced dependents.
fn owner_ref(cfg: &EffectiveConfig) -> Option<OwnerReference> {
    Some(OwnerReference {
        api_version: "network.storm.io/v1".to_string(),
        kind: "Network".to_string(),
        name: cfg.network_name.clone(),
        uid: cfg.network_uid.clone()?,
        controller: Some(true),
        block_owner_deletion: Some(true),
    })
}

/// Namespaced object metadata with the common labels and owner reference.
pub fn meta(cfg: &EffectiveConfig, name: &str, extra: &[(&str, &str)]) -> ObjectMeta {
    let mut labels = common_labels(cfg);
    for (k, v) in extra {
        labels.insert(k.to_string(), v.to_string());
    }
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: Some(NAMESPACE.to_string()),
        labels: Some(labels),
        owner_references: owner_ref(cfg).map(|o| vec![o]),
        ..Default::default()
    }
}

/// Cluster-scoped object metadata (RBAC, Cilium CRs).
pub fn cluster_meta(cfg: &EffectiveConfig, name: &str, extra: &[(&str, &str)]) -> ObjectMeta {
    let mut m = meta(cfg, name, extra);
    m.namespace = None;
    m
}

// --- typed/dynamic bridging ------------------------------------------------

/// Wrap a typed k8s-openapi object as a [`Rendered`], re-attaching the
/// apiVersion/kind that the typed structs do not carry.
pub fn typed<K>(obj: K) -> Rendered
where
    K: kube::Resource<DynamicType = ()> + Serialize,
{
    let api = ApiResource::erase::<K>(&());
    let mut value = serde_json::to_value(&obj).expect("k8s-openapi objects always serialize");
    let map = value
        .as_object_mut()
        .expect("k8s-openapi objects serialize as maps");
    map.insert("apiVersion".into(), api.api_version.clone().into());
    map.insert("kind".into(), api.kind.clone().into());

    let dynamic: DynamicObject =
        serde_json::from_value(value).expect("a typed object is always a valid DynamicObject");
    let namespace = dynamic.metadata.namespace.clone();
    Rendered { api, namespace, obj: dynamic }
}

/// Build a [`Rendered`] for a resource we have no Rust type for — the Cilium
/// CRs, whose CRDs `cilium-operator` installs at runtime.
pub fn custom(
    group: &str,
    version: &str,
    kind: &str,
    plural: &str,
    metadata: ObjectMeta,
    spec: serde_json::Value,
) -> Rendered {
    let api = ApiResource {
        group: group.to_string(),
        version: version.to_string(),
        api_version: format!("{group}/{version}"),
        kind: kind.to_string(),
        plural: plural.to_string(),
    };
    let namespace = metadata.namespace.clone();
    let obj = DynamicObject {
        types: Some(kube::api::TypeMeta {
            api_version: api.api_version.clone(),
            kind: api.kind.clone(),
        }),
        metadata,
        data: serde_json::json!({ "spec": spec }),
    };
    Rendered { api, namespace, obj }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{Announce, Mode};
    use crate::testutil::cfg_for;

    #[test]
    fn render_is_deterministic() {
        let cfg = cfg_for(Mode::Overlay);
        let a = render(&cfg);
        let b = render(&cfg);
        let ids: Vec<_> = a.iter().map(|r| r.id()).collect();
        assert_eq!(ids, b.iter().map(|r| r.id()).collect::<Vec<_>>());
        assert_eq!(
            serde_json::to_string(&a[0].obj).unwrap(),
            serde_json::to_string(&b[0].obj).unwrap()
        );
    }

    #[test]
    fn every_object_carries_the_managed_by_label() {
        for r in render(&cfg_for(Mode::Overlay)) {
            let labels = r.obj.metadata.labels.as_ref().unwrap();
            assert_eq!(
                labels.get("app.kubernetes.io/managed-by").map(String::as_str),
                Some(FIELD_MANAGER),
                "{} is missing the managed-by label",
                r.id()
            );
        }
    }

    #[test]
    fn cilium_crs_sort_last_because_their_crds_arrive_late() {
        let objs = render(&cfg_for(Mode::Bgp));
        let first_cr = objs.iter().position(is_cilium_cr).unwrap();
        assert!(objs[first_cr..].iter().all(is_cilium_cr));
        assert!(objs[..first_cr].iter().all(|r| !is_cilium_cr(r)));
    }

    #[test]
    fn overlay_renders_no_cilium_crs() {
        let objs = render(&cfg_for(Mode::Overlay));
        assert!(!objs.iter().any(is_cilium_cr));
    }

    #[test]
    fn owner_reference_is_set_when_the_uid_is_known() {
        let mut cfg = cfg_for(Mode::Overlay);
        cfg.network_uid = Some("abc-123".into());
        for r in render(&cfg) {
            let owners = r.obj.metadata.owner_references.as_ref().unwrap();
            assert_eq!(owners[0].uid, "abc-123");
            assert_eq!(owners[0].kind, "Network");
        }

        // Off-cluster renders (golden tests, dry runs) have no uid to point at.
        let cfg = cfg_for(Mode::Overlay);
        assert!(render(&cfg)[0].obj.metadata.owner_references.is_none());
    }

    #[test]
    fn bare_metal_renders_l2_not_bgp() {
        let cfg = cfg_for(Mode::BareMetal);
        assert_eq!(cfg.announce, Announce::L2);
        let kinds: Vec<_> = render(&cfg).iter().map(|r| r.api.kind.clone()).collect();
        assert!(kinds.contains(&"CiliumL2AnnouncementPolicy".to_string()));
        assert!(kinds.contains(&"CiliumLoadBalancerIPPool".to_string()));
        assert!(!kinds.iter().any(|k| k.starts_with("CiliumBGP")));
    }
}
