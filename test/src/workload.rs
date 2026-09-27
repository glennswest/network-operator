//! The workload and its client. `/test workload serve <port>` answers every
//! HTTP request with its own pod name (`HOSTNAME`), so a reply says *which*
//! backend a Service reached. [`probe`] is the other end, run from this Job's
//! own pod — itself a pod on the network under test.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Serve until killed. Returns an exit code only if it cannot listen.
pub async fn serve(port: u16) -> i32 {
    let name = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into());
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("listen :{port}: {e}");
            return 2;
        }
    };
    eprintln!("serving {name} on :{port}");
    loop {
        let Ok((mut s, _)) = listener.accept().await else { continue };
        let body = format!("{name}\n");
        tokio::spawn(async move {
            // Read the request head (bounded); a bare TCP check sends none.
            let mut buf = [0u8; 4096];
            let mut seen = Vec::new();
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") && seen.len() < 16384 {
                match tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await {
                    Ok(Ok(n)) if n > 0 => seen.extend_from_slice(&buf[..n]),
                    _ => return,
                }
            }
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = s.write_all(resp.as_bytes()).await;
            let _ = s.shutdown().await;
        });
    }
}

/// One HTTP GET to `addr`: the name the workload answered with, and how long
/// the round trip took (connect included).
pub async fn probe(addr: SocketAddr, timeout: Duration) -> Result<(String, Duration), String> {
    let t = Instant::now();
    let fut = async {
        let mut s = TcpStream::connect(addr).await.map_err(|e| format!("connect {addr}: {e}"))?;
        s.write_all(format!("GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes())
            .await
            .map_err(|e| format!("write {addr}: {e}"))?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.map_err(|e| format!("read {addr}: {e}"))?;
        parse(&out).map_err(|e| format!("{addr}: {e}"))
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(name)) => Ok((name, t.elapsed())),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!("{addr}: no answer in {} ms", timeout.as_millis())),
    }
}

/// Probe until the answer satisfies `want`, or `deadline`. Returns the last
/// answer that did, or the last error.
pub async fn probe_until(addr: SocketAddr, deadline: Instant, want: impl Fn(&str) -> bool) -> Result<(String, Duration), String> {
    loop {
        let last = match probe(addr, Duration::from_secs(3)).await {
            Ok((n, d)) if want(&n) => return Ok((n, d)),
            Ok((n, _)) => format!("{addr} answered {n:?}"),
            Err(e) => e,
        };
        if Instant::now() >= deadline {
            return Err(last);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The body of a `200` reply, trimmed.
pub fn parse(raw: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("no HTTP reply")?;
    let status = head.lines().next().unwrap_or("");
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(format!("status {status:?}"));
    }
    Ok(body.trim().to_string())
}

/// `ip:port`, for an IPv4 or IPv6 address as the API writes it.
pub fn addr(ip: &str, port: u16) -> Result<SocketAddr, String> {
    let ip: std::net::IpAddr = ip.parse().map_err(|_| format!("{ip:?} is not an IP"))?;
    Ok(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_parse_to_the_pod_name() {
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\npod-a\n").unwrap(), "pod-a");
        assert!(parse(b"HTTP/1.1 503 Service Unavailable\r\n\r\n").is_err());
        assert!(parse(b"garbage").is_err());
    }

    #[test]
    fn addresses_take_both_families() {
        assert_eq!(addr("10.244.0.7", 8080).unwrap().to_string(), "10.244.0.7:8080");
        assert_eq!(addr("fd00::7", 80).unwrap().to_string(), "[fd00::7]:80");
        assert!(addr("pod-a", 80).is_err());
    }

    #[tokio::test]
    async fn serve_and_probe_round_trip() {
        // Find a free port, then serve on it.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        tokio::spawn(serve(port));
        let a = addr("127.0.0.1", port).unwrap();
        let (name, _) = probe_until(a, Instant::now() + Duration::from_secs(5), |_| true).await.unwrap();
        assert_eq!(name, std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()));
    }
}
