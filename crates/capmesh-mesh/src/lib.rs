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

/// The serving side of the mesh control endpoint (docs/MESH-PROTOCOL.md §3).
///
/// These are the pure, transport-agnostic pieces — request-line parsing, routing over a
/// snapshot of the host's capability descriptors, and HTTP response framing. The TCP accept
/// loop and the live `list-ports` projection that builds the `caps` snapshot are supplied by
/// capmeshd (which owns the config and the `capmesh-ctl` clients); keeping them out of here
/// lets the routing be unit-tested with no sockets and no `capmesh-ctl` dependency.
pub mod server {
    use capmesh_model::{CapabilitiesResponse, CapabilityDescriptor};

    /// Parse the request path from a raw HTTP/1.1 request head, requiring the `GET` method.
    /// The query string (if any) is stripped. Returns `None` for a non-GET or a malformed
    /// request line — the caller answers those with a 400/405 as it sees fit.
    pub fn parse_get_path(request: &[u8]) -> Option<String> {
        let line_end = super::find(request, b"\r\n").unwrap_or(request.len());
        let line = std::str::from_utf8(&request[..line_end]).ok()?;
        let mut parts = line.split(' ');
        if parts.next()? != "GET" {
            return None;
        }
        let target = parts.next()?;
        Some(target.split('?').next().unwrap_or(target).to_string())
    }

    /// Route a GET path against a snapshot of the host's capability descriptors (§3):
    /// `/caps` → 200 [`CapabilitiesResponse`]; `/caps/<id>` → 200 the descriptor, or 404;
    /// anything else → 404. Returns `(status, json-body)`.
    pub fn route(path: &str, caps: &[CapabilityDescriptor]) -> (u16, Vec<u8>) {
        if path == "/caps" {
            let resp = CapabilitiesResponse { caps: caps.to_vec() };
            return (200, serde_json::to_vec(&resp).unwrap_or_default());
        }
        if let Some(id) = path.strip_prefix("/caps/") {
            return match caps.iter().find(|c| c.id == id) {
                Some(cap) => (200, serde_json::to_vec(cap).unwrap_or_default()),
                None => (404, error_body("no-such-capability")),
            };
        }
        (404, error_body("not-found"))
    }

    /// A JSON error body `{ "error": "<machine-code>" }` (§3 errors).
    fn error_body(code: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "error": code })).unwrap_or_default()
    }

    /// Frame an HTTP/1.1 response with a JSON body and `Connection: close` (the framing the
    /// [`super::fetch_capability`] client expects — read the body to EOF).
    pub fn http_response(status: u16, body: &[u8]) -> Vec<u8> {
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            503 => "Service Unavailable",
            _ => "OK",
        };
        let mut out = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// Supplies the live snapshot of the host's capability descriptors for each request — a
    /// read-only projection built on demand (MESH-PROTOCOL §4). capmeshd implements this over
    /// its `capmesh-ctl` clients; keeping it a trait lets `capmesh-mesh` stay transport-only.
    pub trait CapabilityProvider: Send + Sync {
        /// The capabilities this host currently exposes.
        fn caps(&self) -> impl std::future::Future<Output = Vec<CapabilityDescriptor>> + Send;
    }

    /// Serve the mesh control endpoint on `listener` until it errors: accept a connection,
    /// answer one `GET /caps` / `GET /caps/<id>` from the provider's live snapshot, and close.
    /// Each connection is handled in its own task so a slow client cannot block the endpoint.
    pub async fn serve<P>(listener: tokio::net::TcpListener, provider: std::sync::Arc<P>)
    where
        P: CapabilityProvider + 'static,
    {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!("mesh endpoint accept failed: {e}");
                    continue;
                }
            };
            let provider = std::sync::Arc::clone(&provider);
            tokio::spawn(async move {
                if let Err(e) = handle_conn(stream, provider.as_ref()).await {
                    tracing::debug!("mesh endpoint connection error: {e}");
                }
            });
        }
    }

    /// Read one request, route it against a fresh provider snapshot, and write the response.
    async fn handle_conn<P: CapabilityProvider>(
        mut stream: tokio::net::TcpStream,
        provider: &P,
    ) -> std::io::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Read the request head until the blank line (a GET has no body), bounded so a peer
        // cannot stream unbounded bytes at the endpoint.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if super::find(&buf, b"\r\n\r\n").is_some() || buf.len() > 16 * 1024 {
                break;
            }
        }

        let (status, body) = match parse_get_path(&buf) {
            Some(path) => route(&path, &provider.caps().await),
            None => (400, error_body("bad-request")),
        };
        stream.write_all(&http_response(status, &body)).await?;
        stream.flush().await?;
        Ok(())
    }
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

