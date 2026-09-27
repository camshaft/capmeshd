//! Enumerating local MIDI ports as control-protocol `PortDescriptor`s.
//!
//! The daemon reads ports through a [`PortProvider`] so the request-handling
//! logic can be exercised without a real ALSA/CoreMIDI backend: the production
//! provider scans `midir`, while tests inject a static list.

use crate::protocol::{Format, PortDescriptor};
use nmidi_core::midi::{MidiPortInfo, MidiPortType, MidiPorts, detect_ports};

/// Source of the daemon's local ports. Implemented by the real `midir` scan and
/// by test doubles.
pub trait PortProvider: Send + Sync {
    /// Enumerate current local ports as protocol descriptors.
    fn list_ports(&self) -> anyhow::Result<Vec<PortDescriptor>>;
}

/// Map one enumerated `midir` port to its control-protocol descriptor.
///
/// A `midir` input port is a MIDI *source* (produces events, e.g. a keyboard);
/// an output port is a *sink* (consumes events, e.g. a synth). The `port-id` is
/// derived from the direction and enumeration index so `list-ports` and
/// `describe-port` name the same port consistently within a scan.
pub fn descriptor_for(info: &MidiPortInfo) -> PortDescriptor {
    let dir = match info.port_type {
        MidiPortType::Input => "source",
        MidiPortType::Output => "sink",
    };
    PortDescriptor {
        port_id: format!("{dir}-{}", info.index),
        kind: "stream".to_string(),
        dir: Some(dir.to_string()),
        r#type: "midi".to_string(),
        name: info.name.clone(),
        virtualizable: true,
        formats: vec![Format::midi1()],
    }
}

/// Map a full port snapshot to descriptors (sources first, then sinks).
pub fn descriptors_for(ports: &MidiPorts) -> Vec<PortDescriptor> {
    ports.all_ports().iter().map(descriptor_for).collect()
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

    #[test]
    fn maps_input_to_virtualizable_midi_source() {
        let d = descriptor_for(&MidiPortInfo {
            name: "Keystation 49e".to_string(),
            index: 0,
            port_type: MidiPortType::Input,
        });
        assert_eq!(d.port_id, "source-0");
        assert_eq!(d.dir.as_deref(), Some("source"));
        assert_eq!(d.kind, "stream");
        assert_eq!(d.r#type, "midi");
        assert!(d.virtualizable);
        assert_eq!(d.formats, vec![Format::midi1()]);
    }

    #[test]
    fn maps_output_to_midi_sink() {
        let d = descriptor_for(&MidiPortInfo {
            name: "SuperCollider".to_string(),
            index: 2,
            port_type: MidiPortType::Output,
        });
        assert_eq!(d.port_id, "sink-2");
        assert_eq!(d.dir.as_deref(), Some("sink"));
    }
}
