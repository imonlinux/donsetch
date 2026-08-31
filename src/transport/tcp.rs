//! TCP connect with Happy Eyeballs (RFC 8305): race IPv6/IPv4, 250ms stagger.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;

use crate::error::FetchError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STAGGER: Duration = Duration::from_millis(250);

pub async fn happy_connect(host: &str, port: u16) -> Result<TcpStream, FetchError> {
    let addrs: Vec<SocketAddr> =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| FetchError::Timeout)??
            .collect();
    if addrs.is_empty() {
        return Err(FetchError::Http(format!("dns: no address for {host}")));
    }

    // DNS pinning (SSRF): a hostname that resolves to a
    // private/loopback address is treated exactly like a
    // literal one. Checking the addresses we are about to
    // dial — not the name — also closes rebinding TOCTOU.
    // Escape hatch for deliberate local-egress use (CLI
    // power users, tests): DONSETCH_ALLOW_PRIVATE_EGRESS=1.
    let addrs: Vec<SocketAddr> = if std::env::var_os("DONSETCH_ALLOW_PRIVATE_EGRESS").is_some() {
        addrs
    } else {
        let blocked: Vec<&SocketAddr> = addrs
            .iter()
            .filter(|a| crate::fetch::guards::is_ssrf_resolved_ip(&a.ip()))
            .collect();
        if blocked.len() == addrs.len() {
            return Err(FetchError::Http(format!(
                "dns: {host} resolves to a private/loopback address — SSRF guard"
            )));
        }
        addrs
            .into_iter()
            .filter(|a| !crate::fetch::guards::is_ssrf_resolved_ip(&a.ip()))
            .collect()
    };

    let mut v6: Vec<SocketAddr> = addrs.iter().filter(|a| a.is_ipv6()).copied().collect();
    let mut v4: Vec<SocketAddr> = addrs.iter().filter(|a| a.is_ipv4()).copied().collect();
    // Chrome prefers IPv6.
    let (primary, secondary) = if v6.is_empty() {
        (std::mem::take(&mut v4), v6)
    } else {
        (std::mem::take(&mut v6), v4)
    };

    // Race: start primary family; after 250ms start secondary in parallel.
    let mut primary_fut = Box::pin(connect_all(primary));
    if secondary.is_empty() {
        return primary_fut.await;
    }
    let staggered = async move {
        tokio::time::sleep(STAGGER).await;
        connect_all(secondary).await
    };
    let mut secondary_fut = Box::pin(staggered);

    tokio::select! {
        res = &mut primary_fut => match res {
            Ok(s) => Ok(s),
            Err(e) => secondary_fut.await.map_err(|_| e),
        },
        res = &mut secondary_fut => match res {
            Ok(s) => Ok(s),
            Err(_) => primary_fut.await,
        },
    }
}

async fn connect_all(addrs: Vec<SocketAddr>) -> Result<TcpStream, FetchError> {
    let mut last_err = FetchError::Http("no addresses".into());
    for addr in addrs {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                s.set_nodelay(true).ok();
                return Ok(s);
            }
            Ok(Err(e)) => last_err = e.into(),
            Err(_) => last_err = FetchError::Timeout,
        }
    }
    Err(last_err)
}
