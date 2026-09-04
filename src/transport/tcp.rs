//! TCP connect with Happy Eyeballs (RFC 8305): race IPv6/IPv4, 250ms stagger.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;

use crate::error::FetchError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STAGGER: Duration = Duration::from_millis(250);

pub async fn happy_connect(host: &str, port: u16) -> Result<TcpStream, FetchError> {
    happy_connect_with(host, port, false).await
}

/// `warm` = this origin has a cached TLS session this daemon lifetime
/// (repeat navigation in Chrome-speak). Warm origins get
/// TCP_FASTOPEN_CONNECT on Linux: Chrome TFOs repeat navigations;
/// the kernel still decides per peer cookie state. Non-fatal: every
/// failure degrades to a normal connect.
pub async fn happy_connect_with(
    host: &str,
    port: u16,
    warm: bool,
) -> Result<TcpStream, FetchError> {
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
    // dial : not the name : also closes rebinding TOCTOU.
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
                "dns: {host} resolves to a private/loopback address : SSRF guard"
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
    let mut primary_fut = Box::pin(connect_all(primary, warm));
    if secondary.is_empty() {
        return primary_fut.await;
    }
    let staggered = async move {
        tokio::time::sleep(STAGGER).await;
        connect_all(secondary, warm).await
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

async fn connect_all(addrs: Vec<SocketAddr>, warm: bool) -> Result<TcpStream, FetchError> {
    let mut last_err = FetchError::Http("no addresses".into());
    for addr in addrs {
        match tokio::time::timeout(CONNECT_TIMEOUT, connect_one(addr, warm)).await {
            Ok(Ok(s)) => {
                s.set_nodelay(true).ok();
                return Ok(s);
            }
            Ok(Err(e)) => last_err = e,
            Err(_) => last_err = FetchError::Timeout,
        }
    }
    Err(last_err)
}

async fn connect_one(addr: SocketAddr, warm: bool) -> Result<TcpStream, FetchError> {
    tcp_connect(addr, warm).await
}

/// Linux TFO-capable connect for warm hosts. Everything else runs
/// tokio's normal connect (Chrome does not TFO on Windows/macOS in
/// the same way; parity with the host platform matters more).
#[cfg(target_os = "linux")]
async fn tcp_connect(addr: SocketAddr, warm: bool) -> Result<TcpStream, FetchError> {
    use std::os::fd::FromRawFd;

    if !warm {
        return TcpStream::connect(addr).await.map_err(Into::into);
    }
    let domain = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return TcpStream::connect(addr).await.map_err(Into::into);
    }
    // Request fastopen: the kernel decides per peer TFO-cookie state.
    let one: libc::c_int = 1;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_FASTOPEN_CONNECT,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        unsafe { libc::close(fd) };
        return TcpStream::connect(addr).await.map_err(Into::into);
    }
    let (ptr, len) = sockaddr_of(addr);
    let rc = unsafe { libc::connect(fd, ptr, len) };
    if rc != 0 {
        unsafe { libc::close(fd) };
        return TcpStream::connect(addr).await.map_err(Into::into);
    }
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
    std_stream.set_nonblocking(true).ok();
    TcpStream::from_std(std_stream).map_err(Into::into)
}

#[cfg(not(target_os = "linux"))]
async fn tcp_connect(addr: SocketAddr, _warm: bool) -> Result<TcpStream, FetchError> {
    TcpStream::connect(addr).await.map_err(Into::into)
}

#[cfg(target_os = "linux")]
fn sockaddr_of(addr: SocketAddr) -> (*const libc::sockaddr, libc::socklen_t) {
    match addr {
        SocketAddr::V4(v4) => {
            let sockaddr = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()).to_be(),
                },
                sin_zero: [0; 8],
            };
            (
                &sockaddr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(v6) => {
            let sockaddr = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                },
                sin6_scope_id: v6.scope_id(),
            };
            (
                &sockaddr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}
