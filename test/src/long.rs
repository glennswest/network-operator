//! `long` (the night window): waves of pod networking at the machine's
//! capacity. Each wave:
//!
//! 1. **ramps** Deployments of 10 pods, each behind a ClusterIP Service,
//!    to a size read from the nodes' allocatable pods (never assumed), varying
//!    wave to wave (100%, 50%, 75% of the target);
//! 2. **exercises**: probes every pod and every Service, deletes one pod per
//!    Deployment, waits for replacements, probes the Services again, and
//!    re-reads the `Network` (still Available);
//! 3. **drains**: deletes everything and waits for the objects *and* Cilium's
//!    endpoints to be gone.
//!
//! Each wave is one result line with its numbers; `trend` compares waves
//! (see [`crate::trend`]) and fails on the first slowdown or any residue.
//!
//! The standard's VM waves are not this component's: network-operator's
//! workload is pod networking, and VM networking is tested by stormvm. That
//! is reported as a skip, not left out.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use crate::api::ready_ip;
use crate::ctx::Ctx;
use crate::env::Env;
use crate::medium::schedulable;
use crate::network;
use crate::objects::{self, POD_PORT, SVC_PORT};
use crate::report::{Outcome, Report};
use crate::trend::{self, p95, Wave};
use crate::workload;

/// Pods per Deployment (and per Service).
const GROUP: usize = 10;
/// Waves are at most this many pods, and at least one group.
const MAX_PODS: usize = 600;
/// Without a capacity to size from, a wave this size still measures the trend.
const FALLBACK_PODS: usize = 20;
/// How long a wave may take to get Ready, and to drain.
const RAMP_LIMIT: Duration = Duration::from_secs(15 * 60);
const DRAIN_LIMIT: Duration = Duration::from_secs(5 * 60);
/// Probes in flight at once.
const PARALLEL: usize = 32;

pub async fn run(env: &Env, r: &mut Report) {
    let ctx = match Ctx::setup(env).await {
        Ok(c) => c,
        Err(o) => {
            r.all(&["network-status", "capacity", "trend"], &o);
            return;
        }
    };
    status(&ctx, "network-status", r).await;
    if let Some(skip) = ctx.not_ours() {
        r.all(&["capacity", "trend", "vm-waves"], &skip);
        return;
    }

    let t = Instant::now();
    let (target, o) = capacity(&ctx).await;
    r.record("capacity", o, t.elapsed().as_millis(), None);

    let sizes = [target, target / 2, target * 3 / 4].map(|s| s.max(GROUP).div_ceil(GROUP) * GROUP);
    let mut waves: Vec<Wave> = Vec::new();
    let mut last = Duration::from_secs(10 * 60);
    loop {
        // Room for another wave like the last one, and for the end.
        if env.remaining() < last * 3 / 2 + Duration::from_secs(180) {
            break;
        }
        let n = waves.len() + 1;
        let t = Instant::now();
        let (w, problems) = wave(env, &ctx, n, sizes[(n - 1) % sizes.len()]).await;
        last = t.elapsed();
        let o = if problems.is_empty() {
            Outcome::Pass(format!("{} pods ready in {} ms, all reached, drained in {} ms", w.pods, w.ramp_ms, w.drain_ms))
        } else {
            Outcome::Fail(problems.join("; "))
        };
        r.record(&format!("wave-{n}"), o, last.as_millis(), Some(&w.json()));
        waves.push(w);
    }

    let o = match waves.len() {
        0 => Outcome::Infra(format!("no wave fit in the {} s window", env.timeout.as_secs())),
        _ => match trend::regression(&waves) {
            Ok(s) => Outcome::Pass(s),
            Err((n, why)) => Outcome::Fail(format!("first regression at wave {n}: {why}")),
        },
    };
    r.record("trend", o, 0, None);
    r.record("vm-waves", Outcome::Skip("network-operator's workload is pod networking; VM waves are stormvm's suite".into()), 0, None);
    status(&ctx, "network-status-end", r).await;
    r.run("cleanup", ctx.cleanup(Duration::from_secs(120))).await;
}

