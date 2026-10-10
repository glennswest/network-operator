//! `drift-heal` (declared, 600 s): network-operator puts its own objects
//! back. The one **disruptive** suite (network-operator#19, stormcentral#501):
//! it writes to two named `kube-system` objects, so it is declared in
//! `test/requires.toml` (`kube_system_writes`), run serially and only on test
//! machines, and the runner snapshots and restores those objects around it.
//!
//! - `network-status`: the install is healthy before anything is touched;
//!   drift-heal is only measured from a healthy install.
//! - `configmap-drift`: `debug` in `ConfigMap/cilium-config` is flipped by
//!   hand; the operator's watch (or its 60 s resync) must set it back.
//! - `daemonset-delete`: `DaemonSet/cilium` is deleted with
//!   `propagationPolicy=Orphan` — the agent pods keep running, so the
//!   network (and this Job's own connection) stays up — and the operator
//!   must recreate it (a new uid) and the new one go Ready on every node.
//! - `network-status-end`: the `Network` is healthy again afterwards.
//!
//! A drift that does not heal in time is put back by the test itself, from
//! the snapshot it took, before the next step; the runner restores as well.

use std::time::{Duration, Instant};

use network_operator::modes::NAMESPACE;
use network_operator::render::{AGENT_DS, CONFIG_MAP};
use serde_json::Value;

use crate::api::{Api, Error};
use crate::ctx::Ctx;
use crate::env::Env;
use crate::network;
use crate::report::{Outcome, Report};

pub const TESTS: [&str; 4] = ["network-status", "configmap-drift", "daemonset-delete", "network-status-end"];

/// The `cilium-config` key flipped: harmless either way, and one the operator
/// always renders (`src/render/config.rs`).
pub const DRIFT_KEY: &str = "debug";
/// How long a heal may take: the watch should answer in seconds; the 60 s
/// resync (`controller::RESYNC`) is the backstop, plus a pass's own time.
const HEAL: Duration = Duration::from_secs(150);

pub async fn run(env: &Env, r: &mut Report) {
    let ctx = match Ctx::setup(env).await {
        Ok(c) => c,
        Err(o) => return r.all(&TESTS, &o),
    };
    let o = match ctx.found.cluster() {
        Some(c) => network::verdict(c),
        None => ctx.found.without().unwrap(),
    };
    let healthy = matches!(o, Outcome::Pass(_));
    r.record(TESTS[0], o, 0, None);
    if let Some(skip) = ctx.not_ours() {
        return r.all(&TESTS[1..], &skip);
    }
    if !healthy {
        return r.all(&TESTS[1..3], &Outcome::Skip("the install is not healthy before the test; drift-heal is measured only from a healthy one".into()));
    }

    let t = Instant::now();
    let o = configmap(env, &ctx.api).await;
    r.record(TESTS[1], o, t.elapsed().as_millis(), None);

    let t = Instant::now();
    let o = daemonset(env, &ctx.api).await;
    r.record(TESTS[2], o, t.elapsed().as_millis(), None);

    let t = Instant::now();
    let o = status_end(env, &ctx.api).await;
    r.record(TESTS[3], o, t.elapsed().as_millis(), None);
}

fn cm_path() -> String {
    format!("/api/v1/namespaces/{NAMESPACE}/configmaps/{CONFIG_MAP}")
}

fn ds_path() -> String {
    format!("/apis/apps/v1/namespaces/{NAMESPACE}/daemonsets/{AGENT_DS}")
}

/// A refused write to kube-system is the runner not granting the declared
/// `kube_system_writes` (stormcentral#501): could not run, never a pass.
fn denied(e: Error) -> Outcome {
    match e {
        Error::Forbidden(m) => Outcome::Infra(format!("{m} — the runner does not grant the kube_system_writes this suite declares (stormcentral#501)")),
        Error::Other(m) => Outcome::Infra(m),
    }
}

/// The value to drift `debug` to: the other boolean.
pub fn flipped(v: Option<&str>) -> &'static str {
    if v == Some("true") { "false" } else { "true" }
}

