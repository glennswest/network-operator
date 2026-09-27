//! `short` (< 2 min): network-operator is up and has done its main job — the
//! `Network` reports a healthy, current install, and the network it installed
//! works: a pod gets an address from the pod CIDR, is reachable from another
//! pod, and a ClusterIP Service in front of it answers (with kube-proxy
//! replacement, that is Cilium's eBPF service path).

use std::time::{Duration, Instant};

use crate::ctx::{reach, Ctx};
use crate::env::Env;
use crate::network::{self, within};
use crate::objects::{POD_PORT, SVC_PORT};
use crate::report::{Outcome, Report};

pub const TESTS: [&str; 5] = ["network-status", "pod-ip", "pod-to-pod", "service", "cleanup"];

pub async fn run(env: &Env, r: &mut Report) {
    match Ctx::setup(env).await {
        Ok(ctx) => basics(env, &ctx, r).await,
        Err(o) => r.all(&TESTS, &o),
    }
}

/// The short suite's tests; medium starts with them.
pub async fn basics(env: &Env, ctx: &Ctx, r: &mut Report) {
    let t = Instant::now();
    let o = match ctx.found.cluster() {
        Some(c) => network::verdict(c),
        None => ctx.found.without().unwrap(),
    };
    r.record(TESTS[0], o, t.elapsed().as_millis(), None);

    if let Some(skip) = ctx.not_ours() {
        r.all(&TESTS[1..], &skip);
        return;
    }

    let name = env.name("a");
    let t = Instant::now();
    let deadline = Instant::now() + env.wait(Duration::from_secs(60), Duration::from_secs(45));
    let ip = match ctx.start_pod(&name, "a", None, deadline).await {
        Ok(ip) => ip,
        Err(o) => {
            r.record(TESTS[1], o, t.elapsed().as_millis(), None);
            r.all(&TESTS[2..4], &Outcome::Skip("no workload pod to reach".into()));
            let o = ctx.cleanup(Duration::from_secs(20)).await;
            r.record(TESTS[4], o, 0, None);
            return;
        }
    };
    let o = pod_ip(ctx, &name, &ip);
    r.record(TESTS[1], o, t.elapsed().as_millis(), None);

    let t = Instant::now();
    let o = match reach(&ip, POD_PORT, Some(&name), Instant::now() + Duration::from_secs(15)).await {
        Ok((_, d)) => Outcome::Pass(format!(
            "{} ({}) reached {name} at {ip}:{POD_PORT} in {} ms",
            ctx.me.pod,
            ctx.me.ip.as_deref().unwrap_or("?"),
            d.as_millis()
        )),
        Err(e) => Outcome::Fail(format!("pod {name} is Ready at {ip} and does not answer from this pod: {e}")),
    };
    r.record(TESTS[2], o, t.elapsed().as_millis(), None);

    let t = Instant::now();
    let o = service(env, ctx, &name).await;
    r.record(TESTS[3], o, t.elapsed().as_millis(), None);

    let t = Instant::now();
    let o = ctx.cleanup(Duration::from_secs(20)).await;
    r.record(TESTS[4], o, t.elapsed().as_millis(), None);
}

/// The pod got an address, and it is inside the pod CIDR the operator applied.
fn pod_ip(ctx: &Ctx, name: &str, ip: &str) -> Outcome {
    let cidrs = ctx.pod_cidrs();
    match within(&cidrs, ip) {
        Some(true) => Outcome::Pass(format!("{name} Ready at {ip}, inside the pod network {cidrs:?}")),
        Some(false) => Outcome::Fail(format!("{name} got {ip}, outside the pod network {cidrs:?} the Network applied")),
        None => Outcome::Pass(format!("{name} Ready at {ip} (pod CIDR not checked: the Network could not be read)")),
    }
}

async fn service(env: &Env, ctx: &Ctx, backend: &str) -> Outcome {
    let svc = env.name("svc-a");
    let deadline = Instant::now() + env.wait(Duration::from_secs(30), Duration::from_secs(25));
    let ip = match ctx.start_service(&svc, "a", "ClusterIP", deadline).await {
        Ok(ip) => ip,
        Err(o) => return o,
    };
    let cidrs = ctx.service_cidrs();
    if within(&cidrs, &ip) == Some(false) {
        return Outcome::Fail(format!("Service {svc} got ClusterIP {ip}, outside the service network {cidrs:?}"));
    }
    match reach(&ip, SVC_PORT, Some(backend), deadline).await {
        Ok((_, d)) => Outcome::Pass(format!("ClusterIP {ip}:{SVC_PORT} answered from {backend} in {} ms", d.as_millis())),
        Err(e) => Outcome::Fail(format!("ClusterIP {ip}:{SVC_PORT} does not reach {backend}: {e}")),
    }
}
