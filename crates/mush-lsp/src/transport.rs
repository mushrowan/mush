//! JSON-RPC transport over stdio with Content-Length framing
//!
//! the LSP protocol uses Content-Length headers to frame messages:
//! ```text
//! Content-Length: 42\r\n
//! \r\n
//! {"jsonrpc":"2.0","id":1,"method":"..."}
//! ```

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::error::LspError;

/// read a single JSON-RPC message from the server's stdout
pub async fn read_message<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<serde_json::Value, LspError> {
    let content_length = read_headers(reader).await?;
    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf).await.map_err(|e| {
        LspError::Transport(format!("failed to read message body: {e}"))
    })?;
    serde_json::from_slice(&buf).map_err(|e| {
        LspError::Transport(format!("invalid JSON in message body: {e}"))
    })
}

/// write a JSON-RPC message to the server's stdin
pub async fn write_message(
    writer: &mut tokio::process::ChildStdin,
    message: &serde_json::Value,
) -> Result<(), LspError> {
    let body = serde_json::to_string(message).map_err(|e| {
        LspError::Transport(format!("failed to serialise message: {e}"))
    })?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await.map_err(|e| {
        LspError::Transport(format!("failed to write header: {e}"))
    })?;
    writer.write_all(body.as_bytes()).await.map_err(|e| {
        LspError::Transport(format!("failed to write body: {e}"))
    })?;
    writer.flush().await.map_err(|e| {
        LspError::Transport(format!("failed to flush: {e}"))
    })?;
    Ok(())
}

/// parse Content-Length from headers, returns the body size
async fn read_headers<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<usize, LspError> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.map_err(|e| {
            LspError::Transport(format!("failed to read header line: {e}"))
        })?;
        if n == 0 {
            return Err(LspError::ServerExited);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse().map_err(|_| {
                LspError::Transport(format!("invalid Content-Length: {}", value.trim()))
            })?);
        }
    }
    content_length.ok_or_else(|| {
        LspError::Transport("missing Content-Length header".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_message_parses_framed_json() {
        let data = b"Content-Length: 24\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1}";
        let mut reader = BufReader::new(&data[..]);
        let msg = read_message(&mut reader).await.unwrap();
        assert_eq!(msg["jsonrpc"], "2.0");
        assert_eq!(msg["id"], 1);
    }

    #[tokio::test]
    async fn read_message_eof_returns_server_exited() {
        let data = b"";
        let mut reader = BufReader::new(&data[..]);
        let err = read_message(&mut reader).await.unwrap_err();
        assert!(matches!(err, LspError::ServerExited));
    }

    #[tokio::test]
    async fn read_message_missing_content_length() {
        let data = b"X-Custom: foo\r\n\r\n{}";
        let mut reader = BufReader::new(&data[..]);
        let err = read_message(&mut reader).await.unwrap_err();
        assert!(matches!(err, LspError::Transport(_)));
    }

    #[tokio::test]
    async fn read_headers_handles_extra_headers() {
        let data = b"Content-Type: utf-8\r\nContent-Length: 2\r\n\r\n{}";
        let mut reader = BufReader::new(&data[..]);
        let msg = read_message(&mut reader).await.unwrap();
        assert!(msg.is_object());
    }
}
