//! The apiserver, as the suites use it: plain JSON over HTTPS with the Job's
//! ServiceAccount token, verified against the mounted `ca.crt` (without one,
//! a hand run, the certificate is accepted unverified).
//!
//! Everything the suites create is in the run's namespace and labelled
//! `storm.io/test-run=<run id>`. The only cluster-scoped calls are reads (the
//! `Network` CR, `nodes`); the runner's Role does not grant them yet
//! (stormcentral#55), so a 403 is kept apart and reported as could-not-run.

use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::Value;

use crate::env::Env;
use crate::report::Outcome;

/// The label every object a suite creates carries besides the run's.
pub const WORKLOAD_LABEL: &str = "network-operator-test/workload";

/// Why a call did not succeed.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// 403: this ServiceAccount may not do it.
    Forbidden(String),
    /// Anything else: transport, 5xx, a rejected body.
    Other(String),
}

impl Error {
    pub fn msg(&self) -> String {
        match self {
            Error::Forbidden(m) => format!("{m} — the test's ServiceAccount is not granted this read (stormcentral#55)"),
            Error::Other(m) => m.clone(),
        }
    }

    /// An API error means the test could not run, not that it failed.
    pub fn outcome(&self) -> Outcome {
        Outcome::Infra(self.msg())
    }
}

impl From<Error> for String {
    fn from(e: Error) -> String {
        e.msg()
    }
}

#[derive(Clone)]
pub struct Api {
    base: String,
    http: reqwest::Client,
    token: Option<String>,
    pub ns: String,
    pub run: String,
}

/// This Job's own pod: its image (which the workloads reuse), its node and
/// its address (itself a pod on the network under test).
#[derive(Debug, Clone)]
pub struct Me {
    pub pod: String,
    pub image: String,
    pub node: Option<String>,
    pub ip: Option<String>,
}

