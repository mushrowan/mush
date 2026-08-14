use http::Extensions;
use reqwest::{Request, Response};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware, Middleware, Next, Result};

#[derive(Clone, Copy)]
pub struct WireCallId(pub u64);

pub type HttpClient = ClientWithMiddleware;

#[must_use]
pub fn wrap(inner: reqwest::Client) -> HttpClient {
    ClientBuilder::new(inner).with(WireMiddleware).build()
}

#[must_use]
pub fn client() -> HttpClient {
    wrap(reqwest::Client::new())
}

fn header_pairs(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or("<non-utf8>").to_string(),
            )
        })
        .collect()
}

fn header_refs(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
    pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}

fn is_event_stream(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("event-stream"))
}

struct WireMiddleware;

#[async_trait::async_trait]
impl Middleware for WireMiddleware {
    async fn handle(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> Result<Response> {
        if !crate::wire_log::enabled() {
            return next.run(req, extensions).await;
        }

        let call_id = extensions
            .get::<WireCallId>()
            .map_or_else(crate::wire_log::next_call_id, |c| c.0);

        let method = req.method().as_str().to_string();
        let url = req.url().to_string();
        let req_pairs = header_pairs(req.headers());
        let req_body = req
            .body()
            .and_then(reqwest::Body::as_bytes)
            .map(|b| String::from_utf8_lossy(b).into_owned());
        crate::wire_log::log_request(
            call_id,
            &method,
            &url,
            &header_refs(&req_pairs),
            req_body.as_deref(),
        );

        let resp = next.run(req, extensions).await?;

        let status = resp.status().as_u16();
        let resp_pairs = header_pairs(resp.headers());

        if is_event_stream(resp.headers()) {
            crate::wire_log::log_response(call_id, status, &header_refs(&resp_pairs), None);
            return Ok(resp);
        }

        let version = resp.version();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await?;
        let text = String::from_utf8_lossy(&bytes);
        crate::wire_log::log_response(call_id, status, &header_refs(&resp_pairs), Some(&text));

        let mut builder = http::Response::builder().status(status).version(version);
        if let Some(h) = builder.headers_mut() {
            *h = headers;
        }
        match builder.body(bytes) {
            Ok(http_resp) => Ok(Response::from(http_resp)),
            Err(e) => Err(reqwest_middleware::Error::middleware(e)),
        }
    }
}
