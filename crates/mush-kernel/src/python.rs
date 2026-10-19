use std::path::{Path, PathBuf};

use crate::KernelError;

pub const PYTHON_ENV: &str = "MUSH_KERNEL_PYTHON";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PythonPlan {
    Explicit(PathBuf),
    Managed(crate::venv::ManagedVenv),
    System(PathBuf),
}

impl PythonPlan {
    pub fn resolve(
        explicit: Option<&Path>,
        managed: Option<&crate::venv::ManagedVenv>,
    ) -> Result<Self, KernelError> {
        let environment = std::env::var_os(PYTHON_ENV);
        Self::resolve_with(
            explicit,
            environment.as_deref().map(Path::new),
            managed,
            crate::venv::find_uv().is_some(),
        )
    }

    fn resolve_with(
        explicit: Option<&Path>,
        environment: Option<&Path>,
        managed: Option<&crate::venv::ManagedVenv>,
        uv_available: bool,
    ) -> Result<Self, KernelError> {
        if let Some(path) = explicit.or(environment) {
            return check(path).map(Self::Explicit);
        }
        if let Some(venv) = managed
            && (venv.is_current() || uv_available)
        {
            return Ok(Self::Managed(venv.clone()));
        }
        system_python().map(Self::System)
    }

    pub async fn realise(&self, progress: &(dyn Fn(&str) + Sync)) -> Result<PathBuf, KernelError> {
        match self {
            Self::Explicit(path) | Self::System(path) => Ok(path.clone()),
            Self::Managed(venv) => venv.ensure(progress).await,
        }
    }

    #[must_use]
    pub fn managed(&self) -> bool {
        matches!(self, Self::Managed(_))
    }

    #[must_use]
    pub fn python_if_present(&self) -> Option<PathBuf> {
        match self {
            Self::Explicit(path) | Self::System(path) => Some(path.clone()),
            Self::Managed(venv) => venv.is_current().then(|| venv.python_path()),
        }
    }
}

pub fn runtime_read_paths(python: &Path) -> Result<Vec<PathBuf>, KernelError> {
    let resolved = python.canonicalize()?;
    let mut paths = Vec::new();
    for executable in [python, resolved.as_path()] {
        if executable.starts_with("/nix/store") {
            continue;
        }
        match executable
            .parent()
            .filter(|bin| bin.file_name().is_some_and(|name| name == "bin"))
            .and_then(Path::parent)
        {
            Some(root) if root.join("pyvenv.cfg").is_file() => paths.push(root.to_path_buf()),
            _ => paths.push(executable.to_path_buf()),
        }
    }
    Ok(paths)
}

pub(crate) fn system_python() -> Result<PathBuf, KernelError> {
    ["python3", "python"]
        .into_iter()
        .find_map(find_on_path)
        .ok_or(KernelError::NoPython)
}

fn check(path: &Path) -> Result<PathBuf, KernelError> {
    if path.components().count() == 1 {
        return find_on_path(&path.to_string_lossy())
            .ok_or_else(|| KernelError::PythonMissing(path.to_path_buf()));
    }
    if path.is_file() {
        Ok(path.to_path_buf())
    } else {
        Err(KernelError::PythonMissing(path.to_path_buf()))
    }
}

pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_selection_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let venv = crate::venv::ManagedVenv::at(dir.path().join("venv"));

        let plan = PythonPlan::resolve_with(
            Some(Path::new("/definitely/not/python")),
            None,
            Some(&venv),
            true,
        );
        assert!(matches!(plan, Err(KernelError::PythonMissing(_))));

        let plan = PythonPlan::resolve_with(None, None, Some(&venv), true).unwrap();
        assert_eq!(plan, PythonPlan::Managed(venv));
    }

    #[test]
    fn environment_python_precedes_managed_venv() {
        let dir = tempfile::tempdir().unwrap();
        let venv = crate::venv::ManagedVenv::at(dir.path().join("venv"));
        let current_exe = std::env::current_exe().unwrap();

        let plan = PythonPlan::resolve_with(None, Some(&current_exe), Some(&venv), true).unwrap();
        assert_eq!(plan, PythonPlan::Explicit(current_exe));
    }

    #[test]
    fn python3_is_found_on_the_path() {
        let path = system_python().unwrap();
        assert!(path.is_file(), "{path:?}");
    }
}
