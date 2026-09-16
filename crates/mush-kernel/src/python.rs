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
        if let Some(path) = explicit {
            return check(path).map(Self::Explicit);
        }
        if let Some(path) = std::env::var_os(PYTHON_ENV) {
            return check(Path::new(&path)).map(Self::Explicit);
        }
        if let Some(venv) = managed
            && (venv.is_current() || crate::venv::find_uv().is_some())
        {
            return Ok(Self::Managed(venv.clone()));
        }
        resolve_python(None).map(Self::System)
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

pub fn resolve_python(explicit: Option<&Path>) -> Result<PathBuf, KernelError> {
    if let Some(path) = explicit {
        return check(path);
    }
    if let Some(path) = std::env::var_os(PYTHON_ENV) {
        return check(Path::new(&path));
    }
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

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_explicit_path_is_an_error() {
        let error = resolve_python(Some(Path::new("/definitely/not/python"))).unwrap_err();
        assert!(matches!(error, KernelError::PythonMissing(_)));
    }

    #[test]
    fn a_managed_venv_is_planned_when_current_even_without_uv() {
        let dir = tempfile::tempdir().unwrap();
        let venv = crate::venv::ManagedVenv::at(dir.path().join("venv"));

        let plan = PythonPlan::resolve(Some(Path::new("/definitely/not/python")), Some(&venv));
        assert!(matches!(plan, Err(KernelError::PythonMissing(_))));

        let plan = PythonPlan::resolve(None, Some(&venv)).unwrap();
        if crate::venv::find_uv().is_some() {
            assert_eq!(plan, PythonPlan::Managed(venv));
        } else {
            assert!(matches!(plan, PythonPlan::System(_)));
        }
    }

    #[test]
    fn python3_is_found_on_the_path() {
        let path = resolve_python(None).unwrap();
        assert!(path.is_file(), "{path:?}");
    }
}
