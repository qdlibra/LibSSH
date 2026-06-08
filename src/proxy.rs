//! Outbound proxy parsing and transparent tunnel setup.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProxyKind {
    Socks5,
    Http,
}

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    kind: ProxyKind,
    host: String,
    port: u16,
    auth: Option<(String, String)>,
}

pub fn resolve(session_proxy: &str) -> Option<ProxyConfig> {
    let s = session_proxy.trim();
    if !s.is_empty() {
        return parse(s);
    }
    for var in ["ALL_PROXY", "all_proxy"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return parse(v.trim());
            }
        }
    }
    None
}

fn parse(url: &str) -> Option<ProxyConfig> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("socks5", url));
    let kind = match scheme.to_ascii_lowercase().as_str() {
        "socks5" | "socks5h" | "socks" => ProxyKind::Socks5,
        "http" | "https" => ProxyKind::Http,
        _ => return None,
    };
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((userinfo, hp)) => {
            let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            (Some((user.to_string(), pass.to_string())), hp)
        }
        None => (None, rest),
    };
    let hostport = hostport.trim_end_matches('/');
    let (host, port) = hostport.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some(ProxyConfig {
        kind,
        host: host.to_string(),
        port,
        auth,
    })
}

pub fn describe(cfg: &ProxyConfig) -> String {
    let scheme = match cfg.kind {
        ProxyKind::Socks5 => "socks5",
        ProxyKind::Http => "http",
    };
    format!("{}://{}:{}", scheme, cfg.host, cfg.port)
}

pub async fn connect(cfg: &ProxyConfig, target_host: &str, target_port: u16) -> Result<TcpStream> {
    match cfg.kind {
        ProxyKind::Socks5 => connect_socks5(cfg, target_host, target_port).await,
        ProxyKind::Http => connect_http(cfg, target_host, target_port).await,
    }
}

async fn connect_socks5(cfg: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream> {
    use tokio_socks::tcp::Socks5Stream;

    let proxy = (cfg.host.as_str(), cfg.port);
    let target = (host, port);
    let stream = match &cfg.auth {
        Some((user, pass)) => Socks5Stream::connect_with_password(proxy, target, user, pass)
            .await
            .context("SOCKS5 proxy connect failed")?,
        None => Socks5Stream::connect(proxy, target)
            .await
            .context("SOCKS5 proxy connect failed")?,
    };
    Ok(stream.into_inner())
}

async fn connect_http(cfg: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream> {
    let mut stream = TcpStream::connect((cfg.host.as_str(), cfg.port))
        .await
        .with_context(|| format!("connect to HTTP proxy {}:{} failed", cfg.host, cfg.port))?;

    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some((user, pass)) = &cfg.auth {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("Proxy-Connection: keep-alive\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .context("write CONNECT to proxy")?;

    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = stream
            .read(&mut byte)
            .await
            .context("read proxy response")?;
        if n == 0 {
            bail!("proxy closed the connection during CONNECT");
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            bail!("proxy CONNECT response too large");
        }
    }

    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or("");
    let ok = status_line
        .split_whitespace()
        .nth(1)
        .map(|code| code == "200")
        .unwrap_or(false);
    if !ok {
        return Err(anyhow!("proxy CONNECT rejected: {}", status_line.trim()));
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn restore_env(name: &str, value: Option<String>) {
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    fn parses_default_socks5_and_describes_without_auth() {
        let cfg = parse("127.0.0.1:1080").unwrap();
        assert_eq!(describe(&cfg), "socks5://127.0.0.1:1080");
    }

    #[test]
    fn parses_http_auth_and_normalises_scheme() {
        let cfg = parse("https://user:pass@example.com:8080/").unwrap();
        assert_eq!(describe(&cfg), "http://example.com:8080");
        assert!(matches!(cfg.kind, ProxyKind::Http));
        assert_eq!(
            cfg.auth.as_ref().map(|(u, p)| (u.as_str(), p.as_str())),
            Some(("user", "pass"))
        );
    }

    #[test]
    fn resolve_prefers_session_and_does_not_fall_back_on_invalid_session_proxy() {
        let _guard = env_lock();
        let old_upper = std::env::var("ALL_PROXY").ok();
        std::env::set_var("ALL_PROXY", "http://env:8080");
        assert!(resolve("bad://session:123").is_none());
        restore_env("ALL_PROXY", old_upper);
    }

    #[test]
    fn resolve_uses_all_proxy_before_lowercase() {
        let _guard = env_lock();
        let old_upper = std::env::var("ALL_PROXY").ok();
        let old_lower = std::env::var("all_proxy").ok();
        std::env::set_var("ALL_PROXY", "http://upper:8080");
        std::env::set_var("all_proxy", "http://lower:8080");
        let cfg = resolve("").unwrap();
        assert_eq!(describe(&cfg), "http://upper:8080");
        restore_env("ALL_PROXY", old_upper);
        restore_env("all_proxy", old_lower);
    }

    #[tokio::test]
    async fn http_connect_sends_basic_auth_and_returns_tunnel() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                stream.read_exact(&mut byte).await.unwrap();
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let req = String::from_utf8(buf).unwrap();
            assert!(req.starts_with("CONNECT target.example:2222 HTTP/1.1\r\n"));
            assert!(req.contains("Host: target.example:2222\r\n"));
            assert!(req.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            stream.write_all(b"ok").await.unwrap();
        });

        let cfg = parse(&format!("http://user:pass@127.0.0.1:{port}")).unwrap();
        let mut stream = connect(&cfg, "target.example", 2222).await.unwrap();
        let mut out = [0u8; 2];
        stream.read_exact(&mut out).await.unwrap();
        assert_eq!(&out, b"ok");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_connect_rejects_non_200_status() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                .await
                .unwrap();
        });

        let cfg = parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let err = connect(&cfg, "target.example", 2222).await.unwrap_err();
        assert!(err.to_string().contains("proxy CONNECT rejected"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn http_connect_rejects_oversized_response_headers() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(&vec![b'a'; 8193]).await.unwrap();
        });

        let cfg = parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let err = connect(&cfg, "target.example", 2222)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("response too large"));
        server.await.unwrap();
    }
}
