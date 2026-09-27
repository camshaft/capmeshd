//! Local MIDI port enumeration and change monitoring.
//!
//! This lives in the core library so every nmidi component — the RTP-MIDI
//! server, and the `nmidid` control-socket daemon — enumerates local ports the
//! same way. Detection is backed by `midir`, so it uses ALSA on Linux and
//! CoreMIDI on macOS.

use anyhow::{Context, Result};
use midir::{MidiInput, MidiOutput};
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info};

/// A single local MIDI port and where it sits in the `midir` enumeration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MidiPortInfo {
    pub name: String,
    pub index: usize,
    pub port_type: MidiPortType,
}

/// Which `midir` half a port belongs to.
///
/// A `midir` *input* port is one we can read MIDI *from* (a source, e.g. a
/// keyboard); an *output* port is one we can write MIDI *to* (a sink, e.g. a
/// synth). This maps directly onto the control protocol's port direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MidiPortType {
    Input,
    Output,
}

impl MidiPortType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MidiPortType::Input => "input",
            MidiPortType::Output => "output",
        }
    }
}

fn is_internal_port(name: &str) -> bool {
    // Ignore ports that belong to this process (or other nmidi tools) to avoid
    // advertising our own virtual ports. ALSA/macOS may prepend client numbers
    // (e.g. "Client 72: nmidi-client:0") or use names like "nmidi-server-foo".
    let lname = name.to_lowercase();
    lname.contains("nmidi-server") || lname.contains("nmidi-client") || lname.starts_with("nmidi-")
}

/// A snapshot of all local input and output MIDI ports.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiPorts {
    pub inputs: Vec<MidiPortInfo>,
    pub outputs: Vec<MidiPortInfo>,
}

impl MidiPorts {
    pub fn all_ports(&self) -> Vec<MidiPortInfo> {
        self.inputs
            .iter()
            .chain(self.outputs.iter())
            .cloned()
            .collect()
    }
}

/// Detect available MIDI ports
pub fn detect_ports() -> Result<MidiPorts> {
    let midi_in = MidiInput::new("nmidi-server-detect").context("Failed to create MIDI input")?;
    let midi_out =
        MidiOutput::new("nmidi-server-detect").context("Failed to create MIDI output")?;

    let input_ports = midi_in.ports();
    let output_ports = midi_out.ports();

    let inputs = input_ports
        .iter()
        .enumerate()
        .filter_map(|(idx, port)| {
            midi_in.port_name(port).ok().and_then(|name| {
                if is_internal_port(&name) {
                    return None;
                }

                Some(MidiPortInfo {
                    name,
                    index: idx,
                    port_type: MidiPortType::Input,
                })
            })
        })
        .collect();

    let outputs = output_ports
        .iter()
        .enumerate()
        .filter_map(|(idx, port)| {
            midi_out.port_name(port).ok().and_then(|name| {
                if is_internal_port(&name) {
                    return None;
                }

                Some(MidiPortInfo {
                    name,
                    index: idx,
                    port_type: MidiPortType::Output,
                })
            })
        })
        .collect();

    Ok(MidiPorts { inputs, outputs })
}

/// Start monitoring MIDI ports for changes
/// Returns a watch receiver that will be notified whenever ports change
pub async fn start_port_monitor(poll_interval: Duration) -> watch::Receiver<MidiPorts> {
    // Detect initial ports
    let initial_ports = detect_ports().unwrap_or_else(|_| MidiPorts {
        inputs: Vec::new(),
        outputs: Vec::new(),
    });

    let (tx, rx) = watch::channel(initial_ports.clone());

    tokio::spawn(async move {
        let mut last_ports = initial_ports;

        loop {
            tokio::time::sleep(poll_interval).await;

            match detect_ports() {
                Ok(current_ports) => {
                    if current_ports != last_ports {
                        info!(
                            "MIDI port change detected: {} inputs, {} outputs",
                            current_ports.inputs.len(),
                            current_ports.outputs.len()
                        );
                        debug!("New ports: {:?}", current_ports);
                        last_ports = current_ports.clone();
                        if tx.send(current_ports).is_err() {
                            // Receiver dropped, exit monitoring
                            break;
                        }
                    }
                }
                Err(e) => {
                    debug!("Error detecting MIDI ports: {}", e);
                }
            }
        }
    });

    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_internal_ports() {
        assert!(is_internal_port("nmidi-server-test:0"));
        assert!(is_internal_port("Client 72: nmidi-client:0"));
        assert!(is_internal_port("nmidi-foo"));
    }

    #[test]
    fn ignores_external_ports() {
        assert!(!is_internal_port("Keystation 49e:0"));
        assert!(!is_internal_port("USB MIDI Interface"));
    }
}
