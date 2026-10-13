use std::collections::BTreeSet;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::engine::{Direction, Engine, Scope, SyncError, internal, upload_token};
use crate::local::write_file;
use crate::protocol::{ARTIFACTS, Commit, META, manifest_aad, now_ms};
use crate::remote::Committed;

const RESTORE_PATIENCE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    pub generation: u64,
    pub version: u64,
    pub archived_at: u64,
    pub len: u64,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restorable {
    pub session: String,
    pub title: Option<String>,
    pub deleted_at: Option<u64>,
    pub pruned: bool,
    pub generations: Vec<Generation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restored {
    Undeleted {
        session: String,
    },
    Copied {
        session: String,
        copy: String,
        generation: u64,
        missing: usize,
    },
}

fn refused(message: String) -> SyncError {
    SyncError::Io(std::io::Error::other(message))
}

impl Engine {
    pub async fn restorable(&self) -> Result<Vec<Restorable>, SyncError> {
        let mut found = Vec::new();
        let mut since = 0;
        loop {
            let changes = self.remote().changes(since, Duration::ZERO, false).await?;
            for head in changes.heads {
                let Ok(Some(entry)) = self.open_head(head.clone()) else {
                    continue;
                };
                if internal(&entry.manifest.session)
                    || (!entry.manifest.deleted && head.archived.is_empty())
                {
                    continue;
                }
                let generations = if head.archived.is_empty() {
                    Vec::new()
                } else {
                    self.generations(&head.sid).await?
                };
                found.push(Restorable {
                    session: entry.manifest.session.clone(),
                    title: entry.manifest.title.clone(),
                    deleted_at: entry
                        .manifest
                        .deleted
                        .then(|| head.deleted_at.unwrap_or(head.updated_at)),
                    pruned: head.pruned_at.is_some(),
                    generations,
                });
            }
            since = changes.rev;
            if !changes.more {
                break;
            }
        }
        found.sort_by(|left, right| {
            right
                .deleted_at
                .cmp(&left.deleted_at)
                .then_with(|| left.session.cmp(&right.session))
        });
        Ok(found)
    }

    async fn generations(&self, sid: &str) -> Result<Vec<Generation>, SyncError> {
        let mut generations = Vec::new();
        for archived in self.remote().archive(sid).await? {
            let Ok(Some(entry)) = self.open_head(archived.head.clone()) else {
                continue;
            };
            if entry
                .manifest
                .history
                .as_ref()
                .is_none_or(|history| history.len == 0)
            {
                continue;
            }
            generations.push(Generation {
                generation: archived.head.generation,
                version: archived.head.version,
                archived_at: archived.archived_at,
                len: entry
                    .manifest
                    .history
                    .as_ref()
                    .map_or(0, |history| history.len),
                title: entry.manifest.title.clone(),
            });
        }
        Ok(generations)
    }

    pub async fn restore(
        &self,
        session: &str,
        generation: Option<u64>,
    ) -> Result<Restored, SyncError> {
        let sid = self.keys().sid(session);
        let head = self
            .remote()
            .head(&sid)
            .await?
            .ok_or_else(|| refused(format!("the sync server has never seen {session}")))?;
        let entry = self
            .open_head(head.clone())?
            .ok_or_else(|| refused(format!("the sync server has nothing stored for {session}")))?;
        let Some(generation) = generation else {
            if !entry.manifest.deleted {
                return Err(refused(format!(
                    "{session} is not deleted, pass --generation to bring back an older generation of it"
                )));
            }
            if head.pruned_at.is_some() {
                return Err(refused(format!(
                    "{session} was deleted long enough ago that the server pruned it, there is nothing left to restore"
                )));
            }
            let mut manifest = entry.manifest.clone();
            manifest.deleted = false;
            manifest.writer = self.machine().to_owned();
            manifest.written_at = now_ms();
            let sealed = self.keys().seal(
                &manifest_aad(&sid, head.version + 1),
                &serde_json::to_vec(&manifest)?,
            )?;
            let blobs: BTreeSet<String> = manifest
                .files
                .values()
                .map(|file| file.blob.clone())
                .collect();
            let commit = Commit {
                expect: head.version,
                holder: self.machine().to_owned(),
                upload: upload_token()?,
                generation: head.generation,
                seq: head.seq,
                blobs: blobs.into_iter().collect(),
                manifest: STANDARD.encode(sealed),
                deleted: false,
            };
            match self.remote().commit(&sid, &commit).await? {
                Committed::Accepted(_) => {}
                Committed::Conflict(_) => {
                    return Err(refused(format!(
                        "{session} changed on the server while it was being restored, try again"
                    )));
                }
                Committed::Locked { lease, .. } => {
                    return Err(refused(format!("{session} is open on {}", lease.holder)));
                }
            }
            self.pass(
                Scope::Sessions(vec![session.to_owned()]),
                Direction::Both,
                None,
                RESTORE_PATIENCE,
            )
            .await?;
            return Ok(Restored::Undeleted {
                session: session.to_owned(),
            });
        };
        let chosen = if generation == head.generation && !entry.manifest.deleted {
            entry
        } else {
            let archived = self
                .remote()
                .archive(&sid)
                .await?
                .into_iter()
                .filter(|archived| archived.head.generation == generation)
                .max_by_key(|archived| archived.head.version)
                .ok_or_else(|| {
                    refused(format!(
                        "generation {generation} of {session} is not kept on the server"
                    ))
                })?;
            self.open_head(archived.head)?.ok_or_else(|| {
                refused(format!(
                    "generation {generation} of {session} has no header"
                ))
            })?
        };
        let copy = self.keys().fork_id(
            session,
            "restore",
            &format!("generation {generation} version {}", chosen.head.version),
        );
        if self.layout().meta(&copy).exists() {
            return Ok(Restored::Copied {
                session: session.to_owned(),
                copy,
                generation,
                missing: 0,
            });
        }
        let meta = chosen.manifest.files.get(META).ok_or_else(|| {
            refused(format!(
                "generation {generation} of {session} has no header"
            ))
        })?;
        let header = self.fetch_blob(session, &sid, meta).await?;
        if let Some(history) = &chosen.manifest.history {
            let download = self.fetch_history(session, &sid, history, None).await?;
            std::fs::rename(&download.path, self.layout().history(&copy))?;
        }
        let mut missing = 0;
        for (key, file) in &chosen.manifest.files {
            if !key.starts_with(ARTIFACTS) {
                continue;
            }
            let Some(target) = self.layout().file(&copy, key) else {
                continue;
            };
            match self.fetch_blob_into(session, &sid, file, &target).await {
                Ok(()) => {}
                Err(error) if error.offline() => return Err(error),
                Err(error) => {
                    tracing::warn!(%error, key, "a file of the restored generation is gone");
                    missing += 1;
                }
            }
        }
        let mut value: serde_json::Value = serde_json::from_slice(&header)?;
        if let Some(fields) = value
            .get_mut("meta")
            .and_then(serde_json::Value::as_object_mut)
        {
            let title = fields
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("untitled")
                .to_owned();
            fields.insert("id".into(), copy.clone().into());
            fields.insert(
                "title".into(),
                format!("{title} (generation {generation})").into(),
            );
            fields.remove("parent");
        }
        write_file(
            &self.layout().meta(&copy),
            &serde_json::to_vec_pretty(&value)?,
        )?;
        self.pass(
            Scope::Sessions(vec![copy.clone()]),
            Direction::Both,
            None,
            RESTORE_PATIENCE,
        )
        .await?;
        Ok(Restored::Copied {
            session: session.to_owned(),
            copy,
            generation,
            missing,
        })
    }
}
