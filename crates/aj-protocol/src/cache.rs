//! The only state a Runner keeps.
//!
//! Layout and rules are `docs/specs/FILE_INTEGRITY.md`, accepted 2026-08-04 and
//! amended 2026-09-15, which specifies them for the Runner rather than leaving
//! them to it:
//!
//! ```text
//! cache/packages/ch/ec/ks/<fileId>/    ← checksum "checks…" decides the path,
//!     file                               the file id decides the name
//!     manifest.json
//!     _extracted/
//!     _build/<key>/
//! ```
//!
//! Three levels of 256 keep any one directory small enough that listing it
//! stays cheap. The checksum names the bytes, so a re-published package under
//! the same problem version lands in a different entry and the stale-tests
//! problem never arises.
//!
//! **An entry is a directory, and what it holds beside the bytes is derived
//! from them.** Unpacking an archive and compiling the judge it declares is the
//! same work for every submission to one problem, so it is done once, here,
//! under a lock every Runner sharing the volume can see. See [`Entry::lock`],
//! [`Locked::published`] and [`Locked::filling`].
//!
//! **`packages/` is a root an older Runner never looks at**, and that is what
//! makes a rollback survivable. A cache hit is "something exists at this path";
//! a Runner from before this layout would find an entry *directory* where it
//! expects the archive, hand it to a zip reader, and fail every submission of
//! every package it had cached. Under `packages/` it finds nothing, downloads
//! again, and writes its own flat entries — which this Runner's [`Cache::sweep`]
//! reclaims the next time it starts.
//!
//! Losing the whole cache costs a download, not a result.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::client::Server;
use crate::error::{Error, Result};
use crate::stopping::Stopping;

/// Where a Runner says, on disk, which entries it is reading.
const HOLDING: &str = "holding";

/// Where the eviction order is recorded, one marker per entry.
const USED: &str = "used";

/// Where the lock that keeps two Runners out of one preparation lives.
///
/// **Outside the entry it guards, and that is not tidiness.** `flock` is held
/// on an inode rather than on a name, so a lock file inside the entry is
/// unlinked by eviction — and a Runner blocked on the unlinked inode and a
/// Runner opening the re-created file are then both granted "the" lock, and
/// both unpack into the same directory believing they are alone.
///
/// **Nothing removes these**, for the same reason: removing one another Runner
/// has open is exactly how the two inodes come about. One empty file per file
/// id this Runner has ever fetched is the price, and it is a price worth
/// naming rather than a leak worth sweeping.
const LOCKS: &str = "locks";

/// The root every entry of this layout sits under.
const PACKAGES: &str = "packages";

/// What work in progress is called, until `rename` publishes it.
///
/// The whole suffix is `.<instance>.partial`: the instance is in it so that two
/// Runners sharing this volume never write to one file, and it is **appended**
/// rather than substituted so that a file id containing a dot — which
/// [`a_name`] allows — cannot produce the name of a different entry.
const PARTIAL: &str = ".partial";

/// The bytes the Server served, inside the entry directory.
const FILE: &str = "file";

/// What this entry holds, and which archive it was derived from.
const MANIFEST: &str = "manifest.json";

/// How often a Runner waiting for another's preparation asks again.
///
/// **Asked rather than blocked on**, so that the wait can hear a stop. A
/// blocking `flock` parks a thread that dropping the future does not interrupt
/// and that the runtime waits for at shutdown — so a stop arriving during
/// somebody else's judge build would end in a `SIGKILL`, which is not a slower
/// release but none.
const LOCK_POLL: Duration = Duration::from_millis(100);

pub struct Cache {
    root: PathBuf,
    /// The cache as the **container runtime's daemon** sees it, where that
    /// differs from this process's view.
    ///
    /// A judge's container is given the package's `tests/` and the program
    /// built from it straight out of here, and a bind mount is resolved by the
    /// daemon — so a Runner in a container of its own has to be told the other
    /// view. `None` means the two are the same path.
    host_root: Option<PathBuf>,
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
    in_use: Mutex<HashMap<String, usize>>,
}

/// A cached file, held against eviction for as long as somebody is using it.
///
/// Against eviction, not open: this carries a path, so a reader that has not
/// opened it yet is still racing anybody's `evict_to_fit`.
///
/// The refcount is released by dropping this, so it is released on every path
/// out of an evaluation including a panic — which is the only way to get that
/// right without remembering to.
pub struct Entry {
    cache: Arc<Cache>,
    file_id: String,
    sha256: String,
    dir: PathBuf,
    file: PathBuf,
}

impl Entry {
    /// The bytes the Server served.
    pub fn path(&self) -> &Path {
        &self.file
    }

    /// The entry directory, which holds those bytes and everything derived
    /// from them.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A path inside this entry as the **daemon** sees it.
    pub fn on_host(&self, inside: &Path) -> PathBuf {
        self.cache.on_host(inside)
    }

    /// Weighs the whole cache again, because this entry has grown.
    ///
    /// **Preparing a package is what makes an entry larger after it arrived**,
    /// and eviction otherwise runs only where something was downloaded — so a
    /// cache that hits every time would unpack and build into itself for ever
    /// without ever being asked whether it still fits.
    pub fn evict_to_fit(&self) {
        self.cache.evict_to_fit();
    }

