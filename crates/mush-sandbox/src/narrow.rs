use crate::{Profile, Result, canonical, network};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Narrow {
    pub read: Option<Vec<PathBuf>>,
    pub write: Option<Vec<PathBuf>>,
    pub secrets: Option<Vec<PathBuf>>,
    pub network: Option<Vec<String>>,
    pub websearch: Option<bool>,
    pub allowed_host_requests: Option<Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("narrow sandbox path contains a parent traversal: {0}")]
    Traversal(PathBuf),
    #[error("sandbox {kind} snapshot path changed: {path}")]
    Changed { kind: &'static str, path: PathBuf },
    #[error("narrow sandbox {kind} grant exceeds the parent: {grant}")]
    Wider { kind: &'static str, grant: String },
}

fn path(path: &Path) -> Result<PathBuf> {
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(Error::Traversal(path.into()).into());
    }
    canonical(path)
}

fn paths(
    requested: &[PathBuf],
    allowed: &[&[PathBuf]],
    kind: &'static str,
    exact: bool,
) -> Result<Vec<PathBuf>> {
    let allowed = allowed
        .iter()
        .flat_map(|paths| paths.iter())
        .collect::<Vec<_>>();
    requested
        .iter()
        .map(|requested| {
            let requested = path(requested)?;
            if !allowed.iter().any(|allowed| {
                if exact {
                    &requested == *allowed
                } else {
                    requested.starts_with(allowed)
                }
            }) {
                return Err(Error::Wider {
                    kind,
                    grant: requested.display().to_string(),
                }
                .into());
            }
            Ok(requested)
        })
        .collect()
}

fn entries(requested: &[String], allowed: &[String], kind: &'static str) -> Result<Vec<String>> {
    for entry in requested {
        if !allowed.contains(entry) {
            return Err(Error::Wider {
                kind,
                grant: entry.clone(),
            }
            .into());
        }
    }
    Ok(requested.to_vec())
}

impl Profile {
    pub fn canonicalized(&self) -> Result<Self> {
        let mut profile = self.clone();
        profile.read = self.read.iter().map(|p| path(p)).collect::<Result<_>>()?;
        profile.write = self.write.iter().map(|p| path(p)).collect::<Result<_>>()?;
        profile.secrets = self
            .secrets
            .iter()
            .map(|p| path(p))
            .collect::<Result<_>>()?;
        Ok(profile)
    }

    pub fn validate_snapshot(&self) -> Result<()> {
        for (kind, paths) in [
            ("read", &self.read),
            ("write", &self.write),
            ("secrets", &self.secrets),
        ] {
            for stored in paths {
                if path(stored)? != *stored {
                    return Err(Error::Changed {
                        kind,
                        path: stored.clone(),
                    }
                    .into());
                }
            }
        }
        Ok(())
    }

