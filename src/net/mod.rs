pub mod http;
pub mod tls;

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::task::JoinSet;

/// How long one attempt gets a head start before the next address is tried
/// in parallel (RFC 8305 "Connection Attempt Delay").
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// Resolve `host:port` and connect, optionally restricted to one IP family.
///
/// Addresses are raced Happy Eyeballs style: families are interleaved and a
/// new attempt starts every [`ATTEMPT_DELAY`] (or as soon as one fails), so a
/// host whose IPv6 route silently drops packets still connects over IPv4
/// instead of spending the whole timeout on the first AAAA record.
pub async fn connect(host: &str, port: u16, family: Option<Family>, timeout: Duration) -> io::Result<TcpStream> {
    let fut = async {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await?
            .filter(|a| match family {
                Some(Family::V4) => a.is_ipv4(),
                Some(Family::V6) => a.is_ipv6(),
                None => true,
            })
            .collect();
        race(interleave(addrs), attempt).await
    };
    tokio::time::timeout(timeout, fut).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timeout"))?
}

/// Order addresses by alternating family, starting with whichever family the
/// resolver listed first, keeping the resolver's order within each family.
fn interleave(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let Some(first_v6) = addrs.first().map(SocketAddr::is_ipv6) else { return addrs };
    let mut out = Vec::with_capacity(addrs.len());
    let (lead, other): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|a| a.is_ipv6() == first_v6);
    let (mut lead, mut other) = (lead.into_iter(), other.into_iter());
    loop {
        match (lead.next(), other.next()) {
            (None, None) => return out,
            (a, b) => out.extend(a.into_iter().chain(b)),
        }
    }
}

/// Start attempts in order, staggered by [`ATTEMPT_DELAY`]; the first stream
/// to connect wins and dropping the set aborts the rest.
async fn race<T, F, Fut>(addrs: Vec<SocketAddr>, attempt: F) -> io::Result<T>
where
    T: Send + 'static,
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let mut pending = addrs.into_iter();
    let mut running = JoinSet::new();
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no address");
    loop {
        if running.is_empty() {
            let Some(addr) = pending.next() else { return Err(last) };
            running.spawn(attempt(addr));
        }
        tokio::select! {
            Some(done) = running.join_next() => {
                match done {
                    Ok(Ok(s)) => return Ok(s),
                    Ok(Err(e)) => last = e,
                    Err(e) => last = io::Error::other(e),
                }
                // A fast failure (refused, unreachable) hands over at once.
                if let Some(addr) = pending.next() {
                    running.spawn(attempt(addr));
                }
            }
            _ = tokio::time::sleep(ATTEMPT_DELAY), if pending.len() > 0 => {
                if let Some(addr) = pending.next() {
                    running.spawn(attempt(addr));
                }
            }
        }
    }
}

async fn attempt(addr: SocketAddr) -> io::Result<TcpStream> {
    let s = TcpStream::connect(addr).await.map_err(|e| io::Error::new(e.kind(), format!("{addr}: {e}")))?;
    let _ = s.set_nodelay(true);
    tracing::debug!(%addr, "tcp connected");
    Ok(s)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn interleave_alternates_families() {
        let got = interleave(vec![a("[::1]:1"), a("[::2]:1"), a("1.1.1.1:1"), a("2.2.2.2:1"), a("3.3.3.3:1")]);
        assert_eq!(got, vec![a("[::1]:1"), a("1.1.1.1:1"), a("[::2]:1"), a("2.2.2.2:1"), a("3.3.3.3:1")]);
        let got = interleave(vec![a("1.1.1.1:1"), a("[::1]:1")]);
        assert_eq!(got, vec![a("1.1.1.1:1"), a("[::1]:1")]);
        assert!(interleave(vec![]).is_empty());
    }

    /// Stand-in for a TCP connect: `[::1]` hangs like a blackholed IPv6 route,
    /// `.0` refuses at once, anything else connects after 20ms.
    async fn fake(addr: SocketAddr) -> io::Result<SocketAddr> {
        if addr.is_ipv6() {
            std::future::pending::<()>().await;
        }
        if addr.ip().to_string().ends_with(".0") {
            return Err(io::ErrorKind::ConnectionRefused.into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(addr)
    }

    /// A blackholed first address must not stop the next one from winning.
    #[tokio::test]
    async fn falls_back_past_unresponsive_address() {
        let addrs = interleave(vec![a("[::1]:443"), a("[::2]:443"), a("10.0.0.0:443"), a("10.0.0.1:443")]);
        let won = tokio::time::timeout(Duration::from_secs(2), race(addrs, fake)).await.unwrap().unwrap();
        assert_eq!(won, a("10.0.0.1:443"));
    }

    #[tokio::test]
    async fn reports_last_error_when_all_fail() {
        let err = race(vec![a("10.0.0.0:443")], fake).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        let err = race(Vec::new(), fake).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
