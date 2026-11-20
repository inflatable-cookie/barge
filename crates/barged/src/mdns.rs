//! mDNS (`_barged._tcp`) advertisement through Avahi over D-Bus on Linux.
//!
//! Hidden behind [`MdnsAdvertiser`] so tests never require a running Avahi
//! daemon. If Avahi is absent in production, `barged` logs once and keeps
//! serving.

use std::net::IpAddr;

use crate::Result;

/// Parameters for advertising `_barged._tcp` on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceAdvertisement {
    pub service_type: String,
    pub instance_name: String,
    pub port: u16,
    pub address: IpAddr,
    pub interface_index: Option<i32>,
    pub txt: Vec<(String, String)>,
}

/// Active mDNS registration; dropping it withdraws the advertisement.
pub trait MdnsRegistration: Send + Sync {}

/// Registers `_barged._tcp` on the local network.
pub trait MdnsAdvertiser: Send + Sync + 'static {
    fn advertise(&self, service: &ServiceAdvertisement) -> Result<Box<dyn MdnsRegistration>>;
}

/// Avahi over the system D-Bus (Linux only).
#[cfg(target_os = "linux")]
#[derive(Debug, Default, Clone, Copy)]
pub struct AvahiAdvertiser;

#[cfg(target_os = "linux")]
struct AvahiRegistration {
    connection: zbus::blocking::Connection,
    group_path: zbus::zvariant::OwnedObjectPath,
}

#[cfg(target_os = "linux")]
impl MdnsRegistration for AvahiRegistration {}

#[cfg(target_os = "linux")]
impl Drop for AvahiRegistration {
    fn drop(&mut self) {
        let _ = self.connection.call_method(
            Some("org.freedesktop.Avahi"),
            &self.group_path,
            Some("org.freedesktop.Avahi.EntryGroup"),
            "Free",
            &(),
        );
    }
}

#[cfg(target_os = "linux")]
impl MdnsAdvertiser for AvahiAdvertiser {
    fn advertise(&self, service: &ServiceAdvertisement) -> Result<Box<dyn MdnsRegistration>> {
        use crate::Error;
        const AVAHI_IF_UNSPEC: i32 = -1;
        const AVAHI_PROTO_INET: i32 = 0;
        const AVAHI_PROTO_INET6: i32 = 1;
        const AVAHI_PROTO_UNSPEC: i32 = -1;

        let connection = zbus::blocking::Connection::system()
            .map_err(|e| Error::new(format!("cannot reach system D-Bus for Avahi: {e}")))?;
        let reply = connection
            .call_method(
                Some("org.freedesktop.Avahi"),
                "/",
                Some("org.freedesktop.Avahi.Server"),
                "EntryGroupNew",
                &(),
            )
            .map_err(|e| Error::new(format!("Avahi EntryGroupNew failed: {e}")))?;
        let group_path: zbus::zvariant::OwnedObjectPath = reply
            .body()
            .deserialize()
            .map_err(|e| Error::new(format!("invalid Avahi EntryGroup path: {e}")))?;

        let interface = service.interface_index.unwrap_or(AVAHI_IF_UNSPEC);
        let protocol = if service.address.is_unspecified() {
            AVAHI_PROTO_UNSPEC
        } else if service.address.is_ipv4() {
            AVAHI_PROTO_INET
        } else {
            AVAHI_PROTO_INET6
        };
        let txt: Vec<Vec<u8>> = service
            .txt
            .iter()
            .map(|(k, v)| format!("{k}={v}").into_bytes())
            .collect();

        connection
            .call_method(
                Some("org.freedesktop.Avahi"),
                &group_path,
                Some("org.freedesktop.Avahi.EntryGroup"),
                "AddService",
                &(
                    interface,
                    protocol,
                    0_u32,
                    service.instance_name.as_str(),
                    service.service_type.as_str(),
                    "",
                    "",
                    service.port,
                    txt,
                ),
            )
            .map_err(|e| Error::new(format!("Avahi AddService failed: {e}")))?;

        connection
            .call_method(
                Some("org.freedesktop.Avahi"),
                &group_path,
                Some("org.freedesktop.Avahi.EntryGroup"),
                "Commit",
                &(),
            )
            .map_err(|e| Error::new(format!("Avahi Commit failed: {e}")))?;

        Ok(Box::new(AvahiRegistration {
            connection,
            group_path,
        }))
    }
}

/// Non-Linux stub: reports Avahi unavailable so the daemon logs once and
/// continues serving.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableMdns;

#[cfg(not(target_os = "linux"))]
impl MdnsAdvertiser for UnavailableMdns {
    fn advertise(&self, _service: &ServiceAdvertisement) -> Result<Box<dyn MdnsRegistration>> {
        Err(crate::Error::new(format!(
            "Avahi mDNS advertisement is not available on {}",
            std::env::consts::OS
        )))
    }
}
