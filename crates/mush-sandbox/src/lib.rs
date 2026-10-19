use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading sandbox configuration {path}")]
    ConfigIo {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid sandbox configuration")]
    ConfigParse(#[from] toml::de::Error),
    #[error("sandbox path must be absolute: {0}")]
    RelativePath(PathBuf),
    #[error("network grants are not supported")]
    NetworkUnsupported,
    #[error("unknown sandbox profile: {0}")]
    UnknownProfile(String),
    #[error("home directory is unavailable")]
    NoHome,
    #[error("creating private sandbox temporary directory")]
    TempDir(#[source] std::io::Error),
    #[error("sandbox requires landlock ABI 9 or newer: {0}")]
    MinimumAbi(String),
    #[error("sandbox is unsupported on this platform")]
    Unsupported,
    #[error("sandbox path {path}")]
    PathIo {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("edit path is outside writable sandbox roots: {0}")]
    EditDenied(PathBuf),
    #[error("edit path contains a parent traversal: {0}")]
    EditTraversal(PathBuf),
    #[error("opening sandbox edit path {path}")]
    EditOpen {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("sandbox edits require a regular file without hard links")]
    EditFileType,
    #[error("sandbox edit exceeds the 16 MiB size limit")]
    EditTooLarge,
    #[error("sandbox edit requires nonempty old text")]
    EditEmpty,
    #[error("sandbox edit requires one exact match, found {0}")]
    EditMatches(usize),
    #[error("sandbox edit file operation failed")]
    EditIo(#[source] std::io::Error),
    #[error("sandbox secret must be a regular file: {0}")]
    SecretNotFile(PathBuf),
    #[error("read-only sandbox path {readonly} overlaps writable path {writable}")]
    WritableOverlap {
        readonly: PathBuf,
        writable: PathBuf,
    },
    #[cfg(target_os = "linux")]
    #[error("landlock setup failed")]
    Landlock(#[from] landlock::RulesetError),
    #[cfg(target_os = "linux")]
    #[error("landlock path setup failed")]
    LandlockPath(#[from] landlock::PathFdError),
    #[cfg(target_os = "linux")]
    #[error("seccomp setup failed")]
    Seccomp(#[from] seccompiler::Error),
    #[cfg(target_os = "linux")]
    #[error("seccomp compilation failed")]
    SeccompBackend(#[from] seccompiler::BackendError),
    #[error("unsupported seccomp architecture")]
    Architecture,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    profile: BTreeMap<String, Grants>,
    #[serde(default)]
    project: BTreeMap<PathBuf, Project>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Project {
    default: Option<String>,
    #[serde(default)]
    profile: BTreeMap<String, Grants>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grants {
    #[serde(default)]
    read: Vec<PathBuf>,
    #[serde(default)]
    write: Vec<PathBuf>,
    #[serde(default)]
    secrets: Vec<PathBuf>,
    websearch: Option<bool>,
    #[serde(default)]
    allowed_host_requests: Vec<String>,
    #[serde(default)]
    network: Vec<String>,
}

impl Grants {
    fn validate(&self) -> Result<()> {
        if !self.network.is_empty() {
            return Err(Error::NetworkUnsupported);
        }
        for path in self.read.iter().chain(&self.write).chain(&self.secrets) {
            require_absolute(path)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    secrets: Vec<PathBuf>,
    websearch: bool,
    allowed_host_requests: Vec<String>,
}

impl Profile {
    pub fn read(&self) -> &[PathBuf] {
        &self.read
    }
    pub fn write(&self) -> &[PathBuf] {
        &self.write
    }
    pub fn secrets(&self) -> &[PathBuf] {
        &self.secrets
    }
    pub fn websearch(&self) -> bool {
        self.websearch
    }
    pub fn allowed_host_requests(&self) -> &[String] {
        &self.allowed_host_requests
    }

    pub fn allows_host_request(&self, kind: &str) -> bool {
        if kind == "websearch" {
            return self.websearch;
        }
        matches!(
            kind,
            "edit"
                | "bash.start"
                | "bash.wait"
                | "bash.poll"
                | "bash.running"
                | "bash.cancel"
                | "bash.kill"
        ) || self
            .allowed_host_requests
            .iter()
            .any(|allowed| allowed == kind)
    }

    fn extend(&mut self, grants: &Grants) {
        self.read.extend_from_slice(&grants.read);
        self.write.extend_from_slice(&grants.write);
        self.secrets.extend_from_slice(&grants.secrets);
        self.allowed_host_requests
            .extend_from_slice(&grants.allowed_host_requests);
        if let Some(websearch) = grants.websearch {
            self.websearch = websearch;
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::ConfigIo {
                    path: path.into(),
                    source,
                });
            }
        };
        let config: Self = toml::from_str(&text)?;
        for grants in config.profile.values() {
            grants.validate()?;
        }
        for (path, project) in &config.project {
            require_absolute(path)?;
            for grants in project.profile.values() {
                grants.validate()?;
            }
            if let Some(default) = &project.default
                && !config.profile.contains_key(default)
                && !project.profile.contains_key(default)
            {
                return Err(Error::UnknownProfile(default.clone()));
            }
        }
        Ok(Some(config))
    }

    pub fn resolve(&self, repo: &Path, explicit: Option<&str>) -> Result<Profile> {
        require_absolute(repo)?;
        let project = self
            .project
            .iter()
            .filter(|(path, _)| repo.starts_with(path))
            .max_by_key(|(path, _)| path.components().count())
            .map(|(_, project)| project);
        let name = explicit.or_else(|| project.and_then(|p| p.default.as_deref()));
        let mut profile = Profile::default();
        if let Some(name) = name {
            let shared = self.profile.get(name);
            let extension = project.and_then(|p| p.profile.get(name));
            if shared.is_none() && extension.is_none() {
                return Err(Error::UnknownProfile(name.into()));
            }
            if let Some(shared) = shared {
                profile.extend(shared);
            }
            if let Some(extension) = extension {
                profile.extend(extension);
            }
        }
        Ok(profile)
    }
}

pub fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or(Error::NoHome)?;
    let home = PathBuf::from(home);
    require_absolute(&home)?;
    Ok(home.join(".config/mush/sandbox.toml"))
}

fn require_absolute(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::RelativePath(path.into()));
    }
    Ok(())
}

fn canonical(path: &Path) -> Result<PathBuf> {
    require_absolute(path)?;
    path.canonicalize().map_err(|source| Error::PathIo {
        path: path.into(),
        source,
    })
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    profile: Profile,
    workspace: PathBuf,
    temporary: std::sync::Arc<tempfile::TempDir>,
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
    secrets: Vec<PathBuf>,
    protected_read: Vec<PathBuf>,
}

impl Sandbox {
    pub fn check_support() -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        return Err(Error::Unsupported);
        #[cfg(target_os = "linux")]
        {
            let abi = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<libc::c_void>(),
                    0usize,
                    1u32,
                )
            };
            if abi >= 9 {
                Ok(())
            } else {
                Err(Error::MinimumAbi(format!("kernel reports {abi}")))
            }
        }
    }

    pub fn new(
        profile: Profile,
        workspace: &Path,
        extra_read: &[PathBuf],
        extra_write: &[PathBuf],
    ) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        return Err(Error::Unsupported);
        #[cfg(target_os = "linux")]
        {
            Self::check_support()?;
            let mut profile = profile;
            profile.read = profile
                .read
                .iter()
                .map(|path| canonical(path))
                .collect::<Result<_>>()?;
            profile.write = profile
                .write
                .iter()
                .map(|path| canonical(path))
                .collect::<Result<_>>()?;
            profile.secrets = profile
                .secrets
                .iter()
                .map(|path| canonical(path))
                .collect::<Result<_>>()?;
            let mut read = vec![canonical(Path::new("/nix/store"))?];
            if let Some(python) = std::env::var_os("VIRTUAL_ENV") {
                read.push(canonical(Path::new(&python))?);
            }
            let mut protected_read = read.clone();
            protected_read.extend(
                extra_read
                    .iter()
                    .map(|p| canonical(p))
                    .collect::<Result<Vec<_>>>()?,
            );
            read.extend(
                profile
                    .read
                    .iter()
                    .chain(extra_read)
                    .map(|p| canonical(p))
                    .collect::<Result<Vec<_>>>()?,
            );
            let workspace = canonical(workspace)?;
            let temporary = std::sync::Arc::new(
                tempfile::Builder::new()
                    .prefix("mush-sandbox-")
                    .tempdir_in("/tmp")
                    .map_err(Error::TempDir)?,
            );
            let mut write = vec![workspace.clone(), canonical(temporary.path())?];
            write.extend(
                profile
                    .write
                    .iter()
                    .chain(extra_write)
                    .map(|p| canonical(p))
                    .collect::<Result<Vec<_>>>()?,
            );
            let secrets = profile
                .secrets
                .iter()
                .map(|p| canonical(p))
                .collect::<Result<Vec<_>>>()?;
            let sandbox = Self {
                profile,
                workspace,
                temporary,
                read,
                write,
                secrets,
                protected_read,
            };
            sandbox.validate_paths()?;
            sandbox.ruleset()?;
            linux::filter()?;
            Ok(sandbox)
        }
    }

    pub fn with_paths(&self, read: &[PathBuf], write: &[PathBuf]) -> Result<Self> {
        let mut derived = Self {
            profile: self.profile.clone(),
            workspace: self.workspace.clone(),
            temporary: self.temporary.clone(),
            read: self.read.clone(),
            write: self.write.clone(),
            secrets: self.secrets.clone(),
            protected_read: self.protected_read.clone(),
        };
        derived.protected_read.extend(
            read.iter()
                .map(|p| canonical(p))
                .collect::<Result<Vec<_>>>()?,
        );
        derived.read.extend(
            read.iter()
                .map(|p| canonical(p))
                .collect::<Result<Vec<_>>>()?,
        );
        derived.write.extend(
            write
                .iter()
                .map(|p| canonical(p))
                .collect::<Result<Vec<_>>>()?,
        );
        derived.validate_paths()?;
        #[cfg(target_os = "linux")]
        derived.ruleset()?;
        Ok(derived)
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
    pub fn allows_host_request(&self, kind: &str) -> bool {
        self.profile.allows_host_request(kind)
    }

    pub fn edit_file(&self, path: &Path, old: &str, new: &str) -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path, old, new);
            Err(Error::Unsupported)
        }
        #[cfg(target_os = "linux")]
        edit::edit_file(self, path, old, new)
    }

    pub fn allows_write(&self, path: &Path) -> bool {
        let Ok(path) = path.canonicalize() else {
            return false;
        };
        self.write.iter().any(|grant| path.starts_with(grant))
    }

    pub fn allows_read(&self, path: &Path) -> bool {
        let Ok(path) = path.canonicalize() else {
            return false;
        };
        self.read
            .iter()
            .chain(&self.write)
            .chain(&self.secrets)
            .any(|grant| path.starts_with(grant))
    }

    fn validate_paths(&self) -> Result<()> {
        for secret in &self.secrets {
            if !secret.is_file() {
                return Err(Error::SecretNotFile(secret.clone()));
            }
        }
        for readonly in self.secrets.iter().chain(&self.protected_read) {
            for writable in &self.write {
                if readonly.starts_with(writable) || writable.starts_with(readonly) {
                    return Err(Error::WritableOverlap {
                        readonly: readonly.clone(),
                        writable: writable.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn prepare_command(&self, command: &mut tokio::process::Command) -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = command;
            Err(Error::Unsupported)
        }
        #[cfg(target_os = "linux")]
        {
            let mut ruleset = Some(self.ruleset()?);
            let filter = linux::filter()?;
            command.env_clear();
            for name in [
                "PATH",
                "HOME",
                "USER",
                "LOGNAME",
                "LANG",
                "LC_ALL",
                "LC_CTYPE",
                "TERM",
                "TZ",
                "VIRTUAL_ENV",
            ] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command.env("PYTHONNOUSERSITE", "1");
            for name in ["TMPDIR", "TMP", "TEMP"] {
                command.env(name, self.temporary.path());
            }
            unsafe {
                command.pre_exec(move || {
                    let ruleset = ruleset
                        .take()
                        .ok_or(std::io::Error::from_raw_os_error(libc::EPERM))?;
                    let status = ruleset
                        .restrict_self()
                        .map_err(|_| std::io::Error::from_raw_os_error(libc::EPERM))?;
                    if status.ruleset != landlock::RulesetStatus::FullyEnforced
                        || !status.no_new_privs
                    {
                        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                    }
                    seccompiler::apply_filter(&filter)
                        .map_err(|_| std::io::Error::from_raw_os_error(libc::EPERM))?;
                    Ok(())
                });
            }
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    fn ruleset(&self) -> Result<landlock::RulesetCreated> {
        linux::ruleset(self)
    }
}

#[cfg(target_os = "linux")]
mod edit;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(test)]
mod tests;
