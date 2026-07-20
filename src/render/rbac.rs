//! ServiceAccounts and cluster RBAC for the agent and the operator.
//!
//! The rules are the minimum Cilium needs to run in CRD-identity mode: the
//! agent reads cluster state and owns its own CRs; the operator additionally
//! installs the Cilium CRDs and garbage-collects identities and endpoints.

use k8s_openapi::api::core::v1::ServiceAccount;
use k8s_openapi::api::rbac::v1::{
    ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding, RoleRef, Subject,
};

use crate::modes::{EffectiveConfig, NAMESPACE};

use super::{cluster_meta, meta, typed, Rendered, AGENT_SA, OPERATOR_SA};

/// Namespaced Role letting the agent read `cilium-config`.
///
/// The agent's `build-config` init container reads the ConfigMap through the
/// API, not from its mounted volume, so it needs an explicit grant. Upstream
/// scopes this to a namespaced Role rather than widening the ClusterRole, and
/// so do we — the agent has no business reading ConfigMaps cluster-wide.
const CONFIG_AGENT_ROLE: &str = "cilium-config-agent";

pub fn render(cfg: &EffectiveConfig) -> Vec<Rendered> {
    vec![
        typed(service_account(cfg, AGENT_SA)),
        typed(service_account(cfg, OPERATOR_SA)),
        typed(cluster_role(cfg, AGENT_SA, agent_rules())),
        typed(cluster_role(cfg, OPERATOR_SA, operator_rules())),
        typed(binding(cfg, AGENT_SA)),
        typed(binding(cfg, OPERATOR_SA)),
        typed(config_agent_role(cfg)),
        typed(config_agent_role_binding(cfg)),
    ]
}

fn config_agent_role(cfg: &EffectiveConfig) -> Role {
    Role {
        metadata: meta(cfg, CONFIG_AGENT_ROLE, &[]),
        rules: Some(vec![rule(&[""], &["configmaps"], &["get", "list", "watch"])]),
    }
}

fn config_agent_role_binding(cfg: &EffectiveConfig) -> RoleBinding {
    RoleBinding {
        metadata: meta(cfg, CONFIG_AGENT_ROLE, &[]),
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "Role".to_string(),
            name: CONFIG_AGENT_ROLE.to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: AGENT_SA.to_string(),
            namespace: Some(NAMESPACE.to_string()),
            api_group: None,
        }]),
    }
}

fn service_account(cfg: &EffectiveConfig, name: &str) -> ServiceAccount {
    ServiceAccount { metadata: meta(cfg, name, &[]), ..Default::default() }
}

fn cluster_role(cfg: &EffectiveConfig, name: &str, rules: Vec<PolicyRule>) -> ClusterRole {
    ClusterRole {
        metadata: cluster_meta(cfg, name, &[]),
        rules: Some(rules),
        ..Default::default()
    }
}

fn binding(cfg: &EffectiveConfig, name: &str) -> ClusterRoleBinding {
    ClusterRoleBinding {
        metadata: cluster_meta(cfg, name, &[]),
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "ClusterRole".to_string(),
            name: name.to_string(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: name.to_string(),
            namespace: Some(NAMESPACE.to_string()),
            api_group: None,
        }]),
    }
}

