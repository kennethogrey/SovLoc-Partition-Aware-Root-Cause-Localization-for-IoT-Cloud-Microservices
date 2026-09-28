//! Minimal, dependency-free HTTP/1.1 for the wide-area (Crane Cloud) tier only.
//!
//! The managed platform exposes an HTTP(S) ingress rather than a raw TCP port,
//! so the wide-area cloud collector needs to receive telemetry over HTTP. The
//! local process mesh and the Docker mesh keep using the raw-TCP telemetry path
//! in `main.rs` unchanged; nothing here runs unless a `--http-*` flag is set.
//!
//! TLS is terminated by the platform ingress, so the in-container server speaks
//! plain HTTP/1.1 and the client speaks plain HTTP to an `http://` URL. Only
//! `Content-Length` bodies are used (no chunked transfer), which is all the
//! SovLoc client ever sends, so the parser stays tiny and obvious.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// A parsed HTTP request: only the method, path, and body the collector needs.
pub struct Req {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

/// Read one HTTP/1.1 request from a client connection. Returns `None` on an
/// empty or malformed request. Reads exactly `Content-Length` body bytes.
pub async fn read_request(stream: &mut TcpStream) -> Option<Req> {
    let mut reader = BufReader::new(&mut *stream);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.ok()? == 0 {
        return None; // peer closed with nothing
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // blank line ends the headers
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body).await.is_err() {
        return None;
    }
    Some(Req { method, path, body })
}

/// Write a minimal HTTP/1.1 response, keeping the connection open so a streaming
/// client can reuse it (the server loop breaks when the client closes). Every
/// response carries an explicit `Content-Length`, so a keep-alive client always
/// knows where the response ends.
pub async fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: keep-alive\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(msg.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Read and discard one HTTP response (status line, headers, and a
/// `Content-Length` body) so a keep-alive connection can be reused for the next
/// request. Errors when the peer closed, so the caller reconnects.
async fn read_response(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(&mut *stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "peer closed",
        ));
    }
    let mut content_length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).await? == 0 {
            break;
        }
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((name, value)) = t.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).await?;
    }
    Ok(())
}

/// A streaming HTTP/1.1 client that reuses one keep-alive connection for many
/// POSTs, so cross-link telemetry costs one round trip per sample instead of a
/// fresh TCP handshake each time. This mirrors the persistent connection the
/// raw-TCP telemetry path already uses, keeping the edge and cloud transports on
/// an equal footing. Best effort: on any error the connection is dropped and the
/// next `post` reconnects.
pub struct HttpSender {
    host: String,
    port: u16,
    base: String,
    conn: Option<TcpStream>,
}

impl HttpSender {
    /// Build a sender for a plain `http://host[:port][/base]` URL, or `None` if
    /// the URL is not plain http (an https URL needs TLS the platform terminates
    /// at its ingress; point at the platform's http endpoint instead).
    pub fn new(base_url: &str) -> Option<Self> {
        let (host, port, base) = parse_http_url(base_url)?;
        Some(Self {
            host,
            port,
            base,
            conn: None,
        })
    }

    /// POST a JSON body to `<base><path>` over the reused connection.
    pub async fn post(&mut self, path: &str, body: &str) -> std::io::Result<()> {
        if self.conn.is_none() {
            self.conn = Some(TcpStream::connect((self.host.as_str(), self.port)).await?);
        }
        let target = format!("{}{}", self.base.trim_end_matches('/'), path);
        let req = format!(
            "POST {target} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
            self.host,
            body.len()
        );
        let result = async {
            let s = self.conn.as_mut().unwrap();
            s.write_all(req.as_bytes()).await?;
            s.flush().await?;
            read_response(s).await
        }
        .await;
        if result.is_err() {
            self.conn = None; // force a reconnect on the next call
        }
        result
    }
}

/// Parse `http://host[:port][/base]` into `(host, port, base_path)`. Returns
/// `None` for anything that is not plain `http://` (an `https://` URL needs TLS,
/// which the platform terminates at its ingress, so point the client at the
/// platform's `http://` endpoint or the raw-TCP path instead).
pub fn parse_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, base) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 80u16),
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port, base.to_string()))
}

/// POST a JSON body to `<base_url><path>` over plain HTTP/1.1, best effort.
/// One connection per call (`Connection: close`), which matches the low SovLoc
/// report rate and keeps the client stateless behind the platform ingress.
pub async fn post_json(base_url: &str, path: &str, body: &str) -> std::io::Result<()> {
    let (host, port, base) = parse_http_url(base_url).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("expected an http:// url, got: {base_url}"),
        )
    })?;
    let target = format!("{}{}", base.trim_end_matches('/'), path);

    let mut stream = TcpStream::connect((host.as_str(), port)).await?;
    let req = format!(
        "POST {target} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    // Drain the response briefly so the ingress sees a clean close. Bounded so a
    // slow or blackholed peer cannot wedge the caller.
    let mut buf = [0u8; 512];
    let _ = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await;
    Ok(())
}
