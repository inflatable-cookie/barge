//! Network interface lookup for `GET /v1/state` (MAC and broadcast) and
//! Avahi interface index selection.

use std::ffi::CStr;
use std::net::{IpAddr, Ipv4Addr};
use std::ptr;

/// Hardware MAC, IPv4 broadcast address and OS interface index for a bound IP.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InterfaceInfo {
    pub mac: Option<String>,
    pub broadcast: Option<Ipv4Addr>,
    pub interface_index: Option<i32>,
    /// The OS error from reading interface metadata, when the lookup failed.
    pub lookup_error: Option<String>,
}

/// Resolves interface metadata for the address `barged` binds.
pub trait NetworkLookup: Send + Sync + 'static {
    fn interface_for(&self, ip: IpAddr) -> InterfaceInfo;
}

/// Real POSIX `getifaddrs` lookup.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemNetworkLookup;

impl NetworkLookup for SystemNetworkLookup {
    fn interface_for(&self, ip: IpAddr) -> InterfaceInfo {
        if ip.is_unspecified() || ip.is_loopback() {
            return InterfaceInfo::default();
        }
        lookup_via_getifaddrs(ip)
    }
}

fn lookup_via_getifaddrs(target: IpAddr) -> InterfaceInfo {
    let mut head: *mut libc::ifaddrs = ptr::null_mut();
    // SAFETY: `getifaddrs` initializes `head` on success (return 0) and is freed
    // via `freeifaddrs` before returning.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return InterfaceInfo {
            lookup_error: Some(std::io::Error::last_os_error().to_string()),
            ..InterfaceInfo::default()
        };
    }
    if head.is_null() {
        return InterfaceInfo::default();
    }

    let mut matched_ifname: Option<String> = None;
    let mut broadcast: Option<Ipv4Addr> = None;
    let mut mac: Option<String> = None;
    let mut interface_index: Option<i32> = None;

    // Pass 1: find the interface name whose configured address equals `target`.
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: `cursor` is a valid node in the `getifaddrs` linked list.
        let ifa = unsafe { &*cursor };
        if !ifa.ifa_addr.is_null() && !ifa.ifa_name.is_null() {
            // SAFETY: `ifa_addr` points to a valid `sockaddr`.
            let family = unsafe { (*ifa.ifa_addr).sa_family } as i32;
            match (target, family) {
                (IpAddr::V4(v4), libc::AF_INET) => {
                    // SAFETY: `ifa_addr` is `AF_INET`, so `sockaddr_in` layout applies.
                    let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                    let addr = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    if addr == v4 {
                        // SAFETY: `ifa_name` is a NUL-terminated C string from `getifaddrs`.
                        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }
                            .to_string_lossy()
                            .into_owned();
                        // SAFETY: `ifa_name` is a NUL-terminated C string.
                        let idx = unsafe { libc::if_nametoindex(ifa.ifa_name) };
                        interface_index = i32::try_from(idx).ok().filter(|v| *v > 0);
                        broadcast = extract_broadcast(ifa, addr);
                        matched_ifname = Some(name);
                        break;
                    }
                }
                (IpAddr::V6(v6), libc::AF_INET6) => {
                    // SAFETY: `ifa_addr` is `AF_INET6`, so `sockaddr_in6` layout applies.
                    let sin6 = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in6) };
                    if std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr) == v6 {
                        // SAFETY: `ifa_name` is a NUL-terminated C string from `getifaddrs`.
                        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }
                            .to_string_lossy()
                            .into_owned();
                        // SAFETY: `ifa_name` is a NUL-terminated C string.
                        let idx = unsafe { libc::if_nametoindex(ifa.ifa_name) };
                        interface_index = i32::try_from(idx).ok().filter(|v| *v > 0);
                        matched_ifname = Some(name);
                        break;
                    }
                }
                _ => {}
            }
        }
        cursor = ifa.ifa_next;
    }

    // Pass 2: read the link-layer MAC for that interface name.
    if let Some(target_name) = &matched_ifname {
        let mut cursor = head;
        while !cursor.is_null() {
            // SAFETY: `cursor` is a valid node in the `getifaddrs` linked list.
            let ifa = unsafe { &*cursor };
            if !ifa.ifa_addr.is_null() && !ifa.ifa_name.is_null() {
                // SAFETY: `ifa_name` is a NUL-terminated C string.
                let name = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_string_lossy();
                if name == target_name.as_str() {
                    if let Some(found_mac) = extract_mac(ifa.ifa_addr) {
                        mac = Some(found_mac);
                        break;
                    }
                }
            }
            cursor = ifa.ifa_next;
        }
    }

    // SAFETY: `head` came from `getifaddrs` above and is freed once here.
    unsafe { libc::freeifaddrs(head) };

    InterfaceInfo {
        mac,
        broadcast,
        interface_index,
        lookup_error: None,
    }
}

