use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::{collections::HashMap, sync::Arc};
use tracing::warn;

const SERVICE_TYPE: &str = "_apple-midi._udp.local.";

/// Manages multiple mDNS service advertisements
pub struct ServiceAdvertiser {
    mdns: Arc<ServiceDaemon>,
}

impl ServiceAdvertiser {
    /// Create a new service advertiser
    pub fn new() -> Result<Self> {
        let mdns = ServiceDaemon::new().context("Failed to create mDNS daemon")?;
        Ok(Self {
            mdns: Arc::new(mdns),
        })
    }

    /// Advertise a MIDI service via mDNS
    /// Returns a Service handle that automatically unregisters on drop
    pub fn advertise_service(
        &self,
        service_name: &str,
        device_name: &str,
        control_port: u16,
        properties: HashMap<String, String>,
    ) -> Result<Service> {
        let hostname = format!("{}.local.", device_name);

        let service_info = ServiceInfo::new(
            SERVICE_TYPE,
            service_name,
            &hostname,
            "",
            control_port,
            Some(properties),
        )
        .context("Failed to create service info")?;

        let fullname = service_info.get_fullname().to_string();

        self.mdns
            .register(service_info)
            .context("Failed to register mDNS service")?;

        Ok(Service {
            mdns: Arc::clone(&self.mdns),
            fullname,
        })
    }
}

/// Represents an advertised service that automatically unregisters on drop
pub struct Service {
    mdns: Arc<ServiceDaemon>,
    fullname: String,
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Err(e) = self.mdns.unregister(&self.fullname) {
            warn!("Failed to unregister service {}: {}", self.fullname, e);
        }
    }
}

/// Advertise MIDI service via mDNS (legacy single-service function)
pub fn advertise_service(device_name: &str, control_port: u16) -> Result<ServiceDaemon> {
    let mdns = ServiceDaemon::new().context("Failed to create mDNS daemon")?;

    // Create TXT records
    let mut properties = HashMap::new();
    properties.insert("name".to_string(), device_name.to_string());
    properties.insert("ver".to_string(), "2".to_string());

    // Register service
    let hostname = format!("{}.local.", device_name);
    let service_info = ServiceInfo::new(
        SERVICE_TYPE,
        device_name,
        &hostname,
        "",
        control_port,
        Some(properties),
    )
    .context("Failed to create service info")?;

    mdns.register(service_info)
        .context("Failed to register mDNS service")?;

    Ok(mdns)
}

/// Browse for MIDI services on the network
pub fn browse_services() -> Result<mdns_sd::Receiver<mdns_sd::ServiceEvent>> {
    let mdns = ServiceDaemon::new().context("Failed to create mDNS daemon")?;
    let receiver = mdns
        .browse(SERVICE_TYPE)
        .context("Failed to browse for services")?;
    Ok(receiver)
}
