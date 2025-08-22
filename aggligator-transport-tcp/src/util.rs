//! Utils for working with IP connections.

use network_interface::NetworkInterfaceConfig;
use std::{
    collections::HashSet,
    io::{Error, Result},
    net::{IpAddr, SocketAddr},
};
use tokio::net::{lookup_host, TcpSocket};

pub use network_interface::{Addr, Netmask, NetworkInterface, V4IfAddr, V6IfAddr};

use crate::IpVersion;

/// Gets the list of local network interfaces from the operating system.
///
/// Filters out interfaces that are most likely useless.
pub fn local_interfaces() -> Result<Vec<NetworkInterface>> {
    Ok(NetworkInterface::show()
        .map_err(|err| Error::other(err.to_string()))?
        .into_iter()
        .filter(|iface| !iface.name.starts_with("ifb"))
        .collect())
}

/// Translate IPv4 address mapped to an IPv6 alias into proper IPv4 address.
pub fn use_proper_ipv4(sa: &mut SocketAddr) {
    if let IpAddr::V6(addr) = sa.ip() {
        if let Some(addr) = addr.to_ipv4_mapped() {
            sa.set_ip(addr.into());
        }
    }
}

/// Gets the local interface for the specified IP address.
pub fn local_interface_for_ip(ip: IpAddr) -> Result<Option<Vec<u8>>> {
    let interfaces = local_interfaces()?;
    let iface = interfaces.into_iter().find_map(|interface| {
        interface.addr.iter().any(|addr| addr.ip() == ip).then_some(interface.name.into_bytes())
    });
    Ok(iface)
}

/// Resolves the specified hosts to IP addresses.
pub async fn resolve_hosts(
    hosts: impl IntoIterator<Item = impl AsRef<str>>, ip_version: IpVersion,
) -> Vec<SocketAddr> {
    let mut all_addrs = HashSet::new();

    for host in hosts {
        let Ok(addrs) = lookup_host(host.as_ref()).await else { continue };
        all_addrs.extend(addrs.filter(|addr| {
            !((addr.is_ipv4() && ip_version.is_only_ipv6()) || (addr.is_ipv6() && ip_version.is_only_ipv4()))
        }));
    }

    let mut all_addrs: Vec<_> = all_addrs.into_iter().collect();
    all_addrs.sort();
    all_addrs
}

/// Returns the interface usable for connecting to target.
///
/// Filters interfaces out that either have no IP address or only support
/// an IP protocol version that does not match the target address.
pub fn interface_names_for_target(interfaces: &[NetworkInterface], target: SocketAddr) -> HashSet<Vec<u8>> {
    interfaces
        .iter()
        .cloned()
        .filter_map(|iface| {
            iface
                .addr
                .iter()
                .any(|addr| {
                    !addr.ip().is_unspecified()
                        && addr.ip().is_loopback() == target.ip().is_loopback()
                        && addr.ip().is_ipv4() == target.is_ipv4()
                        && addr.ip().is_ipv6() == target.is_ipv6()
                })
                .then(|| iface.name.as_bytes().to_vec())
        })
        .collect()
}

/// Binds the socket the the specifed network interface.
pub fn bind_socket_to_interface(socket: &TcpSocket, interface: &[u8], remote: IpAddr) -> Result<()> {
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    {
        let _ = remote;
        socket.bind_device(Some(interface))
    }

    #[cfg(not(any(target_os = "android", target_os = "fuchsia", target_os = "linux")))]
    {
        for ifn in local_interfaces()? {
            if ifn.name.as_bytes() == interface {
                for addr in ifn.addr {
                    match (addr.ip(), remote) {
                        (IpAddr::V4(_), IpAddr::V4(_)) => (),
                        (IpAddr::V6(_), IpAddr::V6(_)) => (),
                        _ => continue,
                    }

                    if addr.ip().is_loopback() != remote.is_loopback() {
                        continue;
                    }

                    tracing::debug!("binding to {addr:?} on interface {}", &ifn.name);
                    socket.bind(SocketAddr::new(addr.ip(), 0))?;
                    return Ok(());
                }
            }
        }

        Err(Error::new(std::io::ErrorKind::NotFound, "no IP address for interface"))
    }
}