    /// Keeps every other Runner out of this entry's preparation.
    ///
    /// **Preparation only.** Unpacking the archive and building the judge it
    /// declares is what two Runners must not do at once; judging is not, and
    /// the lock is dropped before it. What protects an entry while it is being
    /// judged is the holding marker this `Entry` already carries.
    ///
    /// `None` means the word came while waiting. The caller has nothing to
    /// report — it is stopping, and the job goes back to the queue by the road
    /// every other stop takes.
    pub async fn lock(&self, stopping: &Stopping) -> Result<Option<Locked<'_>>> {
        let at = self.cache.lock_for(&self.file_id);
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Storage {
                path: parent.display().to_string(),
                doing: "made to hold the cache's locks",
                source,
            })?;
        }

        let mut waited = false;
        loop {
            match taken(&at)? {
                Some(held) => {
                    return Ok(Some(Locked {
                        entry: self,
                        _held: held,
                    }))
                }
                None => {
                    if !waited {
                        tracing::info!(
                            file_id = %self.file_id,
                            "another Runner is preparing this package; waiting",
                        );
                        waited = true;
                    }
                    if !stopping.sleep(LOCK_POLL).await {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("file_id", &self.file_id)
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.cache.release(&self.file_id);
    }
}

/// Takes the lock at `at` without waiting for it, or says somebody else has it.
#[cfg(unix)]
fn taken(at: &Path) -> Result<Option<std::fs::File>> {
    use std::os::unix::io::AsRawFd as _;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(at)
        .map_err(|source| Error::Storage {
            path: at.display().to_string(),
            doing: "opened to lock a cache entry",
            source,
        })?;

    // SAFETY: the descriptor is open and owned by `file` for the whole call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }

    let why = std::io::Error::last_os_error();
    match why.raw_os_error() {
        // The one answer that is not a failure: somebody else is holding it.
        Some(libc::EWOULDBLOCK) => Ok(None),
        _ => Err(Error::Storage {
            path: at.display().to_string(),
            doing: "locked for a cache entry",
            source: why,
        }),
    }
}

/// **Refused rather than ignored.** A cache shared between Runners rests on
/// this lock, and a host that cannot take one must say so instead of letting
/// two Runners unpack into one directory.
#[cfg(not(unix))]
fn taken(at: &Path) -> Result<Option<std::fs::File>> {
    Err(Error::Unreadable(format!(
        "{} cannot be locked on this host, and a Runner's cache is prepared under a lock",
        at.display()
    )))
}

/// The lock on one entry, held for as long as this value is.
///
/// Dropping it closes the descriptor, which is what releases the lock — so it
/// is released on every path out, including a panic and a cancelled future.
pub struct Locked<'a> {
    entry: &'a Entry,
    _held: std::fs::File,
}

/// A product being assembled, removed if it is not published.
///
/// **The removal is the point of the type.** `aj_package::extract` refuses a
/// target that already exists, so a fill abandoned half way — a refused
/// archive, a full disk, a build that would not run — would otherwise make
/// every later job of that package fail with "already exists" on this Runner,
/// naming a cause that has nothing to do with what went wrong.
pub struct Filling<'a, 'b> {
    locked: &'a Locked<'b>,
    name: String,
    at: PathBuf,
    published: bool,
}

impl Filling<'_, '_> {
    /// Where to assemble it. **It does not exist yet**, and the caller is the
    /// one who creates it: `extract` insists on making its own target.
    pub fn at(&self) -> &Path {
        &self.at
    }

    /// Puts it where every Runner sharing this cache will find it.
    ///
    /// `record` is what the manifest says about it — whatever the caller needs
    /// in order to decide later that it is the right one. This crate stores it
    /// and never reads it.
    pub fn publish(mut self, record: serde_json::Value) -> Result<PathBuf> {
        let final_at = self.locked.entry.dir.join(&self.name);
        if let Some(parent) = final_at.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Storage {
                path: parent.display().to_string(),
                doing: "made to hold a prepared package",
                source,
            })?;
        }

        // **Unrecorded is unreachable, which is why this may remove it.** The
        // manifest is written here and nowhere else, always under the lock, and
        // `published` is the only way a caller is given one of these paths — so
        // a directory the manifest does not name is one nobody was ever handed.
        // It is what a crash between this rename and the manifest write leaves.
        if final_at.exists() {
            let _ = std::fs::remove_dir_all(&final_at);
        }

        std::fs::rename(&self.at, &final_at).map_err(|source| Error::Storage {
            path: final_at.display().to_string(),
            doing: "published into the cache",
            source,
        })?;
        self.published = true;

        let mut manifest = self.locked.manifest();
        manifest.products.insert(self.name.clone(), record);
        self.locked.write(&manifest)?;

        tracing::info!(name = %self.name, entry = %self.locked.entry.file_id, "prepared");
        Ok(final_at)
    }
}

impl Drop for Filling<'_, '_> {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_dir_all(&self.at);
        }
    }
}

/// What an entry holds, and which archive it was derived from.
///
/// **The identity is here so that it can be checked rather than assumed.** The
/// path already carries both — the file id names the directory, the checksum
/// chooses the shard — so this agrees with it in every ordinary case; where it
/// does not, the entry was assembled somewhere else and copied in, and nothing
/// derived from it is reused.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Manifest {
    #[serde(rename = "fileId")]
    file_id: String,
    sha256: String,
    bytes: u64,
    #[serde(default)]
    products: serde_json::Map<String, serde_json::Value>,
}

impl<'b> Locked<'b> {
    /// Where a product is, when this entry's manifest vouches for it and it is
    /// on disk.
    ///
    /// Both halves are required and neither implies the other: a record
    /// without a directory is a product evicted or never finished, and a
    /// directory without a record is one this crate cannot say the provenance
    /// of.
    pub fn published(&self, name: &str) -> Option<PathBuf> {
        let manifest = self.manifest();
        if manifest.file_id != self.entry.file_id || manifest.sha256 != self.entry.sha256 {
            return None;
        }
        manifest.products.get(name)?;

        let at = self.entry.dir.join(name);
        at.exists().then_some(at)
    }

    /// Everything this entry's manifest records, for a caller deciding what is
    /// still wanted.
    pub fn products(&self) -> Vec<(String, serde_json::Value)> {
        let manifest = self.manifest();
        if manifest.file_id != self.entry.file_id || manifest.sha256 != self.entry.sha256 {
            return Vec::new();
        }
        manifest.products.into_iter().collect()
    }