impl Api {
    pub fn new(env: &Env) -> Result<Api, String> {
        if env.api.is_empty() {
            return Err("STORM_API is not set".into());
        }
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(20));
        b = match &env.ca {
            Some(pem) => b.add_root_certificate(reqwest::Certificate::from_pem(pem).map_err(|e| format!("ca.crt: {e}"))?),
            None => b.danger_accept_invalid_certs(true),
        };
        Ok(Api {
            base: env.api.trim_end_matches('/').to_string(),
            http: b.build().map_err(|e| format!("http client: {e}"))?,
            token: env.token.clone(),
            ns: env.namespace.clone(),
            run: env.run_id.clone(),
        })
    }

    async fn call(&self, m: Method, path: &str, body: Option<&Value>) -> Result<(u16, Value), Error> {
        let mut rq = self.http.request(m.clone(), format!("{}{path}", self.base));
        if let Some(t) = &self.token {
            rq = rq.bearer_auth(t);
        }
        if let Some(b) = body {
            rq = rq.json(b);
        }
        let resp = rq.send().await.map_err(|e| Error::Other(format!("{m} {path}: {e}")))?;
        let st = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
        if st == 403 {
            return Err(Error::Forbidden(format!("{m} {path}: 403 {}", msg(&v))));
        }
        Ok((st, v))
    }

    /// GET; `None` on 404.
    pub async fn get(&self, path: &str) -> Result<Option<Value>, Error> {
        match self.call(Method::GET, path, None).await? {
            (404, _) => Ok(None),
            (st, v) if ok(st) => Ok(Some(v)),
            (st, v) => Err(Error::Other(format!("GET {path}: {st} {}", msg(&v)))),
        }
    }

    /// A list's items; `None` when the resource is not served (404).
    pub async fn list(&self, path: &str) -> Result<Option<Vec<Value>>, Error> {
        Ok(self.get(path).await?.map(|v| v["items"].as_array().cloned().unwrap_or_default()))
    }

    pub async fn create(&self, path: &str, body: &Value) -> Result<Value, Error> {
        match self.call(Method::POST, path, Some(body)).await? {
            (st, v) if ok(st) => Ok(v),
            (st, v) => Err(Error::Other(format!("create {} {}: {st} {}", body["kind"].as_str().unwrap_or("?"), body["metadata"]["name"], msg(&v)))),
        }
    }

    pub async fn replace(&self, path: &str, body: &Value) -> Result<Value, Error> {
        match self.call(Method::PUT, path, Some(body)).await? {
            (st, v) if ok(st) => Ok(v),
            (st, v) => Err(Error::Other(format!("replace {path}: {st} {}", msg(&v)))),
        }
    }

    /// Delete now; already gone is fine.
    pub async fn delete(&self, path: &str) -> Result<(), Error> {
        let p = format!("{path}?gracePeriodSeconds=0&propagationPolicy=Background");
        match self.call(Method::DELETE, &p, None).await? {
            (st, _) if st == 404 || ok(st) => Ok(()),
            (st, v) => Err(Error::Other(format!("delete {path}: {st} {}", msg(&v)))),
        }
    }

    /// A namespaced collection in the run's namespace: `pods`, `services`, …
    pub fn ns_path(&self, group_version: &str, resource: &str) -> String {
        let root = if group_version == "v1" { "/api/v1".to_string() } else { format!("/apis/{group_version}") };
        format!("{root}/namespaces/{}/{resource}", self.ns)
    }

    pub fn pods(&self) -> String {
        self.ns_path("v1", "pods")
    }

    pub fn services(&self) -> String {
        self.ns_path("v1", "services")
    }

    pub fn deployments(&self) -> String {
        self.ns_path("apps/v1", "deployments")
    }

    pub fn policies(&self) -> String {
        self.ns_path("networking.k8s.io/v1", "networkpolicies")
    }

    pub fn cilium_endpoints(&self) -> String {
        self.ns_path("cilium.io/v2", "ciliumendpoints")
    }

    /// `?labelSelector=` for this run's workloads, optionally one app of them.
    pub fn selector(&self, app: Option<&str>) -> String {
        let mut s = format!("storm.io/test-run={},{WORKLOAD_LABEL}", self.run);
        if let Some(a) = app {
            s.push_str(&format!(",{}={a}", crate::objects::APP_LABEL));
        }
        format!("?labelSelector={}", enc(&s))
    }

    /// This Job's pod, by `HOSTNAME`.
    pub async fn me(&self, hostname: &str) -> Result<Me, String> {
        let pod = self.get(&format!("{}/{hostname}", self.pods())).await?.ok_or_else(|| format!("no pod {hostname:?} in {}", self.ns))?;
        Ok(Me {
            pod: hostname.to_string(),
            image: pod["spec"]["containers"][0]["image"].as_str().ok_or("this pod names no image")?.to_string(),
            node: pod["spec"]["nodeName"].as_str().map(str::to_string),
            ip: pod["status"]["podIP"].as_str().map(str::to_string),
        })
    }

    /// Poll a pod until it is Ready with an IP. A pod that ends first, or
    /// is not ready by `deadline`, is an error naming its state.
    pub async fn ready_pod(&self, name: &str, deadline: Instant) -> Result<String, Error> {
        let mut last = String::from("not listed");
        loop {
            if let Some(p) = self.get(&format!("{}/{name}", self.pods())).await? {
                if let Some(ip) = ready_ip(&p) {
                    return Ok(ip);
                }
                let phase = p["status"]["phase"].as_str().unwrap_or("Pending");
                if matches!(phase, "Succeeded" | "Failed") {
                    return Err(Error::Other(format!("pod {name} ended ({phase}): {}", why(&p))));
                }
                last = format!("{phase} {}", why(&p));
            }
            if Instant::now() >= deadline {
                return Err(Error::Other(format!("pod {name} not ready in time; last: {last}")));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// This run's workload pods (optionally one app's).
    pub async fn workload_pods(&self, app: Option<&str>) -> Result<Vec<Value>, Error> {
        Ok(self.list(&format!("{}{}", self.pods(), self.selector(app))).await?.unwrap_or_default())
    }

    /// Delete every workload object of this run: deployments first (so they
    /// stop replacing pods), then pods, services and policies — by listing,
    /// because the runner's Role may not cover deletecollection everywhere.
    pub async fn cleanup(&self) -> Result<usize, Error> {
        let mut n = 0;
        for coll in [self.deployments(), self.pods(), self.services(), self.policies()] {
            let Some(items) = self.list(&format!("{coll}{}", self.selector(None))).await? else { continue };
            for it in items {
                if let Some(name) = it["metadata"]["name"].as_str() {
                    self.delete(&format!("{coll}/{name}")).await?;
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// Workload objects of this run still listed, by kind.
    pub async fn leftovers(&self) -> Result<Vec<(String, usize)>, Error> {
        let mut out = Vec::new();
        for (kind, coll) in [("deployments", self.deployments()), ("pods", self.pods()), ("services", self.services()), ("networkpolicies", self.policies())] {
            let n = self.list(&format!("{coll}{}", self.selector(None))).await?.map(|v| v.len()).unwrap_or(0);
            if n > 0 {
                out.push((kind.to_string(), n));
            }
        }
        Ok(out)
    }

    /// CiliumEndpoints in the run's namespace other than this Job's own pod's:
    /// what Cilium still holds for workloads. `None` when the resource is not
    /// served at all.
    pub async fn endpoints_besides(&self, me: &str) -> Result<Option<Vec<String>>, Error> {
        Ok(self.list(&self.cilium_endpoints()).await?.map(|items| {
            items.iter().filter_map(|e| e["metadata"]["name"].as_str()).filter(|n| *n != me).map(str::to_string).collect()
        }))
    }
}

fn ok(st: u16) -> bool {
    (200..300).contains(&st)
}

/// A pod's IP once it is Running and Ready.
pub fn ready_ip(p: &Value) -> Option<String> {
    if p["status"]["phase"] != "Running" || !p["metadata"]["deletionTimestamp"].is_null() {
        return None;
    }
    let ready = p["status"]["conditions"].as_array()?.iter().any(|c| c["type"] == "Ready" && c["status"] == "True");
    let ip = p["status"]["podIP"].as_str().filter(|s| !s.is_empty())?;
    ready.then(|| ip.to_string())
}

pub fn msg(v: &Value) -> String {
    let m = v["message"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    m.chars().take(300).collect()
}

/// A pod's state in a few words, for error details.
pub fn why(p: &Value) -> String {
    let c = &p["status"]["containerStatuses"][0]["state"];
    for k in ["waiting", "terminated"] {
        if let Some(r) = c[k]["reason"].as_str() {
            return format!("{k}: {r} {}", c[k]["message"].as_str().unwrap_or(""));
        }
    }
    p["status"]["reason"].as_str().unwrap_or("").to_string()
}

/// Percent-encode a query value.
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_pod_is_ready_only_running_ready_and_addressed() {
        let mut p = json!({"metadata": {}, "status": {"phase": "Running", "podIP": "10.244.0.7",
            "conditions": [{"type": "Ready", "status": "True"}]}});
        assert_eq!(ready_ip(&p).as_deref(), Some("10.244.0.7"));
        p["status"]["conditions"][0]["status"] = json!("False");
        assert_eq!(ready_ip(&p), None);
        p["status"]["conditions"][0]["status"] = json!("True");
        p["metadata"]["deletionTimestamp"] = json!("2026-09-27T00:00:00Z");
        assert_eq!(ready_ip(&p), None, "a terminating pod is not a backend");
    }

    #[test]
    fn selectors_are_encoded() {
        assert_eq!(enc("storm.io/test-run=r1,a"), "storm.io%2Ftest-run%3Dr1%2Ca");
    }

    #[test]
    fn forbidden_names_the_runner_gap() {
        let e = Error::Forbidden("GET /apis/network.storm.io/v1/networks: 403".into());
        assert!(matches!(e.outcome(), Outcome::Infra(m) if m.contains("stormcentral#55")));
    }
}