async fn status(ctx: &Ctx, name: &str, r: &mut Report) {
    // Re-read: the point is what the operator says now.
    let found = network::find(&ctx.api).await;
    let o = match found.cluster() {
        Some(c) => network::verdict(c),
        None => found.without().unwrap(),
    };
    r.record(name, o, 0, None);
}

/// The wave target: half the pods the schedulable nodes will take, bounded.
async fn capacity(ctx: &Ctx) -> (usize, Outcome) {
    let nodes = match ctx.api.list("/api/v1/nodes").await {
        Ok(Some(n)) => n,
        Ok(None) => return (FALLBACK_PODS, Outcome::Infra(format!("nodes not served; waves of {FALLBACK_PODS}"))),
        Err(e) => return (FALLBACK_PODS, Outcome::Infra(format!("{}; waves of {FALLBACK_PODS} pods instead", e.msg()))),
    };
    let names = schedulable(&nodes);
    let pods: usize = nodes
        .iter()
        .filter(|n| n["metadata"]["name"].as_str().is_some_and(|m| names.iter().any(|x| x == m)))
        .filter_map(|n| n["status"]["allocatable"]["pods"].as_str().and_then(|p| p.parse::<usize>().ok()))
        .sum();
    if pods == 0 {
        return (FALLBACK_PODS, Outcome::Fail(format!("{} schedulable nodes report no allocatable pods", names.len())));
    }
    let target = (pods / 2).clamp(GROUP, MAX_PODS);
    (target, Outcome::Pass(format!("{} schedulable nodes, {pods} allocatable pods: waves up to {target} pods", names.len())))
}

