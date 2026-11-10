//! `vlan` (#32): the VLAN layer on a live node, as the owner's done-when
//! puts it: 100 VLAN resources on one node; pods on two VLANs reach their own
//! VLAN's peers and not each other's; a namespace default works; deleting a
//! VLAN removes its NADs and its bridge entries.
//!
//! Declared disruptive in `requires.toml`: it creates cluster-scoped VLAN CRs,
//! and through them the node agent turns `vlan_filtering` on on the node's
//! bridge and adds (then removes) VLAN entries on its uplinks. IDs 3899-3999.
//!
//! Every pod is pinned to this Job's node, so the isolation test does not
//! depend on the switch beyond the node carrying the VLANs.
//! Could-not-run (exit 2) when the VLAN layer is not deployed: no `vlans`
//! resource, or no agent report on this node.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::api::{enc, Api};
use crate::ctx::Ctx;
use crate::env::Env;
use crate::objects::{self, POD_PORT};
use crate::report::{Outcome, Report};
use crate::workload;

pub const TESTS: [&str; 6] = ["vlan-layer", "hundred-vlans", "isolation", "namespace-default", "delete", "cleanup"];

const VLANS: &str = "/apis/network.storm.io/v1/vlans";
const AGENT_ANNOTATION: &str = "network.storm.io/vlan-agent";
const DEFAULT_ANNOTATION: &str = "network.storm.io/default-networks";
const NETWORKS: &str = "k8s.v1.cni.cncf.io/networks";
const NETWORK_STATUS: &str = "k8s.v1.cni.cncf.io/network-status";
/// The hundred use 3900-3999; the namespace default 3899.
const FIRST_ID: u16 = 3900;
const DEFAULT_ID: u16 = 3899;
/// One subnet on both isolation VLANs: only the VLAN keeps them apart.
const SUBNET_PREFIX: &str = "10.250.0";

pub async fn run(env: &Env, r: &mut Report) {
    let ctx = match Ctx::setup(env).await {
        Ok(c) => c,
        Err(o) => return r.all(&TESTS, &o),
    };
    let t = Instant::now();
    let node = match layer(&ctx).await {
        Ok((node, detail)) => {
            r.record(TESTS[0], Outcome::Pass(detail), t.elapsed().as_millis(), None);
            node
        }
        Err(o) => {
            r.record(TESTS[0], o.clone(), t.elapsed().as_millis(), None);
            r.all(&TESTS[1..5], &Outcome::Infra("the VLAN layer is not running here".into()));
            r.record(TESTS[5], Outcome::Skip("nothing was created".into()), 0, None);
            return;
        }
    };

    let names: Vec<String> = (0..100).map(|i| env.name(&format!("v{i}"))).collect();
    let t = Instant::now();
    let deadline = Instant::now() + env.wait(Duration::from_secs(180), Duration::from_secs(120));
    let o = hundred(&ctx, &names, &node, deadline).await;
    let hundred_ok = matches!(o, Outcome::Pass(_));
    r.record(TESTS[1], o, t.elapsed().as_millis(), None);

    let t = Instant::now();
    let o = if hundred_ok {
        let deadline = Instant::now() + env.wait(Duration::from_secs(120), Duration::from_secs(80));
        isolation(env, &ctx, &names[0], &names[1], &node, deadline).await
    } else {
        Outcome::Skip("the VLANs did not come up".into())
    };
    r.record(TESTS[2], o, t.elapsed().as_millis(), None);

    let t = Instant::now();
    let default_name = env.name("vdefault");
    let deadline = Instant::now() + env.wait(Duration::from_secs(90), Duration::from_secs(50));
    let o = namespace_default(env, &ctx, &default_name, &node, deadline).await;
    r.record(TESTS[3], o, t.elapsed().as_millis(), None);

    let t = Instant::now();
    let deadline = Instant::now() + env.wait(Duration::from_secs(120), Duration::from_secs(20));
    let mut all = names.clone();
    all.push(default_name);
    let o = delete(&ctx, &all, &node, deadline).await;
    r.record(TESTS[4], o, t.elapsed().as_millis(), None);

    let t = Instant::now();
    let o = match ctx.cleanup(Duration::from_secs(30)).await {
        Outcome::Pass(d) => match delete_run_vlans(&ctx.api).await {
            Ok(0) => Outcome::Pass(d),
            Ok(n) => Outcome::Pass(format!("{d}; {n} leftover VLANs deleted")),
            Err(e) => Outcome::Fail(format!("{d}; deleting VLANs: {e}")),
        },
        other => {
            let _ = delete_run_vlans(&ctx.api).await;
            other
        }
    };
    r.record(TESTS[5], o, t.elapsed().as_millis(), None);
}

