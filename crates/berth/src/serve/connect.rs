//! A byte tunnel to an endpoint of a resource, as `resources.connect` opens
//! it. Only an endpoint the resource's provisioner reported can be opened,
//! and its address is resolved and checked when it is dialled, so the
//! tunnel is no way to an address of the caller's choosing.

use super::api::{Answer, App, Fail, Ws};
use axum::{extract::Path, response::Response};
use berth_api::{Endpoint, code};
use berth_core::resource::{self, Kept};
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite};

/// How long resolving and dialling an endpoint may take.
const DIAL_FOR: Duration = Duration::from_secs(10);

/// A connection to an endpoint, whichever way it was dialled.
pub trait Connection: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Connection for T {}

/// Who dials the endpoints of a provisioner's resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialler {
    /// The server, from its own machine: right for an engine there or on a
    /// network the server routes to.
    Server,
}

impl Dialler {
    /// The dialler for the provisioner that made a resource.
    pub fn of(_kept: &Kept) -> Dialler {
        Dialler::Server
    }

    /// Resolve, check and dial an endpoint.
    pub async fn dial(&self, endpoint: &Endpoint) -> Answer<Box<dyn Connection>> {
        match self {
            Dialler::Server => {
                let addresses = resolve(endpoint).await?;
                Ok(Box::new(dial_first(endpoint, &addresses).await?))
            }
        }
    }
}

/// The endpoint a caller names: by its name, or else the first with that port.
pub fn chosen<'a>(endpoints: &'a [Endpoint], given: &str) -> Option<&'a Endpoint> {
    endpoints
        .iter()
        .find(|endpoint| endpoint.name == given)
        .or_else(|| {
            let port = given.parse::<u16>().ok()?;
            endpoints.iter().find(|endpoint| endpoint.port == port)
        })
}

/// An address as its IPv4 self when it is an IPv4 address written as IPv6,
/// so that each check sees it once.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

/// The addresses of clouds' instance metadata services, which hand out
/// credentials to whoever asks from the machine.
fn metadata(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(v4) => v4 == Ipv4Addr::new(169, 254, 169, 254),
        IpAddr::V6(v6) => v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254),
    }
}

/// An address that names the dialling machine itself or its own link, not
/// one a provisioner's engine elsewhere hands out.
fn own_machine(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unicast_link_local()
                || v6.is_unspecified()
                || v6.is_multicast()
        }
    }
}

/// Whether the server may dial `resolved` for `endpoint`, or why not.
pub fn allowed(endpoint: &Endpoint, resolved: IpAddr) -> Result<(), String> {
    let resolved = canonical(resolved);
    let place = format!("{}:{}", endpoint.address, endpoint.port);
    // Credentials of the server's own machine are never a resource's to hand out.
    if metadata(resolved) {
        return Err(format!(
            "{place} is a cloud's metadata address, which berth never dials"
        ));
    }
    if !own_machine(resolved) {
        return Ok(());
    }
    let reported = endpoint.address.parse::<IpAddr>().ok().map(canonical);
    let named_so = reported == Some(resolved)
        || (endpoint.address.eq_ignore_ascii_case("localhost") && resolved.is_loopback());
    // Only an engine on this machine can mean this machine by such an address.
    if endpoint.local && named_so {
        return Ok(());
    }
    match (endpoint.local, reported.is_some_and(own_machine)) {
        // An address of the engine's own machine, which is not the server's.
        (false, true) => Err(format!(
            "{place} is an address of the machine the provisioner's engine runs on, which this server cannot reach"
        )),
        // A name that resolves to the server itself now was not reported as such.
        _ => Err(format!(
            "{} resolves to {resolved}, an address of the server's own machine, which the provisioner did not report",
            endpoint.address
        )),
    }
}

/// The addresses an endpoint has now, each one checked.
async fn resolve(endpoint: &Endpoint) -> Answer<Vec<SocketAddr>> {
    let addresses: Vec<SocketAddr> = match endpoint.address.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, endpoint.port)],
        Err(_) => {
            let looked = tokio::time::timeout(
                DIAL_FOR,
                tokio::net::lookup_host((endpoint.address.as_str(), endpoint.port)),
            )
            .await;
            match looked {
                Ok(Ok(found)) => found.collect(),
                Ok(Err(error)) => {
                    return Err(Fail::new(
                        code::UNREACHABLE,
                        format!("{} does not resolve: {error}", endpoint.address),
                    ));
                }
                Err(_) => {
                    return Err(Fail::new(
                        code::TIMEOUT,
                        format!("{} did not resolve in time", endpoint.address),
                    ));
                }
            }
        }
    };
    if addresses.is_empty() {
        return Err(Fail::new(
            code::UNREACHABLE,
            format!("{} has no address", endpoint.address),
        ));
    }
    // One refused address refuses the name, as which of them is dialled is not the caller's to choose.
    for address in &addresses {
        allowed(endpoint, address.ip()).map_err(|why| Fail::new(code::FORBIDDEN, why))?;
    }
    Ok(addresses)
}

