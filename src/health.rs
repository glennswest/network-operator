//! Health aggregation: cluster observations -> `Available`/`Progressing`/
//! `Degraded` conditions, with cluster-operator semantics.
//!
//! [`observe`] does the reading; [`conditions`] is a pure function of the
//! resulting [`Snapshot`], so every rule below is testable without a cluster.

use std::collections::BTreeSet;

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::api::{Api, ApiResource, DynamicObject, ListParams};
use kube::{Client, Error as KubeError};

use crate::crd::conditions::{AVAILABLE, DEGRADED, PROGRESSING};
use crate::modes::NAMESPACE;
use crate::render::{AGENT_DS, ENVOY_DS, OPERATOR_DEPLOY};

/// The API group whose CRDs `cilium-operator` registers.
const CILIUM_GROUP: &str = "cilium.io";

/// Restarts before a crash-looping pod is called `Degraded` rather than
/// `Progressing`. Cilium legitimately restarts once or twice while the host is
/// still being prepared.
const CRASHLOOP_RESTARTS: i32 = 3;

/// What the reconciler saw of the install this pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// `None` when the DaemonSet does not exist yet.
    pub agent: Option<Rollout>,
    /// `None` when the Deployment does not exist yet.
    pub operator: Option<Rollout>,
    /// Cilium CRs whose CRDs are not registered yet.
    pub deferred: Vec<String>,
    /// Set when this pass failed to render or apply.
    pub failure: Option<String>,
    /// Pods that have restarted past [`CRASHLOOP_RESTARTS`].
    pub crashlooping: Vec<String>,
    /// Whether this config runs the standalone `cilium-envoy` DaemonSet.
    pub envoy_wanted: bool,
    /// `None` when the envoy DaemonSet does not exist (or is not wanted).
    pub envoy: Option<Rollout>,
    /// `cilium.io` CRDs that exist but are not `Established=True`.
    pub unestablished_crds: Vec<String>,
    /// Nodes with a ready agent pod.
    pub agent_nodes: Vec<String>,
    /// Names of the `CiliumNode` objects; `None` while that kind is not
    /// registered yet.
    pub cilium_nodes: Option<Vec<String>>,
}

/// Rollout progress of one workload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rollout {
    pub desired: i32,
    pub ready: i32,
    /// Replicas already running the current pod template.
    pub updated: i32,
}

impl Rollout {
    fn settled(&self) -> bool {
        self.desired > 0 && self.ready >= self.desired && self.updated >= self.desired
    }
}

/// Read the current state of the install. `envoy_wanted` is whether the
/// config being reconciled runs the standalone `cilium-envoy` DaemonSet; it is
/// only observed then, since a disabled one is being reaped, not served.
pub async fn observe(
    client: &Client,
    deferred: Vec<String>,
    envoy_wanted: bool,
) -> Result<Snapshot, KubeError> {
    let ds: Api<DaemonSet> = Api::namespaced(client.clone(), NAMESPACE);
    let deploys: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);

    let agent = optional(ds.get(AGENT_DS).await)?.map(daemonset_rollout);
    let envoy = if envoy_wanted {
        optional(ds.get(ENVOY_DS).await)?.map(daemonset_rollout)
    } else {
        None
    };

    let operator = optional(deploys.get(OPERATOR_DEPLOY).await)?.map(|d| {
        let desired = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
        let s = d.status.unwrap_or_default();
        Rollout {
            desired,
            ready: s.ready_replicas.unwrap_or(0),
            updated: s.updated_replicas.unwrap_or(0),
        }
    });

    let agent_pods = pods
        .list(&ListParams::default().labels("k8s-app=cilium"))
        .await?
        .items;
    let agent_nodes = agent_pods
        .iter()
        .filter(|p| is_ready(p))
        .filter_map(|p| p.spec.as_ref()?.node_name.clone())
        .collect();
    let mut crashlooping: Vec<String> = agent_pods
        .into_iter()
        .filter(is_crashlooping)
        .filter_map(|p| p.metadata.name)
        .collect();
    if envoy_wanted {
        crashlooping.extend(
            pods.list(&ListParams::default().labels("k8s-app=cilium-envoy"))
                .await?
                .items
                .into_iter()
                .filter(is_crashlooping)
                .filter_map(|p| p.metadata.name),
        );
    }

    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let unestablished_crds = crds
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter(|c| c.spec.group == CILIUM_GROUP && !is_established(c))
        .filter_map(|c| c.metadata.name)
        .collect();

    // A 404 here is the kind not being registered yet, which `optional`
    // turns into `None` — on a fresh install that is expected.
    let cilium_nodes: Api<DynamicObject> = Api::all_with(client.clone(), &cilium_node_resource());
    let cilium_nodes = optional(cilium_nodes.list(&ListParams::default()).await)?
        .map(|l| l.items.into_iter().filter_map(|n| n.metadata.name).collect());

    Ok(Snapshot {
        agent,
        operator,
        deferred,
        failure: None,
        crashlooping,
        envoy_wanted,
        envoy,
        unestablished_crds,
        agent_nodes,
        cilium_nodes,
    })
}

