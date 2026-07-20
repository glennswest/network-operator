//! Health aggregation: cluster observations -> `Available`/`Progressing`/
//! `Degraded` conditions, with cluster-operator semantics.
//!
//! [`observe`] does the reading; [`conditions`] is a pure function of the
//! resulting [`Snapshot`], so every rule below is testable without a cluster.

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::api::{Api, ListParams};
use kube::{Client, Error as KubeError};

use crate::crd::conditions::{AVAILABLE, DEGRADED, PROGRESSING};
use crate::modes::NAMESPACE;
use crate::render::{AGENT_DS, OPERATOR_DEPLOY};

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

/// Read the current state of the install.
pub async fn observe(client: &Client, deferred: Vec<String>) -> Result<Snapshot, KubeError> {
    let ds: Api<DaemonSet> = Api::namespaced(client.clone(), NAMESPACE);
    let deploys: Api<Deployment> = Api::namespaced(client.clone(), NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);

    let agent = optional(ds.get(AGENT_DS).await)?.map(|d| {
        let s = d.status.unwrap_or_default();
        Rollout {
            desired: s.desired_number_scheduled,
            ready: s.number_ready,
            updated: s.updated_number_scheduled.unwrap_or(0),
        }
    });

    let operator = optional(deploys.get(OPERATOR_DEPLOY).await)?.map(|d| {
        let desired = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
        let s = d.status.unwrap_or_default();
        Rollout {
            desired,
            ready: s.ready_replicas.unwrap_or(0),
            updated: s.updated_replicas.unwrap_or(0),
        }
    });

    let crashlooping = pods
        .list(&ListParams::default().labels("k8s-app=cilium"))
        .await?
        .items
        .into_iter()
        .filter(is_crashlooping)
        .filter_map(|p| p.metadata.name)
        .collect();

    Ok(Snapshot { agent, operator, deferred, failure: None, crashlooping })
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
    } else {
        Draft {
            type_: AVAILABLE,
            status: true,
            reason: "AsExpected",
            message: format!("{}/{} cilium agents ready", agent.ready, agent.desired),
        }
    };

    // --- Progressing: work is still in flight. ---
    let progressing = if snap.agent.is_none() || snap.operator.is_none() {
        Draft {
            type_: PROGRESSING,
            status: true,
            reason: "Installing",
            message: "Creating the Cilium workloads".to_string(),
        }
    } else if !snap.deferred.is_empty() {
        Draft {
            type_: PROGRESSING,
            status: true,
            reason: "WaitingForCiliumCRDs",
            message: format!(
                "waiting on cilium-operator to register: {}",
                snap.deferred.join(", ")
            ),
        }
    } else if !agent.settled() || !operator.settled() {
        Draft {
            type_: PROGRESSING,
            status: true,
            reason: "RolloutInProgress",
            message: format!(
                "agents {}/{} ready ({} updated); operator {}/{} ready",
                agent.ready, agent.desired, agent.updated, operator.ready, operator.desired
            ),
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

    fn healthy() -> Snapshot {
        Snapshot {
            agent: rollout(3, 3),
            operator: rollout(1, 1),
            ..Default::default()
        }
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
