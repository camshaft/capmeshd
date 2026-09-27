//! Enumerating local MIDI ports as control-protocol `PortDescriptor`s.
//!
//! The daemon reads ports through a [`PortProvider`] so the request-handling
//! logic can be exercised without a real ALSA/CoreMIDI backend: the production
//! provider scans `midir`, while tests inject a static list.

use std::collections::HashMap;

use crate::protocol::{Format, PortDescriptor};
use nmidi_core::midi::{MidiPortInfo, MidiPortType, MidiPorts, detect_ports};

/// Source of the daemon's local ports. Implemented by the real `midir` scan and
/// by test doubles.
pub trait PortProvider: Send + Sync {
    /// Enumerate current local ports as protocol descriptors.
    fn list_ports(&self) -> anyhow::Result<Vec<PortDescriptor>>;
}

fn dir_str(port_type: MidiPortType) -> &'static str {
    match port_type {
        // A `midir` input port is a MIDI *source* (produces events, e.g. a
        // keyboard); an output port is a *sink* (consumes events, e.g. a synth).
        MidiPortType::Input => "source",
        MidiPortType::Output => "sink",
    }
}

/// Slugify a device name into a stable, id-safe token: lowercase alphanumerics,
/// other runs collapsed to single dashes, trimmed.
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    let mut pending_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
            pending_dash = false;
        } else if !s.is_empty() && !pending_dash {
            s.push('-');
            pending_dash = true;
        }
    }
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() { "port".to_string() } else { s }
}

/// The stable, name-derived base port-id `<dir>-<slug(name)>`.
///
/// Deriving the id from the device *name* rather than its enumeration index
/// makes it "stable within this daemon/host" (CONTROL-PROTOCOL.md §2): unplugging
/// one device no longer reindexes and renames its siblings, so a peer keying
/// mounts off `port-id` doesn't see spurious churn.
pub fn base_port_id(info: &MidiPortInfo) -> String {
    format!("{}-{}", dir_str(info.port_type), slug(&info.name))
}

fn descriptor_with_id(info: &MidiPortInfo, port_id: String) -> PortDescriptor {
    PortDescriptor {
        port_id,
        kind: "stream".to_string(),
        dir: Some(dir_str(info.port_type).to_string()),
        r#type: "midi".to_string(),
        name: info.name.clone(),
        virtualizable: true,
        formats: vec![Format::midi1()],
    }
}

/// Map one enumerated `midir` port to its control-protocol descriptor, using the
/// stable name-derived id. (Batch callers should prefer [`descriptors_for`],
/// which additionally disambiguates identical-named siblings.)
pub fn descriptor_for(info: &MidiPortInfo) -> PortDescriptor {
    descriptor_with_id(info, base_port_id(info))
}

/// Map a full port snapshot to descriptors (sources first, then sinks).
///
/// Two ports that share a `(dir, name)` — and thus a base id — are disambiguated
/// by appending the enumeration index, so ids stay unique within a scan while
/// unique-named ports keep their stable id across reindex.
pub fn descriptors_for(ports: &MidiPorts) -> Vec<PortDescriptor> {
    let all = ports.all_ports();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for info in &all {
        *counts.entry(base_port_id(info)).or_default() += 1;
    }
    all.iter()
        .map(|info| {
            let base = base_port_id(info);
            let id = if counts[&base] > 1 {
                format!("{base}-{}", info.index)
            } else {
                base
            };
            descriptor_with_id(info, id)
        })
        .collect()
}

/// Production provider: scans local ports via `midir` on each call.
pub struct MidirPortProvider;

impl PortProvider for MidirPortProvider {
    fn list_ports(&self) -> anyhow::Result<Vec<PortDescriptor>> {
        let ports = detect_ports()?;
        Ok(descriptors_for(&ports))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(name: &str, index: usize, port_type: MidiPortType) -> MidiPortInfo {
        MidiPortInfo {
            name: name.to_string(),
            index,
            port_type,
        }
    }

    #[test]
    fn maps_input_to_virtualizable_midi_source() {
        let d = descriptor_for(&port("Keystation 49e", 0, MidiPortType::Input));
        assert_eq!(d.port_id, "source-keystation-49e");
        assert_eq!(d.dir.as_deref(), Some("source"));
        assert_eq!(d.kind, "stream");
        assert_eq!(d.r#type, "midi");
        assert!(d.virtualizable);
        assert_eq!(d.formats, vec![Format::midi1()]);
    }

    #[test]
    fn maps_output_to_midi_sink() {
        let d = descriptor_for(&port("SuperCollider", 2, MidiPortType::Output));
        assert_eq!(d.port_id, "sink-supercollider");
        assert_eq!(d.dir.as_deref(), Some("sink"));
    }

    #[test]
    fn slug_normalizes_punctuation_and_case() {
        assert_eq!(slug("Keystation 49e"), "keystation-49e");
        assert_eq!(slug("Client 72: USB-MIDI!!"), "client-72-usb-midi");
        assert_eq!(slug("  spaced  "), "spaced");
        assert_eq!(slug("***"), "port");
    }

    #[test]
    fn port_id_is_stable_across_sibling_unplug_reindex() {
        // "pad" is at index 1 while "kbd" is present…
        let before = MidiPorts {
            inputs: vec![
                port("kbd", 0, MidiPortType::Input),
                port("pad", 1, MidiPortType::Input),
            ],
            outputs: vec![],
        };
        // …and reindexed to 0 after "kbd" is unplugged.
        let after = MidiPorts {
            inputs: vec![port("pad", 0, MidiPortType::Input)],
            outputs: vec![],
        };
        let id_before = descriptors_for(&before)
            .into_iter()
            .find(|d| d.name == "pad")
            .unwrap()
            .port_id;
        let id_after = descriptors_for(&after)[0].port_id.clone();
        assert_eq!(id_before, "source-pad");
        assert_eq!(id_after, "source-pad"); // stable despite the index change
    }

    #[test]
    fn identical_named_siblings_are_disambiguated() {
        let ports = MidiPorts {
            inputs: vec![
                port("USB MIDI", 0, MidiPortType::Input),
                port("USB MIDI", 3, MidiPortType::Input),
            ],
            outputs: vec![],
        };
        let ids: Vec<String> = descriptors_for(&ports)
            .into_iter()
            .map(|d| d.port_id)
            .collect();
        assert_eq!(ids, vec!["source-usb-midi-0", "source-usb-midi-3"]);
    }
}