    /// Somewhere to assemble a product, this Runner's alone.
    pub fn filling<'a>(&'a self, name: &str) -> Result<Filling<'a, 'b>> {
        let at = self
            .entry
            .dir
            .join(format!("{name}.{}{PARTIAL}", self.entry.cache.instance));
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Storage {
                path: parent.display().to_string(),
                doing: "made to prepare a package in",
                source,
            })?;
        }
        // Whatever is there is this Runner's own work from an attempt that did
        // not finish. The target has to be **absent**, not empty: `extract`
        // makes its own.
        let _ = std::fs::remove_dir_all(&at);

        Ok(Filling {
            locked: self,
            name: name.to_owned(),
            at,
            published: false,
        })
    }

    /// Stops offering a product, and puts what it named where [`Cache::sweep`]
    /// will reclaim it.
    ///
    /// **Renamed rather than removed, and that is the whole of the care.**
    /// Another Runner may be judging with it at this instant — the lock covers
    /// preparation, not judging — and a bind mount survives a rename of its
    /// source where it does not survive the removal. What is left is a partial
    /// of this Runner's, which `sweep` takes away at a start, and only from an
    /// entry nobody is holding.
    pub fn forget(&self, name: &str) -> Result<()> {
        let mut manifest = self.manifest();
        manifest.products.remove(name);
        self.write(&manifest)?;

        let at = self.entry.dir.join(name);
        if at.exists() {
            let aside = self
                .entry
                .dir
                .join(format!("{name}.{}{PARTIAL}", self.entry.cache.instance));
            let _ = std::fs::remove_dir_all(&aside);
            let _ = std::fs::rename(&at, &aside);
        }
        Ok(())
    }

    fn manifest(&self) -> Manifest {
        let at = self.entry.dir.join(MANIFEST);
        match std::fs::read(&at) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                // Not a reason to remove anything: a manifest that will not
                // parse means nothing is vouched for, so everything is prepared
                // again and this one is replaced.
                tracing::warn!(path = %at.display(), %e, "a cache manifest could not be read");
                Manifest::default()
            }),
            Err(_) => Manifest::default(),
        }
    }

    fn write(&self, manifest: &Manifest) -> Result<()> {
        let mut manifest = Manifest {
            file_id: self.entry.file_id.clone(),
            sha256: self.entry.sha256.clone(),
            bytes: manifest.bytes,
            products: manifest.products.clone(),
        };
        if manifest.bytes == 0 {
            manifest.bytes = std::fs::metadata(&self.entry.file)
                .map(|m| m.len())
                .unwrap_or(0);
        }

        let at = self.entry.dir.join(MANIFEST);
        let partial = self
            .entry
            .dir
            .join(format!("{MANIFEST}.{}{PARTIAL}", self.entry.cache.instance));
        let written = serde_json::to_vec_pretty(&manifest).map_err(|e| {
            Error::Unreadable(format!("the cache manifest could not be written: {e}"))
        })?;

        let storage = |doing: &'static str| {
            let path = at.display().to_string();
            move |source| Error::Storage {
                path,
                doing,
                source,
            }
        };
        std::fs::write(&partial, &written).map_err(storage("written"))?;
        std::fs::rename(&partial, &at).map_err(storage("published"))?;
        Ok(())
    }
}