/// The first of the checked addresses that answers.
async fn dial_first(
    endpoint: &Endpoint,
    addresses: &[SocketAddr],
) -> Answer<tokio::net::TcpStream> {
    let mut last = String::new();
    for address in addresses {
        match tokio::time::timeout(DIAL_FOR, tokio::net::TcpStream::connect(address)).await {
            Ok(Ok(tcp)) => {
                let _ = tcp.set_nodelay(true);
                return Ok(tcp);
            }
            Ok(Err(error)) => last = format!("{address}: {error}"),
            Err(_) => {
                return Err(Fail::new(
                    code::TIMEOUT,
                    format!(
                        "The endpoint {} at {address} does not answer",
                        endpoint.name
                    ),
                ));
            }
        }
    }
    Err(Fail::new(
        code::NOT_SERVED,
        format!("Nothing answers on the endpoint {}, {last}", endpoint.name),
    ))
}

/// The endpoint is dialled before the stream opens, so one that nothing
/// answers on is an answer of its own and not a stream that ends at once.
pub async fn connect(
    axum::extract::State(state): App,
    Path((id, given)): Path<(String, String)>,
    ws: Ws,
) -> Answer<Response> {
    let ws = ws.map_err(|_| {
        Fail::new(
            code::BAD_REQUEST,
            "An endpoint is opened only as a WebSocket",
        )
    })?;
    // Read now, so the endpoints are those the provisioner reported last.
    let kept = resource::find(&state.paths, &id)
        .map_err(|error| Fail::new(code::NOT_FOUND, error.to_string()))?;
    let endpoints = &kept.record.endpoints;
    let endpoint = chosen(endpoints, &given).cloned().ok_or_else(|| {
        let names: Vec<&str> = endpoints.iter().map(|known| known.name.as_str()).collect();
        Fail::new(
            code::NOT_FOUND,
            match names.is_empty() {
                true => format!("{} reports no endpoints", kept.title()),
                false => format!(
                    "{} has no endpoint {given}. Its endpoints: {}",
                    kept.title(),
                    names.join(", ")
                ),
            },
        )
    })?;
    if endpoint.protocol != Endpoint::TCP {
        return Err(Fail::new(
            code::BAD_REQUEST,
            format!(
                "The endpoint {} is {}, and a tunnel carries TCP alone",
                endpoint.name, endpoint.protocol
            ),
        ));
    }
    let connection = Dialler::of(&kept).dial(&endpoint).await?;
    Ok(ws.on_upgrade(move |socket| berthd::ports::tunnel(socket, connection)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(address: &str, local: bool) -> Endpoint {
        Endpoint {
            name: "db".into(),
            address: address.into(),
            port: 5432,
            protocol: Endpoint::TCP.into(),
            local,
            ..Endpoint::default()
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn the_server_dials_what_was_reported_and_not_its_own_machine_by_another_name() {
        // A container's address, or a name that resolves to one like it.
        assert!(allowed(&endpoint("192.0.2.20", false), ip("192.0.2.20")).is_ok());
        assert!(allowed(&endpoint("db.example", false), ip("192.0.2.20")).is_ok());
        // Loopback reported by an engine on this machine is this machine's.
        assert!(allowed(&endpoint("127.0.0.1", true), ip("127.0.0.1")).is_ok());
        assert!(allowed(&endpoint("localhost", true), ip("::1")).is_ok());
        // Reported by an engine elsewhere, it is that machine's, which the server cannot reach.
        let elsewhere = allowed(&endpoint("127.0.0.1", false), ip("127.0.0.1")).unwrap_err();
        assert!(elsewhere.contains("cannot reach"), "{elsewhere}");
        // A name that resolves to the server's own machine now is refused.
        for (address, local) in [("db.example", false), ("db.example", true)] {
            let refused = allowed(&endpoint(address, local), ip("127.0.0.1")).unwrap_err();
            assert!(refused.contains("did not report"), "{refused}");
        }
        assert!(allowed(&endpoint("db.example", true), ip("fe80::1")).is_err());
        assert!(allowed(&endpoint("db.example", false), ip("::ffff:127.0.0.1")).is_err());
        // Metadata services are refused however they are reported.
        for address in ["169.254.169.254", "fd00:ec2::254"] {
            let refused = allowed(&endpoint(address, true), ip(address)).unwrap_err();
            assert!(refused.contains("metadata"), "{refused}");
        }
        assert!(allowed(&endpoint("169.254.10.1", false), ip("169.254.10.1")).is_err());
    }

    #[test]
    fn an_endpoint_is_named_or_given_by_its_port() {
        let endpoints = [
            Endpoint {
                name: "web".into(),
                port: 8080,
                ..endpoint("192.0.2.20", false)
            },
            endpoint("192.0.2.20", false),
        ];
        assert_eq!(chosen(&endpoints, "db").unwrap().port, 5432);
        assert_eq!(chosen(&endpoints, "8080").unwrap().name, "web");
        assert!(chosen(&endpoints, "22").is_none());
    }
}