fn daemonset_rollout(d: DaemonSet) -> Rollout {
    let s = d.status.unwrap_or_default();
    Rollout {
        desired: s.desired_number_scheduled,
        ready: s.number_ready,
        updated: s.updated_number_scheduled.unwrap_or(0),
    }
}

/// `cilium.io/v2 CiliumNode`, cluster-scoped; one per node, created by the
/// agent on that node once it has registered with the cluster.
fn cilium_node_resource() -> ApiResource {
    ApiResource {
        group: CILIUM_GROUP.to_string(),
        version: "v2".to_string(),
        api_version: format!("{CILIUM_GROUP}/v2"),
        kind: "CiliumNode".to_string(),
        plural: "ciliumnodes".to_string(),
    }
}

fn is_established(crd: &CustomResourceDefinition) -> bool {
    crd.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|c| c.type_ == "Established" && c.status == "True")
}

fn is_ready(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .into_iter()
        .flatten()
        .any(|c| c.type_ == "Ready" && c.status == "True")
}

/// `Ok(None)` for a 404, so "not created yet" is not an error.
fn optional<T>(res: Result<T, KubeError>) -> Result<Option<T>, KubeError> {
    match res {
        Ok(v) => Ok(Some(v)),
        Err(KubeError::Api(e)) if e.code == 404 => Ok(None),
        Err(e) => Err(e),
    }
}

fn is_crashlooping(pod: &Pod) -> bool {
    let Some(status) = &pod.status else { return false };
    status
        .container_statuses
        .iter()
        .flatten()
        .any(|c| {
            c.restart_count >= CRASHLOOP_RESTARTS
                && c.state
                    .as_ref()
                    .and_then(|s| s.waiting.as_ref())
                    .and_then(|w| w.reason.as_deref())
                    == Some("CrashLoopBackOff")
        })
}

/// Derive the three conditions. `prev` is the CR's current conditions, used only
/// to preserve `lastTransitionTime` across passes where the status did not flip.
pub fn conditions(
    prev: &[Condition],
    snap: &Snapshot,
    generation: i64,
    now: &Time,
) -> Vec<Condition> {
    let (available, progressing, degraded) = evaluate(snap);
    [available, progressing, degraded]
        .into_iter()
        .map(|c| finish(prev, c, generation, now))
        .collect()
}

/// A condition before its timestamp has been resolved.
struct Draft {
    type_: &'static str,
    status: bool,
    reason: &'static str,
    message: String,
}

