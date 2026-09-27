//! `medium` (< 30 min): network-operator's features and failure paths, end
//! to end, from inside the run's namespace. Everything in `short`, then:
//!
//! - `network-config`: `status.applied*` is what the operator's own code
//!   resolves the spec to (the immutability baseline is current);
//! - `service-scale`: a Service follows a Deployment from 1 to 3 backends;
//! - `backend-loss`: a backend deleted is replaced, gets a pod-CIDR address,
//!   and the Service stops sending to the dead one;
//! - `network-policy`: Cilium enforces a NetworkPolicy — deny, allow, lift;
//! - `cross-node`: pod to pod across nodes (needs 2 schedulable nodes);
//! - `load-balancer`: a LoadBalancer Service gets an address from the
//!   operator's LB-IPAM pool and answers (needs LB-IPAM on);
//! - `endpoints-reaped`: Cilium made an endpoint per workload, and drops them
//!   when the workloads go.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::api::{ready_ip, why, Error};
use crate::ctx::{reach, Ctx};
use crate::env::Env;
use crate::network::{self, cidrs, within};
use crate::objects::{self, POD_PORT, SVC_PORT};
use crate::report::{Outcome, Report};
use crate::short;
use crate::workload;

pub const TESTS: [&str; 8] = [
    "network-config",
    "service-scale",
    "backend-loss",
    "network-policy",
    "cross-node",
    "load-balancer",
    "endpoints-reaped",
    "cleanup-all",
];

pub async fn run(env: &Env, r: &mut Report) {
    let ctx = match Ctx::setup(env).await {
        Ok(c) => c,
        Err(o) => {
            r.all(&short::TESTS, &o);
            r.all(&TESTS, &o);
            return;
        }
    };
    short::basics(env, &ctx, r).await;
    if let Some(skip) = ctx.not_ours() {
        r.all(&TESTS, &skip);
        return;
    }

    let o = match ctx.found.cluster() {
        Some(c) => network::consistency(c),
        None => ctx.found.without().unwrap(),
    };
    r.record(TESTS[0], o, 0, None);

    let t = Instant::now();
    let o = service_scale(env, &ctx).await;
    let scaled = matches!(o, Outcome::Pass(_));
    r.record(TESTS[1], o, t.elapsed().as_millis(), None);

    let t = Instant::now();
    let o = if scaled { backend_loss(env, &ctx).await } else { Outcome::Skip("service-scale did not pass: no backends to lose".into()) };
    r.record(TESTS[2], o, t.elapsed().as_millis(), None);

    r.run(TESTS[3], network_policy(env, &ctx)).await;
    r.run(TESTS[4], cross_node(env, &ctx)).await;
    let o = if scaled { load_balancer(env, &ctx).await } else { Outcome::Skip("service-scale did not pass: no backends to put behind a LoadBalancer".into()) };
    r.record(TESTS[5], o, 0, None);
    r.run(TESTS[6], endpoints_reaped(env, &ctx)).await;
    r.run(TESTS[7], ctx.cleanup(Duration::from_secs(60))).await;
}

/// Ready pods of `app`: name → IP.
async fn ready(ctx: &Ctx, app: &str) -> Result<Vec<(String, String)>, Error> {
    Ok(ctx
        .api
        .workload_pods(Some(app))
        .await?
        .iter()
        .filter_map(|p| Some((p["metadata"]["name"].as_str()?.to_string(), ready_ip(p)?)))
        .collect())
}

