//! The network gate (PERMISSIONS.md §4): an HTTP proxy the harness runs on a
//! Unix socket. Inside the sandbox there is no network beyond loopback;
//! `HTTPS_PROXY` points at a loopback port the init forwards to this socket,
//! and every connection is decided here by hostname — package registries by
//! default, anything else only while a per-call grant is active. Refused
//! hosts are remembered so the tool result can name them.
//!
//! Runs on its own thread with its own runtime: the sandbox starts from
//! synchronous code and must outlive any caller's runtime.

use anyhow::{Context, Result, anyhow, bail};
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixListener, UnixStream};

/// Package registries, reachable without asking (§4.2).
pub const REGISTRIES: &[&str] = &[
    "crates.io",
    "index.crates.io",
    "static.crates.io",
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    "pypi.org",
    "files.pythonhosted.org",
    "proxy.golang.org",
    "sum.golang.org",
];

/// The loopback port inside the sandbox that forwards here.
pub const PORT: u16 = 3128;

/// Request heads longer than this are refused outright.
const MAX_HEAD: usize = 16 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// `host` (ports 80/443), `.suffix` (any subdomain), either with `:port`.
#[derive(Debug, Clone)]
struct Rule {
    host: String,
    port: Option<u16>,
}

impl Rule {
    fn parse(entry: &str) -> Rule {
        let entry = entry.trim().to_ascii_lowercase();
        match entry.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
                Rule { host: host.to_string(), port: port.parse().ok() }
            }
            _ => Rule { host: entry, port: None },
        }
    }

    fn allows(&self, host: &str, port: u16) -> bool {
        let host_ok = match self.host.strip_prefix('.') {
            Some(suffix) => host == suffix || host.ends_with(&self.host) || host.ends_with(suffix) && host.len() > suffix.len() && host.as_bytes()[host.len() - suffix.len() - 1] == b'.',
            None => host == self.host,
        };
        host_ok && self.port.map_or(port == 80 || port == 443, |p| p == port)
    }
}

#[derive(Default)]
struct State {
    /// A per-call full-network grant is active (§4.3).
    full: bool,
    /// Hosts refused since the last `take_blocked`, in order, deduplicated.
    blocked: Vec<String>,
}

pub struct Proxy {
    #[cfg(test)]
    rules: Arc<Vec<Rule>>,
    state: Arc<Mutex<State>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Proxy {
    /// Listen on `socket` (removed and recreated) with the registries plus
    /// `allow` from config.
    pub fn start(socket: &Path, allow: &[String]) -> Result<Proxy> {
        let _ = std::fs::remove_file(socket);
        let std_listener = std::os::unix::net::UnixListener::bind(socket)
            .with_context(|| format!("binding the proxy socket {}", socket.display()))?;
        std_listener.set_nonblocking(true)?;
        let rules: Arc<Vec<Rule>> = Arc::new(REGISTRIES.iter().map(|r| Rule::parse(r)).chain(allow.iter().map(|a| Rule::parse(a))).collect());
        let state = Arc::new(Mutex::new(State::default()));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let (thread_rules, thread_state) = (rules.clone(), state.clone());
        std::thread::Builder::new().name("sandbox-proxy".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("proxy runtime: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                let listener = match UnixListener::from_std(std_listener) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!("proxy listener: {e}");
                        return;
                    }
                };
                let mut shutdown = shutdown_rx;
                loop {
                    tokio::select! {
                        _ = &mut shutdown => break,
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { break };
                            let (rules, state) = (thread_rules.clone(), thread_state.clone());
                            tokio::spawn(async move {
                                if let Err(e) = handle(stream, &rules, &state).await {
                                    tracing::debug!("proxy connection: {e:#}");
                                }
                            });
                        }
                    }
                }
            });
        })?;
        #[cfg(not(test))]
        drop(rules);
        Ok(Proxy {
            #[cfg(test)]
            rules,
            state,
            shutdown: Some(shutdown_tx),
        })
    }

    /// Full network for the duration of one call (§4.3).
    pub fn grant_full(&self, on: bool) {
        self.state.lock().unwrap().full = on;
    }

    /// Hosts refused since the last call, cleared.
    pub fn take_blocked(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().unwrap().blocked)
    }

    #[cfg(test)]
    pub fn allows(&self, host: &str, port: u16) -> bool {
        self.state.lock().unwrap().full || self.rules.iter().any(|r| r.allows(host, port))
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// One proxied connection: CONNECT (TLS tunnels) or an absolute-URI request
/// (plain HTTP), decided by hostname.
async fn handle(mut client: UnixStream, rules: &[Rule], state: &Mutex<State>) -> Result<()> {
    let (head, mut rest) = read_head(&mut client).await?;
    let request = parse(&head)?;
    let allowed = {
        let mut s = state.lock().unwrap();
        let ok = request.ip_literal.is_none()
            && (s.full || rules.iter().any(|r| r.allows(&request.host, request.port)));
        if !ok {
            let name = request.ip_literal.clone().unwrap_or_else(|| format!("{}:{}", request.host, request.port));
            if !s.blocked.contains(&name) {
                s.blocked.push(name);
            }
        }
        ok
    };
    if !allowed {
        client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }
    let upstream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((request.host.as_str(), request.port))).await;
    let mut upstream = match upstream {
        Ok(Ok(s)) => s,
        _ => {
            client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
            return Ok(());
        }
    };
    match request.forward {
        None => client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?,
        Some(head) => {
            upstream.write_all(head.as_bytes()).await?;
            upstream.write_all(&rest).await?;
            rest.clear();
        }
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

/// The request head up to the blank line, plus any bytes read past it.
async fn read_head(client: &mut UnixStream) -> Result<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            bail!("client closed before the request head ended");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Ok((String::from_utf8_lossy(&buf).into_owned(), rest));
        }
        if buf.len() > MAX_HEAD {
            bail!("request head too large");
        }
    }
}

