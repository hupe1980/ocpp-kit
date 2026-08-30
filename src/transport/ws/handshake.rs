//! The HTTP/1.1 upgrade handshake.
//!
//! Performed here rather than delegated, because OCPP puts requirements on it that a generic
//! WebSocket library's callback cannot meet: authentication is a database lookup and so must
//! be `async`; an unknown Charging Station identity is answered with **404** and bad
//! credentials with **401** (Part 4 §3.1.1); and a client whose subprotocols the server cannot
//! speak gets a *successful* handshake with no `Sec-WebSocket-Protocol` header followed by an
//! immediate close.

use std::fmt;

use base64::Engine as _;
use sha1::{Digest as _, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::types::Identity;
use crate::version::Version;

use super::super::TransportError;

/// The GUID RFC 6455 §1.3 concatenates with the client key.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The maximum size of an HTTP head we will read before giving up.
///
/// A handshake that has not finished in 16 KiB is not a handshake.
const MAX_HEAD: usize = 16 * 1024;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// `Sec-WebSocket-Accept` for a given `Sec-WebSocket-Key`.
#[must_use]
pub(crate) fn accept_key(key: &str) -> String {
    let mut hash = Sha1::new();
    hash.update(key.as_bytes());
    hash.update(WS_GUID.as_bytes());
    B64.encode(hash.finalize())
}

/// A fresh, unpredictable `Sec-WebSocket-Key`.
///
/// The key is what stops a cached or confused intermediary from splicing a stale handshake
/// response into this connection, so there is no safe fallback if the entropy source fails:
/// a predictable key is worse than no connection.
pub(crate) fn client_key() -> Result<String, TransportError> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| {
        TransportError::Configuration(format!("no entropy for a Sec-WebSocket-Key: {error}"))
    })?;
    Ok(B64.encode(nonce))
}

/// A parsed HTTP head: a request line or status line, plus lower-cased header names.
#[derive(Debug)]
pub(crate) struct Head {
    /// For a request, the path; for a response, the status code as text.
    pub(crate) target: String,
    pub(crate) headers: Vec<(String, String)>,
    /// Bytes that arrived after the head — the first WebSocket frames.
    pub(crate) rest: Vec<u8>,
}

impl Head {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// A list-valued header, with every occurrence joined.
    ///
    /// RFC 9110 §5.3 lets a sender split a comma-separated header across several lines, and
    /// proxies do — reading only the first would silently lose half of a client's
    /// `Sec-WebSocket-Protocol` offer.
    pub(crate) fn header_list(&self, name: &str) -> Option<String> {
        let mut joined: Option<String> = None;
        for (key, value) in &self.headers {
            if key != name {
                continue;
            }
            match &mut joined {
                Some(out) => {
                    out.push(',');
                    out.push_str(value);
                }
                None => joined = Some(value.clone()),
            }
        }
        joined
    }

    /// Whether a comma-separated header lists `token`, case-insensitively.
    fn header_lists(&self, name: &str, token: &str) -> bool {
        self.header_list(name).is_some_and(|value| {
            value
                .split(',')
                .any(|item| item.trim().eq_ignore_ascii_case(token))
        })
    }
}

