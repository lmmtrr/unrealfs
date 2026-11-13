use crate::reader::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub type Producer = Box<dyn Fn() -> Result<Vec<(PathBuf, Vec<u8>)>> + Send + Sync>;

struct Pending {
    producer: Producer,
    done: OnceLock<Result<()>>,
}

fn pending() -> &'static Mutex<HashMap<String, Arc<Pending>>> {
    static PENDING: OnceLock<Mutex<HashMap<String, Arc<Pending>>>> = OnceLock::new();
    PENDING.get_or_init(Default::default)
}

fn normalize(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) { text.to_lowercase() } else { text }
}

fn key(path: &Path) -> String {
    normalize(&path.with_extension(""))
}

fn write_all(files: Vec<(PathBuf, Vec<u8>)>) -> Result<()> {
    for (path, bytes) in files {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {}", parent.display(), e))?;
        }
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {}", path.display(), e))?;
    }
    Ok(())
}

pub fn register(package: &Path, producer: Producer) {
    pending().lock().unwrap().insert(key(package), Arc::new(Pending { producer, done: OnceLock::new() }));
}

pub fn ensure(package: &Path) -> Result<()> {
    let Some(entry) = pending().lock().unwrap().get(&key(package)).cloned() else { return Ok(()) };
    entry.done.get_or_init(|| (entry.producer)().and_then(write_all)).clone()
}

pub fn forget(dir: &Path) {
    let prefix = format!("{}/", normalize(dir).trim_end_matches('/'));
    pending().lock().unwrap().retain(|k, _| !k.starts_with(&prefix));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn writes_once_on_first_use() {
        let dir = std::env::temp_dir().join(format!("unrealfs_lazy_{}", std::process::id()));
        let uasset = dir.join("Game").join("A.uasset");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let uexp = uasset.with_extension("uexp");
        let target = uexp.clone();
        register(&uasset, Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(vec![(target.clone(), vec![1, 2, 3])])
        }));
        assert!(!uexp.exists());
        ensure(&uasset).unwrap();
        ensure(&dir.join("Game").join("A.umap")).unwrap();
        assert_eq!(std::fs::read(&uexp).unwrap(), [1, 2, 3]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        forget(&dir);
        assert!(pending().lock().unwrap().get(&key(&uasset)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
