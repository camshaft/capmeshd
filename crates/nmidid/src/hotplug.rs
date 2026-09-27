//! Hot-plug notifications (§5): watch the local MIDI ports and emit
//! `port-added` / `port-removed` notifications to capmeshd so it re-reconciles
//! on device plug/unplug instead of polling `list-ports`.
//!
//! Gated behind the `hotplug-events` capability (advertised in `hello`); the
//! notifications flow over the same per-daemon broadcast as `mount-state`.

use std::collections::HashSet;

use serde_json::Value;
use tokio::sync::{broadcast, watch};
use tracing::debug;

use crate::ports::descriptors_for;
use crate::protocol::PortDescriptor;
use nmidi_core::midi::MidiPorts;

/// Compute the `port-added` / `port-removed` §5 notifications for a transition
/// from `old` to `new` local ports, matched by `port-id`. Pure, so it is
/// unit-tested without a real MIDI backend.
pub(crate) fn diff_notifications(old: &[PortDescriptor], new: &[PortDescriptor]) -> Vec<Value> {
    let old_ids: HashSet<&str> = old.iter().map(|p| p.port_id.as_str()).collect();
    let new_ids: HashSet<&str> = new.iter().map(|p| p.port_id.as_str()).collect();

    let mut out = Vec::new();
    for port in new {
        if !old_ids.contains(port.port_id.as_str()) {
            out.push(serde_json::json!({
                "jsonrpc": "2.0", "method": "port-added", "params": { "port": port }
            }));
        }
    }
    for port in old {
        if !new_ids.contains(port.port_id.as_str()) {
            out.push(serde_json::json!({
                "jsonrpc": "2.0", "method": "port-removed", "params": { "port-id": port.port_id }
            }));
        }
    }
    out
}

/// Spawn a task that watches `rx` for local-port changes and broadcasts the
/// resulting `port-added` / `port-removed` notifications.
pub fn spawn(mut rx: watch::Receiver<MidiPorts>, notifier: broadcast::Sender<Value>) {
    tokio::spawn(async move {
        let mut prev = descriptors_for(&rx.borrow_and_update());
        loop {
            if rx.changed().await.is_err() {
                break; // monitor stopped
            }
            let current = descriptors_for(&rx.borrow_and_update());
            for notification in diff_notifications(&prev, &current) {
                debug!("hotplug notification: {}", notification["method"]);
                // Err just means no control connection is subscribed right now.
                let _ = notifier.send(notification);
            }
            prev = current;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Format;

    fn port(id: &str, dir: &str, name: &str) -> PortDescriptor {
        PortDescriptor {
            port_id: id.to_string(),
            kind: "stream".to_string(),
            dir: Some(dir.to_string()),
            r#type: "midi".to_string(),
            name: name.to_string(),
            virtualizable: true,
            formats: vec![Format::midi1()],
        }
    }

    #[test]
    fn added_port_yields_port_added_with_descriptor() {
        let old = vec![port("source-0", "source", "kbd")];
        let new = vec![
            port("source-0", "source", "kbd"),
            port("source-1", "source", "pad"),
        ];
        let out = diff_notifications(&old, &new);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["method"], "port-added");
        assert_eq!(out[0]["params"]["port"]["port-id"], "source-1");
        assert_eq!(out[0]["params"]["port"]["name"], "pad");
    }

    #[test]
    fn removed_port_yields_port_removed_with_id() {
        let old = vec![
            port("source-0", "source", "kbd"),
            port("sink-0", "sink", "synth"),
        ];
        let new = vec![port("source-0", "source", "kbd")];
        let out = diff_notifications(&old, &new);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["method"], "port-removed");
        assert_eq!(out[0]["params"]["port-id"], "sink-0");
    }

    #[test]
    fn no_change_yields_nothing() {
        let ports = vec![port("source-0", "source", "kbd")];
        assert!(diff_notifications(&ports, &ports).is_empty());
    }

    #[test]
    fn simultaneous_add_and_remove() {
        let old = vec![port("source-0", "source", "kbd")];
        let new = vec![port("sink-0", "sink", "synth")];
        let out = diff_notifications(&old, &new);
        assert_eq!(out.len(), 2);
        let methods: Vec<&str> = out.iter().map(|n| n["method"].as_str().unwrap()).collect();
        assert!(methods.contains(&"port-added"));
        assert!(methods.contains(&"port-removed"));
    }
}
