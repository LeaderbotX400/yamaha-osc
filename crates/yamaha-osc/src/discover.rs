//! Finding a console on the network by scanning for open RCP ports.
//!
//! Yamaha's own editors discover consoles with an undocumented broadcast protocol. Rather than
//! reverse-engineer it, this does something duller and more reliable: connect to port 49280 across
//! the configured subnet and ask each responder what it is. A /24 finishes well inside a second,
//! and it depends on nothing that a firmware update can change.
//!
//! A static address remains the recommended setup — pair it with a DHCP reservation. Discovery is
//! for the first time you meet a console, and for the day someone reset the network.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tracing::debug;

use yamaha_rcp::DEFAULT_PORT;

/// Sockets opened at once. High enough to finish a /24 quickly, low enough to stay well under the
/// default file-descriptor limit.
const CONCURRENCY: usize = 128;

/// The largest subnet worth scanning. A /20 is 4094 hosts; anything larger is a mistake in the
/// config rather than a network someone means to sweep.
const MIN_PREFIX: u8 = 20;

/// A console that answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub addr: SocketAddr,
    /// The console's model, e.g. `TF1`.
    pub product: String,
    /// The unit name set in the console's network settings.
    pub name: String,
}

impl std::fmt::Display for Found {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} — {} ({})", self.addr, self.product, self.name)
    }
}

/// Scan a CIDR range for consoles.
pub async fn scan(cidr: &str, timeout: Duration) -> Result<Vec<Found>> {
    let hosts = hosts_in(cidr)?;
    debug!(cidr, hosts = hosts.len(), "scanning for consoles");

    let mut found = Vec::new();
    let mut in_flight = JoinSet::new();
    let mut queue = hosts.into_iter();

    loop {
        while in_flight.len() < CONCURRENCY {
            let Some(ip) = queue.next() else { break };
            in_flight.spawn(probe(
                SocketAddr::new(IpAddr::V4(ip), DEFAULT_PORT),
                timeout,
            ));
        }
        let Some(result) = in_flight.join_next().await else {
            break;
        };
        if let Ok(Some(console)) = result {
            debug!(%console, "found a console");
            found.push(console);
        }
    }

    found.sort_by_key(|f| f.addr);
    Ok(found)
}

/// Ask one address whether it is a console.
///
/// A connection alone is not proof — plenty of things could be listening — so this asks for the
/// product name and requires a well-formed RCP reply.
pub async fn probe(addr: SocketAddr, timeout: Duration) -> Option<Found> {
    let result = tokio::time::timeout(timeout, async {
        let mut socket = TcpStream::connect(addr).await.ok()?;
        socket.set_nodelay(true).ok()?;
        socket.write_all(b"devinfo productname\n").await.ok()?;

        let product = read_quoted_reply(&mut socket, "productname").await?;

        socket.write_all(b"devinfo devicename\n").await.ok()?;
        let name = read_quoted_reply(&mut socket, "devicename")
            .await
            .unwrap_or_default();

        Some(Found {
            addr,
            product,
            name,
        })
    })
    .await;

    result.ok().flatten()
}

/// Read lines until one is an `OK devinfo <field> "value"`, and return the value.
async fn read_quoted_reply(socket: &mut TcpStream, field: &str) -> Option<String> {
    let mut buf = vec![0u8; 1024];
    let mut text = String::new();

    for _ in 0..8 {
        let n = socket.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        text.push_str(&String::from_utf8_lossy(&buf[..n]));

        for line in text.lines() {
            let expected = format!("OK devinfo {field} ");
            if let Some(rest) = line.strip_prefix(&expected) {
                return Some(rest.trim().trim_matches('"').to_owned());
            }
            // The console pushes notifications unprompted; those are not our answer.
            if line.starts_with("ERROR") {
                return None;
            }
        }
    }
    None
}

/// Expand a CIDR into the host addresses inside it, excluding the network and broadcast addresses.
pub fn hosts_in(cidr: &str) -> Result<Vec<Ipv4Addr>> {
    let (base, prefix) = cidr
        .split_once('/')
        .with_context(|| format!("{cidr:?} is not a CIDR range like 192.168.50.0/24"))?;

    let base: Ipv4Addr = base
        .parse()
        .with_context(|| format!("{base:?} is not an IPv4 address"))?;
    let prefix: u8 = prefix
        .parse()
        .with_context(|| format!("{prefix:?} is not a prefix length"))?;

    if prefix > 32 {
        bail!("{cidr:?}: prefix length must be 32 or less");
    }
    if prefix < MIN_PREFIX {
        bail!(
            "{cidr:?} covers too many addresses to scan; use /{MIN_PREFIX} or smaller \
             (or set console.address directly)"
        );
    }

    let base = u32::from(base);
    let host_bits = 32 - u32::from(prefix);
    let mask = if host_bits == 32 {
        0
    } else {
        u32::MAX << host_bits
    };
    let network = base & mask;
    let count = 1u64 << host_bits;

    // A /31 or /32 has no network or broadcast address to skip.
    let (first, last) = if count <= 2 {
        (0, count)
    } else {
        (1, count - 1)
    };

    Ok((first..last)
        .map(|offset| Ipv4Addr::from(network + offset as u32))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcp_mock::MockConsole;

    #[test]
    fn a_slash_24_expands_to_its_usable_hosts() {
        let hosts = hosts_in("192.168.50.0/24").unwrap();
        assert_eq!(hosts.len(), 254, "network and broadcast are excluded");
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 50, 1));
        assert_eq!(hosts[253], Ipv4Addr::new(192, 168, 50, 254));
    }

    /// An address inside the range should give the same host list as the network address, so
    /// "192.168.50.20/24" works as well as "192.168.50.0/24".
    #[test]
    fn a_host_address_is_normalised_to_its_network() {
        assert_eq!(
            hosts_in("192.168.50.20/24").unwrap(),
            hosts_in("192.168.50.0/24").unwrap()
        );
    }

    #[test]
    fn a_single_address_scans_just_itself() {
        assert_eq!(
            hosts_in("192.168.50.20/32").unwrap(),
            [Ipv4Addr::new(192, 168, 50, 20)]
        );
    }

    #[test]
    fn a_slash_25_is_half_a_slash_24() {
        assert_eq!(hosts_in("192.168.50.0/25").unwrap().len(), 126);
    }

    #[test]
    fn an_absurdly_large_range_is_refused_rather_than_attempted() {
        let err = hosts_in("10.0.0.0/8").unwrap_err();
        assert!(err.to_string().contains("too many"), "{err}");
    }

    #[test]
    fn malformed_ranges_are_reported_clearly() {
        assert!(hosts_in("192.168.50.0").is_err());
        assert!(hosts_in("not-an-ip/24").is_err());
        assert!(hosts_in("192.168.50.0/nope").is_err());
        assert!(hosts_in("192.168.50.0/33").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_probe_identifies_a_console() {
        let console = MockConsole::start().await;
        let found = probe(console.addr(), Duration::from_secs(2))
            .await
            .expect("the mock console should identify itself");

        assert_eq!(found.addr, console.addr());
        assert_eq!(found.product, "TF1");
        assert_eq!(found.name, "MockTF");
    }

    /// Something listening on the port is not necessarily a console; the probe must not claim it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_probe_rejects_something_that_is_not_a_console() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let _ = socket.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            }
        });

        assert_eq!(probe(addr, Duration::from_millis(500)).await, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_probe_of_a_closed_port_gives_up_quickly() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let started = std::time::Instant::now();
        assert_eq!(probe(addr, Duration::from_millis(300)).await, None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