/// Wait for exactly `n` ready pods of `app` satisfying `ok`.
async fn wait_ready(ctx: &Ctx, app: &str, n: usize, deadline: Instant, ok: impl Fn(&str) -> bool) -> Result<Vec<(String, String)>, Outcome> {
    loop {
        let r: Vec<_> = ready(ctx, app).await.map_err(|e| e.outcome())?.into_iter().filter(|(name, _)| ok(name)).collect();
        if r.len() == n {
            return Ok(r);
        }
        if Instant::now() >= deadline {
            let all = ctx.api.workload_pods(Some(app)).await.unwrap_or_default();
            let states: Vec<String> = all.iter().map(|p| format!("{} {} {}", p["metadata"]["name"].as_str().unwrap_or("?"), p["status"]["phase"].as_str().unwrap_or("?"), why(p))).collect();
            return Err(Outcome::Fail(format!("{} of {n} {app} pods ready in time: {states:?}", r.len())));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Every address in the pod CIDR, when it is known.
fn all_in_cidr(ctx: &Ctx, pods: &[(String, String)]) -> Result<(), Outcome> {
    let c = ctx.pod_cidrs();
    match pods.iter().find(|(_, ip)| within(&c, ip) == Some(false)) {
        Some((n, ip)) => Err(Outcome::Fail(format!("{n} got {ip}, outside the pod network {c:?}"))),
        None => Ok(()),
    }
}

/// Probe `ip:port` until the set of names answering covers `want`.
async fn names_seen(ip: &str, port: u16, want: &BTreeSet<String>, deadline: Instant) -> Result<BTreeSet<String>, String> {
    let a = workload::addr(ip, port)?;
    let mut seen = BTreeSet::new();
    let mut last_err = None;
    while !want.is_subset(&seen) {
        match workload::probe(a, Duration::from_secs(3)).await {
            Ok((n, _)) => {
                seen.insert(n);
            }
            Err(e) => last_err = Some(e),
        }
        if Instant::now() >= deadline {
            return Err(format!("saw {seen:?} of {want:?}; last error: {last_err:?}"));
        }
    }
    Ok(seen)
}

async fn service_scale(env: &Env, ctx: &Ctx) -> Outcome {
    let (api, d, svc) = (&ctx.api, env.name("d"), env.name("svc-d"));
    let deadline = |s| Instant::now() + env.wait(Duration::from_secs(s), Duration::from_secs(300));
    let dep = objects::deployment(&api.ns, &api.run, &d, "d", &ctx.me.image, 1);
    if let Err(e) = api.create(&api.deployments(), &dep).await {
        return e.outcome();
    }
    if let Err(o) = wait_ready(ctx, "d", 1, deadline(120), |_| true).await {
        return o;
    }
    let ip = match ctx.start_service(&svc, "d", "ClusterIP", deadline(30)).await {
        Ok(ip) => ip,
        Err(o) => return o,
    };
    if let Err(o) = scale(ctx, &d, 3).await {
        return o;
    }
    let pods = match wait_ready(ctx, "d", 3, deadline(120), |_| true).await {
        Ok(p) => p,
        Err(o) => return o,
    };
    if let Err(o) = all_in_cidr(ctx, &pods) {
        return o;
    }
    let want: BTreeSet<String> = pods.iter().map(|(n, _)| n.clone()).collect();
    match names_seen(&ip, SVC_PORT, &want, deadline(60)).await {
        Ok(seen) if seen == want => Outcome::Pass(format!("ClusterIP {ip} reached all 3 backends after scaling 1 → 3: {want:?}")),
        Ok(seen) => Outcome::Fail(format!("ClusterIP {ip} answered from pods that are not ready backends: {seen:?} vs {want:?}")),
        Err(e) => Outcome::Fail(format!("ClusterIP {ip} after scaling 1 → 3: {e}")),
    }
}

/// Set a Deployment's replicas: read, change, replace.
async fn scale(ctx: &Ctx, name: &str, replicas: u32) -> Result<(), Outcome> {
    let path = format!("{}/{name}", ctx.api.deployments());
    let mut d: Value = ctx.api.get(&path).await.map_err(|e| e.outcome())?.ok_or_else(|| Outcome::Fail(format!("Deployment {name} vanished")))?;
    d["spec"]["replicas"] = json!(replicas);
    ctx.api.replace(&path, &d).await.map_err(|e| e.outcome())?;
    Ok(())
}

async fn backend_loss(env: &Env, ctx: &Ctx) -> Outcome {
    let deadline = |s| Instant::now() + env.wait(Duration::from_secs(s), Duration::from_secs(240));
    let before = match ready(ctx, "d").await {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return Outcome::Fail("no ready backend of d to delete".into()),
        Err(e) => return e.outcome(),
    };
    let victim = before[0].0.clone();
    if let Err(e) = ctx.api.delete(&format!("{}/{victim}", ctx.api.pods())).await {
        return e.outcome();
    }
    let v = victim.clone();
    let after = match wait_ready(ctx, "d", before.len(), deadline(120), move |n| n != v).await {
        Ok(p) => p,
        Err(o) => return o,
    };
    if let Err(o) = all_in_cidr(ctx, &after) {
        return o;
    }
    let old: BTreeSet<&String> = before.iter().map(|(n, _)| n).collect();
    let Some((new, new_ip)) = after.iter().find(|(n, _)| !old.contains(n)).cloned() else {
        return Outcome::Fail(format!("{victim} was deleted and no new backend appeared: {after:?}"));
    };
    if let Err(e) = reach(&new_ip, POD_PORT, Some(&new), deadline(20)).await {
        return Outcome::Fail(format!("replacement {new} at {new_ip} does not answer: {e}"));
    }
    // The Service must stop sending to the dead backend: 20 answers in a
    // row, none from it, within 45 s.
    let svc = match ctx.api.get(&format!("{}/{}", ctx.api.services(), env.name("svc-d"))).await {
        Ok(Some(s)) => s["spec"]["clusterIP"].as_str().unwrap_or("").to_string(),
        Ok(None) => return Outcome::Fail("Service svc-d vanished".into()),
        Err(e) => return e.outcome(),
    };
    let Ok(a) = workload::addr(&svc, SVC_PORT) else { return Outcome::Fail(format!("Service svc-d has no ClusterIP ({svc:?})")) };
    let end = deadline(45);
    loop {
        let mut bad = Vec::new();
        for _ in 0..20 {
            match workload::probe(a, Duration::from_secs(3)).await {
                Ok((n, _)) if n == victim => bad.push(format!("answered by deleted {n}")),
                Ok(_) => {}
                Err(e) => bad.push(e),
            }
        }
        if bad.is_empty() {
            return Outcome::Pass(format!("{victim} deleted → {new} at {new_ip}; ClusterIP {svc} answered 20/20 without the dead backend"));
        }
        if Instant::now() >= end {
            return Outcome::Fail(format!("ClusterIP {svc} still failing 45 s after losing {victim}: {bad:?}"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn network_policy(env: &Env, ctx: &Ctx) -> Outcome {
    let (api, p) = (&ctx.api, env.name("p"));
    let deadline = |s| Instant::now() + env.wait(Duration::from_secs(s), Duration::from_secs(180));
    let ip = match ctx.start_pod(&p, "p", None, deadline(90)).await {
        Ok(ip) => ip,
        Err(o) => return o,
    };
    if let Err(e) = reach(&ip, POD_PORT, Some(&p), deadline(15)).await {
        return Outcome::Fail(format!("{p} not reachable before any policy: {e}"));
    }
    let Ok(a) = workload::addr(&ip, POD_PORT) else { return Outcome::Fail(format!("{p} has no usable IP {ip:?}")) };

    // Deny: the pod must stop answering.
    let deny = objects::deny_ingress(&api.ns, &api.run, &env.name("deny-p"), "p");
    if let Err(e) = api.create(&api.policies(), &deny).await {
        return e.outcome();
    }
    let end = deadline(30);
    let t = Instant::now();
    loop {
        if workload::probe(a, Duration::from_secs(2)).await.is_err() {
            break;
        }
        if Instant::now() >= end {
            return Outcome::Fail(format!("{p} still answers this pod 30 s after a deny-all-ingress NetworkPolicy: policy not enforced"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let denied_ms = t.elapsed().as_millis();

    // Allow from this Job's pod (labelled storm.io/component=network-operator
    // by the runner): answers again.
    let allow = objects::allow_from(&api.ns, &api.run, &env.name("allow-p"), "p", ("storm.io/component", "network-operator"));
    if let Err(e) = api.create(&api.policies(), &allow).await {
        return e.outcome();
    }
    if let Err(e) = reach(&ip, POD_PORT, Some(&p), deadline(30)).await {
        return Outcome::Fail(format!("denied, then allowed from this pod's labels, and {p} still does not answer: {e}"));
    }
    // Lift both: open again.
    for n in [env.name("allow-p"), env.name("deny-p")] {
        if let Err(e) = api.delete(&format!("{}/{n}", api.policies())).await {
            return e.outcome();
        }
    }
    match reach(&ip, POD_PORT, Some(&p), deadline(30)).await {
        Ok(_) => Outcome::Pass(format!("deny took effect in {denied_ms} ms; allow-from-label and removal both restored {p}")),
        Err(e) => Outcome::Fail(format!("policies deleted and {p} does not answer again: {e}")),
    }
}

async fn cross_node(env: &Env, ctx: &Ctx) -> Outcome {
    let nodes = match ctx.api.list("/api/v1/nodes").await {
        Ok(Some(n)) => n,
        Ok(None) => return Outcome::Infra("the apiserver does not serve nodes".into()),
        Err(e) => return e.outcome(),
    };
    let here = ctx.me.node.clone().unwrap_or_default();
    let others: Vec<String> = schedulable(&nodes).into_iter().filter(|n| *n != here).collect();
    let Some(there) = others.first() else {
        return Outcome::Skip(format!("requires min-nodes: 2 — {} schedulable node(s) here", schedulable(&nodes).len()));
    };
    let name = env.name("x");
    let deadline = Instant::now() + env.wait(Duration::from_secs(90), Duration::from_secs(120));
    let ip = match ctx.start_pod(&name, "x", Some(there), deadline).await {
        Ok(ip) => ip,
        Err(Outcome::Fail(m)) if m.contains("ImagePull") || m.contains("ErrImage") => {
            return Outcome::Infra(format!("the test image is not on node {there}'s registry (stormblock-registry#27): {m}"))
        }
        Err(o) => return o,
    };
    match reach(&ip, POD_PORT, Some(&name), Instant::now() + Duration::from_secs(20)).await {
        Ok((_, d)) => Outcome::Pass(format!("{here} → {there}: {name} at {ip} answered in {} ms", d.as_millis())),
        Err(e) => Outcome::Fail(format!("{name} on {there} at {ip} not reachable from {here}: {e}")),
    }
}

/// Nodes that are Ready and not cordoned.
pub fn schedulable(nodes: &[Value]) -> Vec<String> {
    nodes
        .iter()
        .filter(|n| n["spec"]["unschedulable"] != true)
        .filter(|n| n["status"]["conditions"].as_array().is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] == "True")))
        .filter_map(|n| n["metadata"]["name"].as_str().map(str::to_string))
        .collect()
}

async fn load_balancer(env: &Env, ctx: &Ctx) -> Outcome {
    let Some(c) = ctx.found.cluster() else { return ctx.found.without().unwrap() };
    let cfg = match &c.cfg {
        Ok(cfg) => cfg,
        Err(e) => return Outcome::Fail(format!("the spec does not resolve: {e}")),
    };
    if !cfg.lb_ipam {
        return Outcome::Skip(format!("requires loadBalancer.ipam: off in this Network (mode {})", cfg.mode.as_str()));
    }
    let svc = env.name("lb-d");
    let deadline = |s| Instant::now() + env.wait(Duration::from_secs(s), Duration::from_secs(150));
    let ip = match ctx.start_service(&svc, "d", "LoadBalancer", deadline(60)).await {
        Ok(ip) => ip,
        Err(o) => return o,
    };
    let pools = cidrs(&cfg.lb_pools);
    if within(&pools, &ip) == Some(false) {
        return Outcome::Fail(format!("LoadBalancer {svc} got {ip}, outside the LB pools {:?}", cfg.lb_pools));
    }
    match reach(&ip, SVC_PORT, None, deadline(30)).await {
        Ok((n, d)) => Outcome::Pass(format!("LoadBalancer {ip} (pool {:?}, announce {:?}) answered from {n} in {} ms", cfg.lb_pools, cfg.announce, d.as_millis())),
        Err(e) => Outcome::Fail(format!("LoadBalancer {ip} does not answer: {e}")),
    }
}

async fn endpoints_reaped(env: &Env, ctx: &Ctx) -> Outcome {
    let pods = match ctx.api.workload_pods(None).await {
        Ok(p) => p.iter().filter(|p| ready_ip(p).is_some()).count(),
        Err(e) => return e.outcome(),
    };
    let before = match ctx.api.endpoints_besides(&ctx.me.pod).await {
        Ok(Some(e)) => e,
        Ok(None) => return Outcome::Fail("cilium.io/v2 ciliumendpoints is not served: Cilium's CRDs are missing".into()),
        Err(e) => return e.outcome(),
    };
    if before.len() < pods {
        return Outcome::Fail(format!("{pods} ready workload pods but only {} CiliumEndpoints: {before:?}", before.len()));
    }
    if let Err(e) = ctx.api.cleanup().await {
        return e.outcome();
    }
    let end = Instant::now() + env.wait(Duration::from_secs(90), Duration::from_secs(60));
    loop {
        match ctx.api.endpoints_besides(&ctx.me.pod).await {
            Ok(Some(left)) if left.is_empty() => {
                return Outcome::Pass(format!("{} CiliumEndpoints for {pods} ready pods; all gone after the workloads were deleted", before.len()))
            }
            Ok(Some(left)) if Instant::now() >= end => return Outcome::Fail(format!("CiliumEndpoints left after the workloads went: {left:?}")),
            Ok(None) => return Outcome::Fail("ciliumendpoints stopped being served".into()),
            Err(e) => return e.outcome(),
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedulable_means_ready_and_not_cordoned() {
        let n = |name: &str, ready: &str, cordoned: bool| {
            json!({"metadata": {"name": name}, "spec": {"unschedulable": cordoned},
                   "status": {"conditions": [{"type": "Ready", "status": ready}]}})
        };
        let nodes = vec![n("a", "True", false), n("b", "False", false), n("c", "True", true), n("d", "True", false)];
        assert_eq!(schedulable(&nodes), vec!["a".to_string(), "d".to_string()]);
    }
}
