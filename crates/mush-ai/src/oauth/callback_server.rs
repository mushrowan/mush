use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::OAuthError;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_BUF_LIMIT: usize = 16 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

impl CallbackParams {
    #[must_use]
    pub fn error_detail(&self) -> Option<String> {
        let error = self.error.clone()?;
        Some(match &self.error_description {
            Some(description) => format!("{error}: {description}"),
            None => error,
        })
    }
}

#[derive(Debug, Clone)]
pub struct CallbackConfig {
    pub path: String,
    pub port: u16,
    pub expected_state: Option<String>,
}

impl CallbackConfig {
    #[must_use]
    pub fn new(path: impl Into<String>, port: u16) -> Self {
        Self {
            path: path.into(),
            port,
            expected_state: None,
        }
    }

    #[must_use]
    pub fn expecting_state(mut self, state: impl Into<String>) -> Self {
        self.expected_state = Some(state.into());
        self
    }
}

pub struct CallbackServer {
    listener: TcpListener,
    port: u16,
    config: CallbackConfig,
}

impl CallbackServer {
    pub async fn bind(config: CallbackConfig) -> Result<Self, OAuthError> {
        let listener = bind_listener(config.port).await?;
        let port = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(config.port);
        Ok(Self {
            listener,
            port,
            config,
        })
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    #[must_use]
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}{}", self.port, self.config.path)
    }

    pub async fn await_callback(self) -> Result<(CallbackParams, ResponseWriter), OAuthError> {
        loop {
            let (mut stream, _) = self.listener.accept().await.map_err(OAuthError::Io)?;
            let buf = match read_request(&mut stream).await {
                Ok(buf) => buf,
                Err(_) => continue,
            };

            match classify_request(&buf, &self.config.path) {
                Some(RequestOutcome::Callback(params)) => {
                    if let Some(expected) = self.config.expected_state.as_deref()
                        && params.state.as_deref() != Some(expected)
                    {
                        let body = render_error_body("oauth state mismatch");
                        let _ = respond(&mut stream, "400 Bad Request", &body).await;
                        return Err(OAuthError::TokenExchange(
                            "oauth state mismatch, please retry login".into(),
                        ));
                    }
                    return Ok((params, ResponseWriter { stream }));
                }
                Some(RequestOutcome::NotFound) | None => {
                    let _ = respond(&mut stream, "404 Not Found", "not found").await;
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
        respond(&mut self.stream, "200 OK", SUCCESS_BODY).await
    }

    pub async fn write_error(mut self, message: &str) -> io::Result<()> {
        let body = render_error_body(message);
        respond(&mut self.stream, "500 Internal Server Error", &body).await
    }
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    stream.write_all(&build_html_response(status, body)).await?;
    stream.shutdown().await
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
            "state" => params.state = Some(value),
            "error" => params.error = Some(value),
            "error_description" => params.error_description = Some(value),
            _ => {}
        }
    }

    (path, params)
}

enum RequestOutcome {
    Callback(CallbackParams),
    NotFound,
}

fn classify_request(buf: &[u8], callback_path: &str) -> Option<RequestOutcome> {
    let text = std::str::from_utf8(buf).ok()?;
    let line = text.lines().next()?;
    let (_, target) = parse_request_line(line)?;
    let (path, params) = parse_request_target(target);
    if path == callback_path {
        Some(RequestOutcome::Callback(params))
    } else {
        Some(RequestOutcome::NotFound)
    }
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

    const CALLBACK_PATH: &str = "/auth/callback";

    fn config() -> CallbackConfig {
        CallbackConfig::new(CALLBACK_PATH, 0)
    }

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
        assert_eq!(path, CALLBACK_PATH);
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
    fn error_detail_joins_code_and_description() {
        let params = CallbackParams {
            error: Some("access_denied".into()),
            error_description: Some("user cancelled".into()),
            ..CallbackParams::default()
        };
        assert_eq!(
            params.error_detail().as_deref(),
            Some("access_denied: user cancelled")
        );

        let bare = CallbackParams {
            error: Some("access_denied".into()),
            ..CallbackParams::default()
        };
        assert_eq!(bare.error_detail().as_deref(), Some("access_denied"));
        assert!(CallbackParams::default().error_detail().is_none());
    }

    #[test]
    fn classify_request_matches_the_configured_path_only() {
        let req = b"GET /callback?code=abc HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert!(matches!(
            classify_request(req, "/callback"),
            Some(RequestOutcome::Callback(_))
        ));
        assert!(matches!(
            classify_request(req, "/auth/callback"),
            Some(RequestOutcome::NotFound)
        ));
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

    async fn send_request(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let req = format!(
            "GET {target} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n",
        );
        stream.write_all(req.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    async fn server_receives_callback_and_writes_success() {
        let server = CallbackServer::bind(config().expecting_state("test-state"))
            .await
            .expect("bind ephemeral port");
        let port = server.port();
        assert_eq!(
            server.redirect_uri(),
            format!("http://localhost:{port}/auth/callback")
        );

        let client = tokio::spawn(async move {
            send_request(port, "/auth/callback?code=abc123&state=test-state").await
        });

        let (params, writer) = server.await_callback().await.expect("await callback");
        assert_eq!(params.code.as_deref(), Some("abc123"));
        assert_eq!(params.state.as_deref(), Some("test-state"));

        writer.write_success().await.expect("write success");

        let response = client.await.expect("client task");
        assert!(response.contains("HTTP/1.1 200 OK"), "response: {response}");
        assert!(response.contains("Signed in to mush"));
    }

    #[tokio::test]
    async fn server_rejects_state_mismatch() {
        let server = CallbackServer::bind(config().expecting_state("expected"))
            .await
            .expect("bind");
        let port = server.port();

        let client =
            tokio::spawn(
                async move { send_request(port, "/auth/callback?code=abc&state=wrong").await },
            );

        let result = server.await_callback().await;
        assert!(matches!(result, Err(OAuthError::TokenExchange(_))));

        let body = client.await.unwrap();
        assert!(body.contains("400 Bad Request"));
        assert!(body.contains("oauth state mismatch"));
    }

    #[tokio::test]
    async fn server_without_expected_state_accepts_stateless_callback() {
        let server = CallbackServer::bind(config()).await.expect("bind");
        let port = server.port();

        let client =
            tokio::spawn(async move { send_request(port, "/auth/callback?code=abc").await });

        let (params, writer) = server.await_callback().await.expect("await callback");
        assert_eq!(params.code.as_deref(), Some("abc"));
        assert!(params.state.is_none());
        writer.write_success().await.expect("write success");
        client.await.unwrap();
    }

    #[tokio::test]
    async fn server_serves_404_for_other_paths_then_handles_callback() {
        let server = CallbackServer::bind(config()).await.expect("bind");
        let port = server.port();

        let client = tokio::spawn(async move {
            let favicon = send_request(port, "/favicon.ico").await;
            assert!(favicon.contains("404 Not Found"));
            send_request(port, "/auth/callback?code=c").await;
        });

        let (params, writer) = server.await_callback().await.expect("await");
        assert_eq!(params.code.as_deref(), Some("c"));
        writer.write_success().await.unwrap();
        client.await.unwrap();
    }

    #[tokio::test]
    async fn bind_falls_back_to_ephemeral_when_the_port_is_taken() {
        let primary = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let busy_port = primary.local_addr().unwrap().port();
        let server = CallbackServer::bind(CallbackConfig::new(CALLBACK_PATH, busy_port))
            .await
            .expect("fallback bind should succeed");
        assert_ne!(server.port(), busy_port);
        assert!(server.redirect_uri().starts_with("http://localhost:"));
    }
}
