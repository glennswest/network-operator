//! What every suite sets up once: the apiserver, this Job's own pod (whose
//! image the workloads reuse, and whose address is a pod on the network under
//! test), and the cluster's `Network`.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::api::{Api, Me};
use crate::env::Env;
use crate::network::{self, Found};
use crate::objects;
use crate::report::Outcome;
use crate::workload;

pub struct Ctx {
    pub api: Api,
    pub me: Me,
    pub found: Found,
}

impl Ctx {
    /// `Err` is why no test can run at all.
    pub async fn setup(env: &Env) -> Result<Ctx, Outcome> {
        let missing = env.missing();
        if !missing.is_empty() {
            return Err(Outcome::Infra(format!("the runner did not set {}", missing.join(", "))));
        }
        let api = Api::new(env).map_err(Outcome::Infra)?;
        let me = api.me(&env.pod).await.map_err(|e| Outcome::Infra(format!("reading this Job's pod: {e}")))?;
        let found = network::find(&api).await;
        Ok(Ctx { api, me, found })
    }

    /// Whether the workload tests apply here: not when network-operator is
    /// not deployed (then the CNI, if any, is not ours to test).
    pub fn not_ours(&self) -> Option<Outcome> {
        match &self.found {
            Found::NotDeployed(w) => Some(Outcome::Skip(w.clone())),
            _ => None,
        }
    }

    pub fn pod_cidrs(&self) -> Vec<ipnet::IpNet> {
        self.found.cluster().map(|c| c.pod_cidrs()).unwrap_or_default()
    }

    pub fn service_cidrs(&self) -> Vec<ipnet::IpNet> {
        self.found.cluster().map(|c| c.service_cidrs()).unwrap_or_default()
    }

    /// Start one workload pod and wait for it to be Ready. Returns its IP.
    pub async fn start_pod(&self, name: &str, app: &str, node: Option<&str>, deadline: Instant) -> Result<String, Outcome> {
        let p = objects::pod(&self.api.ns, &self.api.run, name, app, &self.me.image, node);
        self.api.create(&self.api.pods(), &p).await.map_err(|e| e.outcome())?;
        self.api.ready_pod(name, deadline).await.map_err(|e| Outcome::Fail(e.msg()))
    }

    /// Create a Service in front of `app` and wait for its address: the
    /// ClusterIP, or for a LoadBalancer the ingress IP.
    pub async fn start_service(&self, name: &str, app: &str, kind: &str, deadline: Instant) -> Result<String, Outcome> {
        let s = objects::service(&self.api.ns, &self.api.run, name, app, kind);
        self.api.create(&self.api.services(), &s).await.map_err(|e| e.outcome())?;
        let path = format!("{}/{name}", self.api.services());
        loop {
            let v: Option<Value> = self.api.get(&path).await.map_err(|e| e.outcome())?;
            let ip = v.as_ref().and_then(|v| match kind {
                "LoadBalancer" => v["status"]["loadBalancer"]["ingress"][0]["ip"].as_str().map(str::to_string),
                _ => v["spec"]["clusterIP"].as_str().filter(|s| !s.is_empty() && *s != "None").map(str::to_string),
            });
            if let Some(ip) = ip {
                return Ok(ip);
            }
            if Instant::now() >= deadline {
                return Err(Outcome::Fail(format!("{kind} Service {name} got no address in time")));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Delete everything this run made and wait for it to go. `Pass` names
    /// what went; `Fail` names what stayed.
    pub async fn cleanup(&self, wait: Duration) -> Outcome {
        let n = match self.api.cleanup().await {
            Ok(n) => n,
            Err(e) => return e.outcome(),
        };
        let deadline = Instant::now() + wait;
        loop {
            match self.api.leftovers().await {
                Ok(l) if l.is_empty() => return Outcome::Pass(format!("{n} objects deleted, none left")),
                Ok(l) if Instant::now() >= deadline => return Outcome::Fail(format!("still listed after {} s: {l:?}", wait.as_secs())),
                Err(e) => return e.outcome(),
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }
}

/// Reach `ip:port` and expect `name` (or any name when `None`).
pub async fn reach(ip: &str, port: u16, name: Option<&str>, deadline: Instant) -> Result<(String, Duration), String> {
    let a = workload::addr(ip, port)?;
    workload::probe_until(a, deadline, |got| name.is_none_or(|n| got == n)).await
}
