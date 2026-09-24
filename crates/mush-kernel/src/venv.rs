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
    Package {
        spec: "dill",
        import_name: "dill",
        label: "dill",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedVenv {
    pub dir: PathBuf,
    pub python_version: String,
    pub packages: Vec<String>,
    pub skills: Vec<SkillPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPackage {
    pub import_name: String,
    pub dir: PathBuf,
}

impl SkillPackage {
    fn installed(&self) -> Option<InstalledSkill> {
        let pyproject = std::fs::read(self.dir.join("pyproject.toml")).ok()?;
        Some(InstalledSkill {
            import_name: self.import_name.clone(),
            dir: self.dir.clone(),
            pyproject_sha256: sha256_hex(&pyproject),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InstalledSkill {
    import_name: String,
    dir: PathBuf,
    pyproject_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    schema: u32,
    python_version: String,
    packages: Vec<String>,
    #[serde(default)]
    skills: Vec<InstalledSkill>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
            skills: Vec::new(),
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

    fn base_marker(&self) -> Marker {
        let mut packages = self.packages.clone();
        packages.sort();
        packages.dedup();
        Marker {
            schema: MARKER_SCHEMA,
            python_version: self.python_version.clone(),
            packages,
            skills: Vec::new(),
        }
    }

    fn read_marker(&self) -> Option<Marker> {
        std::fs::read_to_string(self.dir.join(MARKER_FILE))
            .ok()
            .and_then(|json| serde_json::from_str::<Marker>(&json).ok())
    }

    fn write_marker(&self, marker: &Marker) -> Result<(), KernelError> {
        std::fs::write(
            self.dir.join(MARKER_FILE),
            serde_json::to_string_pretty(marker)?,
        )?;
        Ok(())
    }

    fn base_current(&self, marker: Option<&Marker>) -> bool {
        let expected = self.base_marker();
        self.python_path().is_file()
            && marker.is_some_and(|marker| {
                marker.schema == expected.schema
                    && marker.python_version == expected.python_version
                    && marker.packages == expected.packages
            })
    }

    fn stale_skills(&self, marker: Option<&Marker>) -> Vec<(&SkillPackage, InstalledSkill)> {
        let installed = marker.map_or(&[][..], |marker| &marker.skills[..]);
        self.skills
            .iter()
            .filter_map(|skill| Some((skill, skill.installed()?)))
            .filter(|(_, wanted)| !installed.contains(wanted))
            .collect()
    }

    #[must_use]
    pub fn is_current(&self) -> bool {
        let marker = self.read_marker();
        self.base_current(marker.as_ref()) && self.stale_skills(marker.as_ref()).is_empty()
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
        let mut marker = self.read_marker();
        if !self.base_current(marker.as_ref()) {
            self.bootstrap(&uv, progress).await?;
            let mut fresh = self.base_marker();
            fresh.skills = marker.map(|old| old.skills).unwrap_or_default();
            self.write_marker(&fresh)?;
            marker = Some(fresh);
        }
        let mut marker = marker.unwrap_or_else(|| self.base_marker());
        let python = self.python_path().to_string_lossy().into_owned();
        for (skill, wanted) in self.stale_skills(Some(&marker)) {
            progress(&format!("installing python skill {}", skill.import_name));
            let dir = skill.dir.to_string_lossy().into_owned();
            let args = [
                "pip",
                "install",
                "--quiet",
                "--python",
                &python,
                "--editable",
                &dir,
            ];
            if let Err(error) = run(&uv, &args, &self.dir).await {
                progress(&format!(
                    "python skill {} failed to install and is unavailable: {error}",
                    skill.import_name
                ));
                continue;
            }
            marker
                .skills
                .retain(|installed| installed.import_name != wanted.import_name);
            marker.skills.push(wanted);
            self.write_marker(&marker)?;
        }
        Ok(self.python_path())
    }

    async fn bootstrap(
        &self,
        uv: &Path,
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<(), KernelError> {
        if !self.python_path().is_file() {
            progress(&format!(
                "setting up the kernel's python {} in {}",
                self.python_version,
                self.dir.display()
            ));
            run(uv, &["venv", "--python", &self.python_version], &self.dir).await?;
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
            run(uv, &args, &self.dir).await?;
        }
        Ok(())
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
            serde_json::to_string(&venv.base_marker()).unwrap(),
        )
        .unwrap();
        assert!(venv.is_current());

        let mut more = venv.clone();
        more.packages.push("polars".into());
        assert!(!more.is_current());
    }

    #[test]
    fn a_skill_is_stale_until_the_marker_holds_its_pyproject_hash() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("word-count");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("pyproject.toml"),
            "[project]\nname = \"a\"\n",
        )
        .unwrap();
        let mut venv = ManagedVenv::at(dir.path().join("venv"));
        venv.skills.push(SkillPackage {
            import_name: "word_count".into(),
            dir: skill_dir.clone(),
        });
        std::fs::create_dir_all(venv.python_path().parent().unwrap()).unwrap();
        std::fs::write(venv.python_path(), "").unwrap();
        let mut marker = venv.base_marker();
        venv.write_marker(&marker).unwrap();
        assert!(!venv.is_current());

        marker.skills.push(venv.skills[0].installed().unwrap());
        venv.write_marker(&marker).unwrap();
        assert!(venv.is_current());

        std::fs::write(
            skill_dir.join("pyproject.toml"),
            "[project]\nname = \"b\"\n",
        )
        .unwrap();
        assert!(!venv.is_current());

        std::fs::remove_file(skill_dir.join("pyproject.toml")).unwrap();
        assert!(venv.is_current());
    }

    #[test]
    fn a_marker_from_before_skills_still_counts_as_current() {
        let dir = tempfile::tempdir().unwrap();
        let venv = ManagedVenv::at(dir.path().join("venv"));
        std::fs::create_dir_all(venv.python_path().parent().unwrap()).unwrap();
        std::fs::write(venv.python_path(), "").unwrap();
        let marker = venv.base_marker();
        std::fs::write(
            venv.dir.join(MARKER_FILE),
            serde_json::json!({
                "schema": marker.schema,
                "python_version": marker.python_version,
                "packages": marker.packages,
            })
            .to_string(),
        )
        .unwrap();

        assert!(venv.is_current());
    }

    #[tokio::test]
    async fn a_current_venv_needs_no_uv() {
        let dir = tempfile::tempdir().unwrap();
        let venv = ManagedVenv::at(dir.path().join("venv"));
        std::fs::create_dir_all(venv.python_path().parent().unwrap()).unwrap();
        std::fs::write(venv.python_path(), "").unwrap();
        std::fs::write(
            venv.dir.join(MARKER_FILE),
            serde_json::to_string(&venv.base_marker()).unwrap(),
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
