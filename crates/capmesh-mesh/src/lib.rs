//! The capmesh **mesh control endpoint** client (docs/MESH-PROTOCOL.md).
//!
//! A browsing host's capmeshd resolves a discovered `_capmesh._tcp` advert's `descr` pointer
//! into a full [`CapabilityDescriptor`] by an HTTP/1.1 `GET` against the advertising peer's
//! endpoint at `<addr>:<ep>`. The peer address is always the IP from the mDNS record (DESIGN
//! §5), never a `.local`/`.lan` name — the caller passes the resolved [`IpAddr`].
//!
//! The transport is a minimal hand-rolled HTTP/1.1 GET over `tokio` TCP rather than a full
//! HTTP client stack: the surface is two read-only routes returning JSON, so this keeps
//! capmeshd's dependency closure lean and matches the codebase's hand-rolled-protocol style
//! (`capmesh-ctl` NDJSON framing, nmidid RTP-MIDI). The client sends `Connection: close` and
//! reads the body to EOF, so it needs no chunked/Content-Length decoding.

use capmesh_model::{CapabilitiesResponse, CapabilityDescriptor};
use std::net::IpAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Errors from fetching a descriptor over the mesh control endpoint.
#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("mesh endpoint io: {0}")]
    Io(#[from] std::io::Error),
    #[error("mesh endpoint json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("mesh endpoint returned HTTP {status}")]
    Http { status: u16 },
    #[error("malformed HTTP response from mesh endpoint")]
    Malformed,
}

/// Fetch one capability's descriptor by resolving its `descr` path (§3 `GET /caps/<id>`).
pub async fn fetch_capability(
    addr: IpAddr,
    port: u16,
    descr_path: &str,
) -> Result<CapabilityDescriptor, MeshError> {
    let body = get(addr, port, descr_path).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Fetch every capability the peer exposes in one round trip (§3 `GET /caps`).
pub async fn fetch_all(addr: IpAddr, port: u16) -> Result<CapabilitiesResponse, MeshError> {
    let body = get(addr, port, "/caps").await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Issue a minimal HTTP/1.1 `GET <path>` to `addr:port` and return the response body bytes,
/// mapping a non-2xx status to [`MeshError::Http`].
async fn get(addr: IpAddr, port: u16, path: &str) -> Result<Vec<u8>, MeshError> {
    let mut stream = TcpStream::connect((addr, port)).await?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    // `Connection: close` → the peer closes after the body, so read to EOF.
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;

    let sep = find(&raw, b"\r\n\r\n").ok_or(MeshError::Malformed)?;
    let status = parse_status(&raw[..sep])?;
    if !(200..300).contains(&status) {
        return Err(MeshError::Http { status });
    }
    Ok(raw[sep + 4..].to_vec())
}

/// Parse the status code out of an HTTP head (`HTTP/1.1 200 OK\r\n...`).
fn parse_status(head: &[u8]) -> Result<u16, MeshError> {
    let first = head.split(|&b| b == b'\r').next().unwrap_or(head);
    let line = std::str::from_utf8(first).map_err(|_| MeshError::Malformed)?;
    line.split(' ')
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or(MeshError::Malformed)
}

/// Index of the first occurrence of `needle` in `hay`, if any.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawn a one-shot TCP server that reads the request, replies with `status`/`body` as a
    /// well-formed HTTP/1.1 response, and closes. Returns the bound port.
    async fn serve_once(status_line: &'static str, body: &'static str) -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Drain the request head (up to the blank line) — enough for the test.
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await.unwrap();
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            // Drop closes the connection → the client's read_to_end sees EOF.
        });
        port
    }

    #[tokio::test]
    async fn fetches_and_parses_a_capability_descriptor() {
        let body = r#"{"id":"green-machine-midi","host":"green-machine","kind":"midi",
            "dir":"source","ports":[{"port-id":"kbd-0","kind":"stream","dir":"source",
            "type":"midi","name":"Keystation 49e","formats":[{"codec":"midi1"}]}]}"#;
        let port = serve_once("200 OK", body).await;
        let cap = fetch_capability(Ipv4Addr::LOCALHOST.into(), port, "/caps/green-machine-midi")
            .await
            .unwrap();
        assert_eq!(cap.id, "green-machine-midi");
        assert_eq!(cap.kind, "midi");
        assert_eq!(cap.ports.len(), 1);
        assert_eq!(cap.ports[0].port_id, "kbd-0");
        assert_eq!(cap.ports[0].formats[0].codec, "midi1");
    }

    #[tokio::test]
    async fn fetches_the_caps_list() {
        let body = r#"{"caps":[{"id":"h-midi","host":"h","kind":"midi","dir":"duplex","ports":[]}]}"#;
        let port = serve_once("200 OK", body).await;
        let resp = fetch_all(Ipv4Addr::LOCALHOST.into(), port).await.unwrap();
        assert_eq!(resp.caps.len(), 1);
        assert_eq!(resp.caps[0].id, "h-midi");
    }

    #[tokio::test]
    async fn non_2xx_status_maps_to_http_error() {
        let port = serve_once("404 Not Found", r#"{"error":"no-such-capability"}"#).await;
        let err = fetch_capability(Ipv4Addr::LOCALHOST.into(), port, "/caps/nope")
            .await
            .unwrap_err();
        assert!(matches!(err, MeshError::Http { status: 404 }));
    }

    #[test]
    fn parses_status_code() {
        assert_eq!(parse_status(b"HTTP/1.1 200 OK").unwrap(), 200);
        assert_eq!(parse_status(b"HTTP/1.1 503 Service Unavailable").unwrap(), 503);
        assert!(parse_status(b"garbage").is_err());
    }
}