fn evaluate(snap: &Snapshot) -> (Draft, Draft, Draft) {
    let agent = snap.agent.unwrap_or_default();
    let operator = snap.operator.unwrap_or_default();
    let envoy = snap.envoy.unwrap_or_default();
    let envoy_missing = snap.envoy_wanted && snap.envoy.is_none();
    let envoy_settled = !snap.envoy_wanted || envoy.settled();
    let nodes_without_ciliumnode = missing_cilium_nodes(snap);

    // --- Degraded: something is wrong that will not fix itself. ---
    let degraded = if let Some(failure) = &snap.failure {
        Draft {
            type_: DEGRADED,
            status: true,
            reason: "ReconcileFailed",
            message: failure.clone(),
        }
    } else if !snap.crashlooping.is_empty() {
        Draft {
            type_: DEGRADED,
            status: true,
            reason: "PodsCrashLooping",
            message: format!(
                "{} pod(s) crash-looping past backoff: {}",
                snap.crashlooping.len(),
                snap.crashlooping.join(", ")
            ),
        }
    } else {
        Draft {
            type_: DEGRADED,
            status: false,
            reason: "AsExpected",
            message: "No degraded conditions".to_string(),
        }
    };

    // --- Available: the dataplane is serving on every node. ---
    let available = if snap.agent.is_none() || snap.operator.is_none() {
        Draft {
            type_: AVAILABLE,
            status: false,
            reason: "Installing",
            message: "Cilium workloads have not been created yet".to_string(),
        }
    } else if agent.desired == 0 {
        // A DaemonSet with no nodes to run on is not "available" in any sense a
        // caller can use, even though nothing is failing.
        Draft {
            type_: AVAILABLE,
            status: false,
            reason: "NoSchedulableNodes",
            message: "The cilium DaemonSet has no nodes to schedule on".to_string(),
        }
    } else if agent.ready < agent.desired {
        Draft {
            type_: AVAILABLE,
            status: false,
            reason: "AgentNotReady",
            message: format!("{}/{} cilium agents ready", agent.ready, agent.desired),
        }
    } else if operator.ready < 1 {
        Draft {
            type_: AVAILABLE,
            status: false,
            reason: "OperatorNotReady",
            message: "cilium-operator is not ready".to_string(),
        }
    } else if envoy_missing {
        Draft {
            type_: AVAILABLE,
            status: false,
            reason: "EnvoyNotReady",
            message: "the cilium-envoy DaemonSet has not been created yet".to_string(),
        }
    } else if snap.envoy_wanted && envoy.desired == 0 {
        // The agent has nodes (checked above) but envoy schedules on none, so
        // nothing serves the agent's L7 proxy socket.
        Draft {
            type_: AVAILABLE,
            status: false,
            reason: "EnvoyNotReady",
            message: "the cilium-envoy DaemonSet has no nodes to schedule on".to_string(),
        }
    } else if snap.envoy_wanted && envoy.ready < envoy.desired {
        Draft {
            type_: AVAILABLE,
            status: false,
            reason: "EnvoyNotReady",
            message: format!("{}/{} cilium-envoy pods ready", envoy.ready, envoy.desired),
        }
    } else if !snap.unestablished_crds.is_empty() {
        Draft {
            type_: AVAILABLE,
            status: false,
            reason: "CiliumCRDsNotEstablished",
            message: format!(
                "Cilium CRDs not Established: {}",
                snap.unestablished_crds.join(", ")
            ),
        }
    } else if !nodes_without_ciliumnode.is_empty() {
        Draft {
            type_: AVAILABLE,
            status: false,
            reason: "CiliumNodesMissing",
            message: format!(
                "ready agents with no CiliumNode: {}",
                nodes_without_ciliumnode.join(", ")
            ),
        }
    } else {
        Draft {
            type_: AVAILABLE,
            status: true,
            reason: "AsExpected",
            message: format!("{}/{} cilium agents ready", agent.ready, agent.desired),
        }
    };

    // --- Progressing: work is still in flight. ---
    let progressing = if snap.agent.is_none() || snap.operator.is_none() || envoy_missing {
        Draft {
            type_: PROGRESSING,
            status: true,
            reason: "Installing",
            message: "Creating the Cilium workloads".to_string(),
        }
    } else if !snap.deferred.is_empty() || !snap.unestablished_crds.is_empty() {
        let waiting: Vec<&str> = snap
            .deferred
            .iter()
            .chain(&snap.unestablished_crds)
            .map(String::as_str)
            .collect();
        Draft {
            type_: PROGRESSING,
            status: true,
            reason: "WaitingForCiliumCRDs",
            message: format!("waiting on cilium-operator to register: {}", waiting.join(", ")),
        }
    } else if !agent.settled() || !operator.settled() || !envoy_settled {
        let mut message = format!(
            "agents {}/{} ready ({} updated); operator {}/{} ready",
            agent.ready, agent.desired, agent.updated, operator.ready, operator.desired
        );
        if snap.envoy_wanted {
            message += &format!(
                "; envoy {}/{} ready ({} updated)",
                envoy.ready, envoy.desired, envoy.updated
            );
        }
        Draft {
            type_: PROGRESSING,
            status: true,
            reason: "RolloutInProgress",
            message,
        }
    } else {
        Draft {
            type_: PROGRESSING,
            status: false,
            reason: "AsExpected",
            message: "Cilium is at the desired version".to_string(),
        }
    };

    (available, progressing, degraded)
}

