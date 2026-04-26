//! local http server for receiving the openai codex oauth callback
//!
//! the codex flow registers `http://localhost:1455/auth/callback` as the
//! redirect uri. after the user authorises in their browser, openai
//! redirects to that url with `code` and `state` query params. we run a
//! short-lived single-request http server here to receive that callback
//! automatically (so the user does not have to copy/paste a code).

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{OAuthError, PkceChallenge};

const DEFAULT_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_BUF_LIMIT: usize = 16 * 1024;

/// query params extracted from an oauth callback request
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// parse the request line "METHOD PATH HTTP/x.y" and return (method, path)
fn parse_request_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    Some((method, target))
}

/// split a request target into its path and parsed callback params
fn parse_request_target(target: &str) -> (&str, CallbackParams) {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    let mut params = CallbackParams::default();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
        let value = urlencoding::decode(raw)
            .map(|s| s.into_owned())
            .unwrap_or_else(|_| raw.to_string());
        match key {
            "code" => params.code = Some(value),
            "state" => params.state = Some(value),
            "error" => params.error = Some(value),
            "error_description" => params.error_description = Some(value),
            _ => {}
        }
    }

    (path, params)
}

/// build a small HTTP/1.1 response with the given status and html body
fn build_html_response(status_line: &str, body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 128);
    out.extend_from_slice(format!("HTTP/1.1 {status_line}\r\n").as_bytes());
    out.extend_from_slice(b"Content-Type: text/html; charset=utf-8\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body.as_bytes());
    out
}