async fn configmap(env: &Env, api: &Api) -> Outcome {
    let path = cm_path();
    let snap = match api.get(&path).await {
        Ok(Some(v)) => v,
        Ok(None) => return Outcome::Fail(format!("ConfigMap {NAMESPACE}/{CONFIG_MAP} does not exist on a healthy install")),
        Err(e) => return denied(e),
    };
    let want = snap["data"][DRIFT_KEY].as_str().map(str::to_string);
    let Some(want) = want else {
        return Outcome::Fail(format!("{CONFIG_MAP} has no {DRIFT_KEY:?} key; the operator always renders it"));
    };
    let mut drifted = snap.clone();
    drifted["data"][DRIFT_KEY] = Value::String(flipped(Some(&want)).into());
    if let Err(e) = api.replace(&path, &drifted).await {
        return denied(e);
    }
    let t = Instant::now();
    let deadline = t + env.wait(HEAL, Duration::from_secs(120));
    loop {
        match api.get(&path).await {
            Ok(Some(v)) if v["data"][DRIFT_KEY].as_str() == Some(want.as_str()) => {
                return Outcome::Pass(format!("{DRIFT_KEY} set to {:?} by hand; back to {want:?} in {} ms", flipped(Some(&want)), t.elapsed().as_millis()));
            }
            Ok(_) | Err(Error::Other(_)) if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(500)).await,
            Err(e @ Error::Forbidden(_)) => return denied(e),
            _ => break,
        }
    }
    // Not healed: put it back ourselves, then fail.
    let back = match api.get(&path).await {
        Ok(Some(mut cur)) => {
            cur["data"][DRIFT_KEY] = Value::String(want.clone());
            api.replace(&path, &cur).await.map(|_| "put back by the test").unwrap_or("NOT put back: the runner's restore must")
        }
        _ => "NOT put back: the runner's restore must",
    };
    Outcome::Fail(format!("{DRIFT_KEY} drifted to {:?} was not restored to {want:?} within {} s ({back})", flipped(Some(&want)), HEAL.as_secs()))
}

/// The DaemonSet as it can be created again from a snapshot: no uid,
/// resourceVersion, status or server-set metadata.
pub fn recreatable(snap: &Value) -> Value {
    let mut v = snap.clone();
    if let Some(m) = v["metadata"].as_object_mut() {
        for k in ["uid", "resourceVersion", "creationTimestamp", "generation", "managedFields", "deletionTimestamp", "deletionGracePeriodSeconds", "selfLink"] {
            m.remove(k);
        }
    }
    if let Some(o) = v.as_object_mut() {
        o.remove("status");
    }
    v
}

/// A DaemonSet replacing `old_uid` that has rolled out: its generation
/// observed and every scheduled pod ready. `Err` says what it is waiting on.
pub fn healed(ds: &Value, old_uid: &str) -> Result<(), String> {
    let uid = ds["metadata"]["uid"].as_str().unwrap_or("");
    if uid.is_empty() || uid == old_uid {
        return Err(if ds["metadata"]["deletionTimestamp"].is_null() { "still the old DaemonSet".into() } else { "the old DaemonSet is being deleted".into() });
    }
    let s = &ds["status"];
    let gen = ds["metadata"]["generation"].as_i64();
    if gen.is_some() && s["observedGeneration"].as_i64() != gen {
        return Err(format!("new DaemonSet {uid}: generation {gen:?} not yet observed"));
    }
    let desired = s["desiredNumberScheduled"].as_i64().unwrap_or(0);
    let ready = s["numberReady"].as_i64().unwrap_or(0);
    if desired == 0 || ready < desired {
        return Err(format!("new DaemonSet {uid}: {ready}/{desired} ready"));
    }
    Ok(())
}

