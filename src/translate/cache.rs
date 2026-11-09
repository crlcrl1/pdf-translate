use anyhow::Result;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub(super) fn cache_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("pdf-translate").join("translations.json")
}

pub(super) fn load(path: &Path) -> Result<HashMap<String, String>> {
    match fs::read(path) {
        Ok(data) => Ok(serde_json::from_slice(&data)?),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(HashMap::new()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn merge(path: &Path, updates: HashMap<String, String>) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Lock a stable sibling: the data file itself is atomically replaced.
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    guard.lock()?;
    let mut cache = load(path)?;
    cache.extend(updates);
    crate::output::atomic_write(path, |file| Ok(serde_json::to_writer(file, &cache)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_updates_preserve_each_others_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("translations.json");
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for key in ["one", "two"] {
                scope.spawn(|| {
                    barrier.wait();
                    merge(&path, HashMap::from([(key.to_owned(), key.to_owned())])).unwrap();
                });
            }
        });
        assert_eq!(load(&path).unwrap().len(), 2);
    }
}
