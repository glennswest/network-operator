//! What the runner hands the container (stormcentral `docs/test-standard.md`).
//! `STORM_NODE` is not read: everything here goes through the API.

use std::time::{Duration, Instant};

/// Where the kubelet mounts the Job's ServiceAccount.
pub const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

pub struct Env {
    pub suite: String,
    pub run_id: String,
    /// `STORM_NAMESPACE`: everything the run creates goes here.
    pub namespace: String,
    /// `STORM_API`: the apiserver.
    pub api: String,
    /// This pod's name (`HOSTNAME`, which the kubelet sets to it).
    pub pod: String,
    pub token: Option<String>,
    pub ca: Option<Vec<u8>>,
    pub timeout: Duration,
    pub started: Instant,
}

impl Env {
    pub fn read(suite_arg: Option<String>) -> Env {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let sa = |f: &str| std::fs::read_to_string(format!("{SA_DIR}/{f}")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let suite = suite_arg.or_else(|| var("STORM_SUITE")).unwrap_or_else(|| "short".into());
        let budget = match suite.as_str() {
            "medium" => 1800,
            "long" => 8 * 3600,
            "drift-heal" => 600,
            _ => 120,
        };
        Env {
            run_id: var("STORM_RUN_ID").unwrap_or_default(),
            namespace: var("STORM_NAMESPACE").or_else(|| sa("namespace")).unwrap_or_default(),
            api: var("STORM_API").unwrap_or_default(),
            pod: var("HOSTNAME").unwrap_or_default(),
            token: sa("token"),
            ca: std::fs::read(format!("{SA_DIR}/ca.crt")).ok(),
            timeout: Duration::from_secs(var("STORM_TIMEOUT").and_then(|v| v.parse().ok()).unwrap_or(budget)),
            started: Instant::now(),
            suite,
        }
    }

    /// What the runner must have set and did not.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.api.is_empty() {
            m.push("STORM_API");
        }
        if self.namespace.is_empty() {
            m.push("STORM_NAMESPACE");
        }
        if self.run_id.is_empty() {
            m.push("STORM_RUN_ID");
        }
        m
    }

    pub fn remaining(&self) -> Duration {
        self.timeout.saturating_sub(self.started.elapsed())
    }

    /// A wait of at most `want`, leaving `reserve` of the budget for what
    /// follows (cleanup, the summary).
    pub fn wait(&self, want: Duration, reserve: Duration) -> Duration {
        want.min(self.remaining().saturating_sub(reserve))
    }

    /// The run id as a DNS label, at most 20 characters, so object names
    /// that carry it stay under 63.
    pub fn slug(&self) -> String {
        let s: String = self.run_id.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let s = s.trim_matches('-');
        let s = s[..s.len().min(20)].trim_end_matches('-');
        if s.is_empty() { "run".into() } else { s.to_string() }
    }

    /// An object name for this run: `nt-<slug>-<what>`, a DNS label.
    pub fn name(&self, what: &str) -> String {
        let n = format!("nt-{}-{what}", self.slug());
        n[..n.len().min(63)].trim_end_matches('-').to_string()
    }
}

#[cfg(test)]
pub fn fake(run_id: &str) -> Env {
    Env {
        suite: "short".into(),
        run_id: run_id.into(),
        namespace: "test-network-operator-short-r1".into(),
        api: "https://127.0.0.1:6443".into(),
        pod: "test-abc".into(),
        token: None,
        ca: None,
        timeout: Duration::from_secs(120),
        started: Instant::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_dns_labels_and_per_run() {
        assert_eq!(fake("Run_42/x").name("a"), "nt-run-42-x-a");
        assert_eq!(fake("--").slug(), "run");
        let long = fake(&"x".repeat(90));
        assert!(long.name(&"y".repeat(80)).len() <= 63);
    }

    #[test]
    fn the_runner_must_set_api_namespace_and_run() {
        let mut e = fake("r");
        assert!(e.missing().is_empty());
        e.api.clear();
        e.run_id.clear();
        assert_eq!(e.missing(), vec!["STORM_API", "STORM_RUN_ID"]);
    }

    #[test]
    fn waits_leave_the_reserve() {
        let e = fake("r");
        assert_eq!(e.wait(Duration::from_secs(30), Duration::from_secs(10)), Duration::from_secs(30));
        assert!(e.wait(Duration::from_secs(300), Duration::from_secs(10)) <= Duration::from_secs(110));
    }
}
