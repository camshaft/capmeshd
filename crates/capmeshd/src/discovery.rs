//! DNS-SD / mDNS discovery for the capmesh (DESIGN §5).
//!
//! One service type — `_capmesh._tcp.local.` — carries one record per advertised
//! capability. The TXT record holds only the coarse, filterable keys (`cap`, `dir`,
//! `id`, `host`, `v`, `ep`, `descr`); the rich typed descriptor is fetched separately
//! via the `descr` pointer. The advertiser is generalized from nmidi's
//! `ServiceAdvertiser` (`nmidi-core/src/discovery.rs`), which advertised
//! `_apple-midi._udp` — here it advertises the capmesh control endpoint.
//!
//! ⚠ Peers are always connected by the **IP address from the mDNS A/AAAA record**
//! ([`resolved_addr`]), never by resolving a `.local`/`.lan` name (DESIGN §5).

use anyhow::{Context, Result};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceInfo};
use std::{collections::HashMap, net::IpAddr, sync::Arc};
use tracing::warn;

/// The capmesh DNS-SD service type (DESIGN §5): the daemon's control endpoint.
pub const SERVICE_TYPE: &str = "_capmesh._tcp.local.";

/// Advertisement schema version carried in the `v` TXT key.
pub const ADVERT_VERSION: &str = "1";

/// The coarse, filterable TXT keys of a `_capmesh._tcp` record (DESIGN §5).
///
/// This is deliberately *not* the rich descriptor — TXT has size/typing limits, so it
/// carries only what a browser filters on, plus `descr`, a pointer to fetch the full
/// typed descriptor over the daemon's own RPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityAdvert {
    /// Capability kind: `midi | audio | screen | control-api | generic-stream | generic-rpc`.
    pub cap: String,
    /// Direction: `source | sink | duplex | control`.
    pub dir: String,
    /// Stable capability id (UUID).
    pub id: String,
    /// The advertising host's id.
    pub host: String,
    /// Control endpoint port.
    pub ep: u16,
    /// Pointer to fetch the rich descriptor, e.g. `/caps/<uuid>`.
    pub descr: String,
}

impl CapabilityAdvert {
    /// Render the advert into the TXT key/value map (DESIGN §5).
    pub fn to_txt(&self) -> HashMap<String, String> {
        let mut props = HashMap::new();
        props.insert("cap".to_string(), self.cap.clone());
        props.insert("dir".to_string(), self.dir.clone());
        props.insert("id".to_string(), self.id.clone());
        props.insert("host".to_string(), self.host.clone());
        props.insert("v".to_string(), ADVERT_VERSION.to_string());
        props.insert("ep".to_string(), self.ep.to_string());
        props.insert("descr".to_string(), self.descr.clone());
        props
    }

    /// Parse an advert back out of a TXT key/value map.
    ///
    /// Rejects a record whose `v` is not [`ADVERT_VERSION`] (an incompatible schema) and
    /// a record missing a required key.
    pub fn from_txt(props: &HashMap<String, String>) -> Result<Self> {
        let get = |k: &str| -> Result<String> {
            props
                .get(k)
                .cloned()
                .with_context(|| format!("capmesh advert missing TXT key `{k}`"))
        };
        let v = get("v")?;
        if v != ADVERT_VERSION {
            anyhow::bail!("capmesh advert version `{v}` != supported `{ADVERT_VERSION}`");
        }
        let ep = get("ep")?
            .parse::<u16>()
            .context("capmesh advert `ep` is not a valid port")?;
        Ok(Self {
            cap: get("cap")?,
            dir: get("dir")?,
            id: get("id")?,
            host: get("host")?,
            ep,
            descr: get("descr")?,
        })
    }

    /// The mDNS instance name for this advert — unique within the service type.
    pub fn instance_name(&self) -> String {
        format!("{}-{}-{}", self.host, self.cap, self.id)
    }
}

/// Manages capmesh mDNS advertisements. Generalized from nmidi's `ServiceAdvertiser`.
pub struct ServiceAdvertiser {
    mdns: Arc<ServiceDaemon>,
}