impl Cache {
    pub fn new(root: impl Into<PathBuf>, max_bytes: u64, instance: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            host_root: None,
            max_bytes,
            instance: instance.into(),
            in_use: Mutex::new(HashMap::new()),
        }
    }

    /// The same directory as the **daemon** sees it, where the Runner is itself
    /// in a container.
    ///
    /// The same pair, and the same trap, as the job scratch: a bind mount is
    /// resolved by the daemon, so a path that is real here and meaningless
    /// there produces an empty directory rather than an error.
    pub fn with_host_root(mut self, host_root: impl Into<PathBuf>) -> Self {
        self.host_root = Some(host_root.into());
        self
    }

    fn on_host(&self, path: &Path) -> PathBuf {
        match &self.host_root {
            Some(host_root) => match path.strip_prefix(&self.root) {
                Ok(inside) => host_root.join(inside),
                Err(_) => path.to_path_buf(),
            },
            None => path.to_path_buf(),
        }
    }

    /// Removes what **this Runner** left behind, and says how many entries it
    /// was holding.
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

        // **Only this Runner's own, and only where nobody is holding the
        // entry.** A partial carrying another instance's name may be work
        // happening right now; ours cannot be, because we are starting. But one
        // of ours can be a product another Runner is still judging with — see
        // [`Locked::forget`] — and the holding marker is what says so.
        let mine = format!(".{}{PARTIAL}", self.instance);
        let mut abandoned = 0;
        for (dir, _, _, _) in self.entries() {
            let file_id = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if self.held_by_anybody(&file_id) {
                continue;
            }
            for inside in walk(&dir) {
                let ours = inside
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(&mine));
                if !ours {
                    continue;
                }
                let gone = match inside.is_dir() {
                    true => std::fs::remove_dir_all(&inside).is_ok(),
                    false => std::fs::remove_file(&inside).is_ok(),
                };
                if gone {
                    abandoned += 1;
                }
            }
        }
        if abandoned > 0 {
            tracing::warn!(
                abandoned,
                "work left unfinished by a previous run was removed"
            );
        }

        self.sweep_the_old_layout();
        cleared
    }

    /// Reclaims entries written before an entry was a directory.
    ///
    /// **Losing them costs a download**, which is what a cache is for. They sit
    /// under the two-character shards at the root rather than under
    /// [`PACKAGES`], so nothing else here can see them — not the ceiling, not
    /// eviction — and left alone they would occupy a disk nothing accounts for.
    ///
    /// An entry another Runner says it is reading is left where it is: a Runner
    /// on the older code may be sharing this volume, and taking its package
    /// away mid-evaluation fails a submission that was going to be judged.
    fn sweep_the_old_layout(&self) {
        let mut reclaimed = 0u64;
        for first in read_dir(&self.root) {
            if first
                .file_name()
                .is_some_and(|n| n == USED || n == HOLDING || n == LOCKS || n == PACKAGES)
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
                        let name = entry
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        // Another Runner's unfinished download, which is not
                        // ours to judge, and an entry somebody is reading.
                        if name.ends_with(PARTIAL) || self.held_by_anybody(&name) {
                            continue;
                        }
                        if std::fs::remove_file(&entry).is_ok() {
                            reclaimed += metadata.len();
                        }
                    }
                }
            }
        }
        if reclaimed > 0 {
            tracing::warn!(
                bytes = reclaimed,
                "cache entries in the layout before 2026-09-15 were reclaimed",
            );
        }
    }

    /// `cache/packages/ch/ec/ks/<fileId>` — the checksum chooses the shard, the
    /// id names the entry.
    ///
    /// **Three bytes of the checksum, and the rest is not kept in the path.** A
    /// hit is the entry's `file` existing and nothing re-reads the bytes, so
    /// what an entry is trusted on is the Server's promise that the bytes under
    /// one file id never change — a corrected file is a new upload with a new
    /// id. Said plainly here because it is a promise this Runner relies on and
    /// never checks.
    fn path_for(&self, sha256: &str, file_id: &str) -> PathBuf {
        let key = sha256.to_ascii_lowercase();
        let pair = |n: usize| key.get(n * 2..n * 2 + 2).unwrap_or("__").to_owned();
        self.root
            .join(PACKAGES)
            .join(pair(0))
            .join(pair(1))
            .join(pair(2))
            .join(file_id)
    }

    fn lock_for(&self, file_id: &str) -> PathBuf {
        self.root.join(LOCKS).join(file_id)
    }

    /// Where this Runner writes something it is still making.
    ///
    /// **One name per Runner, appended, never substituted.** Two Runners
    /// sharing a cache volume miss the same entry at the start of a contest and
    /// both fetch it; under one name they wrote to one file through two
    /// truncating descriptors. Nothing caught it: the checksum is computed from
    /// the stream rather than read back (`Server::download_to`), so each
    /// verified its own bytes while the file held both, and `rename` published
    /// the interleaved result as a correct entry — which a later hit never
    /// re-checks.
    fn partial_for(&self, dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{name}.{}{PARTIAL}", self.instance))
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

        let dir = self.path_for(sha256, file_id);
        self.hold(file_id);
        let entry = Entry {
            cache: Arc::clone(self),
            file_id: file_id.to_owned(),
            sha256: sha256.to_ascii_lowercase(),
            file: dir.join(FILE),
            dir,
        };

        if entry.file.exists() {
            tracing::debug!(file_id, "cache hit");
            self.touch(&entry.dir);
            return Ok(entry);
        }

        // **The download is deliberately not under the lock.** Two Runners
        // fetching one entry at once is wasteful and correct — each writes its
        // own temporary name and the rename is atomic — while a lock here would
        // have to be held across a network transfer, and every Runner wanting
        // that package would wait out the slowest link rather than its own.
        std::fs::create_dir_all(&entry.dir).map_err(|source| Error::Storage {
            path: entry.dir.display().to_string(),
            doing: "made to hold a cached package",
            source,
        })?;
        let temporary = self.partial_for(&entry.dir, FILE);
        let actual = server.download_to(file_id, &temporary).await?;

        if !actual.eq_ignore_ascii_case(sha256) {
            // Discarded **before** the failure is reported, so a retry does not
            // read the same bad bytes again.
            let _ = tokio::fs::remove_file(&temporary).await;
            let _ = tokio::fs::remove_dir(&entry.dir).await;
            return Err(Error::ChecksumMismatch {
                what: format!("file {file_id}"),
                expected: sha256.to_ascii_lowercase(),
                actual,
            });
        }

        // Gone means the entry can be fetched again, not that this evaluation
        // is over. See [`Error::Vanished`].
        tokio::fs::rename(&temporary, &entry.file)
            .await
            .map_err(|e| {
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
        self.touch(&entry.dir);
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
    fn touch(&self, dir: &Path) {
        let marker = self.marker_for(dir);
        if let Some(parent) = marker.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&marker, []);
    }

    fn marker_for(&self, dir: &Path) -> PathBuf {
        self.root
            .join(USED)
            .join(dir.file_name().unwrap_or_default())
    }

    /// Least recently used out first, by whole entries.
    ///
    /// **By whole entries**, because a partially deleted one is a corrupt one —
    /// and an entry is now a directory, so what goes is the archive, what was
    /// unpacked from it and what was built out of that, together. An unbounded
    /// cache on a long-lived Runner is a disk-full outage waiting for the
    /// busiest day of the year, so this must exist and must be bounded.
    ///
    /// **Called after a preparation as well as after a download**, because an
    /// entry now grows after it arrives: a cache that only ever hits still has
    /// packages unpacked and judges built into it.
    pub fn evict_to_fit(&self) {
        let mut entries = self.entries();
        let mut total: u64 = entries.iter().map(|(_, size, _, _)| size).sum();
        if total <= self.max_bytes {
            return;
        }

        // Oldest marker first. An entry nobody has ever touched sorts as the
        // epoch and goes first, which is right: it was downloaded and never read.
        entries.sort_by_key(|(_, _, used, _)| *used);

        for (dir, size, _, working) in entries {
            if total <= self.max_bytes {
                break;
            }
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // **Somebody is writing in there.** Its size is already in `total`
            // above — it is counted, just never chosen — and `sweep` reclaims
            // what a Runner that died mid-preparation left.
            if working {
                continue;
            }
            if self.held_by_anybody(&name) {
                continue;
            }
            if std::fs::remove_dir_all(&dir).is_ok() {
                let _ = std::fs::remove_file(self.marker_for(&dir));
                total = total.saturating_sub(size);
                tracing::info!(entry = %name, bytes = size, "evicted");
            }
        }
    }

    /// Every cached entry: where it is, what it costs, when it was last wanted,
    /// and whether somebody is in the middle of preparing it.
    fn entries(&self) -> Vec<(PathBuf, u64, std::time::SystemTime, bool)> {
        let mut found = Vec::new();
        for first in read_dir(&self.root.join(PACKAGES)) {
            for second in read_dir(&first) {
                for third in read_dir(&second) {
                    for dir in read_dir(&third) {
                        if !dir.is_dir() {
                            continue;
                        }
                        let mut size = 0u64;
                        let mut working = false;
                        for inside in walk(&dir) {
                            if inside
                                .file_name()
                                .is_some_and(|n| n.to_string_lossy().ends_with(PARTIAL))
                            {
                                working = true;
                            }
                            if let Ok(metadata) = std::fs::metadata(&inside) {
                                if metadata.is_file() {
                                    size += metadata.len();
                                }
                            }
                        }
                        let used = std::fs::metadata(self.marker_for(&dir))
                            .and_then(|m| m.modified())
                            .unwrap_or(std::time::UNIX_EPOCH);
                        found.push((dir, size, used, working));
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
pub(crate) fn a_name(what: &str, value: &str) -> Result<()> {
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

/// Everything under `root`, directories included, deepest last.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = read_dir(root);
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            stack.extend(read_dir(&path));
        }
        found.push(path);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "aabbcc0000000000000000000000000000000000000000000000000000000000";

    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("aj-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    /// An entry as `fetch` would leave it, without a Server.
    fn an_entry(cache: &Arc<Cache>, file_id: &str, bytes: usize) -> Entry {
        let dir = cache.path_for(SHA, file_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE), vec![0u8; bytes]).unwrap();
        cache.hold(file_id);
        Entry {
            cache: Arc::clone(cache),
            file_id: file_id.to_owned(),
            sha256: SHA.to_owned(),
            file: dir.join(FILE),
            dir,
        }
    }

    /// A stop nobody ever says, for the locks that are taken at once.
    fn nobody_stops() -> (Stopping, crate::stopping::Teller) {
        Stopping::told()
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
            .take(5)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert_eq!(tail, vec!["a-file-id", "ks", "ec", "ch", "packages"]);
    }

    /// **The root an older Runner never looks at.** Its own hit test is a path
    /// under the bare shards; finding an entry *directory* there it would hand
    /// a directory to a zip reader and fail every submission of every package.
    #[test]
    fn an_entry_is_not_where_the_layout_before_it_kept_one() {
        let cache = Cache::new(scratch("rollback"), 1 << 30, "test");
        let old = cache
            .root
            .join("aa")
            .join("bb")
            .join("cc")
            .join("a-file-id");

        assert!(!cache.path_for(SHA, "a-file-id").starts_with(&old));
        assert!(cache
            .path_for(SHA, "a-file-id")
            .starts_with(cache.root.join(PACKAGES)));
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

    /// **Two Runners must not unpack one package at once**, which is the whole
    /// reason the lock exists. Two `Cache` values in one process are enough:
    /// `flock` is per open file description, so a second `open` of the same
    /// path is a second description and is refused exactly as another process
    /// would be.
    #[tokio::test]
    async fn one_runner_preparing_an_entry_keeps_the_other_out() {
        let root = scratch("lock");
        let mine = Arc::new(Cache::new(&root, 1 << 30, "runner-a"));
        let theirs = Arc::new(Cache::new(&root, 1 << 30, "runner-b"));
        let (stopping, _teller) = nobody_stops();

        let ours = an_entry(&mine, "a-package", 10);
        let yours = an_entry(&theirs, "a-package", 10);

        let held = ours.lock(&stopping).await.unwrap().expect("the lock");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), yours.lock(&stopping))
                .await
                .is_err(),
            "two Runners were preparing one entry at the same time",
        );

        drop(held);
        let after = tokio::time::timeout(Duration::from_secs(2), yours.lock(&stopping))
            .await
            .expect("the lock is released by dropping it");
        assert!(after.unwrap().is_some());
    }

    /// **A Runner told to stop while waiting for another's build leaves.**
    ///
    /// The wait is otherwise unbounded — a judge build has a minute of wall
    /// clock — and a container runtime allows thirty seconds. Blocking on
    /// `flock` in a thread would not hear this at all: dropping the future does
    /// not interrupt the call, and the runtime waits for the thread at
    /// shutdown, so the stop would end in a `SIGKILL`.
    #[tokio::test]
    async fn a_runner_told_to_stop_gives_up_waiting_for_the_lock() {
        let root = scratch("lock-stop");
        let mine = Arc::new(Cache::new(&root, 1 << 30, "runner-a"));
        let theirs = Arc::new(Cache::new(&root, 1 << 30, "runner-b"));
        let (stopping, teller) = Stopping::told();

        let ours = an_entry(&mine, "a-package", 10);
        let yours = an_entry(&theirs, "a-package", 10);
        let _held = ours.lock(&stopping).await.unwrap().expect("the lock");

        let waiting = tokio::spawn(async move { yours.lock(&stopping).await.map(|l| l.is_some()) });
        tokio::time::sleep(Duration::from_millis(150)).await;
        teller.stop();

        let left = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("the waiter heard the word")
            .expect("the task");
        assert!(
            !left.unwrap(),
            "a Runner that is stopping must not go on waiting for a lock",
        );
    }

    /// The whole point of the entry directory: the second job of a package
    /// finds what the first one prepared.
    #[tokio::test]
    async fn what_one_job_prepared_the_next_one_finds() {
        let root = scratch("publish");
        let cache = Arc::new(Cache::new(&root, 1 << 30, "runner-a"));
        let (stopping, _teller) = nobody_stops();
        let entry = an_entry(&cache, "a-package", 10);

        let locked = entry.lock(&stopping).await.unwrap().expect("the lock");
        assert!(locked.published("_extracted").is_none(), "nothing yet");

        let filling = locked.filling("_extracted").unwrap();
        assert!(!filling.at().exists(), "extract makes its own target");
        std::fs::create_dir_all(filling.at().join("tests")).unwrap();
        std::fs::write(filling.at().join("config.yml"), b"type: standard-io@1\n").unwrap();
        let at = filling.publish(serde_json::json!({"files": 2})).unwrap();

        assert!(at.join("config.yml").exists());
        assert_eq!(
            locked.published("_extracted").as_deref(),
            Some(at.as_path())
        );
        assert_eq!(
            locked.products().first().map(|(name, _)| name.as_str()),
            Some("_extracted"),
        );
    }

    /// **A fill that fails leaves nothing behind.** `extract` refuses a target
    /// that exists, so a partial left by a refused archive or a full disk would
    /// make every later job of that package fail with "already exists" — naming
    /// a cause that has nothing to do with what went wrong.
    #[tokio::test]
    async fn work_that_was_never_published_is_taken_away() {
        let root = scratch("abandoned");
        let cache = Arc::new(Cache::new(&root, 1 << 30, "runner-a"));
        let (stopping, _teller) = nobody_stops();
        let entry = an_entry(&cache, "a-package", 10);
        let locked = entry.lock(&stopping).await.unwrap().expect("the lock");

        let half = {
            let filling = locked.filling("_extracted").unwrap();
            std::fs::create_dir_all(filling.at()).unwrap();
            std::fs::write(filling.at().join("half"), b"as far as it got").unwrap();
            filling.at().to_path_buf()
        };

        assert!(
            !half.exists(),
            "a fill that was not published must not survive"
        );
        assert!(locked.published("_extracted").is_none());
        // And the next attempt is not refused by its own leftovers.
        let again = locked.filling("_extracted").unwrap();
        assert!(!again.at().exists());
    }

    /// **The manifest is what says these files came from this archive.** An
    /// entry assembled somewhere else and copied in is not reused: what is
    /// unpacked would be somebody else's tests, judged as though they were this
    /// problem's.
    #[tokio::test]
    async fn what_was_unpacked_from_another_archive_is_not_reused() {
        let root = scratch("identity");
        let cache = Arc::new(Cache::new(&root, 1 << 30, "runner-a"));
        let (stopping, _teller) = nobody_stops();
        let entry = an_entry(&cache, "a-package", 10);
        let locked = entry.lock(&stopping).await.unwrap().expect("the lock");

        let filling = locked.filling("_extracted").unwrap();
        std::fs::create_dir_all(filling.at()).unwrap();
        filling.publish(serde_json::json!({})).unwrap();
        assert!(locked.published("_extracted").is_some());

        // The same directory, vouched for by a manifest naming another archive.
        let foreign = serde_json::json!({
            "fileId": "a-package",
            "sha256": "ffffff0000000000000000000000000000000000000000000000000000000000",
            "bytes": 10,
            "products": { "_extracted": {} },
        });
        std::fs::write(
            entry.dir.join(MANIFEST),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();

        assert!(
            locked.published("_extracted").is_none(),
            "files from another archive were offered as this package's",
        );
        assert!(locked.products().is_empty());
    }

    /// A product nothing vouches for is prepared again rather than trusted —
    /// which is what a crash between the rename and the manifest write leaves.
    #[tokio::test]
    async fn a_product_the_manifest_does_not_name_is_prepared_again() {
        let root = scratch("unrecorded");
        let cache = Arc::new(Cache::new(&root, 1 << 30, "runner-a"));
        let (stopping, _teller) = nobody_stops();
        let entry = an_entry(&cache, "a-package", 10);
        let locked = entry.lock(&stopping).await.unwrap().expect("the lock");

        std::fs::create_dir_all(entry.dir.join("_extracted")).unwrap();
        std::fs::write(entry.dir.join("_extracted/stale"), b"from a crash").unwrap();
        assert!(locked.published("_extracted").is_none());

        let filling = locked.filling("_extracted").unwrap();
        std::fs::create_dir_all(filling.at()).unwrap();
        std::fs::write(filling.at().join("fresh"), b"prepared again").unwrap();
        let at = filling.publish(serde_json::json!({})).unwrap();

        assert!(at.join("fresh").exists());
        assert!(!at.join("stale").exists(), "the unrecorded one was kept");
    }

    /// **A superseded build stops being offered and is not removed under a
    /// Runner that is using it.** The lock covers preparation, not judging, so
    /// another Runner may have its container reading that directory now; a bind
    /// mount survives a rename of its source where it does not survive the
    /// removal, and `sweep` takes the rest away at a start when nobody holds
    /// the entry.
    #[tokio::test]
    async fn a_superseded_build_is_set_aside_and_swept_when_nobody_holds_it() {
        let root = scratch("superseded");
        let cache = Arc::new(Cache::new(&root, 1 << 30, "runner-a"));
        let (stopping, _teller) = nobody_stops();
        let entry = an_entry(&cache, "a-package", 10);
        let locked = entry.lock(&stopping).await.unwrap().expect("the lock");

        let filling = locked.filling("_build/older").unwrap();
        std::fs::create_dir_all(filling.at()).unwrap();
        std::fs::write(filling.at().join("program"), b"an old image built this").unwrap();
        let older = filling
            .publish(serde_json::json!({"source": "checker.cpp"}))
            .unwrap();

        locked.forget("_build/older").unwrap();
        assert!(locked.published("_build/older").is_none(), "still offered");
        assert!(
            !older.exists(),
            "it was left under the name it was offered by"
        );

        drop(locked);
        drop(entry);
        // The restart: nobody holds it now, so what was set aside is reclaimed.
        Cache::new(&root, 1 << 30, "runner-a").sweep();
        assert!(
            !entry_holds(&cache, "a-package", "_build"),
            "the bytes stayed"
        );
    }

    /// Whether any bytes are left under an entry's `<name>`, partials included.
    fn entry_holds(cache: &Arc<Cache>, file_id: &str, name: &str) -> bool {
        let dir = cache.path_for(SHA, file_id).join(name);
        walk(&dir).iter().any(|p| p.is_file())
    }

    /// **A count, and not a set.**
    ///
    /// Two holders of one entry — a trial and a job of the same problem, or any
    /// concurrent claim somebody adds later — and one of them finishes. With a
    /// set, that first drop removed the only member and the entry could be
    /// deleted out from under the other, still reading it.
    #[test]
    fn an_entry_held_twice_survives_the_first_holder_letting_go() {
        let cache = Arc::new(Cache::new(scratch("refcount"), 100, "one-runner"));
        let entry = an_entry(&cache, "held-twice", 200);
        cache.hold("held-twice");

        cache.evict_to_fit();
        assert!(entry.file.exists(), "the second holder is still reading it");

        cache.release("held-twice");
        drop(entry);
        cache.evict_to_fit();
        assert!(
            !cache.path_for(SHA, "held-twice").exists(),
            "nobody holds it now",
        );
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
        let mine = Arc::new(Cache::new(&root, 100, "runner-a"));
        let theirs = Arc::new(Cache::new(&root, 100, "runner-b"));

        let yours = an_entry(&theirs, "theirs", 200);

        mine.evict_to_fit();
        assert!(yours.file.exists(), "evicted from under another Runner");

        drop(yours);
        mine.evict_to_fit();
        assert!(
            !mine.path_for(SHA, "theirs").exists(),
            "nobody holds it now"
        );
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

    /// **Whole entries, and an entry is now everything derived from it too.**
    /// A cache that only ever hits still grows: packages are unpacked and
    /// judges built into entries that were downloaded long ago.
    #[test]
    fn eviction_takes_a_whole_entry_and_spares_what_is_open() {
        let root = scratch("evict");
        let cache = Arc::new(Cache::new(&root, 400, "test"));

        let mut held = Vec::new();
        for id in ["old", "middle", "held"] {
            let entry = an_entry(&cache, id, 100);
            // What was derived from it counts towards the ceiling as well.
            std::fs::create_dir_all(entry.dir.join("_extracted")).unwrap();
            std::fs::write(entry.dir.join("_extracted/tests"), vec![0u8; 100]).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            cache.touch(&entry.dir);
            held.push(entry);
        }

        // Only the last one is still being read.
        let held: Vec<Entry> = held.into_iter().filter(|e| e.file_id == "held").collect();
        cache.evict_to_fit();

        assert!(
            !cache.path_for(SHA, "old").exists(),
            "the oldest entry should have gone, with what was unpacked from it",
        );
        assert!(
            held[0].dir.join("_extracted/tests").exists(),
            "an entry a job is reading is never evicted",
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

        let (_, _, used, _) = cache
            .entries()
            .into_iter()
            .find(|(dir, _, _, _)| dir.file_name().is_some_and(|n| n == "a-file-id"))
            .expect("the download is in the cache");
        assert!(
            used > std::time::UNIX_EPOCH,
            "a fresh download sorted as the epoch, so it evicts before every older entry",
        );

        drop(entry);
    }

    /// The same body to **two** callers, each in two halves with a pause between
    /// them.
    ///
    /// The pause is the whole point: a three-kilobyte package is written in one
    /// go, and two writers never overlap on it. A real package is not three
    /// kilobytes, and the window this leaves is the one an operator's contest
    /// leaves at nine in the morning.
    async fn serving_twice_slowly(body: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            let mut served = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                served.push(tokio::spawn(async move {
                    let mut request = [0u8; 2048];
                    let _ = socket.read(&mut request).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                         Content-Type: application/octet-stream\r\n\r\n",
                        body.len(),
                    );
                    // Write errors are ignored on purpose: whichever caller
                    // finishes first may hang up while this half is still going
                    // out, and a broken pipe here says nothing about the bytes
                    // that did arrive. What the test judges is the file.
                    let _ = socket.write_all(head.as_bytes()).await;
                    let (first, second) = body.split_at(body.len() / 2);
                    let _ = socket.write_all(first).await;
                    let _ = socket.flush().await;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let _ = socket.write_all(second).await;
                    let _ = socket.flush().await;
                }));
            }
            for one in served {
                let _ = one.await;
            }
        });

        (format!("http://127.0.0.1:{port}/api/v1"), handle)
    }

    /// Two Runners wanting one entry at the same moment leave it correct.
    ///
    /// **This is the race the arrangement exists to survive**, and the only test
    /// that reproduces it. Two processes are not needed and never were:
    /// `File::create` hands each caller its own descriptor with its own offset,
    /// so two of them writing one path is the same mechanism whether they sit in
    /// two containers or two tasks.
    ///
    /// Under one temporary name the second `create` truncates the file the first
    /// is still writing, and the first then writes its tail at the offset it had
    /// reached — leaving a hole where the head used to be. Neither notices: the
    /// checksum is computed from the stream each of them received, not from the
    /// bytes on disk, so both verify and `rename` publishes whichever finishes
    /// last as a correct entry. Nothing re-reads it afterwards.
    ///
    /// **The download is deliberately outside the lock**, so this is still the
    /// arrangement being tested rather than one the lock has made impossible.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_runners_fetching_one_entry_leave_it_whole() {
        use sha2::Digest as _;

        static BODY: &[u8] = &[7u8; 64 * 1024];
        let sha256 = hex::encode(sha2::Sha256::digest(BODY));

        let root = scratch("interleave");
        let (base, handle) = serving_twice_slowly(BODY).await;

        let ours = Arc::new(Cache::new(&root, 1 << 30, "runner-a"));
        let theirs = Arc::new(Cache::new(&root, 1 << 30, "runner-b"));
        let to_us = Server::new(&base).unwrap();
        let to_them = Server::new(&base).unwrap();

        let (mine, yours) = tokio::join!(
            ours.fetch(&to_us, "one-package", &sha256),
            theirs.fetch(&to_them, "one-package", &sha256),
        );
        handle.abort();

        let mine = mine.expect("this Runner's fetch");
        let yours = yours.expect("the other Runner's fetch");

        // The entry both of them now believe in.
        let stored = std::fs::read(mine.path()).expect("the cached entry");
        assert_eq!(
            stored.len(),
            BODY.len(),
            "the entry is not the size of what was served",
        );
        assert_eq!(
            hex::encode(sha2::Sha256::digest(&stored)),
            sha256,
            "the entry does not hash to the checksum it is filed under — two \
             writers went through one file and nothing re-reads a hit",
        );

        drop(mine);
        drop(yours);
    }

    #[test]
    fn two_runners_do_not_write_to_one_temporary_file() {
        let root = scratch("partial-name");
        let ours = Cache::new(&root, 1 << 30, "runner-a");
        let theirs = Cache::new(&root, 1 << 30, "runner-b");
        let entry = ours.path_for(SHA, "a-file-id");

        assert_ne!(
            ours.partial_for(&entry, FILE),
            theirs.partial_for(&entry, FILE),
            "one temporary name between two Runners is one file between two writers",
        );
    }

    /// **A dotted file id cannot reach a neighbour's name any more**, because
    /// what is being named sits *inside* the entry rather than beside it. The
    /// hazard was real while the suffix replaced an extension: a file id of
    /// `a.b` produced `a.partial`, which is where the entry named `a` would
    /// have put its own.
    #[test]
    fn work_in_progress_stays_inside_the_entry_it_belongs_to() {
        let cache = Cache::new(scratch("dotted"), 1 << 30, "test");

        let dotted = cache.path_for(SHA, "a.b");
        let partial = cache.partial_for(&dotted, FILE);

        assert!(partial.starts_with(&dotted));
        assert!(!partial.starts_with(cache.path_for(SHA, "a")));
    }

    #[test]
    fn an_entry_being_prepared_is_not_evicted() {
        let root = scratch("evict-partial");
        let cache = Arc::new(Cache::new(&root, 150, "test"));

        let old = an_entry(&cache, "old", 100);
        cache.touch(&old.dir);
        drop(old);
        std::thread::sleep(Duration::from_millis(20));

        // The one being written right now, and the worst case: `touch` runs
        // only after the download, so it carries no marker and `entries` dates
        // it to the epoch — least recently used, first out.
        let arriving = cache.path_for(SHA, "arriving");
        std::fs::create_dir_all(&arriving).unwrap();
        let partial = cache.partial_for(&arriving, FILE);
        std::fs::write(&partial, vec![0u8; 100]).unwrap();

        cache.evict_to_fit();

        assert!(
            partial.exists(),
            "an entry being prepared is never a candidate"
        );
        assert!(
            !cache.path_for(SHA, "old").exists(),
            "and the eviction it was skipped by still ran",
        );
    }

    /// A Runner reclaims what it left half-finished, and leaves every other
    /// Runner's alone — theirs may be happening right now.
    #[test]
    fn a_sweep_removes_this_runners_abandoned_work_and_leaves_another_runners() {
        let root = scratch("sweep-partial");
        let ours = Arc::new(Cache::new(&root, 1 << 30, "runner-a"));
        let theirs = Arc::new(Cache::new(&root, 1 << 30, "runner-b"));

        let dir = ours.path_for(SHA, "interrupted");
        std::fs::create_dir_all(&dir).unwrap();
        let mine = ours.partial_for(&dir, FILE);
        let yours = theirs.partial_for(&dir, FILE);
        std::fs::write(&mine, b"the first half").unwrap();
        std::fs::write(&yours, b"the first half").unwrap();
        // And a directory left half unpacked, which is the new shape of it.
        let unpacked = ours.partial_for(&dir, "_extracted");
        std::fs::create_dir_all(unpacked.join("tests")).unwrap();

        // The restart.
        Cache::new(&root, 1 << 30, "runner-a").sweep();

        assert!(!mine.exists(), "its own unfinished download is cleared");
        assert!(!unpacked.exists(), "and its own half-unpacked package");
        assert!(yours.exists(), "another Runner may be writing that one now");
    }

    /// **What a Runner from before this layout left is reclaimed**, because
    /// nothing else here can see it: it sits under the bare shards rather than
    /// under `packages/`, so neither the ceiling nor eviction accounts for it.
    ///
    /// An entry another Runner says it is reading is left where it is — a
    /// Runner on the older code may be sharing this volume, and taking its
    /// package away mid-evaluation fails a submission that was going to be
    /// judged.
    #[test]
    fn what_the_layout_before_this_one_left_is_reclaimed_unless_somebody_holds_it() {
        let root = scratch("old-layout");
        let cache = Cache::new(&root, 1 << 30, "runner-a");
        let theirs = Cache::new(&root, 1 << 30, "runner-b");

        let shard = root.join("aa").join("bb").join("cc");
        std::fs::create_dir_all(&shard).unwrap();
        for id in ["abandoned", "in-use"] {
            std::fs::write(shard.join(id), vec![0u8; 64]).unwrap();
        }
        // Another Runner's unfinished download, which is not ours to judge.
        std::fs::write(shard.join("arriving.runner-b.partial"), b"half").unwrap();
        theirs.hold("in-use");

        Cache::new(&root, 1 << 30, "runner-a").sweep();

        assert!(!shard.join("abandoned").exists(), "nothing was reclaimed");
        assert!(shard.join("in-use").exists(), "taken from under a Runner");
        assert!(shard.join("arriving.runner-b.partial").exists());

        theirs.release("in-use");
        cache.sweep();
        assert!(!shard.join("in-use").exists(), "nobody holds it now");
    }
}