fn rule(groups: &[&str], resources: &[&str], verbs: &[&str]) -> PolicyRule {
    PolicyRule {
        api_groups: Some(groups.iter().map(|s| s.to_string()).collect()),
        resources: Some(resources.iter().map(|s| s.to_string()).collect()),
        verbs: verbs.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn agent_rules() -> Vec<PolicyRule> {
    vec![
        rule(&[""], &["namespaces", "services", "pods", "endpoints", "nodes"], &["get", "list", "watch"]),
        rule(&[""], &["pods", "pods/finalizers"], &["get", "list", "watch", "update", "delete"]),
        rule(&[""], &["nodes", "nodes/status"], &["patch"]),
        rule(&[""], &["secrets"], &["get", "list", "watch"]),
        rule(&["discovery.k8s.io"], &["endpointslices"], &["get", "list", "watch"]),
        rule(
            &["networking.k8s.io"],
            &["networkpolicies"],
            &["get", "list", "watch"],
        ),
        rule(
            &["apiextensions.k8s.io"],
            &["customresourcedefinitions"],
            &["list", "watch", "get"],
        ),
        rule(
            &["cilium.io"],
            &[
                "ciliumnetworkpolicies",
                "ciliumclusterwidenetworkpolicies",
                "ciliumendpoints",
                "ciliumendpointslices",
                "ciliumnodes",
                "ciliumidentities",
                "ciliumloadbalancerippools",
                "ciliuml2announcementpolicies",
                "ciliumbgpclusterconfigs",
                "ciliumbgppeerconfigs",
                "ciliumbgpadvertisements",
                "ciliumbgpnodeconfigs",
                "ciliumcidrgroups",
                "ciliumclusterwideenvoyconfigs",
                "ciliumenvoyconfigs",
                "ciliumpodippools",
            ],
            &["get", "list", "watch", "create", "update", "patch", "delete"],
        ),
        // Read-only policy/config CRs the agent watches but never writes.
        rule(
            &["cilium.io"],
            &[
                "ciliumbgppeeringpolicies",
                "ciliumegressgatewaypolicies",
                "ciliumlocalredirectpolicies",
                "ciliumnodeconfigs",
            ],
            &["list", "watch"],
        ),
        rule(
            &["cilium.io"],
            &[
                "ciliumnetworkpolicies/status",
                "ciliumclusterwidenetworkpolicies/status",
                "ciliumendpoints/status",
                "ciliumnodes/status",
                "ciliumbgpnodeconfigs/status",
                "ciliuml2announcementpolicies/status",
            ],
            &["patch", "update"],
        ),
        rule(&["cilium.io"], &["ciliumnodes/status"], &["get"]),
        // Leases back the agent-side leader elections (L2 announcement leader,
        // among others).
        rule(
            &["coordination.k8s.io"],
            &["leases"],
            &["create", "get", "update", "list", "delete"],
        ),
    ]
}

fn operator_rules() -> Vec<PolicyRule> {
    vec![
        rule(&[""], &["pods", "nodes", "namespaces", "services", "endpoints"], &["get", "list", "watch"]),
        rule(&[""], &["pods"], &["delete"]),
        rule(&[""], &["nodes", "nodes/status"], &["patch", "update"]),
        rule(&[""], &["secrets"], &["get", "list", "watch", "create", "update", "delete"]),
        rule(&[""], &["events"], &["create", "patch", "update"]),
        // Writes the LB-IPAM-assigned VIP back onto the Service.
        rule(&[""], &["services/status"], &["update", "patch"]),
        rule(&[""], &["configmaps"], &["get", "list", "watch", "patch"]),
        rule(&["discovery.k8s.io"], &["endpointslices"], &["get", "list", "watch"]),
        rule(
            &["networking.k8s.io"],
            &["networkpolicies", "ingresses", "ingressclasses"],
            &["get", "list", "watch"],
        ),
        // The operator installs and then owns the Cilium CRDs — which is why the
        // Cilium CRs we render can 404 until it has run once.
        rule(
            &["apiextensions.k8s.io"],
            &["customresourcedefinitions"],
            &["create", "get", "list", "watch", "update", "patch"],
        ),
        rule(&["cilium.io"], &["*"], &["*"]),
        rule(
            &["coordination.k8s.io"],
            &["leases"],
            &["create", "get", "update", "list", "delete"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::Mode;
    use crate::testutil::cfg_for;

    #[test]
    fn renders_paired_sa_role_and_binding_for_both_identities() {
        let objs = render(&cfg_for(Mode::Overlay));
        let ids: Vec<_> = objs.iter().map(|r| r.id()).collect();
        assert_eq!(
            ids,
            vec![
                "ServiceAccount/kube-system/cilium",
                "ServiceAccount/kube-system/cilium-operator",
                "ClusterRole/cilium",
                "ClusterRole/cilium-operator",
                "ClusterRoleBinding/cilium",
                "ClusterRoleBinding/cilium-operator",
                "Role/kube-system/cilium-config-agent",
                "RoleBinding/kube-system/cilium-config-agent",
            ]
        );
    }

    /// #6: `build-config` reads cilium-config through the API and exited 1 with
    /// "not allowed to get configmaps". Granted via a namespaced Role, the way
    /// upstream does it — the agent must not read ConfigMaps cluster-wide.
    #[test]
    fn agent_can_read_its_own_config_map_but_only_in_its_namespace() {
        let role = config_agent_role(&cfg_for(Mode::Overlay));
        let r = &role.rules.unwrap()[0];
        assert_eq!(r.resources.as_ref().unwrap(), &["configmaps"]);
        assert!(r.verbs.contains(&"get".to_string()));
        assert_eq!(role.metadata.namespace.as_deref(), Some(NAMESPACE));

        let rb = config_agent_role_binding(&cfg_for(Mode::Overlay));
        assert_eq!(rb.role_ref.kind, "Role");
        assert_eq!(rb.role_ref.name, CONFIG_AGENT_ROLE);
        let s = &rb.subjects.unwrap()[0];
        assert_eq!(s.name, AGENT_SA);
        assert_eq!(s.namespace.as_deref(), Some(NAMESPACE));

        // Cluster-wide ConfigMap access stays off the agent's ClusterRole.
        assert!(!agent_rules().iter().any(|r| r
            .resources
            .as_ref()
            .is_some_and(|rs| rs.contains(&"configmaps".to_string()))));
    }

    /// LB-IPAM allocates a VIP and writes it to Service.status; without this the
    /// address is allocated and never surfaces on the Service.
    #[test]
    fn operator_can_write_service_status_and_read_config() {
        let rules = operator_rules();
        let has = |res: &str, verb: &str| {
            rules.iter().any(|r| {
                r.resources.as_ref().is_some_and(|rs| rs.contains(&res.to_string()))
                    && r.verbs.contains(&verb.to_string())
            })
        };
        assert!(has("services/status", "update"), "LB-IPAM cannot publish the VIP");
        assert!(has("services/status", "patch"));
        assert!(has("configmaps", "get"));
    }

    #[test]
    fn bindings_point_at_the_kube_system_service_accounts() {
        let b = binding(&cfg_for(Mode::Overlay), AGENT_SA);
        let subject = &b.subjects.unwrap()[0];
        assert_eq!(subject.kind, "ServiceAccount");
        assert_eq!(subject.name, AGENT_SA);
        assert_eq!(subject.namespace.as_deref(), Some(NAMESPACE));
        assert_eq!(b.role_ref.name, AGENT_SA);
    }

    #[test]
    fn agent_can_write_the_bgp_and_lb_crs_it_is_handed() {
        let rules = agent_rules();
        let cilium = rules
            .iter()
            .find(|r| {
                r.api_groups.as_ref().unwrap() == &["cilium.io"]
                    && r.verbs.contains(&"watch".to_string())
                    && r.resources
                        .as_ref()
                        .unwrap()
                        .contains(&"ciliumbgpclusterconfigs".to_string())
            })
            .expect("agent needs the cilium.io rule");
        for want in ["ciliumloadbalancerippools", "ciliuml2announcementpolicies"] {
            assert!(cilium.resources.as_ref().unwrap().contains(&want.to_string()));
        }
    }

    #[test]
    fn only_the_operator_may_create_crds() {
        let crd_verbs = |rules: &[PolicyRule]| -> Vec<String> {
            rules
                .iter()
                .filter(|r| {
                    r.resources
                        .as_ref()
                        .is_some_and(|rs| rs.contains(&"customresourcedefinitions".to_string()))
                })
                .flat_map(|r| r.verbs.clone())
                .collect()
        };
        assert!(crd_verbs(&operator_rules()).contains(&"create".to_string()));
        assert!(!crd_verbs(&agent_rules()).contains(&"create".to_string()));
    }
}
