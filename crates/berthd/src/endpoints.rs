//! A TCP connection to an address on a network of the host's own, as a
//! stream: how `berth serve` reaches a resource of an engine on this host,
//! such as a container on a bridge that only this host routes to.
//!
//! It reaches further than `ports.connect`, so it is kept narrow: `berth
//! serve` calls it itself for an endpoint a provisioner reported and never
//! passes it through for a caller, and the address has to be on a network
//! that one of the host's interfaces is attached to.

use crate::error::{E, R};
use axum::{
    extract::{
        Path,
        ws::{WebSocketUpgrade, rejection::WebSocketUpgradeRejection},
    },
    response::Response,
};
use berth_api::code;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::net::TcpStream;

/// How long resolving and dialling may take, as for a port.
const DIAL_FOR: Duration = Duration::from_secs(10);

/// A network one of the host's interfaces is attached to: an address of the
/// interface and the length of its prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    pub address: IpAddr,
    pub prefix: u8,
}

impl Network {
    fn holds(&self, ip: IpAddr) -> bool {
        match (self.address, ip) {
            (IpAddr::V4(own), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(own) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(own), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(own) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// An IPv4 address written as IPv6 as its IPv4 self, so each check sees it once.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

/// The addresses of clouds' instance metadata services, which hand out the
/// host's credentials to whoever asks from it.
fn metadata(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4 == Ipv4Addr::new(169, 254, 169, 254),
        IpAddr::V6(v6) => v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254),
    }
}

fn link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_unicast_link_local(),
    }
}

fn special(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_unspecified() || v4.is_broadcast() || v4.is_multicast(),
        IpAddr::V6(v6) => v6.is_unspecified() || v6.is_multicast(),
    }
}

/// Whether `ip` may be dialled from this host, given the networks of its
/// interfaces, or why not.
pub fn allowed(ip: IpAddr, networks: &[Network]) -> Result<(), String> {
    let ip = canonical(ip);
    if metadata(ip) {
        return Err(format!(
            "{ip} is a cloud's metadata address, which berth never dials"
        ));
    }
    // The host's loopback is what ports.connect is for.
    if ip.is_loopback() {
        return Err(format!(
            "{ip} is the host's loopback: open the port with ports.connect"
        ));
    }
    if link_local(ip) || special(ip) {
        return Err(format!("{ip} is no address of a network berth dials"));
    }
    match networks.iter().any(|network| network.holds(ip)) {
        true => Ok(()),
        // Anything further is reached through a router, which is not the host's to offer.
        false => Err(format!(
            "{ip} is on no network of the host's own interfaces"
        )),
    }
}

/// The networks the host's interfaces that are up are attached to, without
/// the loopback and link-local ones.
pub fn networks() -> Vec<Network> {
    let mut found = vec![];
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, which is freed below and read only before that.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return found;
    }
    let mut at = list;
    while !at.is_null() {
        // SAFETY: `at` is an entry of the list getifaddrs made.
        let entry = unsafe { &*at };
        at = entry.ifa_next;
        let flags = entry.ifa_flags;
        if flags & libc::IFF_UP as u32 == 0 || flags & libc::IFF_LOOPBACK as u32 != 0 {
            continue;
        }
        // SAFETY: the address and the mask are of the family the address says, or null.
        let (address, mask) = unsafe { (ip_of(entry.ifa_addr), ip_of(entry.ifa_netmask)) };
        let (Some(address), Some(mask)) = (address, mask) else {
            continue;
        };
        let prefix = match mask {
            IpAddr::V4(mask) => u32::from(mask).leading_ones() as u8,
            IpAddr::V6(mask) => u128::from(mask).leading_ones() as u8,
        };
        // A prefix this short is a route to much of the internet, not a network the host is on.
        let wide = match address {
            IpAddr::V4(_) => prefix < 8,
            IpAddr::V6(_) => prefix < 16,
        };
        if wide || link_local(address) || address.is_loopback() {
            continue;
        }
        found.push(Network { address, prefix });
    }
    // SAFETY: the list came from getifaddrs and is not read after this.
    unsafe { libc::freeifaddrs(list) };
    found
}