/// Reads an HTTP head from the socket.
pub(crate) async fn read_head<S>(socket: &mut S) -> Result<Head, TransportError>
where
    S: AsyncRead + Unpin,
{
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let end = loop {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Err(TransportError::Closed);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_head_end(&buffer) {
            break index;
        }
        if buffer.len() > MAX_HEAD {
            return Err(TransportError::Configuration(
                "the HTTP head is too large".into(),
            ));
        }
    };

    let text = String::from_utf8_lossy(&buffer[..end]);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    Ok(Head {
        target: first.to_owned(),
        headers,
        rest: buffer[end + 4..].to_vec(),
    })
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

// ---------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------

/// What a client asked for.
#[derive(Debug)]
pub(crate) struct UpgradeRequest {
    pub(crate) path: String,
    pub(crate) head: Head,
}

impl UpgradeRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.head.header(name)
    }

    /// The Charging Station identity: the last path segment, percent-decoded.
    pub(crate) fn identity(&self) -> Option<Identity> {
        let path = self.path.split(['?', '#']).next().unwrap_or(&self.path);
        let segment = path.rsplit('/').find(|segment| !segment.is_empty())?;
        Identity::new(percent_decode(segment)).ok()
    }

    /// The OCPP versions offered, in the client's preference order.
    pub(crate) fn subprotocols(&self) -> Vec<Version> {
        self.head
            .header_list("sec-websocket-protocol")
            .into_iter()
            .flat_map(|value| {
                value
                    .split(',')
                    .filter_map(|token| Version::from_subprotocol(token.trim()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// The `Sec-WebSocket-Extensions` offer, with every occurrence joined.
    #[cfg(feature = "compression")]
    pub(crate) fn extensions(&self) -> Option<String> {
        self.head.header_list("sec-websocket-extensions")
    }

    /// Whether this is a well-formed RFC 6455 upgrade.
    #[cfg(test)]
    pub(crate) fn is_websocket_upgrade(&self) -> bool {
        self.upgrade_defect().is_none()
    }

    /// The `Sec-WebSocket-Key` the response has to hash, when the request carried one.
    pub(crate) fn websocket_key(&self) -> Option<&str> {
        self.head.header("sec-websocket-key")
    }

    /// What is wrong with this upgrade request, if anything.
    ///
    /// RFC 6455 §4.2.1 lists the four things a request must carry; §4.4 says a server that
    /// does not speak the client's version answers **426** with its own
    /// `Sec-WebSocket-Version`. Closing the socket instead leaves the other end with a
    /// connection reset and no diagnosis.
    pub(crate) fn upgrade_defect(&self) -> Option<UpgradeDefect> {
        if !self.head.header_lists("upgrade", "websocket")
            || !self.head.header_lists("connection", "upgrade")
        {
            return Some(UpgradeDefect::NotAnUpgrade);
        }
        match self.head.header("sec-websocket-version") {
            Some("13") => {}
            _ => return Some(UpgradeDefect::UnsupportedVersion),
        }
        if self.head.header("sec-websocket-key").is_none() {
            return Some(UpgradeDefect::MissingKey);
        }
        None
    }
}

/// Why an upgrade request was refused, and with which status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpgradeDefect {
    /// No `Upgrade: websocket` / `Connection: Upgrade`.
    NotAnUpgrade,
    /// `Sec-WebSocket-Version` is absent or is not 13.
    UnsupportedVersion,
    /// `Sec-WebSocket-Key` is absent.
    MissingKey,
}

impl UpgradeDefect {
    /// The HTTP status and reason phrase to answer with.
    pub(crate) const fn status(self) -> (u16, &'static str) {
        match self {
            // RFC 6455 §4.4: the server names the version it does speak.
            UpgradeDefect::UnsupportedVersion => (426, "Upgrade Required"),
            UpgradeDefect::NotAnUpgrade | UpgradeDefect::MissingKey => (400, "Bad Request"),
        }
    }
}

/// Reads and validates a client's upgrade request.
pub(crate) async fn read_request<S>(socket: &mut S) -> Result<UpgradeRequest, TransportError>
where
    S: AsyncRead + Unpin,
{
    let head = read_head(socket).await?;
    let mut parts = head.target.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    if !method.eq_ignore_ascii_case("GET") {
        return Err(TransportError::Configuration(format!(
            "unexpected method {method}"
        )));
    }
    Ok(UpgradeRequest { path, head })
}

/// Writes the `101 Switching Protocols` response.
///
/// `subprotocol` is omitted when the server speaks none of the client's — which Part 4 §3.1.1
/// requires, and which the caller follows with an immediate close.
pub(crate) async fn write_accept<S>(
    socket: &mut S,
    key: &str,
    subprotocol: Option<&str>,
    extensions: Option<&str>,
) -> Result<(), TransportError>
where
    S: AsyncWrite + Unpin,
{
    let mut response = String::with_capacity(256);
    response.push_str("HTTP/1.1 101 Switching Protocols\r\n");
    response.push_str("Upgrade: websocket\r\n");
    response.push_str("Connection: Upgrade\r\n");
    response.push_str("Sec-WebSocket-Accept: ");
    response.push_str(&accept_key(key));
    response.push_str("\r\n");
    if let Some(subprotocol) = subprotocol {
        response.push_str("Sec-WebSocket-Protocol: ");
        response.push_str(subprotocol);
        response.push_str("\r\n");
    }
    if let Some(extensions) = extensions {
        response.push_str("Sec-WebSocket-Extensions: ");
        response.push_str(extensions);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

/// Writes a refusal — 401 for bad credentials, 404 for an unknown identity.
pub(crate) async fn write_refusal<S>(
    socket: &mut S,
    status: u16,
    reason: &str,
) -> Result<(), TransportError>
where
    S: AsyncWrite + Unpin,
{
    let challenge = match status {
        401 => "WWW-Authenticate: Basic realm=\"OCPP\", charset=\"UTF-8\"\r\n",
        // RFC 6455 §4.4: a 426 says which version the server does speak.
        426 => "Sec-WebSocket-Version: 13\r\n",
        _ => "",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n{challenge}Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// What a client wants from the handshake.
pub(crate) struct ClientRequest<'a> {
    pub(crate) host: &'a str,
    pub(crate) path: &'a str,
    pub(crate) subprotocols: &'a str,
    pub(crate) authorization: Option<&'a str>,
    pub(crate) extensions: Option<&'a str>,
}

/// What the server answered.
#[derive(Debug)]
pub(crate) struct ClientHandshake {
    pub(crate) subprotocol: Option<String>,
    #[cfg(feature = "compression")]
    pub(crate) extensions: Option<String>,
    /// Bytes that arrived after the response head.
    pub(crate) rest: Vec<u8>,
}

/// Performs the client half of the handshake.
pub(crate) async fn client_handshake<S>(
    socket: &mut S,
    request: &ClientRequest<'_>,
) -> Result<ClientHandshake, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let key = client_key()?;
    let mut head = String::with_capacity(512);
    head.push_str("GET ");
    head.push_str(request.path);
    head.push_str(" HTTP/1.1\r\nHost: ");
    head.push_str(request.host);
    head.push_str("\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n");
    head.push_str("Sec-WebSocket-Key: ");
    head.push_str(&key);
    head.push_str("\r\nSec-WebSocket-Protocol: ");
    head.push_str(request.subprotocols);
    head.push_str("\r\n");
    if let Some(authorization) = request.authorization {
        head.push_str("Authorization: ");
        head.push_str(authorization);
        head.push_str("\r\n");
    }
    if let Some(extensions) = request.extensions {
        head.push_str("Sec-WebSocket-Extensions: ");
        head.push_str(extensions);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await?;
    socket.flush().await?;

    let response = read_head(socket).await?;
    let status = response
        .target
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| TransportError::Configuration("malformed HTTP response".into()))?;
    if status != 101 {
        return Err(TransportError::Rejected { status });
    }
    if !response.header_lists("upgrade", "websocket")
        || !response.header_lists("connection", "upgrade")
    {
        return Err(TransportError::Configuration(
            "the server did not switch to the WebSocket protocol".into(),
        ));
    }
    // The accept key proves the response belongs to *this* handshake, and is what stops a
    // confused intermediary from splicing in a cached one.
    if response.header("sec-websocket-accept") != Some(accept_key(&key).as_str()) {
        return Err(TransportError::Configuration(
            "the server's Sec-WebSocket-Accept does not match the key that was sent".into(),
        ));
    }

    Ok(ClientHandshake {
        subprotocol: response
            .header("sec-websocket-protocol")
            .map(ToOwned::to_owned),
        #[cfg(feature = "compression")]
        extensions: response
            .header("sec-websocket-extensions")
            .map(ToOwned::to_owned),
        rest: response.rest,
    })
}

/// Decodes `%XX` escapes; leaves malformed escapes alone, since the identity is validated
/// afterwards anyway.
pub(crate) fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = core::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes everything outside the RFC 3986 unreserved set, so an identity with a space
/// or a slash still produces exactly one path segment.
#[must_use]
pub(crate) fn percent_encode(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

impl fmt::Display for UpgradeRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GET {}", self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(path: &str, extra: &[(&str, &str)]) -> UpgradeRequest {
        let mut headers = vec![
            ("upgrade".into(), "websocket".into()),
            ("connection".into(), "Upgrade".into()),
            ("sec-websocket-version".into(), "13".into()),
            (
                "sec-websocket-key".into(),
                "dGhlIHNhbXBsZSBub25jZQ==".into(),
            ),
        ];
        headers.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        UpgradeRequest {
            path: path.into(),
            head: Head {
                target: String::new(),
                headers,
                rest: Vec::new(),
            },
        }
    }

    #[test]
    fn the_accept_key_matches_the_rfc_6455_example() {
        // RFC 6455 §1.3 works this exact pair through.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn client_keys_are_16_bytes_and_do_not_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let key = client_key().expect("entropy");
            assert_eq!(B64.decode(&key).unwrap().len(), 16);
            seen.insert(key);
        }
        assert!(
            seen.len() > 60,
            "keys must be unpredictable: {} distinct",
            seen.len()
        );
    }

    #[test]
    fn the_identity_is_the_last_path_segment() {
        assert_eq!(
            request("/ocpp/CS-0001", &[]).identity().unwrap().as_str(),
            "CS-0001"
        );
        assert_eq!(
            request("/tenant-a/ocpp/station%20two", &[])
                .identity()
                .unwrap()
                .as_str(),
            "station two"
        );
        assert_eq!(
            request("/ocpp/CS-1?x=1", &[]).identity().unwrap().as_str(),
            "CS-1"
        );
        // A00.FR.204 — an identity with ':' cannot be used with HTTP Basic.
        assert!(request("/ocpp/a%3Ab", &[]).identity().is_none());
        assert!(request("/", &[]).identity().is_none());
    }

    #[test]
    fn subprotocols_are_read_in_the_clients_preference_order() {
        let request = request(
            "/ocpp/CS-1",
            &[("sec-websocket-protocol", "ocpp2.1, ocpp2.0.1, mqtt")],
        );
        assert_eq!(request.subprotocols(), vec![Version::V2_1, Version::V2_0_1]);
        assert!(request.is_websocket_upgrade());
    }

    #[test]
    fn upgrade_headers_are_checked_as_comma_separated_lists() {
        // Browsers and proxies send `Connection: keep-alive, Upgrade`.
        let request = request("/ocpp/CS-1", &[("connection", "keep-alive, Upgrade")]);
        assert!(request.is_websocket_upgrade());

        let mut broken = request;
        broken
            .head
            .headers
            .retain(|(name, _)| name != "sec-websocket-key");
        assert!(!broken.is_websocket_upgrade());
    }

    /// RFC 6455 §4.2.1 and §4.4: a server that refuses an upgrade says *why*, with a status.
    /// Dropping the socket leaves an operator with a connection reset and no diagnosis.
    #[test]
    fn a_malformed_upgrade_names_its_defect_and_its_status() {
        let mut wrong_version = request("/ocpp/CS-1", &[]);
        wrong_version
            .head
            .headers
            .retain(|(name, _)| name != "sec-websocket-version");
        wrong_version
            .head
            .headers
            .push(("sec-websocket-version".into(), "8".into()));
        assert_eq!(
            wrong_version.upgrade_defect().map(UpgradeDefect::status),
            Some((426, "Upgrade Required"))
        );

        let mut no_key = request("/ocpp/CS-1", &[]);
        no_key
            .head
            .headers
            .retain(|(name, _)| name != "sec-websocket-key");
        assert_eq!(
            no_key.upgrade_defect().map(UpgradeDefect::status),
            Some((400, "Bad Request"))
        );

        let mut plain_get = request("/ocpp/CS-1", &[]);
        plain_get.head.headers.retain(|(name, _)| name != "upgrade");
        assert_eq!(
            plain_get.upgrade_defect().map(UpgradeDefect::status),
            Some((400, "Bad Request"))
        );

        assert_eq!(request("/ocpp/CS-1", &[]).upgrade_defect(), None);
    }

    #[tokio::test]
    async fn a_426_names_the_version_the_server_speaks() {
        let mut out = Vec::new();
        write_refusal(&mut out, 426, "Upgrade Required")
            .await
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("HTTP/1.1 426 Upgrade Required\r\n"),
            "{text}"
        );
        assert!(text.contains("Sec-WebSocket-Version: 13\r\n"), "{text}");
    }

    #[test]
    fn percent_coding_round_trips() {
        for text in ["CS-0001", "station two", "a/b", "ünïcödé"] {
            assert_eq!(percent_decode(&percent_encode(text)), text);
        }
        // A malformed escape is left alone rather than dropping the segment.
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }

    #[tokio::test]
    async fn a_head_is_read_and_the_frames_behind_it_are_kept() {
        let head = b"GET /ocpp/CS-1 HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n\x81\x03abc";
        let mut socket = std::io::Cursor::new(head.to_vec());
        let request = read_request(&mut socket).await.unwrap();
        assert_eq!(request.path, "/ocpp/CS-1");
        assert!(request.is_websocket_upgrade());
        // The first WebSocket frame arrived in the same read and must not be lost.
        assert_eq!(request.head.rest, b"\x81\x03abc");
    }

    #[tokio::test]
    async fn a_head_split_across_reads_is_reassembled() {
        // A pipe delivers the head in two pieces, which is the normal case on a slow link.
        let (mut client, mut server) = tokio::io::duplex(64);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            client
                .write_all(b"GET /ocpp/CS-1 HTTP/1.1\r\nHost: x\r\n")
                .await
                .unwrap();
            client
                .write_all(
                    b"Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                      Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let request = read_request(&mut server).await.unwrap();
        assert!(request.is_websocket_upgrade());
    }
}
