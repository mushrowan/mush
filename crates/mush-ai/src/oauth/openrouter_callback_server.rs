//! local http server for receiving the openrouter PKCE callback
//!
//! parallels [`crate::oauth::openai_codex_server`] but for openrouter's
//! flavour of the PKCE redirect. the wire format differs in two small
//! ways:
//!
//! 1. openrouter doesn't echo a `state` param back, so we drop the
//!    state-mismatch validation; PKCE alone is enough to bind the
//!    callback to this process.
//! 2. the success / error html and parsing helpers are otherwise
//!    identical to the codex server, but kept separate here to avoid
//!    a refactor that touches the existing flow. dedup is a candidate
//!    for a follow-up once a third callback shows up
//!
//! the user-visible result is the same: their browser lands on a small
//! "Signed in to mush" page after the redirect, and the listener
//! returns the parsed `code` so the caller can hand it to
//! `openrouter_rs::api::auth::exchange_code_for_api_key`.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::OAuthError;

const DEFAULT_PORT: u16 = 1456;
const CALLBACK_PATH: &str = "/auth/callback";
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_BUF_LIMIT: usize = 16 * 1024;

/// query params extracted from an openrouter callback request
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

fn parse_request_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    Some((method, target))
}

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
            "error" => params.error = Some(value),
            "error_description" => params.error_description = Some(value),
            _ => {}
        }
    }

    (path, params)
}

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

enum RequestOutcome {
    Callback(CallbackParams),
    NotFound,
}

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

/// running login callback server. holds the bound listener and lets
/// callers drive completion via [`Self::await_callback`]
pub struct OpenRouterCallbackServer {
    listener: TcpListener,
    port: u16,
}

impl OpenRouterCallbackServer {
    /// bind localhost:1456 (or fall back to an ephemeral port)
    pub async fn bind() -> Result<Self, OAuthError> {
        Self::bind_on_port(DEFAULT_PORT).await
    }

    pub async fn bind_on_port(port: u16) -> Result<Self, OAuthError> {
        let listener = bind_listener(port).await?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
        Ok(Self { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}/auth/callback", self.port)
    }

    /// accept connections until we receive the callback. returns the
    /// parsed callback params plus a [`ResponseWriter`] so the caller
    /// can paint a success/failure page after the token exchange
    pub async fn await_callback(self) -> Result<(CallbackParams, ResponseWriter), OAuthError> {
        loop {
            let (mut stream, _) = self.listener.accept().await.map_err(OAuthError::Io)?;
            let buf = match read_request(&mut stream).await {
                Ok(buf) => buf,
                Err(_) => continue,
            };

            match classify_request(&buf) {
                Some(RequestOutcome::Callback(params)) => {
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

    #[test]
    fn parse_request_target_extracts_code() {
        let (path, params) = parse_request_target("/auth/callback?code=abc&extra=ignored");
        assert_eq!(path, "/auth/callback");
        assert_eq!(params.code.as_deref(), Some("abc"));
        assert!(params.error.is_none());
    }

    #[test]
    fn parse_request_target_url_decodes_error_description() {
        let (_, params) =
            parse_request_target("/auth/callback?error=denied&error_description=user%20cancelled");
        assert_eq!(params.error.as_deref(), Some("denied"));
        assert_eq!(params.error_description.as_deref(), Some("user cancelled"));
    }

    #[test]
    fn classify_request_routes_callback() {
        let req = b"GET /auth/callback?code=abc HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let outcome = classify_request(req).expect("classified");
        match outcome {
            RequestOutcome::Callback(params) => {
                assert_eq!(params.code.as_deref(), Some("abc"));
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

    #[tokio::test]
    async fn bind_on_port_falls_back_to_ephemeral_when_taken() {
        // simulate a colliding listener: bind explicit port, then ask
        // the helper to bind the same port. it should silently fall
        // back to an ephemeral port rather than erroring out
        let primary = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let busy_port = primary.local_addr().unwrap().port();
        let server = OpenRouterCallbackServer::bind_on_port(busy_port)
            .await
            .expect("fallback bind should succeed");
        assert_ne!(server.port(), busy_port);
        assert!(server.redirect_uri().starts_with("http://localhost:"));
    }
}
