use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::KernelError;

pub const PYTHON_VERSION: &str = "3.12";
const MARKER_SCHEMA: u32 = 1;
const MARKER_FILE: &str = ".mush-bootstrap.json";
const LOCK_SUFFIX: &str = ".lock";
const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Package {
    pub spec: &'static str,
    pub import_name: &'static str,
    pub label: &'static str,
}

pub const DEFAULT_PACKAGES: &[Package] = &[
    Package {
        spec: "requests",
        import_name: "requests",
        label: "requests",
    },
    Package {
        spec: "httpx",
        import_name: "httpx",
        label: "httpx",
    },
    Package {
        spec: "pyyaml",
        import_name: "yaml",
        label: "yaml (PyYAML)",
    },
    Package {
        spec: "tomli",
        import_name: "tomli",
        label: "tomli",
    },
    Package {
        spec: "python-dotenv",
        import_name: "dotenv",
        label: "dotenv (python-dotenv)",
    },
    Package {
        spec: "pandas",
        import_name: "pandas",
        label: "pandas",
    },
    Package {
        spec: "numpy",
        import_name: "numpy",
        label: "numpy",
    },
    Package {
        spec: "scipy",
        import_name: "scipy",
        label: "scipy",
    },
    Package {
        spec: "beautifulsoup4",
        import_name: "bs4",
        label: "bs4 (Beautiful Soup)",
    },
    Package {
        spec: "lxml",
        import_name: "lxml",
        label: "lxml",
    },
    Package {
        spec: "pydantic",
        import_name: "pydantic",
        label: "pydantic",
    },
    Package {
        spec: "tyro",
        import_name: "tyro",
        label: "tyro",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedVenv {
    pub dir: PathBuf,
    pub python_version: String,
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    schema: u32,
    python_version: String,
    packages: Vec<String>,
}

impl ManagedVenv {
    #[must_use]
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            python_version: PYTHON_VERSION.to_string(),
            packages: DEFAULT_PACKAGES
                .iter()
                .map(|package| package.spec.to_string())
                .collect(),
        }
    }

    #[must_use]
    pub fn python_path(&self) -> PathBuf {
        if cfg!(windows) {
            self.dir.join("Scripts").join("python.exe")
        } else {
            self.dir.join("bin").join("python")
        }
    }

    fn lock_dir(&self) -> PathBuf {
        let mut name = self
            .dir
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_default();
        name.push(LOCK_SUFFIX);
        self.dir.with_file_name(name)
    }

    fn marker(&self) -> Marker {
        let mut packages = self.packages.clone();
        packages.sort();
        packages.dedup();
        Marker {
            schema: MARKER_SCHEMA,
            python_version: self.python_version.clone(),
            packages,
        }
    }

    #[must_use]
    pub fn is_current(&self) -> bool {
        self.python_path().is_file()
            && std::fs::read_to_string(self.dir.join(MARKER_FILE))
                .ok()
                .and_then(|json| serde_json::from_str::<Marker>(&json).ok())
                .is_some_and(|marker| marker == self.marker())
    }

    pub async fn ensure(&self, progress: &(dyn Fn(&str) + Sync)) -> Result<PathBuf, KernelError> {
        if self.is_current() {
            return Ok(self.python_path());
        }
        let uv = find_uv().ok_or(KernelError::NoUv)?;
        if let Some(parent) = self.dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _lock = BootstrapLock::acquire(&self.lock_dir()).await?;
        if self.is_current() {
            return Ok(self.python_path());
        }
        if !self.python_path().is_file() {
            progress(&format!(
                "setting up the kernel's python {} in {}",
                self.python_version,
                self.dir.display()
            ));
            run(&uv, &["venv", "--python", &self.python_version], &self.dir).await?;
        }
        if !self.packages.is_empty() {
            progress(&format!(
                "installing kernel packages: {}",
                self.packages.join(", ")
            ));
            let python = self.python_path();
            let mut args: Vec<&str> = vec!["pip", "install", "--quiet", "--python"];
            let python_str = python.to_string_lossy().into_owned();
            args.push(&python_str);
            args.extend(self.packages.iter().map(String::as_str));
            run(&uv, &args, &self.dir).await?;
        }
        std::fs::write(
            self.dir.join(MARKER_FILE),
            serde_json::to_string_pretty(&self.marker())?,
        )?;
        Ok(self.python_path())
    }
}

async fn run(uv: &Path, args: &[&str], venv_dir: &Path) -> Result<(), KernelError> {
    let mut command = tokio::process::Command::new(uv);
    command
        .args(args)
        .env("UV_NO_PROGRESS", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    if args.first() == Some(&"venv") {
        command.arg(venv_dir);
    }
    let output = command.output().await?;
    if output.status.success() {
        return Ok(());
    }
    Err(KernelError::Bootstrap(format!(
        "uv {} failed: {}",
        args.first().unwrap_or(&""),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[must_use]
pub fn find_uv() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MUSH_UV") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(if cfg!(windows) { "uv.exe" } else { "uv" }))
        .find(|candidate| candidate.is_file())
}

pub async fn available_imports(python: &Path, names: &[&str]) -> Vec<String> {
    const PROBE: &str = "import importlib.util, sys\nprint(' '.join(m for m in sys.argv[1:] if importlib.util.find_spec(m) is not None))";
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(python)
            .arg("-c")
            .arg(PROBE)
            .args(names)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await;
    match output {
        Ok(Ok(output)) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

struct BootstrapLock {
    dir: PathBuf,
}

impl BootstrapLock {
    async fn acquire(dir: &Path) -> Result<Self, KernelError> {
        loop {
            match std::fs::create_dir(dir) {
                Ok(()) => {
                    let _ = std::fs::write(dir.join("pid"), std::process::id().to_string());
                    return Ok(Self {
                        dir: dir.to_path_buf(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(dir)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if stale {
                        let _ = std::fs::remove_dir_all(dir);
                        continue;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for BootstrapLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_venv_is_current_only_with_a_matching_marker_and_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let venv = ManagedVenv::at(dir.path().join("venv"));
        assert!(!venv.is_current());

        std::fs::create_dir_all(venv.python_path().parent().unwrap()).unwrap();
        std::fs::write(venv.python_path(), "").unwrap();
        assert!(!venv.is_current());

        std::fs::write(
            venv.dir.join(MARKER_FILE),
            serde_json::to_string(&venv.marker()).unwrap(),
        )
        .unwrap();
        assert!(venv.is_current());

        let mut more = venv.clone();
        more.packages.push("polars".into());
        assert!(!more.is_current());
    }

    #[tokio::test]
    async fn a_current_venv_needs_no_uv() {
        let dir = tempfile::tempdir().unwrap();
        let venv = ManagedVenv::at(dir.path().join("venv"));
        std::fs::create_dir_all(venv.python_path().parent().unwrap()).unwrap();
        std::fs::write(venv.python_path(), "").unwrap();
        std::fs::write(
            venv.dir.join(MARKER_FILE),
            serde_json::to_string(&venv.marker()).unwrap(),
        )
        .unwrap();

        let python = venv.ensure(&|_| {}).await.unwrap();

        assert_eq!(python, venv.python_path());
    }

    #[tokio::test]
    async fn the_probe_lists_only_importable_modules() {
        let Ok(python) = crate::python::resolve_python(None) else {
            return;
        };

        let found =
            available_imports(&python, &["json", "definitely_not_a_module_xyz", "sys"]).await;

        assert_eq!(found, vec!["json", "sys"]);
    }
}
