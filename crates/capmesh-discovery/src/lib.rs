//! DNS-SD / mDNS discovery for the capmesh (DESIGN §5).
//!
//! This crate is the reusable discovery layer: the `_capmesh._tcp` advert schema, the
//! advertiser, and the browse receiver. `capmeshd` and any future mesh client depend on
//! it rather than re-implementing the wire schema.
//!
//! One service type — `_capmesh._tcp.local.` — carries one record per advertised
//! capability. The TXT record holds only the coarse, filterable keys (`cap`, `dir`,
//! `id`, `host`, `v`, `ep`, `descr`); the rich typed descriptor is fetched separately
//! via the `descr` pointer. The advertiser is generalized from the sibling `nmidi-core`
//! crate's `ServiceAdvertiser`, which advertised `_apple-midi._udp` — here it advertises
//! the capmesh control endpoint.
//!
//! ⚠ Peers are always connected by the **IP address from the mDNS A/AAAA record**
//! ([`resolved_addr`]), never by resolving a `.local`/`.lan` name (DESIGN §5).

use anyhow::{Context, Result};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceInfo};
use std::{collections::HashMap, net::IpAddr, sync::Arc};
use tracing::warn;

/// The capmesh DNS-SD service type (DESIGN §5): the daemon's control endpoint.
pub const SERVICE_TYPE: &str = "_capmesh._tcp.local.";

/// The AppleMIDI/RTP-MIDI DNS-SD service type. A MIDI source (a Mac network session or
/// `nmidi-fake-source`) advertises this natively; capmeshd browses it to learn a MIDI peer's
/// data-plane **control** port, correlated to the capmesh node by IP (DESIGN §5). The SRV port
/// of this record is the control port; the data-plane daemon derives the data port as +1.
pub const APPLE_MIDI_SERVICE_TYPE: &str = "_apple-midi._udp.local.";

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

    /// Whether this advert matches an optional `discover(kind?, dir?, host?)` selector (§7):
    /// each `Some` filter must equal the corresponding field; `None` filters match anything.
    pub fn matches(&self, kind: Option<&str>, dir: Option<&str>, host: Option<&str>) -> bool {
        kind.is_none_or(|k| self.cap == k)
            && dir.is_none_or(|d| self.dir == d)
            && host.is_none_or(|h| self.host == h)
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

/// Browse the LAN for `_apple-midi._udp` records — MIDI sources' data-plane endpoints. The
/// caller matches `ServiceEvent::ServiceResolved`, reads [`resolved_addr`] + the SRV port
/// (`svc.get_port()`, the AppleMIDI control port), and feeds them to an [`AppleMidiPeers`] map
/// to correlate with capmesh MIDI capabilities by IP.
pub fn browse_apple_midi() -> Result<mdns_sd::Receiver<mdns_sd::ServiceEvent>> {
    let mdns = ServiceDaemon::new().context("failed to create mDNS daemon")?;
    let receiver = mdns
        .browse(APPLE_MIDI_SERVICE_TYPE)
        .context("failed to browse for AppleMIDI services")?;
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

/// The set of AppleMIDI peers currently seen on the LAN, keyed by IP — the correlation table
/// that lets capmeshd resolve a discovered MIDI capability (a `_capmesh._tcp` advert, known by
/// its resolved IP) to the data-plane **control** port from the peer's `_apple-midi._udp`
/// record. Populated from [`browse_apple_midi`] events; queried when issuing a mount.
///
/// Keyed by IP because that is the stable join between the two adverts (both resolve to the
/// same host address); the value is the AppleMIDI control (SRV) port, passed into a mount
/// verbatim (the data-plane daemon derives the data port as control + 1).
#[derive(Debug, Clone, Default)]
pub struct AppleMidiPeers {
    by_addr: HashMap<IpAddr, u16>,
}

impl AppleMidiPeers {
    /// A fresh, empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or refresh) the control port observed for a peer IP.
    pub fn observe(&mut self, addr: IpAddr, control_port: u16) {
        self.by_addr.insert(addr, control_port);
    }

    /// Drop a peer IP that went away (its `_apple-midi._udp` record was removed).
    pub fn forget(&mut self, addr: &IpAddr) {
        self.by_addr.remove(addr);
    }

    /// The AppleMIDI control port most recently observed for `addr`, if any.
    pub fn control_port(&self, addr: &IpAddr) -> Option<u16> {
        self.by_addr.get(addr).copied()
    }
}

/// Why selecting a single capability from a discovery result failed (§7). A mount targets one
/// capability, so neither "none" nor "several" can be resolved by guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// No discovered advert matched the selector.
    NoMatch,
    /// More than one advert matched; the selector must be narrowed. Carries the match count.
    Ambiguous(usize),
}

impl std::fmt::Display for SelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectError::NoMatch => write!(f, "no discovered capability matches the selector"),
            SelectError::Ambiguous(n) => write!(
                f,
                "{n} discovered capabilities match the selector; narrow it with --kind/--host/--id"
            ),
        }
    }
}

