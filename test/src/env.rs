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
    /// This pod's name from `HOSTNAME`: right under upstream's kubelet, the
    /// container's name under rustkube-node (rustkube-node#320), so
    /// `Api::me` falls back to [`Env::local_ip`].
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

    /// The address this container reaches the apiserver from: its pod IP on
    /// the pod network. A connected UDP socket sends nothing; it only makes
    /// the kernel pick the route and source address.
    pub fn local_ip(&self) -> Option<String> {
        use std::net::ToSocketAddrs;
        let addr = api_target(&self.api)?.to_socket_addrs().ok()?.next()?;
        let bind = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
        let sock = std::net::UdpSocket::bind(bind).ok()?;
        sock.connect(addr).ok()?;
        Some(sock.local_addr().ok()?.ip().to_string())
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

/// `STORM_API`'s `host:port`, with https's 443 when it names no port.
fn api_target(api: &str) -> Option<String> {
    let rest = api.split_once("://").map_or(api, |(_, r)| r);
    let hp = rest.split('/').next().filter(|h| !h.is_empty())?;
    let has_port = match hp.strip_prefix('[') {
        Some(v6) => v6.contains("]:"),
        None => hp.contains(':'),
    };
    Some(if has_port { hp.to_string() } else { format!("{hp}:443") })
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
    fn the_api_target_has_a_port() {
        assert_eq!(api_target("https://10.96.0.1:6443").as_deref(), Some("10.96.0.1:6443"));
        assert_eq!(api_target("https://kubernetes.default.svc/").as_deref(), Some("kubernetes.default.svc:443"));
        assert_eq!(api_target("https://[fd00::1]:6443/x").as_deref(), Some("[fd00::1]:6443"));
        assert_eq!(api_target("https://[fd00::1]").as_deref(), Some("[fd00::1]:443"));
        assert_eq!(api_target(""), None);
    }

    #[test]
    fn this_containers_address_is_the_route_to_the_api() {
        // 127.0.0.1:6443 routes over loopback, whatever listens there.
        assert_eq!(fake("r").local_ip().as_deref(), Some("127.0.0.1"));
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
