use std::time::Duration;

use crate::protocol::{
    Changes, Commit, Head, Lease, LeaseGrant, LeaseRequest, Leases, Refusal, ReleaseRequest,
};

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("the sync server is unreachable: {0}")]
    Offline(String),
    #[error("the sync server refused the key, check sync.key_file and the server's token hash")]
    Unauthorised,
    #[error("the sync server answered {status}: {body}")]
    Status { status: u16, body: String },
    #[error("the sync server sent something unreadable: {0}")]
    Garbled(String),
}

impl RemoteError {
    #[must_use]
    pub fn offline(&self) -> bool {
        matches!(self, Self::Offline(_))
    }
}

impl From<reqwest::Error> for RemoteError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_connect() || error.is_timeout() || error.is_request() {
            Self::Offline(error.to_string())
        } else if error.is_decode() || error.is_body() {
            Self::Garbled(error.to_string())
        } else {
            Self::Offline(error.to_string())
        }
    }
}

#[derive(Debug)]
pub enum Committed {
    Accepted(Head),
    Conflict(Head),
    Locked { lease: Lease, now: u64 },
}

#[derive(Debug)]
pub enum Claimed {
    Granted(LeaseGrant),
    Held { lease: Lease, now: u64 },
}

#[derive(Clone)]
pub struct Remote {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote").field("base", &self.base).finish()
    }
}

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

impl Remote {
    pub fn new(base: &str, token: &str, connect_timeout: Duration) -> Result<Self, RemoteError> {
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .build()
            .map_err(|error| RemoteError::Offline(error.to_string()))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v1/{path}", self.base)
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<reqwest::Response, RemoteError> {
        let response = request
            .bearer_auth(&self.token)
            .timeout(timeout)
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(RemoteError::Unauthorised);
        }
        Ok(response)
    }

    async fn failed(response: reqwest::Response) -> RemoteError {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        RemoteError::Status {
            status,
            body: body.chars().take(400).collect(),
        }
    }

    async fn json<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, RemoteError> {
        if !response.status().is_success() {
            return Err(Self::failed(response).await);
        }
        response
            .json()
            .await
            .map_err(|error| RemoteError::Garbled(error.to_string()))
    }

    pub async fn changes(
        &self,
        since: u64,
        wait: Duration,
        peek: bool,
    ) -> Result<Changes, RemoteError> {
        let request = self.http.get(format!(
            "{}?since={since}&wait={}&peek={peek}",
            self.url("changes"),
            wait.as_secs()
        ));
        Self::json(self.send(request, wait + CALL_TIMEOUT).await?).await
    }

    pub async fn leases(&self, timeout: Duration) -> Result<Leases, RemoteError> {
        Self::json(
            self.send(self.http.get(self.url("leases")), timeout)
                .await?,
        )
        .await
    }

    pub async fn head(&self, sid: &str) -> Result<Option<Head>, RemoteError> {
        let response = self
            .send(
                self.http.get(self.url(&format!("sessions/{sid}"))),
                CALL_TIMEOUT,
            )
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Self::json(response).await.map(Some)
    }

    async fn download(&self, path: &str) -> Result<Vec<u8>, RemoteError> {
        let response = self
            .send(self.http.get(self.url(path)), TRANSFER_TIMEOUT)
            .await?;
        if !response.status().is_success() {
            return Err(Self::failed(response).await);
        }
        Ok(response.bytes().await?.to_vec())
    }

    async fn upload(&self, path: &str, body: Vec<u8>) -> Result<(), RemoteError> {
        let response = self
            .send(self.http.put(self.url(path)).body(body), TRANSFER_TIMEOUT)
            .await?;
        if !response.status().is_success() {
            return Err(Self::failed(response).await);
        }
        Ok(())
    }

    pub async fn chunk(
        &self,
        sid: &str,
        generation: u64,
        seq: u64,
    ) -> Result<Vec<u8>, RemoteError> {
        self.download(&format!("sessions/{sid}/chunks/{generation}/{seq}"))
            .await
    }

    pub async fn stage(
        &self,
        sid: &str,
        upload: &str,
        generation: u64,
        seq: u64,
        sealed: Vec<u8>,
    ) -> Result<(), RemoteError> {
        self.upload(
            &format!("sessions/{sid}/uploads/{upload}/{generation}/{seq}"),
            sealed,
        )
        .await
    }

    pub async fn blob(&self, sid: &str, blob: &str) -> Result<Vec<u8>, RemoteError> {
        self.download(&format!("sessions/{sid}/blobs/{blob}")).await
    }

    pub async fn store_blob(
        &self,
        sid: &str,
        blob: &str,
        sealed: Vec<u8>,
    ) -> Result<(), RemoteError> {
        self.upload(&format!("sessions/{sid}/blobs/{blob}"), sealed)
            .await
    }

    pub async fn commit(&self, sid: &str, commit: &Commit) -> Result<Committed, RemoteError> {
        let response = self
            .send(
                self.http
                    .post(self.url(&format!("sessions/{sid}/commit")))
                    .json(commit),
                CALL_TIMEOUT,
            )
            .await?;
        if response.status().is_success() {
            return Self::json(response).await.map(Committed::Accepted);
        }
        let status = response.status();
        if status == reqwest::StatusCode::CONFLICT || status == reqwest::StatusCode::LOCKED {
            return match Self::json_any(response).await? {
                Refusal::Conflict { head } => Ok(Committed::Conflict(head)),
                Refusal::Locked { lease, now } => Ok(Committed::Locked { lease, now }),
                Refusal::Invalid { reason } => Err(RemoteError::Status {
                    status: status.as_u16(),
                    body: reason,
                }),
            };
        }
        Err(Self::failed(response).await)
    }

    async fn json_any<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, RemoteError> {
        response
            .json()
            .await
            .map_err(|error| RemoteError::Garbled(error.to_string()))
    }

    pub async fn claim(&self, sid: &str, request: &LeaseRequest) -> Result<Claimed, RemoteError> {
        let response = self
            .send(
                self.http
                    .post(self.url(&format!("sessions/{sid}/lease")))
                    .json(request),
                CALL_TIMEOUT,
            )
            .await?;
        if response.status() == reqwest::StatusCode::LOCKED {
            return match Self::json_any(response).await? {
                Refusal::Locked { lease, now } => Ok(Claimed::Held { lease, now }),
                other => Err(RemoteError::Garbled(format!("{other:?}"))),
            };
        }
        Self::json(response).await.map(Claimed::Granted)
    }

    pub async fn release(&self, sid: &str, request: &ReleaseRequest) -> Result<(), RemoteError> {
        let response = self
            .send(
                self.http
                    .post(self.url(&format!("sessions/{sid}/release")))
                    .json(request),
                CALL_TIMEOUT,
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::failed(response).await);
        }
        Ok(())
    }
}
