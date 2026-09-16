use std::collections::HashSet;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    struct Unique;
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Unique;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("json with unique keys")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Unique, A::Error> {
                    let mut keys = HashSet::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if !keys.insert(key) {
                            return Err(serde::de::Error::custom("duplicate json key"));
                        }
                        map.next_value::<Unique>()?;
                    }
                    Ok(Unique)
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<Unique, A::Error> {
                    while seq.next_element::<Unique>()?.is_some() {}
                    Ok(Unique)
                }
                fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
                    Ok(Unique)
                }
            }
            deserializer.deserialize_any(Visitor)
        }
    }
    serde_json::from_slice::<Unique>(bytes)?;
    Ok(serde_json::from_slice(bytes)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictResult {
    Accepted,
    Rejected,
    Incomplete,
    Blocked,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub run_id: String,
    pub worker: String,
    pub assignment: u32,
    pub round: u32,
    pub session: String,
}

impl Identity {
    pub fn valid(&self) -> bool {
        crate::inbox::identifier(&self.run_id)
            && crate::status::worker_name(&self.worker)
            && crate::inbox::identifier(&self.session)
            && self.assignment > 0
            && self.round >= self.assignment
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub id: String,
    pub text: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub id: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub version: u32,
    pub task: String,
    pub assignment: String,
    pub generation: u32,
    pub candidate: String,
    pub reviewer: Identity,
    pub result: VerdictResult,
    pub complete: bool,
    pub checks: Vec<String>,
    pub findings: Vec<Finding>,
    pub blockers: Vec<String>,
    pub evidence: Vec<Evidence>,
    pub regression_commit: Option<String>,
}

impl Verdict {
    pub fn validate(&self) -> io::Result<()> {
        let mut ids = HashSet::new();
        if self.version != 1
            || !crate::inbox::identifier(&self.task)
            || !crate::inbox::identifier(&self.assignment)
            || self.generation == 0
            || !commit(&self.candidate)
            || !self.reviewer.valid()
            || self.checks.len() > 64
            || self
                .checks
                .iter()
                .any(|id| !crate::inbox::identifier(id) || !ids.insert(id))
            || self.findings.len() > 64
            || self.blockers.len() > 64
            || self.findings.iter().any(|f| {
                f.text
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
            })
            || self.blockers.iter().any(|s| s.is_empty() || s.len() > 4096)
            || self.evidence.len() > 64
            || self.regression_commit.as_ref().is_some_and(|s| !commit(s))
        {
            return Err(io::Error::other("invalid review verdict"));
        }
        ids.clear();
        if self
            .evidence
            .iter()
            .any(|e| !crate::inbox::identifier(&e.id) || !hash(&e.sha256) || !ids.insert(&e.id))
        {
            return Err(io::Error::other("invalid review evidence"));
        }
        let mut findings = HashSet::new();
        if self.findings.iter().any(|f| {
            !crate::inbox::identifier(&f.id)
                || !findings.insert(&f.id)
                || f.text.is_empty()
                || f.text.len() > 4096
                || f.evidence.len() > 64
                || f.evidence.iter().any(|id| !ids.contains(id))
        }) {
            return Err(io::Error::other("invalid review findings"));
        }
        match self.result {
            VerdictResult::Accepted
                if !self.complete || !self.findings.is_empty() || !self.blockers.is_empty() =>
            {
                return Err(io::Error::other("incomplete acceptance"));
            }
            VerdictResult::Rejected
                if !self.complete || self.findings.is_empty() || !self.blockers.is_empty() =>
            {
                return Err(io::Error::other("incomplete rejection"));
            }
            VerdictResult::Incomplete if self.complete => {
                return Err(io::Error::other("contradictory completeness"));
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub verdict: Verdict,
    pub capture_sha256: String,
    pub producer_action: String,
    pub evidence_directory: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<Pairing>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub run_id: String,
    pub worker: String,
    pub session: String,
    pub directory: PathBuf,
    pub workspace: PathBuf,
    pub manifest: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    pub producer: Participant,
    pub reviewer: Participant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pairing {
    pub task: String,
    pub pair: Pair,
}

impl Notice {
    pub fn validate(&self) -> io::Result<()> {
        if let Some(pairing) = &self.pairing {
            if !crate::inbox::identifier(&pairing.task) || pairing.task != self.verdict.task {
                return Err(io::Error::other("invalid review task"));
            }
            for actor in [&pairing.pair.producer, &pairing.pair.reviewer] {
                if !crate::inbox::identifier(&actor.run_id)
                    || !crate::status::worker_name(&actor.worker)
                    || !crate::inbox::identifier(&actor.session)
                    || [&actor.directory, &actor.workspace, &actor.manifest]
                        .iter()
                        .any(|p| !p.is_absolute() || p.as_os_str().len() > 4096)
                {
                    return Err(io::Error::other("invalid review pairing"));
                }
            }
        }
        self.verdict.validate()?;
        if !hash(&self.capture_sha256)
            || self.producer_action.len() > 512
            || !self.evidence_directory.is_absolute()
            || self.evidence_directory.as_os_str().len() > 4096
            || self
                .evidence_directory
                .to_string_lossy()
                .chars()
                .any(char::is_control)
            || serde_json::to_vec(self)?.len() > 256 * 1024
        {
            return Err(io::Error::other("invalid review notice"));
        }
        Ok(())
    }
}
