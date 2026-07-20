//! ServiceAccounts and cluster RBAC for the agent and the operator.
//!
//! The rules are the minimum Cilium needs to run in CRD-identity mode: the
//! agent reads cluster state and owns its own CRs; the operator additionally
//! installs the Cilium CRDs and garbage-collects identities and endpoints.

use k8s_openapi::api::core::v1::ServiceAccount;
use k8s_openapi::api::rbac::v1::{ClusterRole, ClusterRoleBinding, PolicyRule, RoleRef, Subject};

use crate::modes::{EffectiveConfig, NAMESPACE};

use super::{cluster_meta, meta, typed, Rendered, AGENT_SA, OPERATOR_SA};

pub fn render(cfg: &EffectiveConfig) -> Vec<Rendered> {
    vec![
        typed(service_account(cfg, AGENT_SA)),
        typed(service_account(cfg, OPERATOR_SA)),
        typed(cluster_role(cfg, AGENT_SA, agent_rules())),
        typed(cluster_role(cfg, OPERATOR_SA, operator_rules())),
        typed(binding(cfg, AGENT_SA)),
        typed(binding(cfg, OPERATOR_SA)),
    ]
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
        rule(
            &["cilium.io"],
            &[
                "ciliumnetworkpolicies/status",
                "ciliumclusterwidenetworkpolicies/status",
                "ciliumendpoints/status",
                "ciliumnodes/status",
                "ciliumbgpnodeconfigs/status",
            ],
            &["patch", "update"],
        ),
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
            ]
        );
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
