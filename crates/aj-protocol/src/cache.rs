//! The only state a Runner keeps.
//!
//! Layout and rules are `docs/specs/FILE_INTEGRITY.md`, accepted 2026-08-04,
//! which specifies them for the Runner rather than leaving them to it:
//!
//! ```text
//! cache/ch/ec/ks/<fileId>     ← checksum "checks…" decides the path,
//!                               the file id decides the name
//! ```
//!
//! Three levels of 256 keep any one directory small enough that listing it
//! stays cheap. The checksum names the bytes, so a re-published package under
//! the same problem version lands in a different entry and the stale-tests
//! problem never arises.
//!
//! Losing the whole cache costs a download, not a result.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::client::Server;
use crate::error::{Error, Result};

pub struct Cache {
    root: PathBuf,
    max_bytes: u64,
    /// Entries a job is reading right now. Eviction skips these — deleting a
    /// package out from under a running evaluation would turn a full disk into
    /// a wrong verdict.
    in_use: Mutex<HashSet<String>>,
}

/// A cached file, held open for as long as somebody is using it.
///
/// The refcount is released by dropping this, so it is released on every path
/// out of an evaluation including a panic — which is the only way to get that
/// right without remembering to.
pub struct Entry {
    cache: Arc<Cache>,
    file_id: String,
    path: PathBuf,
}

impl Entry {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("file_id", &self.file_id)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.cache.release(&self.file_id);
    }
}

impl Cache {
    pub fn new(root: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self {
            root: root.into(),
            max_bytes,
            in_use: Mutex::new(HashSet::new()),
        }
    }

    /// `cache/ch/ec/ks/<fileId>` — the checksum decides the path, the id
    /// decides the name, and **both must match** for an entry to be used.
    fn path_for(&self, sha256: &str, file_id: &str) -> PathBuf {
        let key = sha256.to_ascii_lowercase();
        let pair = |n: usize| key.get(n * 2..n * 2 + 2).unwrap_or("__");
        self.root
            .join(pair(0))
            .join(pair(1))
            .join(pair(2))
            .join(file_id)
    }

    /// The file, from disk or from the Server.
    ///
    /// A download goes to a temporary name and is renamed into place **only
    /// after the checksum verifies**, so an entry is either complete and
    /// correct or absent. There is no third state for a later run to trip over.
    pub async fn fetch(
        self: &Arc<Self>,
        server: &Server,
        file_id: &str,
        sha256: &str,
    ) -> Result<Entry> {
        let path = self.path_for(sha256, file_id);
        self.hold(file_id);
        let entry = Entry {
            cache: Arc::clone(self),
            file_id: file_id.to_owned(),
            path: path.clone(),
        };

        if path.exists() {
            tracing::debug!(file_id, "cache hit");
            self.touch(&path);
            return Ok(entry);
        }

        let temporary = path.with_extension("partial");
        let actual = server.download_to(file_id, &temporary).await?;

        if !actual.eq_ignore_ascii_case(sha256) {
            // Discarded **before** the failure is reported, so a retry does not
            // read the same bad bytes again.
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(Error::ChecksumMismatch {
                what: format!("file {file_id}"),
                expected: sha256.to_ascii_lowercase(),
                actual,
            });
        }

        tokio::fs::rename(&temporary, &path).await?;
        tracing::info!(file_id, "cached");

        self.evict_to_fit();
        Ok(entry)
    }

    fn hold(&self, file_id: &str) {
        self.in_use
            .lock()
            .expect("the cache lock is never poisoned")
            .insert(file_id.to_owned());
    }

    fn release(&self, file_id: &str) {
        self.in_use
            .lock()
            .expect("the cache lock is never poisoned")
            .remove(file_id);
    }