impl ServiceAdvertiser {
    /// Create a new advertiser backed by its own mDNS daemon.
    pub fn new() -> Result<Self> {
        let mdns = ServiceDaemon::new().context("failed to create mDNS daemon")?;
        Ok(Self {
            mdns: Arc::new(mdns),
        })
    }

    /// Advertise a capability over `_capmesh._tcp`. The returned [`Advert`] unregisters
    /// the record on drop.
    pub fn advertise(&self, advert: &CapabilityAdvert) -> Result<Advert> {
        // Addresses are auto-detected by the daemon when the ip argument is empty, so
        // browsers learn our real A/AAAA records (connect-by-IP, DESIGN §5).
        let hostname = format!("{}.local.", advert.host);
        let service_info = ServiceInfo::new(
            SERVICE_TYPE,
            &advert.instance_name(),
            &hostname,
            "",
            advert.ep,
            Some(advert.to_txt()),
        )
        .context("failed to build capmesh service info")?;

        let fullname = service_info.get_fullname().to_string();
        self.mdns
            .register(service_info)
            .context("failed to register capmesh mDNS service")?;

        Ok(Advert {
            mdns: Arc::clone(&self.mdns),
            fullname,
        })
    }
}

/// A live advertisement; unregisters its record when dropped.
pub struct Advert {
    mdns: Arc<ServiceDaemon>,
    fullname: String,
}

impl Drop for Advert {
    fn drop(&mut self) {
        if let Err(e) = self.mdns.unregister(&self.fullname) {
            warn!(
                "failed to unregister capmesh service {}: {e}",
                self.fullname
            );
        }
    }
}

/// Browse the mesh for `_capmesh._tcp` records. Returns the raw mDNS event receiver;
/// the caller matches `ServiceEvent::ServiceResolved` and reads [`resolved_addr`] +
/// [`CapabilityAdvert::from_txt`].
pub fn browse() -> Result<mdns_sd::Receiver<mdns_sd::ServiceEvent>> {
    let mdns = ServiceDaemon::new().context("failed to create mDNS daemon")?;
    let receiver = mdns
        .browse(SERVICE_TYPE)
        .context("failed to browse for capmesh services")?;
    Ok(receiver)
}

/// The IP address to connect a discovered peer by (DESIGN §5): the address from the
/// resolved mDNS record, IPv4 preferred, never a `.local`/`.lan` name. `None` if the
/// record carried no address.
pub fn resolved_addr(svc: &ResolvedService) -> Option<IpAddr> {
    let addrs = svc.get_addresses();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.iter().next())
        .map(|a| a.to_ip_addr())
}

/// Parse the capmesh advert out of a resolved peer's TXT record.
pub fn advert_from_resolved(svc: &ResolvedService) -> Result<CapabilityAdvert> {
    let props: HashMap<String, String> = svc
        .get_properties()
        .iter()
        .map(|p| (p.key().to_string(), p.val_str().to_string()))
        .collect();
    CapabilityAdvert::from_txt(&props)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CapabilityAdvert {
        CapabilityAdvert {
            cap: "midi".into(),
            dir: "source".into(),
            id: "b1f0-uuid".into(),
            host: "green-machine".into(),
            ep: 5004,
            descr: "/caps/b1f0-uuid".into(),
        }
    }

    #[test]
    fn txt_round_trips() {
        let a = sample();
        let parsed = CapabilityAdvert::from_txt(&a.to_txt()).expect("round trip");
        assert_eq!(a, parsed);
    }

    #[test]
    fn from_txt_rejects_wrong_version() {
        let mut props = sample().to_txt();
        props.insert("v".into(), "99".into());
        assert!(CapabilityAdvert::from_txt(&props).is_err());
    }

    #[test]
    fn from_txt_rejects_missing_key() {
        let mut props = sample().to_txt();
        props.remove("host");
        assert!(CapabilityAdvert::from_txt(&props).is_err());
    }

    #[test]
    fn from_txt_rejects_bad_port() {
        let mut props = sample().to_txt();
        props.insert("ep".into(), "not-a-port".into());
        assert!(CapabilityAdvert::from_txt(&props).is_err());
    }

    #[test]
    fn instance_name_is_unique_per_capability() {
        let a = sample();
        assert_eq!(a.instance_name(), "green-machine-midi-b1f0-uuid");
    }
}
