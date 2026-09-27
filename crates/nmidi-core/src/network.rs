use crate::{AppleMidiPacket, RtpPacket};
use anyhow::{Context, Result};
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tracing::{debug, warn};

/// Maximum UDP payload size for MIDI packets. Most networks should support at least 1200 bytes without fragmentation.
pub const MAX_UDP_PAYLOAD: usize = 1200;

const RECV_BUFFER_SIZE: usize = 1500;

pub struct NetworkSockets {
    pub control: UdpSocket,
    pub data: UdpSocket,
}

impl NetworkSockets {
    pub async fn bind(control_addr: &str, data_addr: &str) -> Result<Self> {
        let control = UdpSocket::bind(control_addr)
            .await
            .context("Failed to bind control socket")?;

        let data = UdpSocket::bind(data_addr)
            .await
            .context("Failed to bind data socket")?;

        debug!("Control socket bound to: {}", control.local_addr()?);
        debug!("Data socket bound to: {}", data.local_addr()?);

        Ok(Self { control, data })
    }

    /// Bind to consecutive ports (control port and control_port + 1 for data)
    /// This is required for Apple MIDI compatibility
    pub async fn bind_consecutive(bind_addr: &str) -> Result<Self> {
        // Try up to 100 times to find consecutive ports
        for _ in 0..100 {
            // Bind to a random port for control
            let control = UdpSocket::bind(format!("{}:0", bind_addr))
                .await
                .context("Failed to bind control socket")?;

            let control_port = control.local_addr()?.port();
            let data_port = control_port.wrapping_add(1);

            // Try to bind data socket to control_port + 1
            match UdpSocket::bind(format!("{}:{}", bind_addr, data_port)).await {
                Ok(data) => {
                    debug!("Control socket bound to: {}", control.local_addr()?);
                    debug!("Data socket bound to: {}", data.local_addr()?);
                    return Ok(Self { control, data });
                }
                Err(_) => {
                    // Port already in use, try again
                    continue;
                }
            }
        }

        anyhow::bail!("Failed to bind consecutive ports after 100 attempts")
    }

    pub async fn recv_control(&self) -> Result<(AppleMidiPacket, SocketAddr)> {
        let mut buf = [0u8; RECV_BUFFER_SIZE];
        let (len, addr) = self.control.recv_from(&mut buf).await?;

        match AppleMidiPacket::parse(&buf[..len]) {
            Ok(packet) => {
                debug!("Received control packet from {}: {:?}", addr, packet);
                Ok((packet, addr))
            }
            Err(e) => {
                warn!("Failed to parse control packet from {}: {}", addr, e);
                Err(e.into())
            }
        }
    }

    pub async fn send_control(&self, packet: &AppleMidiPacket, addr: &SocketAddr) -> Result<()> {
        let bytes = packet.to_bytes();
        self.control.send_to(&bytes, addr).await?;
        debug!(
            "Sent control packet to {}: {:?} bytes={:02X?}",
            addr, packet, bytes
        );
        Ok(())
    }

    /// Send a control packet using the data socket (used for the second stage of the
    /// AppleMIDI handshake where invitations are repeated on the data port).
    pub async fn send_control_on_data(
        &self,
        packet: &AppleMidiPacket,
        addr: &SocketAddr,
    ) -> Result<()> {
        let bytes = packet.to_bytes();
        self.data.send_to(&bytes, addr).await?;
        debug!(
            "Sent control packet on data socket to {}: {:?} bytes={:02X?}",
            addr, packet, bytes
        );
        Ok(())
    }

    pub async fn recv_data(&self) -> Result<(RtpPacket, SocketAddr)> {
        let mut buf = [0u8; RECV_BUFFER_SIZE];
        let (len, addr) = self.data.recv_from(&mut buf).await?;

        match RtpPacket::parse(&buf[..len]) {
            Ok(packet) => {
                debug!("Received data packet from {}", addr);
                Ok((packet, addr))
            }
            Err(e) => {
                warn!("Failed to parse data packet from {}: {}", addr, e);
                Err(e.into())
            }
        }
    }

    pub async fn send_data(&self, packet: &RtpPacket, addr: &SocketAddr) -> Result<()> {
        let bytes = packet.to_bytes();
        self.data.send_to(&bytes, addr).await?;
        debug!("Sent data packet to {}", addr);
        Ok(())
    }
}