struct Request {
    host: String,
    port: u16,
    /// The target was an IP literal (always refused — rules are by name).
    ip_literal: Option<String>,
    /// For plain HTTP: the head to forward, rewritten to origin form.
    /// None for CONNECT.
    forward: Option<String>,
}

fn parse(head: &str) -> Result<Request> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let (method, target, version) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or("HTTP/1.1"));
    let (authority, forward) = if method.eq_ignore_ascii_case("CONNECT") {
        (target.to_string(), None)
    } else {
        let without_scheme = target.strip_prefix("http://").ok_or_else(|| anyhow!("proxy requests must be CONNECT or an absolute http:// URI"))?;
        let (authority, path) = without_scheme.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((without_scheme, "/".to_string()));
        let mut rewritten = format!("{method} {path} {version}\r\n");
        for line in lines {
            if line.is_empty() {
                break;
            }
            if !line.to_ascii_lowercase().starts_with("proxy-") {
                rewritten.push_str(line);
                rewritten.push_str("\r\n");
            }
        }
        rewritten.push_str("\r\n");
        (authority.to_string(), Some(rewritten))
    };
    let (host, port) = split_authority(&authority, if forward.is_some() { 80 } else { 443 })?;
    let ip_literal = host.trim_matches(['[', ']']).parse::<IpAddr>().ok().map(|_| authority.clone());
    Ok(Request { host: host.to_ascii_lowercase(), port, ip_literal, forward })
}

fn split_authority(authority: &str, default_port: u16) -> Result<(String, u16)> {
    if authority.is_empty() {
        bail!("empty host");
    }
    if let Some(end) = authority.strip_prefix('[').and_then(|a| a.find(']')) {
        let host = &authority[..end + 2];
        let port = authority[end + 2..].strip_prefix(':').map(|p| p.parse::<u16>()).transpose()?.unwrap_or(default_port);
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Ok((host.to_string(), port.parse().with_context(|| format!("bad port in {authority}"))?)),
        None => Ok((authority.to_string(), default_port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_match_hosts_suffixes_and_ports() {
        let r = Rule::parse("crates.io");
        assert!(r.allows("crates.io", 443) && r.allows("crates.io", 80));
        assert!(!r.allows("crates.io", 8443), "non-standard port needs an explicit rule");
        assert!(!r.allows("evil-crates.io", 443) && !r.allows("sub.crates.io", 443));
        let s = Rule::parse(".example.com");
        assert!(s.allows("example.com", 443) && s.allows("api.example.com", 443));
        assert!(!s.allows("notexample.com", 443));
        let p = Rule::parse("localhost:9000");
        assert!(p.allows("localhost", 9000) && !p.allows("localhost", 443));
    }

    #[test]
    fn requests_parse_connect_and_absolute_uris() {
        let c = parse("CONNECT index.crates.io:443 HTTP/1.1\r\nHost: index.crates.io:443\r\n\r\n").unwrap();
        assert_eq!((c.host.as_str(), c.port), ("index.crates.io", 443));
        assert!(c.forward.is_none() && c.ip_literal.is_none());
        let g = parse("GET http://pypi.org/simple/x/ HTTP/1.1\r\nHost: pypi.org\r\nProxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n").unwrap();
        assert_eq!((g.host.as_str(), g.port), ("pypi.org", 80));
        let head = g.forward.unwrap();
        assert!(head.starts_with("GET /simple/x/ HTTP/1.1\r\n") && head.contains("Accept: */*") && !head.contains("Proxy-Connection"));
        assert!(parse("CONNECT 1.1.1.1:443 HTTP/1.1\r\n\r\n").unwrap().ip_literal.is_some());
        assert!(parse("GET /relative HTTP/1.1\r\n\r\n").is_err());
    }

    #[tokio::test]
    async fn blocked_hosts_get_403_and_are_remembered_until_taken() {
        let dir = crate::tools::testutil::tmp("proxy");
        let socket = dir.join("p.sock");
        let proxy = Proxy::start(&socket, &["allowed.example".into()]).unwrap();
        let mut c = UnixStream::connect(&socket).await.unwrap();
        c.write_all(b"CONNECT blocked.example:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut reply = vec![0u8; 256];
        let n = c.read(&mut reply).await.unwrap();
        assert!(String::from_utf8_lossy(&reply[..n]).starts_with("HTTP/1.1 403"));
        assert_eq!(proxy.take_blocked(), vec!["blocked.example:443".to_string()]);
        assert!(proxy.take_blocked().is_empty(), "cleared by take");
        assert!(proxy.allows("allowed.example", 443) && proxy.allows("crates.io", 443));
        assert!(!proxy.allows("blocked.example", 443));
        proxy.grant_full(true);
        assert!(proxy.allows("blocked.example", 443));
    }

    /// A full tunnel through the proxy to a local upstream.
    #[tokio::test]
    async fn connect_tunnels_to_an_allowed_upstream() {
        let dir = crate::tools::testutil::tmp("proxy-tunnel");
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = upstream.accept().await.unwrap();
            let mut b = [0u8; 4];
            s.read_exact(&mut b).await.unwrap();
            s.write_all(b"pong").await.unwrap();
        });
        let socket = dir.join("p.sock");
        let _proxy = Proxy::start(&socket, &[format!("localhost:{port}")]).unwrap();
        let mut c = UnixStream::connect(&socket).await.unwrap();
        c.write_all(format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n").as_bytes()).await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            c.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"));
        c.write_all(b"ping").await.unwrap();
        let mut reply = [0u8; 4];
        c.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"pong");
    }
}