/// The address a `sockaddr` holds, when it is IPv4 or IPv6.
///
/// # Safety
/// `address` is null or points to a `sockaddr` of the family it names.
unsafe fn ip_of(address: *const libc::sockaddr) -> Option<IpAddr> {
    if address.is_null() {
        return None;
    }
    // SAFETY: as the caller promises, the family says which struct it is.
    unsafe {
        match i32::from((*address).sa_family) {
            libc::AF_INET => {
                let v4 = &*(address as *const libc::sockaddr_in);
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))))
            }
            libc::AF_INET6 => {
                let v6 = &*(address as *const libc::sockaddr_in6);
                Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }
}

/// The addresses `address` names now, each one checked.
async fn resolve(address: &str, port: u16) -> R<Vec<SocketAddr>> {
    let found: Vec<SocketAddr> = match address.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => {
            match tokio::time::timeout(DIAL_FOR, tokio::net::lookup_host((address, port))).await {
                Ok(Ok(found)) => found.collect(),
                Ok(Err(error)) => {
                    return Err(E(
                        code::UNREACHABLE,
                        format!("{address} does not resolve: {error}"),
                    ));
                }
                Err(_) => {
                    return Err(E(
                        code::TIMEOUT,
                        format!("{address} did not resolve in time"),
                    ));
                }
            }
        }
    };
    if found.is_empty() {
        return Err(E(code::UNREACHABLE, format!("{address} has no address")));
    }
    let networks = networks();
    // One refused address refuses the name, as which of them is dialled is not the caller's to choose.
    for each in &found {
        allowed(each.ip(), &networks).map_err(|why| match address.parse::<IpAddr>() {
            Ok(_) => E::forbidden(why),
            Err(_) => E::forbidden(format!("{address} resolves to {}: {why}", each.ip())),
        })?;
    }
    Ok(found)
}

/// The address is resolved and checked once, and what was checked is what
/// is dialled; it is dialled before the stream opens, so one that nothing
/// answers on is an answer of its own.
pub async fn connect(
    Path((address, port)): Path<(String, String)>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> R<Response> {
    let upgrade = upgrade.map_err(|_| E::bad("An endpoint is opened only as a WebSocket"))?;
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| E::bad(format!("{port} is no port: a number from 1 to 65535")))?;
    let addresses = resolve(&address, port).await?;
    let mut last = String::new();
    for each in &addresses {
        match tokio::time::timeout(DIAL_FOR, TcpStream::connect(each)).await {
            Ok(Ok(tcp)) => {
                let _ = tcp.set_nodelay(true);
                return Ok(upgrade.on_upgrade(move |socket| crate::ports::tunnel(socket, tcp)));
            }
            Ok(Err(error)) => last = format!("{each}: {error}"),
            Err(_) => {
                return Err(E(
                    code::TIMEOUT,
                    format!("{each} does not answer from the host"),
                ));
            }
        }
    }
    Err(E(
        code::NOT_SERVED,
        format!("Nothing answers on {address} port {port} from the host: {last}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn only_addresses_on_the_hosts_own_networks_are_dialled() {
        let networks = [
            Network {
                address: ip("10.12.0.1"),
                prefix: 24,
            },
            Network {
                address: ip("192.0.2.10"),
                prefix: 24,
            },
            Network {
                address: ip("fd42:1::1"),
                prefix: 64,
            },
        ];
        for on in [
            "10.12.0.57",
            "192.0.2.10",
            "192.0.2.254",
            "fd42:1::99",
            "::ffff:10.12.0.2",
        ] {
            assert!(allowed(ip(on), &networks).is_ok(), "{on}");
        }
        for (off, why) in [
            ("10.12.1.5", "no network"),
            ("198.51.100.7", "no network"),
            ("fd42:2::1", "no network"),
            ("127.0.0.1", "ports.connect"),
            ("::1", "ports.connect"),
            ("169.254.169.254", "metadata"),
            ("fd00:ec2::254", "metadata"),
            ("169.254.3.4", "no address"),
            ("fe80::1", "no address"),
            ("0.0.0.0", "no address"),
            ("224.0.0.1", "no address"),
        ] {
            let refused = allowed(ip(off), &networks).unwrap_err();
            assert!(refused.contains(why), "{off}: {refused}");
        }
        // A metadata address is refused even on a network the host is on.
        let cloud = [Network {
            address: ip("169.254.0.1"),
            prefix: 16,
        }];
        assert!(allowed(ip("169.254.169.254"), &cloud).is_err());
    }

    #[test]
    fn the_hosts_networks_leave_out_loopback() {
        for network in networks() {
            assert!(!network.address.is_loopback(), "{network:?}");
            assert!(!link_local(network.address), "{network:?}");
        }
    }
}
