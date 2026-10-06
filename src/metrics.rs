//! The operator's own health and metrics listener (#15).
//!
//! Three paths, plain HTTP/1.1, one response per connection:
//!
//! - `/healthz` — liveness: 200 while the process is serving.
//! - `/readyz`  — readiness: 200 once the Network CRD is registered and the
//!   controller has started, 503 before that.
//! - `/metrics` — Prometheus text format: reconcile passes by result, failures
//!   by reason, the time of the last successful pass, and build info.
//!
//! Hand-rolled on a `TcpListener` rather than pulling in an HTTP stack: the
//! surface is three GET paths, and everything that decides a response
//! ([`Metrics::render`], [`respond`]) is a pure function the tests cover
//! without a socket.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info};

/// Default listen address. The operator is host-networked, so this is a host
/// port: 9446 is clear of every port Cilium binds (see README "Ports").
pub const DEFAULT_ADDR: &str = "0.0.0.0:9446";

/// Why a reconcile pass failed — the `reason` label. Mirrors
/// `controller::Error`'s variants. The discriminant indexes the counters, in
/// [`FailureReason::ALL`] order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    Invalid = 0,
    Immutable,
    Apply,
    Kube,
}

impl FailureReason {
    pub const ALL: [FailureReason; 4] = [
        FailureReason::Invalid,
        FailureReason::Immutable,
        FailureReason::Apply,
        FailureReason::Kube,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FailureReason::Invalid => "invalid",
            FailureReason::Immutable => "immutable",
            FailureReason::Apply => "apply",
            FailureReason::Kube => "kube",
        }
    }
}

/// Counters shared between the reconciler and the listener.
#[derive(Debug, Default)]
pub struct Metrics {
    ready: AtomicBool,
    successes: AtomicU64,
    failures: [AtomicU64; 4],
    /// Unix seconds of the last successful pass; 0 = never.
    last_success: AtomicI64,
    /// Duration of the last pass, success or failure, in microseconds.
    last_duration_us: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::Relaxed);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    pub fn record_success(&self, at_unix: i64, duration_us: u64) {
        self.successes.fetch_add(1, Ordering::Relaxed);
        self.last_success.store(at_unix, Ordering::Relaxed);
        self.last_duration_us.store(duration_us, Ordering::Relaxed);
    }

    pub fn record_failure(&self, reason: FailureReason, duration_us: u64) {
        self.failures[reason as usize].fetch_add(1, Ordering::Relaxed);
        self.last_duration_us.store(duration_us, Ordering::Relaxed);
    }

    /// The `/metrics` body, Prometheus text exposition format 0.0.4.
    pub fn render(&self) -> String {
        let successes = self.successes.load(Ordering::Relaxed);
        let failures: Vec<u64> = self.failures.iter().map(|f| f.load(Ordering::Relaxed)).collect();
        let failed: u64 = failures.iter().sum();

        let mut out = String::new();
        out.push_str("# HELP network_operator_build_info The running operator's version.\n");
        out.push_str("# TYPE network_operator_build_info gauge\n");
        out.push_str(&format!(
            "network_operator_build_info{{version=\"{}\"}} 1\n",
            env!("CARGO_PKG_VERSION")
        ));

        out.push_str("# HELP network_operator_ready 1 once the CRD is registered and the controller runs.\n");
        out.push_str("# TYPE network_operator_ready gauge\n");
        out.push_str(&format!("network_operator_ready {}\n", u8::from(self.is_ready())));

        out.push_str("# HELP network_operator_reconciles_total Reconcile passes, by result.\n");
        out.push_str("# TYPE network_operator_reconciles_total counter\n");
        out.push_str(&format!("network_operator_reconciles_total{{result=\"success\"}} {successes}\n"));
        out.push_str(&format!("network_operator_reconciles_total{{result=\"error\"}} {failed}\n"));

        out.push_str("# HELP network_operator_reconcile_errors_total Failed reconcile passes, by reason.\n");
        out.push_str("# TYPE network_operator_reconcile_errors_total counter\n");
        for (reason, n) in FailureReason::ALL.iter().zip(&failures) {
            out.push_str(&format!(
                "network_operator_reconcile_errors_total{{reason=\"{}\"}} {n}\n",
                reason.label()
            ));
        }

        out.push_str("# HELP network_operator_last_reconcile_success_timestamp_seconds Unix time of the last successful pass (0 = never).\n");
        out.push_str("# TYPE network_operator_last_reconcile_success_timestamp_seconds gauge\n");
        out.push_str(&format!(
            "network_operator_last_reconcile_success_timestamp_seconds {}\n",
            self.last_success.load(Ordering::Relaxed)
        ));

        out.push_str("# HELP network_operator_last_reconcile_duration_seconds Duration of the last pass.\n");
        out.push_str("# TYPE network_operator_last_reconcile_duration_seconds gauge\n");
        out.push_str(&format!(
            "network_operator_last_reconcile_duration_seconds {:.6}\n",
            self.last_duration_us.load(Ordering::Relaxed) as f64 / 1e6
        ));
        out
    }
}

/// A response: status line, content type, body.
#[derive(Debug, PartialEq, Eq)]
pub struct Response {
    pub status: &'static str,
    pub content_type: &'static str,
    pub body: String,
}