    pub fn narrow(&self, narrow: &Narrow) -> Result<Self> {
        self.validate_snapshot()?;
        let mut derived = self.clone();
        if let Some(read) = &narrow.read {
            derived.read = paths(read, &[&self.read, &self.write], "read", false)?;
        }
        if let Some(write) = &narrow.write {
            derived.write = paths(write, &[&self.write], "write", false)?;
        }
        if let Some(secrets) = &narrow.secrets {
            derived.secrets = paths(secrets, &[&self.secrets], "secrets", true)?;
        }
        if let Some(requested) = &narrow.network {
            for entry in requested {
                network::validate_entry(entry)?;
            }
            derived.network = entries(requested, &self.network, "network")?;
        }
        if let Some(websearch) = narrow.websearch {
            if websearch && !self.websearch {
                return Err(Error::Wider {
                    kind: "websearch",
                    grant: "true".into(),
                }
                .into());
            }
            derived.websearch = websearch;
        }
        if let Some(requested) = &narrow.allowed_host_requests {
            derived.allowed_host_requests = entries(
                requested,
                &self.allowed_host_requests,
                "allowed_host_requests",
            )?;
        }
        Ok(derived)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn profile() -> Profile {
        Profile {
            read: vec![PathBuf::from("/nix/store")],
            write: vec![PathBuf::from("/tmp")],
            network: vec!["example.com:443".into()],
            websearch: true,
            allowed_host_requests: vec!["attach_image".into()],
            ..Profile::default()
        }
    }

    #[test]
    fn unknown_narrowing_fields_are_rejected() {
        assert!(toml::from_str::<Narrow>("unknown = []").is_err());
        assert!(toml::from_str::<Narrow>("websearch = 'yes'").is_err());
        assert_eq!(toml::from_str::<Narrow>("").unwrap(), Narrow::default());
    }

    #[test]
    fn omitted_fields_inherit_and_empty_fields_remove() {
        let parent = profile();
        assert_eq!(parent.narrow(&Narrow::default()).unwrap(), parent);
        let child = parent
            .narrow(&Narrow {
                read: Some(vec![]),
                write: Some(vec![]),
                network: Some(vec![]),
                websearch: Some(false),
                allowed_host_requests: Some(vec![]),
                ..Narrow::default()
            })
            .unwrap();
        assert!(child.read().is_empty());
        assert!(child.write().is_empty());
        assert!(child.network().is_empty());
        assert!(!child.websearch());
        assert!(!child.allows_host_request("attach_image"));
    }

    #[test]
    fn wider_read_is_rejected() {
        assert!(
            profile()
                .narrow(&Narrow {
                    read: Some(vec![PathBuf::from("/etc")]),
                    ..Narrow::default()
                })
                .is_err()
        );
    }

    #[test]
    fn wider_write_is_rejected() {
        assert!(
            profile()
                .narrow(&Narrow {
                    write: Some(vec![PathBuf::from("/nix/store")]),
                    ..Narrow::default()
                })
                .is_err()
        );
    }

    #[test]
    fn wider_secrets_is_rejected() {
        assert!(
            profile()
                .narrow(&Narrow {
                    secrets: Some(vec![PathBuf::from("/etc/passwd")]),
                    ..Narrow::default()
                })
                .is_err()
        );
    }

    #[test]
    fn wider_network_is_rejected() {
        assert!(
            profile()
                .narrow(&Narrow {
                    network: Some(vec!["other.example:443".into()]),
                    ..Narrow::default()
                })
                .is_err()
        );
    }

    #[test]
    fn wider_host_requests_is_rejected() {
        assert!(
            profile()
                .narrow(&Narrow {
                    allowed_host_requests: Some(vec!["arbitrary".into()]),
                    ..Narrow::default()
                })
                .is_err()
        );
    }

    #[test]
    fn wider_websearch_is_rejected() {
        assert!(
            Profile::default()
                .narrow(&Narrow {
                    websearch: Some(true),
                    ..Narrow::default()
                })
                .is_err()
        );
    }

    #[test]
    fn paths_are_canonical_subsets_and_cannot_traverse() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let parent = Profile {
            write: vec![root.path().to_path_buf()],
            ..Profile::default()
        };
        let child = parent
            .narrow(&Narrow {
                write: Some(vec![nested.clone()]),
                ..Narrow::default()
            })
            .unwrap();
        assert_eq!(child.write(), &[nested.canonicalize().unwrap()]);
        assert!(
            parent
                .narrow(&Narrow {
                    write: Some(vec![nested.join("..")]),
                    ..Narrow::default()
                })
                .is_err()
        );
        assert!(
            parent
                .narrow(&Narrow {
                    write: Some(vec![PathBuf::from("relative")]),
                    ..Narrow::default()
                })
                .is_err()
        );
        assert!(
            child
                .narrow(&Narrow {
                    write: Some(vec![root.path().to_path_buf()]),
                    ..Narrow::default()
                })
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn retargeted_parent_snapshot_cannot_widen_descendants() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let granted = root.path().join("granted");
        std::fs::create_dir(&granted).unwrap();
        let parent = Profile {
            write: vec![granted.clone()],
            ..Profile::default()
        }
        .canonicalized()
        .unwrap();
        std::fs::remove_dir(&granted).unwrap();
        std::os::unix::fs::symlink(outside.path(), &granted).unwrap();
        assert!(parent.validate_snapshot().is_err());
        assert!(parent.narrow(&Narrow::default()).is_err());
        assert!(
            parent
                .narrow(&Narrow {
                    write: Some(vec![outside.path().to_path_buf()]),
                    ..Default::default()
                })
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_and_secrets_remain_exact_files() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, "private").unwrap();
        let parent = Profile {
            write: vec![root.path().to_path_buf()],
            secrets: vec![secret.clone()],
            ..Profile::default()
        };
        assert!(
            parent
                .narrow(&Narrow {
                    write: Some(vec![link]),
                    ..Narrow::default()
                })
                .is_err()
        );
        assert!(
            parent
                .narrow(&Narrow {
                    secrets: Some(vec![outside.path().to_path_buf()]),
                    ..Narrow::default()
                })
                .is_err()
        );
        assert_eq!(
            parent
                .narrow(&Narrow {
                    secrets: Some(vec![secret.clone()]),
                    ..Narrow::default()
                })
                .unwrap()
                .secrets(),
            &[secret]
        );
    }
}