/// The broadcast address field: `ifa_ifu` on Linux (a union with the
/// destination address), `ifa_dstaddr` on the BSDs and macOS.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn broadcast_sockaddr(ifa: &libc::ifaddrs) -> *mut libc::sockaddr {
    ifa.ifa_ifu
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn broadcast_sockaddr(ifa: &libc::ifaddrs) -> *mut libc::sockaddr {
    ifa.ifa_dstaddr
}

fn extract_broadcast(ifa: &libc::ifaddrs, addr: Ipv4Addr) -> Option<Ipv4Addr> {
    let broadcast_addr = broadcast_sockaddr(ifa);
    if (ifa.ifa_flags & libc::IFF_BROADCAST as u32) != 0 && !broadcast_addr.is_null() {
        // SAFETY: for a broadcast interface `getifaddrs` stores its broadcast
        // address in this field, which is null-checked above.
        let bcast = unsafe { &*(broadcast_addr as *const libc::sockaddr_in) };
        if i32::from(bcast.sin_family) == libc::AF_INET {
            return Some(Ipv4Addr::from(u32::from_be(bcast.sin_addr.s_addr)));
        }
    }
    if !ifa.ifa_netmask.is_null() {
        // SAFETY: `ifa_netmask` is valid `sockaddr_in` for an `AF_INET` entry.
        let mask = unsafe { &*(ifa.ifa_netmask as *const libc::sockaddr_in) };
        if i32::from(mask.sin_family) == libc::AF_INET {
            let addr_bits = u32::from(addr);
            let mask_bits = u32::from_be(mask.sin_addr.s_addr);
            return Some(Ipv4Addr::from(addr_bits | !mask_bits));
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn extract_mac(sa: *const libc::sockaddr) -> Option<String> {
    // SAFETY: `sa` points to a valid `sockaddr`.
    let family = unsafe { (*sa).sa_family } as i32;
    if family != libc::AF_PACKET {
        return None;
    }
    // SAFETY: `family == AF_PACKET`, so `sockaddr_ll` layout applies.
    let sll = unsafe { &*(sa as *const libc::sockaddr_ll) };
    if sll.sll_halen != 6 {
        return None;
    }
    let bytes = &sll.sll_addr[..6];
    if bytes.iter().all(|b| *b == 0) {
        return None;
    }
    Some(format_mac(bytes))
}

#[cfg(target_os = "macos")]
fn extract_mac(sa: *const libc::sockaddr) -> Option<String> {
    // SAFETY: `sa` points to a valid `sockaddr`.
    let family = unsafe { (*sa).sa_family } as i32;
    if family != libc::AF_LINK {
        return None;
    }
    // SAFETY: `family == AF_LINK`, so `sockaddr_dl` layout applies.
    let sdl = unsafe { &*(sa as *const libc::sockaddr_dl) };
    if sdl.sdl_alen != 6 {
        return None;
    }
    let nlen = sdl.sdl_nlen as usize;
    let alen = sdl.sdl_alen as usize;
    if nlen + alen > sdl.sdl_data.len() {
        return None;
    }
    let slice = &sdl.sdl_data[nlen..nlen + alen];
    let bytes: [u8; 6] = [
        slice[0] as u8,
        slice[1] as u8,
        slice[2] as u8,
        slice[3] as u8,
        slice[4] as u8,
        slice[5] as u8,
    ];
    if bytes.iter().all(|b| *b == 0) {
        return None;
    }
    Some(format_mac(&bytes))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn extract_mac(_sa: *const libc::sockaddr) -> Option<String> {
    None
}

fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_unspecified_return_empty_interface_info() {
        let lookup = SystemNetworkLookup;
        assert_eq!(
            lookup.interface_for(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            InterfaceInfo::default()
        );
        assert_eq!(
            lookup.interface_for(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            InterfaceInfo::default()
        );
    }
}