const SUCCESS_BODY: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>Sign-in complete</title>
    <style>
      body {
        font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto,
          Helvetica, Arial, sans-serif;
        background: #0b0c0d;
        color: #f5f5f5;
        display: flex;
        align-items: center;
        justify-content: center;
        height: 100vh;
        margin: 0;
      }
      .card {
        max-width: 480px;
        padding: 32px;
        border-radius: 12px;
        background: #161718;
        text-align: center;
      }
      h1 { margin: 0 0 12px; font-size: 24px; }
      p { margin: 0; color: #a8a8a8; line-height: 1.5; }
    </style>
  </head>
  <body>
    <div class="card">
      <h1>Signed in to mush</h1>
      <p>You can close this window and return to your terminal.</p>
    </div>
  </body>
</html>
"#;
const ERROR_BODY_PREFIX: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Sign-in failed</title></head><body><h1>Sign-in failed</h1><pre>";
const ERROR_BODY_SUFFIX: &str = "</pre></body></html>";

fn render_error_body(message: &str) -> String {
    let escaped = html_escape(message);
    format!("{ERROR_BODY_PREFIX}{escaped}{ERROR_BODY_SUFFIX}")
}

fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// outcome of inspecting a single incoming request
enum RequestOutcome {
    /// the callback was received - exchange tokens and finish
    Callback(CallbackParams),
    /// non-callback path (favicon, etc) - send 404 and keep listening
    NotFound,
}

/// classify a raw request buffer
fn classify_request(buf: &[u8]) -> Option<RequestOutcome> {
    let text = std::str::from_utf8(buf).ok()?;
    let line = text.lines().next()?;
    let (_, target) = parse_request_line(line)?;
    let (path, params) = parse_request_target(target);
    if path == CALLBACK_PATH {
        Some(RequestOutcome::Callback(params))
    } else {
        Some(RequestOutcome::NotFound)
    }
}

/// running login callback server. holds the bound listener and the auth
/// url that the user should visit in their browser. call
/// [`Self::await_callback`] to drive the server to completion.
pub struct CodexCallbackServer {
    listener: TcpListener,
    port: u16,
    pkce: PkceChallenge,
    state: String,
}

impl CodexCallbackServer {
    /// bind localhost:1455 (or fall back to an ephemeral port)
    pub async fn bind(pkce: PkceChallenge, state: String) -> Result<Self, OAuthError> {
        Self::bind_on_port(DEFAULT_PORT, pkce, state).await
    }

    /// bind a specific port (used for tests). pass 0 for an ephemeral
    /// port. on `AddrInUse` for a non-zero port, falls back to ephemeral.
    pub async fn bind_on_port(
        port: u16,
        pkce: PkceChallenge,
        state: String,
    ) -> Result<Self, OAuthError> {
        let listener = bind_listener(port).await?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
        Ok(Self {
            listener,
            port,
            pkce,
            state,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}/auth/callback", self.port)
    }

    pub fn pkce(&self) -> &PkceChallenge {
        &self.pkce
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    /// accept connections until we receive the callback. returns the parsed
    /// callback params once the matching request arrives.
    pub async fn await_callback(self) -> Result<(CallbackParams, ResponseWriter), OAuthError> {
        loop {
            let (mut stream, _) = self.listener.accept().await.map_err(OAuthError::Io)?;
            let buf = match read_request(&mut stream).await {
                Ok(buf) => buf,
                Err(_) => continue,
            };

            match classify_request(&buf) {
                Some(RequestOutcome::Callback(params)) => {
                    if params.state.as_deref() != Some(self.state.as_str()) {
                        let body = render_error_body("oauth state mismatch");
                        let response = build_html_response("400 Bad Request", &body);
                        let _ = stream.write_all(&response).await;
                        let _ = stream.shutdown().await;
                        return Err(OAuthError::TokenExchange(
                            "oauth state mismatch, please retry login".into(),
                        ));
                    }
                    return Ok((params, ResponseWriter { stream }));
                }
                Some(RequestOutcome::NotFound) | None => {
                    let response = build_html_response("404 Not Found", "not found");
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                }
            }
        }
    }
}

/// holds the still-open browser connection so we can write a success or
/// error page after the token exchange completes
pub struct ResponseWriter {
    stream: TcpStream,
}

impl ResponseWriter {
    pub async fn write_success(mut self) -> io::Result<()> {
        let response = build_html_response("200 OK", SUCCESS_BODY);
        self.stream.write_all(&response).await?;
        self.stream.shutdown().await
    }

    pub async fn write_error(mut self, message: &str) -> io::Result<()> {
        let body = render_error_body(message);
        let response = build_html_response("500 Internal Server Error", &body);
        self.stream.write_all(&response).await?;
        self.stream.shutdown().await
    }
}

async fn bind_listener(port: u16) -> Result<TcpListener, OAuthError> {
    let primary: SocketAddr = ([127, 0, 0, 1], port).into();
    match TcpListener::bind(primary).await {
        Ok(l) => Ok(l),
        Err(err) if port != 0 && err.kind() == io::ErrorKind::AddrInUse => {
            let fallback: SocketAddr = ([127, 0, 0, 1], 0).into();
            TcpListener::bind(fallback).await.map_err(OAuthError::Io)
        }
        Err(err) => Err(OAuthError::Io(err)),
    }
}

async fn read_request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut chunk))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "request read timed out"))??;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > REQUEST_BUF_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::generate_pkce;

    #[test]
    fn parse_request_line_basic() {
        let line = "GET /auth/callback?code=x&state=y HTTP/1.1";
        assert_eq!(
            parse_request_line(line),
            Some(("GET", "/auth/callback?code=x&state=y"))
        );
    }

    #[test]
    fn parse_request_line_missing_target() {
        assert_eq!(parse_request_line("GET"), None);
    }

    #[test]
    fn parse_request_target_extracts_callback_params() {
        let (path, params) =
            parse_request_target("/auth/callback?code=abc&state=xyz&error=&extra=ignored");
        assert_eq!(path, "/auth/callback");
        assert_eq!(params.code.as_deref(), Some("abc"));
        assert_eq!(params.state.as_deref(), Some("xyz"));
        assert_eq!(params.error.as_deref(), Some(""));
    }

    #[test]
    fn parse_request_target_url_decodes_values() {
        let (_, params) = parse_request_target("/auth/callback?error_description=needs%20setup");
        assert_eq!(params.error_description.as_deref(), Some("needs setup"));
    }

    #[test]
    fn parse_request_target_no_query() {
        let (path, params) = parse_request_target("/favicon.ico");
        assert_eq!(path, "/favicon.ico");
        assert_eq!(params, CallbackParams::default());
    }

    #[test]
    fn classify_request_routes_callback() {
        let req = b"GET /auth/callback?code=abc&state=xyz HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let outcome = classify_request(req).expect("classified");
        match outcome {
            RequestOutcome::Callback(params) => {
                assert_eq!(params.code.as_deref(), Some("abc"));
                assert_eq!(params.state.as_deref(), Some("xyz"));
            }
            _ => panic!("expected Callback"),
        }
    }

    #[test]
    fn classify_request_other_path_is_not_found() {
        let req = b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let outcome = classify_request(req).expect("classified");
        assert!(matches!(outcome, RequestOutcome::NotFound));
    }

    #[test]
    fn build_html_response_includes_headers_and_body() {
        let response = build_html_response("200 OK", "<p>hi</p>");
        let text = std::str::from_utf8(&response).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 9\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with("<p>hi</p>"));
    }

    #[test]
    fn render_error_body_escapes_html() {
        let body = render_error_body("<bad>\"x\"");
        assert!(body.contains("&lt;bad&gt;"));
        assert!(body.contains("&quot;x&quot;"));
    }

    #[tokio::test]
    async fn server_receives_callback_and_writes_success() {
        let pkce = generate_pkce().expect("pkce");
        let state = "test-state".to_string();
        let server = CodexCallbackServer::bind_on_port(0, pkce, state.clone())
            .await
            .expect("bind ephemeral port");
        let port = server.port();
        assert!(server.redirect_uri().contains(&format!(":{port}")));

        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let req = format!(
                "GET /auth/callback?code=abc123&state={state} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n",
            );
            stream.write_all(req.as_bytes()).await.expect("write");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.expect("read");
            buf
        });

        let (params, writer) = server.await_callback().await.expect("await callback");
        assert_eq!(params.code.as_deref(), Some("abc123"));
        assert_eq!(params.state.as_deref(), Some("test-state"));

        writer.write_success().await.expect("write success");

        let response = client.await.expect("client task");
        let text = String::from_utf8(response).expect("utf8");
        assert!(text.contains("HTTP/1.1 200 OK"), "response: {text}");
        assert!(text.contains("Signed in to mush"));
    }

    #[tokio::test]
    async fn server_rejects_state_mismatch() {
        let pkce = generate_pkce().expect("pkce");
        let server = CodexCallbackServer::bind_on_port(0, pkce, "expected".into())
            .await
            .expect("bind");
        let port = server.port();

        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let req = format!(
                "GET /auth/callback?code=abc&state=wrong HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n",
            );
            stream.write_all(req.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            let _ = stream.read_to_end(&mut buf).await;
            buf
        });

        let result = server.await_callback().await;
        assert!(matches!(result, Err(OAuthError::TokenExchange(_))));

        let body = String::from_utf8(client.await.unwrap()).unwrap();
        assert!(body.contains("400 Bad Request"));
        assert!(body.contains("oauth state mismatch"));
    }

    #[tokio::test]
    async fn server_serves_404_for_other_paths_then_handles_callback() {
        let pkce = generate_pkce().expect("pkce");
        let server = CodexCallbackServer::bind_on_port(0, pkce, "s".into())
            .await
            .expect("bind");
        let port = server.port();

        let client = tokio::spawn(async move {
            // first request hits favicon
            let mut s1 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s1.write_all(
                format!(
                    "GET /favicon.ico HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            let mut b1 = Vec::new();
            s1.read_to_end(&mut b1).await.unwrap();
            assert!(String::from_utf8_lossy(&b1).contains("404 Not Found"));

            // then real callback
            let mut s2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s2.write_all(
                format!(
                    "GET /auth/callback?code=c&state=s HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            let mut b2 = Vec::new();
            let _ = s2.read_to_end(&mut b2).await;
        });

        let (params, writer) = server.await_callback().await.expect("await");
        assert_eq!(params.code.as_deref(), Some("c"));
        writer.write_success().await.unwrap();
        client.await.unwrap();
    }
}