    /// Records that an entry was wanted, for the eviction order.
    ///
    /// A sidecar marker rather than the entry's own timestamp: `mtime` does not
    /// move when a file is read, and `atime` is unreliable on a filesystem
    /// mounted `relatime`, which most are. Rewriting an empty marker is the
    /// portable way to say "now".
    fn touch(&self, path: &Path) {
        let marker = self.marker_for(path);
        if let Some(parent) = marker.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&marker, []);
    }

    fn marker_for(&self, path: &Path) -> PathBuf {
        self.root
            .join("used")
            .join(path.file_name().unwrap_or_default())
    }

    /// Least recently used out first, by whole entries.
    ///
    /// **By whole entries**, because a partially deleted one is a corrupt one.
    /// An unbounded cache on a long-lived Runner is a disk-full outage waiting
    /// for the busiest day of the year, so this must exist and must be bounded.
    fn evict_to_fit(&self) {
        let mut entries = self.entries();
        let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum();
        if total <= self.max_bytes {
            return;
        }

        // Oldest marker first. An entry nobody has ever touched sorts as the
        // epoch and goes first, which is right: it was downloaded and never read.
        entries.sort_by_key(|(_, _, used)| *used);

        let in_use = self
            .in_use
            .lock()
            .expect("the cache lock is never poisoned");
        for (path, size, _) in entries {
            if total <= self.max_bytes {
                break;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if in_use.contains(&name) {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                let _ = std::fs::remove_file(self.marker_for(&path));
                total = total.saturating_sub(size);
                tracing::info!(entry = %name, "evicted");
            }
        }
    }

    /// Every cached file, with its size and when it was last wanted.
    fn entries(&self) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
        let mut found = Vec::new();
        for first in read_dir(&self.root) {
            // `used/` holds the markers, not the entries.
            if first.file_name().is_some_and(|n| n == "used") {
                continue;
            }
            for second in read_dir(&first) {
                for third in read_dir(&second) {
                    for entry in read_dir(&third) {
                        let Ok(metadata) = entry.metadata() else {
                            continue;
                        };
                        if !metadata.is_file() {
                            continue;
                        }
                        let used = std::fs::metadata(self.marker_for(&entry))
                            .and_then(|m| m.modified())
                            .unwrap_or(std::time::UNIX_EPOCH);
                        found.push((entry, metadata.len(), used));
                    }
                }
            }
        }
        found
    }
}

fn read_dir(path: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("aj-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn the_checksum_decides_the_path_and_the_id_decides_the_name() {
        let cache = Cache::new(scratch("layout"), 1 << 30);
        let path = cache.path_for(
            "checks0000000000000000000000000000000000000000000000000000000000",
            "a-file-id",
        );

        let tail: Vec<_> = path
            .components()
            .rev()
            .take(4)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert_eq!(tail, vec!["a-file-id", "ks", "ec", "ch"]);
    }

    #[test]
    fn an_uppercase_checksum_lands_in_the_same_place() {
        let cache = Cache::new(scratch("case"), 1 << 30);
        let lower = "abcdef0000000000000000000000000000000000000000000000000000000000";
        assert_eq!(
            cache.path_for(lower, "id"),
            cache.path_for(&lower.to_ascii_uppercase(), "id"),
        );
    }

    #[test]
    fn eviction_takes_the_least_recently_wanted_and_spares_what_is_open() {
        let root = scratch("evict");
        let cache = Cache::new(&root, 200);

        let mut paths = Vec::new();
        for (n, id) in ["old", "middle", "held"].iter().enumerate() {
            let path = cache.path_for(
                "aabbcc0000000000000000000000000000000000000000000000000000000000",
                id,
            );
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, vec![0u8; 100]).unwrap();
            // Touched in order, so `old` is the least recently wanted.
            std::thread::sleep(std::time::Duration::from_millis(20));
            cache.touch(&path);
            paths.push((n, path));
        }

        cache.hold("held");
        cache.evict_to_fit();

        assert!(!paths[0].1.exists(), "the oldest should have gone");
        assert!(
            paths[2].1.exists(),
            "an entry a job is reading is never evicted"
        );
    }
}
