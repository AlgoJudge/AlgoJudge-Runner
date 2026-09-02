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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::client::Server;
use crate::error::{Error, Result};

/// Where a Runner says, on disk, which entries it is reading.
const HOLDING: &str = "holding";

/// What a download in progress is called, until `rename` publishes it.
///
/// The whole suffix is `.<instance>.partial`: the instance is in it so that two
/// Runners sharing this volume never write to one file, and it is **appended**
/// rather than substituted so that a file id containing a dot — which
/// [`a_name`] allows — cannot produce the name of a different entry.
const PARTIAL: &str = ".partial";

pub struct Cache {
    root: PathBuf,
    max_bytes: u64,
    /// Whose Runner this is, in a name that survives a restart.
    ///
    /// The same value and the same reason as the sandbox's instance: a marker
    /// left behind by a crash carries the id this Runner has **again**, so it
    /// can clear its own and nobody else's.
    instance: String,
    /// How many holders each entry has **in this process**.
    ///
    /// A count and not a set. The same file can be held twice — a trial and a
    /// job of one problem, or any concurrent claim somebody adds later — and
    /// with a set the first `Entry` dropped removed the only member, leaving
    /// the second holder's package evictable while it was still being read.
    /// `Entry`'s own comment called this a refcount before it was one.
    in_use: Mutex<HashMap<String, usize>>,
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
    pub fn new(root: impl Into<PathBuf>, max_bytes: u64, instance: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            max_bytes,
            instance: instance.into(),
            in_use: Mutex::new(HashMap::new()),
        }
    }

    /// Removes the holding markers **this Runner** left behind, and says how
    /// many there were.
    ///
    /// **Run at start**, like the sandbox's sweep and for the same reason: a
    /// Runner that stopped mid-evaluation left markers saying it was reading
    /// entries it is not reading any more, and an entry nobody can evict is a
    /// disk that fills. The instance name survives a restart, so this finds its
    /// own and leaves every other Runner's alone.
    ///
    /// A Runner retired for good does leave its markers, and those entries stay
    /// un-evictable. The leak is bounded by what it held at that moment — a
    /// package or two — and the alternative is inventing an expiry, which would
    /// be a policy nobody has chosen.
    pub fn sweep(&self) -> usize {
        let mut cleared = 0;
        for held in read_dir(&self.root.join(HOLDING)) {
            if std::fs::remove_file(held.join(&self.instance)).is_ok() {
                cleared += 1;
            }
            // Refuses while somebody else still holds it, which is the answer.
            let _ = std::fs::remove_dir(&held);
        }
        if cleared > 0 {
            tracing::warn!(
                cleared,
                "cache entries held by a previous run were released"
            );
        }

        // **Only this Runner's own.** A partial carrying another instance's name
        // may be a download happening right now; ours cannot be, because we are
        // starting. Eviction never removes these, so without this a Runner that
        // was killed mid-download would leave the bytes occupying the budget
        // until somebody noticed them.
        let mine = format!(".{}{PARTIAL}", self.instance);
        let mut abandoned = 0;
        for (path, _, _) in self.entries() {
            let ours = path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(&mine));
            if ours && std::fs::remove_file(&path).is_ok() {
                abandoned += 1;
            }
        }
        if abandoned > 0 {
            tracing::warn!(abandoned, "downloads left unfinished by a previous run were removed");
        }

        cleared
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
    /// Where this Runner writes an entry it is still downloading.
    ///
    /// **One name per Runner, appended, never substituted.** Two Runners sharing
    /// a cache volume miss the same entry at the start of a contest and both
    /// fetch it; under one name they wrote to one file through two truncating
    /// descriptors. Nothing caught it: the checksum is computed from the stream
    /// rather than read back (`Server::download_to`), so each verified its own
    /// bytes while the file held both, and `rename` published the interleaved
    /// result as a correct entry — which a later hit never re-checks.
    ///
    /// `with_extension` was the other half of it. [`a_name`] allows dots, so a
    /// file id of `a.b` became `a.partial`, which is where the entry named `a`
    /// would put its own.
    fn partial_for(&self, path: &Path) -> PathBuf {
        path.with_file_name(format!(
            "{}.{}{PARTIAL}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            self.instance,
        ))
    }

    pub async fn fetch(
        self: &Arc<Self>,
        server: &Server,
        file_id: &str,
        sha256: &str,
    ) -> Result<Entry> {
        // Before either is used as a name on this host, and before anything is
        // held under one. See [`a_name`].
        a_name("the file id", file_id)?;
        a_name("the checksum", sha256)?;

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

        let temporary = self.partial_for(&path);
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

        // Gone means the entry can be fetched again, not that this evaluation
        // is over. See [`Error::Vanished`].
        tokio::fs::rename(&temporary, &path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::Vanished {
                    what: format!("the download of file {file_id}"),
                }
            } else {
                Error::Io(e)
            }
        })?;
        // Wanted **now**, not at the first later hit. An entry with no marker
        // sorts as the epoch, so without this the package downloaded a moment
        // ago is the first thing evicted and every entry older than it survives
        // — the eviction order upside down, and a cache that re-downloads the
        // one package a contest is about to ask for again.
        self.touch(&path);
        tracing::info!(file_id, "cached");

        self.evict_to_fit();
        Ok(entry)
    }

    fn hold(&self, file_id: &str) {
        let mut in_use = self
            .in_use
            .lock()
            .expect("the cache lock is never poisoned");
        let holders = in_use.entry(file_id.to_owned()).or_insert(0);
        *holders += 1;

        if *holders == 1 {
            // The first holder in this process writes the marker every other
            // process can see. Later ones are already covered by it.
            let marker = self.holding_marker(file_id);
            if let Some(parent) = marker.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&marker, []);
        }
    }

    fn release(&self, file_id: &str) {
        let mut in_use = self
            .in_use
            .lock()
            .expect("the cache lock is never poisoned");
        let Some(holders) = in_use.get_mut(file_id) else {
            return;
        };
        *holders -= 1;

        if *holders == 0 {
            in_use.remove(file_id);
            let _ = std::fs::remove_file(self.holding_marker(file_id));
            let _ = std::fs::remove_dir(self.holding_dir(file_id));
        }
    }

    /// `holding/<fileId>/<instance>` — one directory per entry, one file per
    /// Runner holding it. A directory rather than a name with both parts in it,
    /// so asking "is anybody holding this" is a listing rather than a guess at
    /// where one name ends and the other begins.
    fn holding_dir(&self, file_id: &str) -> PathBuf {
        self.root.join(HOLDING).join(file_id)
    }

    fn holding_marker(&self, file_id: &str) -> PathBuf {
        self.holding_dir(file_id).join(&self.instance)
    }

    /// Whether **any** Runner on this host says it is reading this entry.
    ///
    /// **The filesystem is the shared state, because the lock is not.** Two
    /// Runners pointed at one cache volume share the files and nothing else, so
    /// a `Mutex` in one process says nothing about what the other is reading —
    /// and eviction runs on every download. Several Runners on one host is a
    /// supported arrangement, and one cache volume between them is the saving
    /// an operator reaches for first.
    ///
    /// This process's own holds are here too: `hold` writes the marker before
    /// anything can read it, so what is on disk covers what is in memory and
    /// eviction needs no lock at all.
    fn held_by_anybody(&self, file_id: &str) -> bool {
        !read_dir(&self.holding_dir(file_id)).is_empty()
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

        for (path, size, _) in entries {
            if total <= self.max_bytes {
                break;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // **A download in progress cannot say it is being used.** Its
            // holder took the hold under the file id, and this file is named
            // for the id *and* the instance, so `held_by_anybody` looks in a
            // directory that will never exist and answers no. Its size is
            // already in `total` above — it is counted, just never chosen.
            //
            // Left in, it sorted *first*: `touch` runs only after `rename`, so
            // a partial has no marker and `entries` dates it to the epoch. The
            // least recently used entry out first meant the one being written
            // right now.
            //
            // A partial nobody is writing is reclaimed by `sweep` at the next
            // start of the Runner that left it.
            if name.ends_with(PARTIAL) {
                continue;
            }
            if self.held_by_anybody(&name) {
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
            // `used/` and `holding/` are the Runner's own bookkeeping beside
            // the entries, not entries themselves. A shard is two characters of
            // the checksum; neither of these is.
            if first
                .file_name()
                .is_some_and(|n| n == "used" || n == HOLDING)
            {
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

/// A name the Server chose, about to become a name on this host.
///
/// **Checked rather than trusted.** The Server is not an attacker, but the
/// Runner's own threat model puts the boundary at the host, and every other
/// path this product builds from input it did not write is validated — the
/// package archive has refused these names since it existed. Here they were
/// taken verbatim: a `sha256` of `"../../.."` yields `".."` for each of the
/// three pairs and walks straight out of the cache root, a `file_id` carrying a
/// separator does the same, and `download_to` splices it into a URL unescaped
/// besides.
///
/// Not a checksum test. Requiring 64 hex would also be right and would refuse a
/// Server that spells the field some other way, which is a bigger claim than
/// this needs to make: what has to be impossible is a name that is not a name.
fn a_name(what: &str, value: &str) -> Result<()> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if value.is_empty() || value == "." || value == ".." || !value.chars().all(allowed) {
        return Err(Error::Unreadable(format!(
            "{what} is {value:?}, which is not a name this Runner will put on disk"
        )));
    }
    Ok(())
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
        let cache = Cache::new(scratch("layout"), 1 << 30, "test");
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
        let cache = Cache::new(scratch("case"), 1 << 30, "test");
        let lower = "abcdef0000000000000000000000000000000000000000000000000000000000";
        assert_eq!(
            cache.path_for(lower, "id"),
            cache.path_for(&lower.to_ascii_uppercase(), "id"),
        );
    }

    /// One file over HTTP, once. Enough to drive `fetch` end to end without a
    /// Server, which is the only way to observe what a download leaves behind.
    async fn serving(body: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Read only far enough for the client to finish sending. What it
            // asked for is not interesting: this server has one file.
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                 Content-Type: application/octet-stream\r\n\r\n",
                body.len(),
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
            socket.flush().await.unwrap();
        });

        (format!("http://127.0.0.1:{port}/api/v1"), handle)
    }

    /// **The eviction order, for the entry most likely to be wanted again.**
    ///
    /// An entry with no marker sorts as the epoch, so a download that is not
    /// recorded as wanted is evicted ahead of everything cached before it — and
    /// during a contest that is the package every submission is about to ask
    /// for, re-downloaded each time.
    #[tokio::test]
    async fn a_download_is_recorded_as_wanted_when_it_arrives() {
        use sha2::Digest as _;

        const BODY: &[u8] = b"a package, as far as the cache is concerned";
        let sha256 = hex::encode(sha2::Sha256::digest(BODY));

        let cache = Arc::new(Cache::new(scratch("fresh"), 1 << 30, "test"));
        let (base, handle) = serving(BODY).await;
        let server = Server::new(&base).unwrap();

        let entry = cache.fetch(&server, "a-file-id", &sha256).await.unwrap();
        handle.await.unwrap();

        let (_, _, used) = cache
            .entries()
            .into_iter()
            .find(|(path, _, _)| path.file_name().is_some_and(|n| n == "a-file-id"))
            .expect("the download is in the cache");
        assert!(
            used > std::time::UNIX_EPOCH,
            "a fresh download sorted as the epoch, so it evicts before every older entry",
        );

        drop(entry);
    }

    const SHA: &str = "aabbcc0000000000000000000000000000000000000000000000000000000000";

    /// An entry of a known size, already in the cache.
    fn lying_there(cache: &Cache, file_id: &str, bytes: usize) -> PathBuf {
        let path = cache.path_for(SHA, file_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0u8; bytes]).unwrap();
        path
    }

    /// **A count, and not a set.**
    ///
    /// Two holders of one entry — a trial and a job of the same problem, or any
    /// concurrent claim somebody adds later — and one of them finishes. With a
    /// set, that first drop removed the only member and the entry could be
    /// deleted out from under the other, still reading it.
    #[test]
    fn an_entry_held_twice_survives_the_first_holder_letting_go() {
        let cache = Cache::new(scratch("refcount"), 100, "one-runner");
        let path = lying_there(&cache, "held-twice", 200);

        cache.hold("held-twice");
        cache.hold("held-twice");
        cache.release("held-twice");

        cache.evict_to_fit();
        assert!(path.exists(), "the second holder is still reading it");

        cache.release("held-twice");
        cache.evict_to_fit();
        assert!(!path.exists(), "nobody holds it now");
    }

    /// **Two Runners, one cache volume.**
    ///
    /// Several Runners on one host is a supported arrangement, and sharing the
    /// cache between them is the first saving an operator reaches for. The lock
    /// is per process and the files are not, so one Runner's eviction pass —
    /// which runs on every download — saw nothing of what the other was
    /// reading.
    #[test]
    fn one_runner_does_not_evict_what_another_is_reading() {
        let root = scratch("shared");
        let mine = Cache::new(&root, 100, "runner-a");
        let theirs = Cache::new(&root, 100, "runner-b");

        let path = lying_there(&mine, "theirs", 200);
        theirs.hold("theirs");

        mine.evict_to_fit();
        assert!(path.exists(), "evicted from under another Runner");

        theirs.release("theirs");
        mine.evict_to_fit();
        assert!(!path.exists(), "nobody holds it now");
    }

    /// A Runner that stopped mid-evaluation left markers behind, and only it can
    /// release them — its instance name is the one it has again after a restart.
    #[test]
    fn a_sweep_releases_this_runners_holds_and_leaves_another_runners() {
        let root = scratch("sweep");
        let before = Cache::new(&root, 1 << 30, "runner-a");
        let theirs = Cache::new(&root, 1 << 30, "runner-b");

        before.hold("an-entry");
        theirs.hold("an-entry");

        // The restart: the marker is on disk, the count in memory is gone.
        let after = Cache::new(&root, 1 << 30, "runner-a");
        assert_eq!(after.sweep(), 1, "its own hold, and only its own");
        assert!(
            after.held_by_anybody("an-entry"),
            "the other Runner is still reading it",
        );

        theirs.release("an-entry");
        assert!(!after.held_by_anybody("an-entry"));
    }

    /// The Server is not an attacker, and these are still names this Runner
    /// puts on disk. A checksum of `"../../.."` is `".."` three times over.
    #[test]
    fn a_name_that_is_not_a_name_is_refused() {
        for bad in ["", ".", "..", "../../..", "a/b", "a\\b", "sha 256", "a\0b"] {
            assert!(a_name("the file id", bad).is_err(), "{bad:?} was accepted");
        }
        for good in ["a-file-id", "0123abcd", "file.bin", "A_B-c.1", SHA] {
            assert!(a_name("the file id", good).is_ok(), "{good:?} was refused");
        }
    }

    #[test]
    fn eviction_takes_the_least_recently_wanted_and_spares_what_is_open() {
        let root = scratch("evict");
        let cache = Cache::new(&root, 200, "test");

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

    #[test]
    fn two_runners_do_not_write_to_one_temporary_file() {
        let root = scratch("partial-name");
        let ours = Cache::new(&root, 1 << 30, "runner-a");
        let theirs = Cache::new(&root, 1 << 30, "runner-b");
        let entry = ours.path_for(SHA, "a-file-id");

        assert_ne!(
            ours.partial_for(&entry),
            theirs.partial_for(&entry),
            "one temporary name between two Runners is one file between two writers",
        );
    }

    #[test]
    fn a_dotted_file_id_does_not_borrow_another_entrys_name() {
        let cache = Cache::new(scratch("dotted"), 1 << 30, "test");

        // `a_name` allows dots, so this is a file id the Server may send.
        // Substituting the extension turned it into the neighbour's name.
        let dotted = cache.partial_for(&cache.path_for(SHA, "a.b"));

        assert_ne!(dotted, cache.path_for(SHA, "a"));
        assert!(dotted.to_string_lossy().contains("a.b"));
    }

    #[test]
    fn a_download_in_progress_is_not_evicted() {
        let root = scratch("evict-partial");
        let cache = Cache::new(&root, 150, "test");

        let old = lying_there(&cache, "old", 100);
        cache.touch(&old);
        std::thread::sleep(std::time::Duration::from_millis(20));

        // The one being written right now, and the worst case: `touch` runs
        // only after `rename`, so it carries no marker and `entries` dates it
        // to the epoch — least recently used, first out.
        let arriving = cache.partial_for(&cache.path_for(SHA, "arriving"));
        std::fs::create_dir_all(arriving.parent().unwrap()).unwrap();
        std::fs::write(&arriving, vec![0u8; 100]).unwrap();

        cache.evict_to_fit();

        assert!(
            arriving.exists(),
            "a download in progress is never a candidate",
        );
        assert!(!old.exists(), "and the eviction it was skipped by still ran");
    }

    #[test]
    fn a_sweep_removes_this_runners_abandoned_download_and_leaves_another_runners() {
        let root = scratch("sweep-partial");
        let ours = Cache::new(&root, 1 << 30, "runner-a");
        let theirs = Cache::new(&root, 1 << 30, "runner-b");

        let entry = ours.path_for(SHA, "interrupted");
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        let mine = ours.partial_for(&entry);
        let yours = theirs.partial_for(&entry);
        std::fs::write(&mine, b"the first half").unwrap();
        std::fs::write(&yours, b"the first half").unwrap();

        // The restart.
        Cache::new(&root, 1 << 30, "runner-a").sweep();

        assert!(!mine.exists(), "its own unfinished download is cleared");
        assert!(yours.exists(), "another Runner may be writing that one now");
    }
}