impl std::error::Error for SelectError {}

/// Select exactly one capability from discovered adverts by an optional `(kind, dir, host, id)`
/// selector (§7): each `Some` field must match; `id` pins the exact capability. Errors if none
/// match ([`SelectError::NoMatch`]) or more than one does ([`SelectError::Ambiguous`]) — a mount
/// targets a single capability, so ambiguity is surfaced to the caller rather than guessed.
pub fn select_one<'a>(
    adverts: &'a [CapabilityAdvert],
    kind: Option<&str>,
    dir: Option<&str>,
    host: Option<&str>,
    id: Option<&str>,
) -> Result<&'a CapabilityAdvert, SelectError> {
    let mut matching = adverts
        .iter()
        .filter(|a| a.matches(kind, dir, host) && id.is_none_or(|i| a.id == i));
    let first = matching.next().ok_or(SelectError::NoMatch)?;
    let extra = matching.count();
    if extra > 0 {
        return Err(SelectError::Ambiguous(extra + 1));
    }
    Ok(first)
}

/// A capmesh peer whose auto-mount rule matched but which is waiting on the peer's AppleMIDI
/// **control** port — its `_apple-midi._udp` record has not resolved yet (DESIGN §6.1). Holds
/// exactly what the daemon needs to re-run auto-mount once that record arrives.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingPeer {
    /// The peer's capmesh advert (kind/dir/id/host/descr).
    pub advert: CapabilityAdvert,
    /// The peer's capmesh control endpoint port (its `_capmesh._tcp` SRV port), for the descriptor fetch.
    pub ep: u16,
    /// The peer's mDNS fullname, so a re-issued mount is tracked under the same key.
    pub fullname: String,
}

/// Peers whose auto-mount matched but await their AppleMIDI control port, keyed by IP — the join
/// to an incoming `_apple-midi._udp` record ([`AppleMidiPeers`]). When that record resolves the
/// daemon [`take`](Self::take)s the peers here for that IP and re-runs auto-mount (now the control
/// port is known), instead of waiting for the next `_capmesh._tcp` re-resolve. Deduplicated by
/// fullname so a capmesh re-resolve arriving before the apple-midi record does not stack copies.
#[derive(Debug, Clone, Default)]
pub struct PendingAutoMounts {
    by_addr: HashMap<IpAddr, Vec<PendingPeer>>,
}

impl PendingAutoMounts {
    /// A fresh, empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a peer awaiting its control port at `addr`. A prior entry with the same fullname at
    /// this addr is replaced (a re-resolve refreshes rather than duplicates).
    pub fn record(&mut self, addr: IpAddr, peer: PendingPeer) {
        let peers = self.by_addr.entry(addr).or_default();
        peers.retain(|p| p.fullname != peer.fullname);
        peers.push(peer);
    }

    /// Remove and return every peer awaiting a control port at `addr` — called when that addr's
    /// `_apple-midi._udp` record resolves. Empty if none were waiting.
    pub fn take(&mut self, addr: &IpAddr) -> Vec<PendingPeer> {
        self.by_addr.remove(addr).unwrap_or_default()
    }

    /// Drop every parked peer with this mDNS fullname, across all addresses — called when a
    /// peer's `_capmesh._tcp` advert is removed before its control port ever arrived, so its
    /// auto-mount can no longer be issued (§6.1). Keeps the registry from retaining dead peers.
    pub fn forget_fullname(&mut self, fullname: &str) {
        self.by_addr.retain(|_addr, peers| {
            peers.retain(|p| p.fullname != fullname);
            !peers.is_empty()
        });
    }
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

    #[test]
    fn matches_applies_optional_filters() {
        let a = sample(); // cap=midi, dir=source, host=green-machine
        assert!(a.matches(None, None, None)); // no filters → any
        assert!(a.matches(Some("midi"), Some("source"), Some("green-machine")));
        assert!(a.matches(Some("midi"), None, None));
        assert!(!a.matches(Some("audio"), None, None)); // wrong kind
        assert!(!a.matches(None, Some("sink"), None)); // wrong dir
        assert!(!a.matches(None, None, Some("other-host"))); // wrong host
    }

