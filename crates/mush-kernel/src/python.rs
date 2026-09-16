use std::path::{Path, PathBuf};

use crate::KernelError;

pub const PYTHON_ENV: &str = "MUSH_KERNEL_PYTHON";

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
    fn python3_is_found_on_the_path() {
        let path = resolve_python(None).unwrap();
        assert!(path.is_file(), "{path:?}");
    }
}