/// The VLAN CRD is served and this node's agent reports. Returns the node.
async fn layer(ctx: &Ctx) -> Result<(String, String), Outcome> {
    if ctx.api.list(VLANS).await.map_err(|e| e.outcome())?.is_none() {
        return Err(Outcome::Infra("no vlans.network.storm.io: the VLAN controller is not deployed (deploy/vlan.yaml)".into()));
    }
    let node = ctx.me.node.clone().ok_or_else(|| Outcome::Infra("this Job's pod names no node".into()))?;
    let state = agent_state(&ctx.api, &node).await?;
    let Some(state) = state else {
        return Err(Outcome::Infra(format!("node {node} has no {AGENT_ANNOTATION}: no VLAN node agent runs there")));
    };
    let bridge = state["bridge"].as_str().unwrap_or("?").to_string();
    if let Some(e) = state["error"].as_str() {
        return Err(Outcome::Fail(format!("the agent on {node} reports for bridge {bridge}: {e}")));
    }
    Ok((node.clone(), format!("VLAN CRD served; the agent on {node} manages bridge {bridge}")))
}

/// The node's agent report, parsed.
async fn agent_state(api: &Api, node: &str) -> Result<Option<Value>, Outcome> {
    let n = api.get(&format!("/api/v1/nodes/{node}")).await.map_err(|e| e.outcome())?;
    Ok(n.and_then(|n| n["metadata"]["annotations"][AGENT_ANNOTATION].as_str().and_then(|s| serde_json::from_str(s).ok())))
}

pub fn vlan(run: &str, ns: &str, name: &str, id: u16, static_addresses: bool, default_for: bool) -> Value {
    let mut spec = json!({"id": id, "namespaces": [ns]});
    if static_addresses {
        spec["addresses"] = json!({"type": "static"});
    }
    if default_for {
        spec["defaultFor"] = json!([ns]);
    }
    json!({
        "apiVersion": "network.storm.io/v1",
        "kind": "VLAN",
        "metadata": {"name": name, "labels": {"storm.io/test-run": run}},
        "spec": spec,
    })
}