/// One wave. Returns its numbers and what went wrong in it.
async fn wave(env: &Env, ctx: &Ctx, n: usize, size: usize) -> (Wave, Vec<String>) {
    let api = &ctx.api;
    let mut w = Wave { n, pods: size, ..Default::default() };
    let mut problems = Vec::new();
    let groups = size / GROUP;
    let app = |g: usize| format!("w{n}g{g}");

    // Ramp.
    let t0 = Instant::now();
    for g in 0..groups {
        let d = objects::deployment(&api.ns, &api.run, &env.name(&app(g)), &app(g), &ctx.me.image, GROUP as u32);
        let s = objects::service(&api.ns, &api.run, &env.name(&format!("s-{}", app(g))), &app(g), "ClusterIP");
        for (coll, obj) in [(api.deployments(), d), (api.services(), s)] {
            if let Err(e) = api.create(&coll, &obj).await {
                problems.push(e.msg());
            }
        }
    }
    let mut ready_at: HashMap<String, u64> = HashMap::new();
    let mut ips: HashMap<String, String> = HashMap::new();
    let ramp_end = t0 + RAMP_LIMIT.min(env.remaining().saturating_sub(DRAIN_LIMIT));
    loop {
        match api.workload_pods(None).await {
            Ok(pods) => {
                ips.clear();
                for p in &pods {
                    if let (Some(name), Some(ip)) = (p["metadata"]["name"].as_str(), ready_ip(p)) {
                        ready_at.entry(name.to_string()).or_insert(t0.elapsed().as_millis() as u64);
                        ips.insert(name.to_string(), ip);
                    }
                }
            }
            Err(e) => problems.push(e.msg()),
        }
        if ips.len() >= size || Instant::now() >= ramp_end {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    w.ramp_ms = t0.elapsed().as_millis() as u64;
    w.ready_p95_ms = p95(&ready_at.values().copied().collect::<Vec<_>>());
    if ips.len() < size {
        problems.push(format!("{} of {size} pods ready after {} s", ips.len(), w.ramp_ms / 1000));
    }
    let cidrs = ctx.pod_cidrs();
    let outside: Vec<&String> = ips.values().filter(|ip| network::within(&cidrs, ip) == Some(false)).collect();
    if !outside.is_empty() {
        problems.push(format!("{} pods outside the pod network {cidrs:?}, e.g. {}", outside.len(), outside[0]));
    }

    // Exercise: every pod, every Service; lose one pod per group; Services again.
    let svc_ips = service_ips(ctx, n, groups, env).await;
    let mut targets: Vec<(String, u16)> = ips.values().map(|ip| (ip.clone(), POD_PORT)).collect();
    targets.extend(svc_ips.iter().flat_map(|ip| std::iter::repeat_n((ip.clone(), SVC_PORT), 3)));
    let (lat, fails) = probe_all(&targets).await;
    w.reach_p95_ms = p95(&lat);
    w.reach_fail = fails.len();
    if !fails.is_empty() {
        problems.push(format!("{} of {} probes unanswered, e.g. {}", fails.len(), targets.len(), fails[0]));
    }
    let victims: Vec<String> = (0..groups).filter_map(|g| ips.keys().find(|k| k.contains(&format!("-{}-", app(g)))).cloned()).collect();
    for v in &victims {
        if let Err(e) = api.delete(&format!("{}/{v}", api.pods())).await {
            problems.push(e.msg());
        }
    }
    let back_end = Instant::now() + Duration::from_secs(300).min(env.remaining().saturating_sub(DRAIN_LIMIT));
    loop {
        let ready = api.workload_pods(None).await.map(|p| p.iter().filter(|p| ready_ip(p).is_some() && !victims.iter().any(|v| p["metadata"]["name"] == *v)).count()).unwrap_or(0);
        if ready >= size {
            break;
        }
        if Instant::now() >= back_end {
            problems.push(format!("after losing {} pods, {ready} of {size} ready again", victims.len()));
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let again: Vec<(String, u16)> = svc_ips.iter().map(|ip| (ip.clone(), SVC_PORT)).collect();
    let (_, fails) = probe_all(&again).await;
    if !fails.is_empty() {
        problems.push(format!("after losing a backend each, {} of {} Services unanswered, e.g. {}", fails.len(), again.len(), fails[0]));
    }
    let found = network::find(api).await;
    if let Some(c) = found.cluster() {
        if let Outcome::Fail(m) = network::verdict(c) {
            problems.push(format!("mid-wave: {m}"));
        }
    }

    // Drain: objects and Cilium's endpoints for them.
    let t = Instant::now();
    if let Err(e) = api.cleanup().await {
        problems.push(e.msg());
    }
    let drain_end = Instant::now() + DRAIN_LIMIT;
    loop {
        let objects: usize = api.leftovers().await.map(|l| l.iter().map(|(_, n)| n).sum()).unwrap_or(usize::MAX);
        let endpoints = api.endpoints_besides(&ctx.me.pod).await.ok().flatten().map(|e| e.len()).unwrap_or(0);
        w.residue = objects.saturating_add(endpoints);
        if w.residue == 0 || Instant::now() >= drain_end {
            break;
        }
        // Deployments may still be replacing pods as the drain starts.
        let _ = api.cleanup().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    w.drain_ms = t.elapsed().as_millis() as u64;
    if w.residue > 0 {
        problems.push(format!("{} objects/endpoints still there {} s after draining", w.residue, DRAIN_LIMIT.as_secs()));
    }
    (w, problems)
}

async fn service_ips(ctx: &Ctx, n: usize, groups: usize, env: &Env) -> Vec<String> {
    let mut out = Vec::new();
    for g in 0..groups {
        let path = format!("{}/{}", ctx.api.services(), env.name(&format!("s-w{n}g{g}")));
        if let Ok(Some(s)) = ctx.api.get(&path).await {
            if let Some(ip) = s["spec"]["clusterIP"].as_str().filter(|i| !i.is_empty() && *i != "None") {
                out.push(ip.to_string());
            }
        }
    }
    out
}

/// Probe every `(ip, port)`, [`PARALLEL`] at a time. Latencies of the
/// answers, and the errors.
async fn probe_all(targets: &[(String, u16)]) -> (Vec<u64>, Vec<String>) {
    let sem = Arc::new(Semaphore::new(PARALLEL));
    let mut set = tokio::task::JoinSet::new();
    for (ip, port) in targets.iter().cloned() {
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await;
            let a = workload::addr(&ip, port)?;
            workload::probe(a, Duration::from_secs(3)).await.map(|(_, d)| d.as_millis() as u64)
        });
    }
    let (mut lat, mut fails) = (Vec::new(), Vec::new());
    while let Some(r) = set.join_next().await {
        match r {
            Ok(Ok(ms)) => lat.push(ms),
            Ok(Err(e)) => fails.push(e),
            Err(e) => fails.push(format!("probe task: {e}")),
        }
    }
    (lat, fails)
}