async fn daemonset(env: &Env, api: &Api) -> Outcome {
    let path = ds_path();
    let snap = match api.get(&path).await {
        Ok(Some(v)) => v,
        Ok(None) => return Outcome::Fail(format!("DaemonSet {NAMESPACE}/{AGENT_DS} does not exist on a healthy install")),
        Err(e) => return denied(e),
    };
    let old_uid = snap["metadata"]["uid"].as_str().unwrap_or("").to_string();
    if let Err(e) = api.delete_orphan(&path).await {
        return denied(e);
    }
    let t = Instant::now();
    let deadline = t + env.wait(HEAL, Duration::from_secs(60));
    let mut last = String::from("not listed");
    loop {
        match api.get(&path).await {
            Ok(Some(v)) => match healed(&v, &old_uid) {
                Ok(()) => {
                    let n = v["status"]["numberReady"].as_i64().unwrap_or(0);
                    return Outcome::Pass(format!("{AGENT_DS} deleted (orphaning its pods); recreated as {} and {n}/{n} ready in {} ms", v["metadata"]["uid"].as_str().unwrap_or("?"), t.elapsed().as_millis()));
                }
                Err(w) => last = w,
            },
            Ok(None) => last = "not recreated".into(),
            Err(e @ Error::Forbidden(_)) => return denied(e),
            Err(Error::Other(m)) => last = m,
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // Not healed: if it is gone, create it again from the snapshot.
    let back = match api.get(&path).await {
        Ok(None) => {
            let coll = path.rsplit_once('/').map(|(c, _)| c.to_string()).unwrap_or_default();
            api.create(&coll, &recreatable(&snap)).await.map(|_| "recreated by the test from its snapshot").unwrap_or("NOT recreated: the runner's restore must")
        }
        _ => "left as it is",
    };
    Outcome::Fail(format!("{AGENT_DS} not healed within {} s; last: {last} ({back})", HEAL.as_secs()))
}

/// The `Network` reports a healthy, current install again. The heal may
/// still be settling, so it is polled for a while.
async fn status_end(env: &Env, api: &Api) -> Outcome {
    let deadline = Instant::now() + env.wait(Duration::from_secs(90), Duration::from_secs(5));
    loop {
        let o = match network::find(api).await.cluster() {
            Some(c) => network::verdict(c),
            None => Outcome::Fail("the Network could not be read after the drift".into()),
        };
        if matches!(o, Outcome::Pass(_)) || Instant::now() >= deadline {
            return o;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn debug_is_flipped_to_the_other_boolean() {
        assert_eq!(flipped(Some("false")), "true");
        assert_eq!(flipped(Some("true")), "false");
        assert_eq!(flipped(None), "true");
    }

    #[test]
    fn the_snapshot_recreates_without_server_fields() {
        let s = json!({"apiVersion": "apps/v1", "kind": "DaemonSet",
            "metadata": {"name": "cilium", "namespace": "kube-system", "uid": "u1", "resourceVersion": "9", "generation": 3,
                         "creationTimestamp": "2026-10-10T00:00:00Z", "labels": {"k8s-app": "cilium"}, "ownerReferences": [{"uid": "n1"}]},
            "spec": {"selector": {}}, "status": {"numberReady": 1}});
        let r = recreatable(&s);
        assert!(r.get("status").is_none());
        for k in ["uid", "resourceVersion", "generation", "creationTimestamp"] {
            assert!(r["metadata"].get(k).is_none(), "{k}");
        }
        assert_eq!(r["metadata"]["labels"], s["metadata"]["labels"]);
        assert_eq!(r["metadata"]["ownerReferences"], s["metadata"]["ownerReferences"], "the Network still owns it");
    }

    #[test]
    fn healed_means_a_new_uid_rolled_out_everywhere() {
        let ds = |uid: &str, gen: i64, obs: i64, desired: i64, ready: i64| {
            json!({"metadata": {"uid": uid, "generation": gen},
                   "status": {"observedGeneration": obs, "desiredNumberScheduled": desired, "numberReady": ready}})
        };
        assert!(healed(&ds("u1", 1, 1, 3, 3), "u1").is_err(), "the same object is not a heal");
        assert!(healed(&ds("u2", 1, 0, 3, 3), "u1").unwrap_err().contains("not yet observed"));
        assert!(healed(&ds("u2", 1, 1, 3, 2), "u1").unwrap_err().contains("2/3"));
        assert!(healed(&ds("u2", 1, 1, 0, 0), "u1").is_err(), "nothing scheduled is not ready");
        assert!(healed(&ds("u2", 1, 1, 3, 3), "u1").is_ok());
        let mut old = ds("u1", 1, 1, 3, 3);
        old["metadata"]["deletionTimestamp"] = json!("2026-10-10T00:00:00Z");
        assert!(healed(&old, "u1").unwrap_err().contains("being deleted"));
    }

    #[test]
    fn a_refused_write_names_the_runner_issue() {
        assert!(matches!(denied(Error::Forbidden("PUT x: 403".into())), Outcome::Infra(m) if m.contains("stormcentral#501")));
    }
}