/// Create the hundred (the first two with static addresses, for
/// `isolation`) and wait for each to have its NAD and be on this node.
async fn hundred(ctx: &Ctx, names: &[String], node: &str, deadline: Instant) -> Outcome {
    let api = &ctx.api;
    for (i, name) in names.iter().enumerate() {
        let v = vlan(&api.run, &api.ns, name, FIRST_ID + i as u16, i < 2, false);
        if let Err(e) = api.create(VLANS, &v).await {
            return Outcome::Infra(format!("creating VLAN {name}: {}", e.msg()));
        }
    }
    let created = Instant::now();
    let want_nad = |name: &str| format!("{}/{name}", api.ns);
    loop {
        let items = match api.list(&format!("{VLANS}?labelSelector={}", enc(&format!("storm.io/test-run={}", api.run)))).await {
            Ok(Some(i)) => i,
            Ok(None) => return Outcome::Infra("vlans stopped being served".into()),
            Err(e) => return e.outcome(),
        };
        let mut pending = Vec::new();
        for name in names {
            let Some(v) = items.iter().find(|v| v["metadata"]["name"] == name.as_str()) else {
                pending.push(format!("{name}: not listed"));
                continue;
            };
            let has = |field: &str, want: &str| v["status"][field].as_array().is_some_and(|a| a.iter().any(|x| x == want));
            if !has("nads", &want_nad(name)) || !has("nodes", node) {
                let ready = v["status"]["conditions"].as_array().and_then(|c| c.iter().find(|c| c["type"] == "Ready")).cloned().unwrap_or(Value::Null);
                pending.push(format!("{name}: {}", ready["message"].as_str().unwrap_or("no status yet")));
            }
        }
        if pending.is_empty() {
            let nads = api
                .list(&format!("{}?labelSelector={}", api.ns_path("k8s.cni.cncf.io/v1", "network-attachment-definitions"), enc("network.storm.io/vlan")))
                .await
                .ok()
                .flatten()
                .map(|l| l.len())
                .unwrap_or(0);
            return Outcome::Pass(format!(
                "100 VLANs ({FIRST_ID}-{}): each has its NAD in {} ({nads} listed) and is on {node}'s uplinks, {} s after creation",
                FIRST_ID + 99,
                api.ns,
                created.elapsed().as_secs()
            ));
        }
        if Instant::now() >= deadline {
            return Outcome::Fail(format!("{} of 100 VLANs not ready in time; first: {}", pending.len(), pending[0]));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// A workload pod on `node` attached to `vlan` with `ip`, or (with `None`)
/// naming no networks.
fn vlan_pod(ctx: &Ctx, name: &str, node: &str, attach: Option<(&str, &str)>) -> Value {
    let mut p = objects::pod(&ctx.api.ns, &ctx.api.run, name, "vlan", &ctx.me.image, Some(node));
    if let Some((vlan, ip)) = attach {
        let nets = json!([{"name": vlan, "ips": [format!("{ip}/24")]}]).to_string();
        p["metadata"]["annotations"] = json!({NETWORKS: nets});
    }
    p
}

async fn start(ctx: &Ctx, pod: &Value, deadline: Instant) -> Result<String, Outcome> {
    let name = pod["metadata"]["name"].as_str().unwrap_or("?").to_string();
    ctx.api.create(&ctx.api.pods(), pod).await.map_err(|e| e.outcome())?;
    ctx.api.ready_pod(&name, deadline).await.map_err(|e| Outcome::Fail(e.msg()))
}

/// Ask the workload at `from_ip` (its pod-network address) to reach
/// `to` on its VLAN interface.
async fn connect(from_ip: &str, to: &str) -> Result<String, String> {
    let a = workload::addr(from_ip, POD_PORT)?;
    workload::get(a, &format!("/connect/{to}:{POD_PORT}"), Duration::from_secs(10)).await.map(|(b, _)| b)
}

async fn isolation(env: &Env, ctx: &Ctx, vlan_a: &str, vlan_b: &str, node: &str, deadline: Instant) -> Outcome {
    let (a1, a2, b1) = (env.name("a1"), env.name("a2"), env.name("b1"));
    let ip = |n: u8| format!("{SUBNET_PREFIX}.{n}");
    let mut eth0 = Vec::new();
    for (pod, vlan, n) in [(&a1, vlan_a, 1), (&a2, vlan_a, 2), (&b1, vlan_b, 3)] {
        match start(ctx, &vlan_pod(ctx, pod, node, Some((vlan, &ip(n)))), deadline).await {
            Ok(addr) => eth0.push(addr),
            Err(o) => return o,
        }
    }

    // Same VLAN: retried a little, since a fresh port may still be learning.
    let until = Instant::now() + Duration::from_secs(20);
    let same = loop {
        match connect(&eth0[0], &ip(2)).await {
            Ok(b) if b == format!("reached {a2}") => break Ok(b),
            other if Instant::now() >= until => break Err(format!("{other:?}")),
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    };
    if let Err(e) = same {
        return Outcome::Fail(format!("{a1} ({vlan_a}, {}) could not reach its peer {a2} at {}: {e}", ip(1), ip(2)));
    }
    for (from, from_ip, to, to_name) in [(&a1, &eth0[0], ip(3), &b1), (&b1, &eth0[2], ip(1), &a1)] {
        match connect(from_ip, &to).await {
            Ok(b) if b.starts_with("unreachable") => {}
            Ok(b) => return Outcome::Fail(format!("{from} reached {to_name} at {to} across VLANs: {b}")),
            Err(e) => return Outcome::Infra(format!("asking {from} to probe: {e}")),
        }
    }
    Outcome::Pass(format!(
        "{a1}->{a2} on {vlan_a} ({}->{}) reached; {a1}->{b1} and {b1}->{a1} across {vlan_a}/{vlan_b} (same subnet {SUBNET_PREFIX}.0/24) did not",
        ip(1),
        ip(2)
    ))
}

async fn namespace_default(env: &Env, ctx: &Ctx, name: &str, node: &str, deadline: Instant) -> Outcome {
    let api = &ctx.api;
    if let Err(e) = api.create(VLANS, &vlan(&api.run, &api.ns, name, DEFAULT_ID, false, true)).await {
        return Outcome::Infra(format!("creating VLAN {name}: {}", e.msg()));
    }
    // The operator writes the namespace's default.
    loop {
        let ns = match api.get(&format!("/api/v1/namespaces/{}", api.ns)).await {
            Ok(n) => n.unwrap_or(Value::Null),
            Err(e) => return e.outcome(),
        };
        if ns["metadata"]["annotations"][DEFAULT_ANNOTATION] == name {
            break;
        }
        if Instant::now() >= deadline {
            return Outcome::Fail(format!("namespace {} never got {DEFAULT_ANNOTATION}: {name}", api.ns));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let pod = env.name("d1");
    if let Err(o) = start(ctx, &vlan_pod(ctx, &pod, node, None), deadline).await {
        return o;
    }
    let p = match api.get(&format!("{}/{pod}", api.pods())).await {
        Ok(Some(p)) => p,
        Ok(None) => return Outcome::Fail(format!("pod {pod} vanished")),
        Err(e) => return e.outcome(),
    };
    let status = p["metadata"]["annotations"][NETWORK_STATUS].as_str().unwrap_or("");
    let want = format!("{}/{name}", api.ns);
    let attached: Vec<String> = serde_json::from_str::<Vec<Value>>(status)
        .unwrap_or_default()
        .iter()
        .filter_map(|e| e["name"].as_str().map(str::to_string))
        .collect();
    if attached.contains(&want) {
        Outcome::Pass(format!("namespace default {name} set by the operator; {pod}, naming no networks, is attached to {want}"))
    } else {
        Outcome::Fail(format!(
            "namespace {} defaults to {name}, but {pod} (no networks annotation) is attached to {attached:?} (rustkube-node#346)",
            api.ns
        ))
    }
}

/// Delete the run's VLANs; their NADs and this node's bridge entries go.
async fn delete(ctx: &Ctx, names: &[String], node: &str, deadline: Instant) -> Outcome {
    let api = &ctx.api;
    // The isolation pods hold ports on the VLANs: they go first.
    let _ = ctx.api.cleanup().await;
    for n in names {
        if let Err(e) = api.delete(&format!("{VLANS}/{n}")).await {
            return Outcome::Infra(format!("deleting VLAN {n}: {}", e.msg()));
        }
    }
    let started = Instant::now();
    let ids: Vec<u64> = (u64::from(DEFAULT_ID)..=u64::from(FIRST_ID) + 99).collect();
    let nads_path = format!("{}?labelSelector={}", api.ns_path("k8s.cni.cncf.io/v1", "network-attachment-definitions"), enc("network.storm.io/vlan"));
    loop {
        let nads = match api.list(&nads_path).await {
            Ok(l) => l.map(|l| l.len()).unwrap_or(0),
            Err(e) => return e.outcome(),
        };
        let state = match agent_state(api, node).await {
            Ok(s) => s.unwrap_or(Value::Null),
            Err(o) => return o,
        };
        let ours = |field: &str| -> usize {
            let vals: Vec<u64> = match field {
                "present" => state["present"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect(),
                _ => state["owned"].as_object().into_iter().flat_map(|o| o.values()).filter_map(Value::as_array).flatten().filter_map(Value::as_u64).collect(),
            };
            vals.iter().filter(|v| ids.contains(v)).count()
        };
        let (present, owned) = (ours("present"), ours("owned"));
        if nads == 0 && present == 0 && owned == 0 {
            return Outcome::Pass(format!(
                "{} VLANs deleted: their NADs in {} and their entries on {node}'s bridge gone in {} s",
                names.len(),
                api.ns,
                started.elapsed().as_secs()
            ));
        }
        if Instant::now() >= deadline {
            return Outcome::Fail(format!("after deleting, {nads} NADs left in {}, {node} still has {present} present / {owned} owned bridge entries", api.ns));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Delete whatever VLANs this run still has (a failed step's leftovers).
async fn delete_run_vlans(api: &Api) -> Result<usize, String> {
    let items = api.list(&format!("{VLANS}?labelSelector={}", enc(&format!("storm.io/test-run={}", api.run)))).await?.unwrap_or_default();
    for v in &items {
        if let Some(n) = v["metadata"]["name"].as_str() {
            api.delete(&format!("{VLANS}/{n}")).await?;
        }
    }
    Ok(items.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vlan_objects_are_what_the_operator_reads() {
        let v = vlan("run1", "ns1", "nt-run1-v0", 3900, true, false);
        let parsed: network_operator::vlan::crd::Vlan = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(parsed.spec.id, 3900);
        assert_eq!(parsed.spec.namespaces, vec!["ns1"]);
        assert_eq!(parsed.spec.addresses.as_ref().unwrap().kind, network_operator::vlan::crd::AddressType::Static);
        assert!(network_operator::vlan::validate(&parsed.spec).is_ok());
        let d: network_operator::vlan::crd::Vlan = serde_json::from_value(vlan("r", "ns1", "d", DEFAULT_ID, false, true)).unwrap();
        assert_eq!(d.spec.default_for, vec!["ns1"]);
    }

    // The IDs stay in range and apart: checked at compile time.
    const _: () = assert!(FIRST_ID + 99 <= 4094 && DEFAULT_ID < FIRST_ID);
}
