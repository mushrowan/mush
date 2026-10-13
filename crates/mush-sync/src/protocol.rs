use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub sid: String,
    pub version: u64,
    pub rev: u64,
    pub generation: u64,
    pub seq: u64,
    #[serde(default)]
    pub blobs: Vec<String>,
    #[serde(default)]
    pub manifest: Option<String>,
    #[serde(default)]
    pub lease: Option<Lease>,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pruned_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub archived: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub holder: String,
    pub fence: u64,
    pub expires_at: u64,
}

impl Lease {
    #[must_use]
    pub fn active(&self, now: u64) -> bool {
        self.expires_at > now
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub expect: u64,
    pub holder: String,
    pub upload: String,
    pub generation: u64,
    pub seq: u64,
    pub blobs: Vec<String>,
    pub manifest: String,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Archived {
    pub head: Head,
    pub archived_at: u64,
    pub superseded_rev: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineReport {
    pub rev: u64,
    #[serde(default)]
    pub sessions: u64,
    #[serde(default)]
    pub waiting: u64,
    #[serde(default)]
    pub failed: u64,
    #[serde(default)]
    pub review: u64,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub name: String,
    pub seen_at: u64,
    pub report: MachineReport,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Machines {
    pub now: u64,
    pub machines: Vec<Machine>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PruneRequest {
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrunedGeneration {
    pub sid: String,
    pub generation: u64,
    pub version: u64,
    pub archived_at: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrunedSession {
    pub sid: String,
    pub deleted_at: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocked {
    pub sid: String,
    pub what: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PruneReport {
    pub dry_run: bool,
    pub now: u64,
    pub keep_ms: u64,
    pub generations: Vec<PrunedGeneration>,
    pub sessions: Vec<PrunedSession>,
    pub blocked: Vec<Blocked>,
    pub stale_machines: Vec<String>,
    pub swept_bytes: u64,
    pub freed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub holder: String,
    pub ttl_ms: u64,
    pub take: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRequest {
    pub holder: String,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub lease: Lease,
    pub now: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Changes {
    pub server: String,
    pub now: u64,
    pub rev: u64,
    pub heads: Vec<Head>,
    pub more: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Leases {
    pub now: u64,
    pub held: Vec<(String, Lease)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum Refusal {
    Conflict { head: Head },
    Locked { lease: Lease, now: u64 },
    Invalid { reason: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub session: String,
    pub writer: String,
    pub written_at: u64,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub history: Option<HistoryRef>,
    #[serde(default)]
    pub files: BTreeMap<String, FileRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRef {
    pub generation: u64,
    pub seq: u64,
    pub len: u64,
    pub chain: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub blob: String,
    pub len: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<u64>,
}

pub const META: &str = "meta";
pub const ARTIFACTS: &str = "artifacts/";

fn lower_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[must_use]
pub fn valid_sid(text: &str) -> bool {
    lower_hex(text, 32)
}

#[must_use]
pub fn valid_blob(text: &str) -> bool {
    lower_hex(text, 32)
}

#[must_use]
pub fn valid_upload(text: &str) -> bool {
    (1..=64).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[must_use]
pub fn valid_holder(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

#[must_use]
pub fn valid_session(text: &str) -> bool {
    (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

#[must_use]
pub fn chunk_aad(sid: &str, generation: u64, seq: u64) -> Vec<u8> {
    format!("mush-sync chunk {sid} {generation} {seq}").into_bytes()
}

#[must_use]
pub fn blob_aad(sid: &str, blob: &str) -> Vec<u8> {
    format!("mush-sync blob {sid} {blob}").into_bytes()
}

#[must_use]
pub fn manifest_aad(sid: &str, version: u64) -> Vec<u8> {
    format!("mush-sync manifest {sid} {version}").into_bytes()
}

#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