/// Decide the response to a request line (`GET /path HTTP/1.1`).
pub fn respond(request_line: &str, metrics: &Metrics) -> Response {
    let text = |status, body: &str| Response {
        status,
        content_type: "text/plain; charset=utf-8",
        body: body.to_string(),
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    // Ignore any query string: `/metrics?x=y` is `/metrics`.
    let path = parts.next().unwrap_or("").split('?').next().unwrap_or("");
    if method != "GET" && method != "HEAD" {
        return text("405 Method Not Allowed", "method not allowed\n");
    }
    match path {
        "/healthz" | "/livez" => text("200 OK", "ok\n"),
        "/readyz" if metrics.is_ready() => text("200 OK", "ok\n"),
        "/readyz" => text("503 Service Unavailable", "not ready\n"),
        "/metrics" => Response {
            status: "200 OK",
            content_type: "text/plain; version=0.0.4; charset=utf-8",
            body: metrics.render(),
        },
        _ => text("404 Not Found", "not found\n"),
    }
}

/// Bind `addr` and serve until the process exits. Binding happens before this
/// returns, so a port clash fails startup loudly instead of in a background
/// task.
pub async fn serve(addr: SocketAddr, metrics: Arc<Metrics>) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    info!(addr = %local, "health/metrics listener up");
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                continue;
            };
            let metrics = metrics.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(stream, &metrics).await {
                    debug!(%peer, error = %e, "health/metrics request failed");
                }
            });
        }
    });
    Ok(local)
}

async fn handle(mut stream: tokio::net::TcpStream, metrics: &Metrics) -> std::io::Result<()> {
    // Only the request line matters; read until the end of the headers (or
    // 8 KiB, or 5 s) and answer.
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let read = async {
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 8192 {
                break;
            }
        }
        Ok::<_, std::io::Error>(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), read)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "request timed out"))??;

    let head = String::from_utf8_lossy(&buf);
    let request_line = head.lines().next().unwrap_or("");
    let resp = respond(request_line, metrics);
    let body = if request_line.starts_with("HEAD ") { "" } else { resp.body.as_str() };
    let out = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        resp.status,
        resp.content_type,
        resp.body.len(),
        body
    );
    stream.write_all(out.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthz_is_always_ok() {
        let m = Metrics::default();
        assert_eq!(respond("GET /healthz HTTP/1.1", &m).status, "200 OK");
        assert_eq!(respond("GET /livez HTTP/1.1", &m).status, "200 OK");
    }

    #[test]
    fn readyz_follows_ready() {
        let m = Metrics::default();
        assert_eq!(respond("GET /readyz HTTP/1.1", &m).status, "503 Service Unavailable");
        m.set_ready();
        assert_eq!(respond("GET /readyz HTTP/1.1", &m).status, "200 OK");
    }

    #[test]
    fn unknown_path_and_method() {
        let m = Metrics::default();
        assert_eq!(respond("GET /nope HTTP/1.1", &m).status, "404 Not Found");
        assert_eq!(respond("POST /metrics HTTP/1.1", &m).status, "405 Method Not Allowed");
        assert_eq!(respond("", &m).status, "405 Method Not Allowed");
    }

    #[test]
    fn query_string_is_ignored() {
        let m = Metrics::default();
        assert_eq!(respond("GET /metrics?x=1 HTTP/1.1", &m).status, "200 OK");
    }

    #[test]
    fn metrics_count_by_result_and_reason() {
        let m = Metrics::default();
        m.record_success(1_700_000_000, 1_500_000);
        m.record_success(1_700_000_060, 250_000);
        m.record_failure(FailureReason::Immutable, 1000);
        m.record_failure(FailureReason::Apply, 1000);
        m.record_failure(FailureReason::Apply, 2000);
        let body = m.render();
        for line in [
            "network_operator_reconciles_total{result=\"success\"} 2",
            "network_operator_reconciles_total{result=\"error\"} 3",
            "network_operator_reconcile_errors_total{reason=\"invalid\"} 0",
            "network_operator_reconcile_errors_total{reason=\"immutable\"} 1",
            "network_operator_reconcile_errors_total{reason=\"apply\"} 2",
            "network_operator_reconcile_errors_total{reason=\"kube\"} 0",
            "network_operator_last_reconcile_success_timestamp_seconds 1700000060",
            "network_operator_last_reconcile_duration_seconds 0.002000",
            "network_operator_ready 0",
        ] {
            assert!(body.lines().any(|l| l == line), "missing {line:?} in:\n{body}");
        }
        assert!(body.contains(&format!(
            "network_operator_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
    }

    #[test]
    fn metrics_are_well_formed() {
        // Every sample line belongs to a family declared by a TYPE line.
        let body = Metrics::default().render();
        let families: Vec<&str> = body
            .lines()
            .filter_map(|l| l.strip_prefix("# TYPE "))
            .map(|l| l.split_whitespace().next().unwrap())
            .collect();
        for l in body.lines().filter(|l| !l.starts_with('#')) {
            let name = l.split(['{', ' ']).next().unwrap();
            assert!(families.contains(&name), "undeclared sample {l:?}");
        }
    }

    #[tokio::test]
    async fn serves_over_tcp() {
        let m = Metrics::new();
        m.set_ready();
        let addr = serve("127.0.0.1:0".parse().unwrap(), m).await.unwrap();
        for (path, want) in [("/healthz", "200 OK"), ("/readyz", "200 OK"), ("/metrics", "200 OK"), ("/x", "404 Not Found")] {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).await.unwrap();
            assert!(resp.starts_with(&format!("HTTP/1.1 {want}\r\n")), "{path}: {resp}");
            if path == "/metrics" {
                assert!(resp.contains("network_operator_ready 1"));
            }
        }
    }
}
