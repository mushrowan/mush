use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

pub(crate) fn load_json<T: DeserializeOwned>(path: Option<&Path>) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path?).ok()?).ok()
}

pub(crate) fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = mush_ai::private_io::write_private(path, bytes);
}

pub(crate) fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) {
    if let Ok(bytes) = serde_json::to_vec_pretty(value) {
        write_private(path, bytes);
    }
}