/// Nodes with a ready agent but no `CiliumNode`. The agent creates its
/// `CiliumNode` as it registers, so a ready agent without one is not serving
/// that node. An unregistered kind counts every such node as missing.
fn missing_cilium_nodes(snap: &Snapshot) -> Vec<String> {
    let have: BTreeSet<&str> = snap
        .cilium_nodes
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    snap.agent_nodes
        .iter()
        .filter(|n| !have.contains(n.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Attach the timestamp, carrying `lastTransitionTime` forward when the status
/// has not actually flipped — a condition that re-stamps every pass is useless
/// for answering "how long has this been broken?".
fn finish(prev: &[Condition], d: Draft, generation: i64, now: &Time) -> Condition {
    let status = if d.status { "True" } else { "False" };
    let last_transition_time = prev
        .iter()
        .find(|c| c.type_ == d.type_ && c.status == status)
        .map(|c| c.last_transition_time.clone())
        .unwrap_or_else(|| now.clone());

    Condition {
        type_: d.type_.to_string(),
        status: status.to_string(),
        reason: d.reason.to_string(),
        message: d.message,
        observed_generation: Some(generation),
        last_transition_time,
    }
}

/// Look a condition up by type.
pub fn find<'a>(conditions: &'a [Condition], type_: &str) -> Option<&'a Condition> {
    conditions.iter().find(|c| c.type_ == type_)
}

/// Whether a condition of this type is `True`.
pub fn is_true(conditions: &[Condition], type_: &str) -> bool {
    find(conditions, type_).is_some_and(|c| c.status == "True")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn now() -> Time {
        Time(Utc.with_ymd_and_hms(2026, 7, 20, 12, 0, 0).unwrap())
    }

    fn later() -> Time {
        Time(Utc.with_ymd_and_hms(2026, 7, 20, 13, 0, 0).unwrap())
    }

    fn rollout(desired: i32, ready: i32) -> Option<Rollout> {
        Some(Rollout { desired, ready, updated: ready })
    }

    fn nodes(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn healthy() -> Snapshot {
        Snapshot {
            agent: rollout(3, 3),
            operator: rollout(1, 1),
            agent_nodes: nodes(&["n1", "n2", "n3"]),
            cilium_nodes: Some(nodes(&["n1", "n2", "n3"])),
            ..Default::default()
        }
    }

    fn with_envoy(envoy: Option<Rollout>) -> Snapshot {
        Snapshot { envoy_wanted: true, envoy, ..healthy() }
    }

    fn eval(snap: &Snapshot) -> Vec<Condition> {
        conditions(&[], snap, 1, &now())
    }

    fn status_of(c: &[Condition], t: &str) -> String {
        find(c, t).unwrap().status.clone()
    }

    #[test]
    fn a_settled_install_is_available_and_nothing_else() {
        let c = eval(&healthy());
        assert_eq!(status_of(&c, AVAILABLE), "True");
        assert_eq!(status_of(&c, PROGRESSING), "False");
        assert_eq!(status_of(&c, DEGRADED), "False");
    }

    #[test]
    fn a_fresh_install_is_progressing_not_degraded() {
        let c = eval(&Snapshot::default());
        assert_eq!(status_of(&c, AVAILABLE), "False");
        assert_eq!(status_of(&c, PROGRESSING), "True");
        assert_eq!(status_of(&c, DEGRADED), "False");
        assert_eq!(find(&c, PROGRESSING).unwrap().reason, "Installing");
    }

    #[test]
    fn a_partial_rollout_is_progressing_and_unavailable() {
        let snap = Snapshot { agent: rollout(3, 2), ..healthy() };
        let c = eval(&snap);
        assert_eq!(status_of(&c, AVAILABLE), "False");
        assert_eq!(status_of(&c, PROGRESSING), "True");
        assert_eq!(find(&c, AVAILABLE).unwrap().reason, "AgentNotReady");
        assert!(find(&c, AVAILABLE).unwrap().message.contains("2/3"));
    }

    #[test]
    fn an_upgrade_still_carrying_old_pods_is_progressing_while_available() {
        // Every agent is ready, but some are still on the previous image.
        let snap = Snapshot {
            agent: Some(Rollout { desired: 3, ready: 3, updated: 1 }),
            ..healthy()
        };
        let c = eval(&snap);
        assert_eq!(status_of(&c, AVAILABLE), "True");
        assert_eq!(status_of(&c, PROGRESSING), "True");
        assert_eq!(find(&c, PROGRESSING).unwrap().reason, "RolloutInProgress");
    }

    #[test]
    fn crash_looping_pods_are_degraded() {
        let snap = Snapshot {
            crashlooping: vec!["cilium-abcde".into()],
            ..healthy()
        };
        let c = eval(&snap);
        assert_eq!(status_of(&c, DEGRADED), "True");
        assert_eq!(find(&c, DEGRADED).unwrap().reason, "PodsCrashLooping");
        assert!(find(&c, DEGRADED).unwrap().message.contains("cilium-abcde"));
    }

    #[test]
    fn a_reconcile_failure_outranks_a_crash_loop_in_the_message() {
        let snap = Snapshot {
            failure: Some("apply failed: forbidden".into()),
            crashlooping: vec!["cilium-abcde".into()],
            ..healthy()
        };
        let c = eval(&snap);
        assert_eq!(find(&c, DEGRADED).unwrap().reason, "ReconcileFailed");
        assert!(find(&c, DEGRADED).unwrap().message.contains("forbidden"));
    }

    #[test]
    fn no_schedulable_nodes_is_unavailable_rather_than_vacuously_ready() {
        let snap = Snapshot { agent: rollout(0, 0), ..healthy() };
        let c = eval(&snap);
        assert_eq!(status_of(&c, AVAILABLE), "False");
        assert_eq!(find(&c, AVAILABLE).unwrap().reason, "NoSchedulableNodes");
    }

    #[test]
    fn deferred_cilium_crs_keep_us_progressing() {
        let snap = Snapshot {
            deferred: vec!["CiliumBGPClusterConfig/storm".into()],
            ..healthy()
        };
        let c = eval(&snap);
        assert_eq!(status_of(&c, PROGRESSING), "True");
        assert_eq!(find(&c, PROGRESSING).unwrap().reason, "WaitingForCiliumCRDs");
        // The dataplane is up even though the BGP CR is not applied yet.
        assert_eq!(status_of(&c, AVAILABLE), "True");
    }

    #[test]
    fn an_operator_outage_does_not_hide_a_healthy_dataplane_as_degraded() {
        let snap = Snapshot { operator: rollout(1, 0), ..healthy() };
        let c = eval(&snap);
        assert_eq!(status_of(&c, AVAILABLE), "False");
        assert_eq!(find(&c, AVAILABLE).unwrap().reason, "OperatorNotReady");
        assert_eq!(status_of(&c, DEGRADED), "False");
    }

    #[test]
    fn a_ready_envoy_keeps_the_install_available() {
        let c = eval(&with_envoy(rollout(3, 3)));
        assert_eq!(status_of(&c, AVAILABLE), "True");
        assert_eq!(status_of(&c, PROGRESSING), "False");
    }

    #[test]
    fn a_broken_envoy_makes_the_install_unavailable() {
        let c = eval(&with_envoy(rollout(3, 1)));
        assert_eq!(status_of(&c, AVAILABLE), "False");
        assert_eq!(find(&c, AVAILABLE).unwrap().reason, "EnvoyNotReady");
        assert!(find(&c, AVAILABLE).unwrap().message.contains("1/3"));
        assert_eq!(status_of(&c, PROGRESSING), "True");
        assert!(find(&c, PROGRESSING).unwrap().message.contains("envoy 1/3"));
    }

    #[test]
    fn a_wanted_envoy_not_created_yet_is_installing() {
        let c = eval(&with_envoy(None));
        assert_eq!(find(&c, AVAILABLE).unwrap().reason, "EnvoyNotReady");
        assert_eq!(find(&c, PROGRESSING).unwrap().reason, "Installing");
    }

    #[test]
    fn an_envoy_scheduled_nowhere_is_not_vacuously_ready() {
        let c = eval(&with_envoy(rollout(0, 0)));
        assert_eq!(find(&c, AVAILABLE).unwrap().reason, "EnvoyNotReady");
    }

    #[test]
    fn an_envoy_that_is_not_wanted_is_ignored() {
        // A disabled envoy's DaemonSet, still being reaped, is not observed.
        let snap = Snapshot { envoy: rollout(3, 0), ..healthy() };
        assert_eq!(status_of(&eval(&snap), AVAILABLE), "True");
    }

    #[test]
    fn an_unestablished_cilium_crd_is_unavailable_and_progressing() {
        let snap = Snapshot {
            unestablished_crds: vec!["ciliumendpoints.cilium.io".into()],
            ..healthy()
        };
        let c = eval(&snap);
        assert_eq!(status_of(&c, AVAILABLE), "False");
        assert_eq!(find(&c, AVAILABLE).unwrap().reason, "CiliumCRDsNotEstablished");
        assert_eq!(find(&c, PROGRESSING).unwrap().reason, "WaitingForCiliumCRDs");
        assert!(find(&c, PROGRESSING).unwrap().message.contains("ciliumendpoints"));
    }

    #[test]
    fn a_ready_agent_without_a_ciliumnode_is_unavailable() {
        let snap = Snapshot { cilium_nodes: Some(nodes(&["n1", "n3"])), ..healthy() };
        let c = eval(&snap);
        assert_eq!(status_of(&c, AVAILABLE), "False");
        assert_eq!(find(&c, AVAILABLE).unwrap().reason, "CiliumNodesMissing");
        assert_eq!(find(&c, AVAILABLE).unwrap().message, "ready agents with no CiliumNode: n2");
    }

    #[test]
    fn an_unregistered_ciliumnode_kind_counts_every_agent_node_missing() {
        let snap = Snapshot { cilium_nodes: None, ..healthy() };
        let c = eval(&snap);
        assert_eq!(find(&c, AVAILABLE).unwrap().reason, "CiliumNodesMissing");
        assert!(find(&c, AVAILABLE).unwrap().message.contains("n1, n2, n3"));
    }

    #[test]
    fn a_stale_ciliumnode_for_a_gone_node_does_not_matter() {
        let snap = Snapshot {
            cilium_nodes: Some(nodes(&["n1", "n2", "n3", "gone"])),
            ..healthy()
        };
        assert_eq!(status_of(&eval(&snap), AVAILABLE), "True");
    }

    #[test]
    fn transition_time_is_kept_while_the_status_holds_and_reset_when_it_flips() {
        let first = conditions(&[], &healthy(), 1, &now());
        let unchanged = conditions(&first, &healthy(), 2, &later());
        assert_eq!(
            find(&unchanged, AVAILABLE).unwrap().last_transition_time,
            now()
        );

        let broken = Snapshot { agent: rollout(3, 1), ..healthy() };
        let flipped = conditions(&first, &broken, 3, &later());
        assert_eq!(
            find(&flipped, AVAILABLE).unwrap().last_transition_time,
            later()
        );
    }

    #[test]
    fn observed_generation_tracks_the_spec_we_acted_on() {
        let c = conditions(&[], &healthy(), 7, &now());
        assert!(c.iter().all(|c| c.observed_generation == Some(7)));
    }
}