#[cfg(test)]
mod server_tests {
    use super::server::*;
    use capmesh_model::{CapabilitiesResponse, CapabilityDescriptor};

    fn cap(id: &str, kind: &str) -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: id.into(),
            host: "green-machine".into(),
            kind: kind.into(),
            dir: "source".into(),
            ports: vec![],
        }
    }

    #[test]
    fn parse_get_path_requires_get_and_strips_query() {
        assert_eq!(
            parse_get_path(b"GET /caps HTTP/1.1\r\nHost: x\r\n\r\n").as_deref(),
            Some("/caps")
        );
        assert_eq!(
            parse_get_path(b"GET /caps/green-machine-midi?v=1 HTTP/1.1\r\n").as_deref(),
            Some("/caps/green-machine-midi")
        );
        assert_eq!(parse_get_path(b"POST /caps HTTP/1.1\r\n"), None);
        assert_eq!(parse_get_path(b"garbage"), None);
    }

    #[test]
    fn routes_caps_list_and_by_id_and_404() {
        let caps = vec![cap("green-machine-midi", "midi")];

        let (status, body) = route("/caps", &caps);
        assert_eq!(status, 200);
        let list: CapabilitiesResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(list.caps.len(), 1);
        assert_eq!(list.caps[0].id, "green-machine-midi");

        let (status, body) = route("/caps/green-machine-midi", &caps);
        assert_eq!(status, 200);
        let one: CapabilityDescriptor = serde_json::from_slice(&body).unwrap();
        assert_eq!(one.kind, "midi");

        let (status, body) = route("/caps/nope", &caps);
        assert_eq!(status, 404);
        assert!(String::from_utf8_lossy(&body).contains("no-such-capability"));

        assert_eq!(route("/other", &caps).0, 404);
    }

    #[test]
    fn http_response_is_well_formed() {
        let body = br#"{"caps":[]}"#;
        let resp = http_response(200, body);
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 11\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with(r#"{"caps":[]}"#));
    }

    /// The full `serve` accept loop answers a `fetch_capability` over loopback TCP — the
    /// browsing half hits the serving half end-to-end through the real endpoint.
    #[tokio::test]
    async fn serve_answers_a_fetch() {
        use std::net::Ipv4Addr;
        use std::sync::Arc;
        use tokio::net::TcpListener;

        struct Fixed(Vec<CapabilityDescriptor>);
        impl CapabilityProvider for Fixed {
            async fn caps(&self) -> Vec<CapabilityDescriptor> {
                self.0.clone()
            }
        }

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let provider = Arc::new(Fixed(vec![cap("green-machine-midi", "midi")]));
        tokio::spawn(async move { serve(listener, provider).await });

        // /caps/<id>
        let one = super::fetch_capability(
            Ipv4Addr::LOCALHOST.into(),
            port,
            "/caps/green-machine-midi",
        )
        .await
        .unwrap();
        assert_eq!(one.id, "green-machine-midi");

        // /caps
        let all = super::fetch_all(Ipv4Addr::LOCALHOST.into(), port)
            .await
            .unwrap();
        assert_eq!(all.caps.len(), 1);

        // unknown id → 404 → Http error
        let err = super::fetch_capability(Ipv4Addr::LOCALHOST.into(), port, "/caps/nope")
            .await
            .unwrap_err();
        assert!(matches!(err, super::MeshError::Http { status: 404 }));
    }

    /// The server core frames a response the client half parses back (end-to-end over
    /// loopback TCP), proving the two halves of docs/MESH-PROTOCOL.md §3 interoperate.
    #[tokio::test]
    async fn server_core_and_client_round_trip() {
        use std::net::Ipv4Addr;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let caps = vec![cap("green-machine-midi", "midi")];
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).await.unwrap();
            let path = parse_get_path(&buf[..n]).unwrap();
            let (status, body) = route(&path, &caps);
            stream
                .write_all(&http_response(status, &body))
                .await
                .unwrap();
            stream.flush().await.unwrap();
        });

        let got = super::fetch_capability(
            Ipv4Addr::LOCALHOST.into(),
            port,
            "/caps/green-machine-midi",
        )
        .await
        .unwrap();
        assert_eq!(got.id, "green-machine-midi");
        assert_eq!(got.kind, "midi");
    }
}
