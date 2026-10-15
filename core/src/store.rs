//! The reader's one file: the matches it saw end, as JSON at the path the
//! app passes in `--ended`.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;
use std::{fs, io};
use tracing::warn;

/// Reads `path` as JSON, falling back to the default when it is missing or
/// unreadable.
pub fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    match fs::read(path) {
        // An editor may have saved the file with a byte order mark, which
        // JSON does not allow.
        Ok(bytes) => serde_json::from_slice(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes)).unwrap_or_else(|e| {
            warn!("Ignoring unreadable {}: {e}", path.display());
            T::default()
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => T::default(),
        Err(e) => {
            warn!("Could not read {}: {e}", path.display());
            T::default()
        }
    }
}

/// Writes `value` to `path` as JSON, through a temporary file so a crash
/// mid-write leaves the old file whole.
pub fn save<T: Serialize + ?Sized>(path: &Path, value: &T) {
    if let Err(e) = try_save(path, value) {
        warn!("Could not write {}: {e}", path.display());
    }
}

fn try_save<T: Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}