    #[test]
    fn apple_midi_peers_correlate_by_addr() {
        let a: IpAddr = "192.168.1.23".parse().unwrap();
        let b: IpAddr = "192.168.1.99".parse().unwrap();
        let mut peers = AppleMidiPeers::new();
        assert_eq!(peers.control_port(&a), None);

        peers.observe(a, 5004);
        assert_eq!(peers.control_port(&a), Some(5004));
        assert_eq!(peers.control_port(&b), None); // unrelated peer

        peers.observe(a, 5006); // refresh (e.g. re-advertised on a new port)
        assert_eq!(peers.control_port(&a), Some(5006));

        peers.forget(&a);
        assert_eq!(peers.control_port(&a), None); // peer went away
    }

    fn pending(fullname: &str) -> PendingPeer {
        PendingPeer {
            advert: sample(),
            ep: 7420,
            fullname: fullname.into(),
        }
    }

    fn advert(cap: &str, dir: &str, host: &str, id: &str) -> CapabilityAdvert {
        CapabilityAdvert {
            cap: cap.into(),
            dir: dir.into(),
            id: id.into(),
            host: host.into(),
            ep: 7420,
            descr: format!("/caps/{id}"),
        }
    }

    #[test]
    fn select_one_no_match_is_an_error() {
        let adverts = vec![advert("midi", "source", "a", "1")];
        assert_eq!(
            select_one(&adverts, Some("audio"), None, None, None),
            Err(SelectError::NoMatch)
        );
    }

    #[test]
    fn select_one_unique_match_is_returned() {
        let adverts = vec![
            advert("midi", "source", "green", "1"),
            advert("audio", "sink", "blue", "2"),
        ];
        // A kind that only one advert has selects it.
        let got = select_one(&adverts, Some("audio"), None, None, None).unwrap();
        assert_eq!(got.id, "2");
        // An id pins the exact capability even when kind alone is ambiguous.
        let got = select_one(&adverts, None, None, None, Some("1")).unwrap();
        assert_eq!(got.host, "green");
    }

    #[test]
    fn select_one_ambiguous_reports_the_count() {
        let adverts = vec![
            advert("midi", "source", "green", "1"),
            advert("midi", "source", "blue", "2"),
            advert("midi", "source", "red", "3"),
        ];
        // kind=midi matches all three → ambiguous; host narrows it to one.
        assert_eq!(
            select_one(&adverts, Some("midi"), None, None, None),
            Err(SelectError::Ambiguous(3))
        );
        let got = select_one(&adverts, Some("midi"), None, Some("blue"), None).unwrap();
        assert_eq!(got.id, "2");
    }

    #[test]
    fn pending_take_drains_the_addr() {
        let a: IpAddr = "192.168.1.23".parse().unwrap();
        let b: IpAddr = "192.168.1.99".parse().unwrap();
        let mut p = PendingAutoMounts::new();
        assert!(p.take(&a).is_empty()); // nothing waiting

        p.record(a, pending("host-midi-1"));
        p.record(b, pending("other-midi-1")); // unrelated addr untouched

        let drained = p.take(&a);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].fullname, "host-midi-1");
        assert!(p.take(&a).is_empty()); // take removed it
        assert_eq!(p.take(&b).len(), 1); // b still waiting
    }

    #[test]
    fn pending_forget_fullname_removes_across_addrs() {
        let a: IpAddr = "192.168.1.23".parse().unwrap();
        let b: IpAddr = "192.168.1.99".parse().unwrap();
        let mut p = PendingAutoMounts::new();
        p.record(a, pending("host-midi-1"));
        p.record(b, pending("host-midi-1")); // the same peer parked at two addrs
        p.record(b, pending("other-midi-2"));

        p.forget_fullname("host-midi-1");
        assert!(p.take(&a).is_empty()); // a held only host-midi-1 → the addr entry is dropped
        let rest = p.take(&b);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].fullname, "other-midi-2"); // an unrelated peer at b is retained
    }

    #[test]
    fn pending_record_dedups_by_fullname() {
        let a: IpAddr = "192.168.1.23".parse().unwrap();
        let mut p = PendingAutoMounts::new();
        p.record(a, pending("host-midi-1"));
        p.record(a, pending("host-midi-1")); // same peer re-resolved → refresh, not stack
        p.record(a, pending("host-midi-2")); // a distinct peer at the same addr is kept
        let drained = p.take(&a);
        assert_eq!(drained.len(), 2);
    }
}
