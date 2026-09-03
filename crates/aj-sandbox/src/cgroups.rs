//! Where a run's processor time and peak memory come from.
//!
//! **The Runner does not measure a container; it measures a cgroup that
//! outlives one.** A container's own cgroup is destroyed the moment its process
//! exits — measured 2026-08-09, and again 2026-09-03, where it was already gone
//! before `docker rm` ran — and the runtime API reports no peak on v2. So the
//! sandbox is started *under* a parent, and the parent is read afterwards.
//!
//! What that parent is depends on the daemon's cgroup driver, and the two are
//! not variations of one arrangement:
//!
//! - **`cgroupfs`** — a cgroup parent is a path. The Runner makes one per run,
//!   hands the daemon `/algojudge/<name>`, reads it and removes it. Docker
//!   Desktop's default, and the only arrangement supported until 2026-09-03.
//! - **`systemd`** — a cgroup parent is a *slice*, and slices belong to systemd.
//!   The Runner names **one** for its whole life; a run is what changed in it
//!   while that run was the only thing there.
//!
//! **Why one slice rather than one per run**, measured on WSL2, kernel 6.18: a
//! slice systemd created for a container is never collected. It stays `loaded
//! active active` indefinitely — twenty-nine hours in the case that settled it
//! — and removing its directory does not release the unit. Two hundred of them
//! cost 49 MB in pid 1, so a slice per test would cost some 37 MB per
//! submission, permanently. Nothing this Runner can reach takes them away; not
//! asking for them does.
//!
//! Every refusal here names the same consequence, because there is only one: a
//! time limit is decided on processor time read from that cgroup, so a Runner
//! that cannot read one produces no verdict rather than a worse number.

use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::{Error, Result};

/// The one name this Runner puts in a host's cgroup tree, under both drivers.
const OURS: &str = "algojudge";

/// The mount point of the unified hierarchy, where nothing says otherwise.
const DEFAULT_ROOT: &str = "/sys/fs/cgroup";

/// How this Runner measures on this host.
///
/// Decided once, from what the daemon reports: a daemon does not change its
/// cgroup driver while it is the same daemon. `root` is the mount point of the
/// unified hierarchy as *this process* sees it; everything else is derived.
#[derive(Debug, Clone)]
pub enum Cgroups {
    /// The Runner makes a directory per run, and the daemon is given its path.
    Cgroupfs { root: PathBuf },
    /// systemd owns the slice. The Runner makes and removes nothing, and reads.
    ///
    /// One slice serves every run, so a run's numbers are differences rather
    /// than readings — which holds only while one run is in it at a time.
    /// `gate` makes a second one wait instead of quietly spoiling both.
    Systemd {
        root: PathBuf,
        slice: String,
        gate: Arc<tokio::sync::Mutex<()>>,
    },
}

impl Cgroups {
    /// Which backend this daemon needs, or a refusal saying why neither fits.
    pub fn resolve(driver: &str, root: PathBuf, instance: &str) -> Result<Self> {
        readable_hierarchy(&root)?;
        Self::choose(driver, root, instance)
    }

    /// The choice alone, as a function of its arguments, so that every case of
    /// it is testable on a host with no cgroups at all.
    fn choose(driver: &str, root: PathBuf, instance: &str) -> Result<Self> {
        match driver {
            "cgroupfs" => Ok(Self::Cgroupfs { root }),
            "systemd" => Ok(Self::Systemd {
                slice: format!("{OURS}-{}.slice", unit_safe(instance)),
                root,
                gate: Arc::new(tokio::sync::Mutex::new(())),
            }),
            other => Err(Error::Refused(format!(
                "this host's cgroup driver is {other:?}, and the Runner knows cgroupfs and \
                 systemd. A time limit is decided on processor time read from a cgroup, so with \
                 neither there is nothing to read it from — this Runner would register, answer \
                 the protocol and then fail every job it claimed. `docker info` reports the \
                 driver as CgroupDriver"
            ))),
        }
    }

    /// What to call this in a log, and what a test asserts it chose.
    pub fn driver(&self) -> &'static str {
        match self {
            Self::Cgroupfs { .. } => "cgroupfs",
            Self::Systemd { .. } => "systemd",
        }
    }

    /// The directory a run's numbers come out of.
    ///
    /// Under `cgroupfs` it is the parent of one directory per run; under
    /// `systemd` it *is* the slice, shared by every run and never removed.
    pub fn home(&self) -> PathBuf {
        match self {
            Self::Cgroupfs { root } => root.join(OURS),
            Self::Systemd { root, slice, .. } => {
                slice_path(root, slice).unwrap_or_else(|| root.join(slice))
            }
        }
    }

    /// The directory a run must not add anything to, at any depth.
    ///
    /// Not the same as [`Self::home`] under `systemd`, and the difference is
    /// exactly what a regression to one slice per run would leak: those land
    /// **beside** the Runner's slice rather than inside it, so a check that only
    /// looked in `home` would watch the wrong directory and say nothing.
    pub fn family(&self) -> PathBuf {
        match self {
            Self::Cgroupfs { .. } => self.home(),
            Self::Systemd { root, .. } => root.join(format!("{OURS}.slice")),
        }
    }

    /// What the daemon is told to start this run under.
    ///
    /// The two strings are not interchangeable and each daemon rejects the
    /// other's: a path under `systemd` is refused with *cgroup-parent for
    /// systemd cgroup should be a valid slice named as "xxx.slice"*, and a slice
    /// name under `cgroupfs` is a directory nothing else has heard of.
    pub fn parent(&self, name: &str) -> String {
        match self {
            Self::Cgroupfs { .. } => format!("/{OURS}/{name}"),
            Self::Systemd { slice, .. } => slice.clone(),
        }
    }

    /// Opens a measurement for one run, and says what the daemon should be told.
    ///
    /// `None` means this run goes unmeasured, which is not an error here: the
    /// limits are the runtime's and hold either way. It is `aj-standard-io` that
    /// refuses to reach a verdict without a reading.
    pub(crate) async fn begin(&self, name: &str) -> Option<(Measuring, String)> {
        let parent = self.parent(name);
        let measuring = match self {
            Self::Cgroupfs { root } => {
                let here = root.join(OURS).join(name);
                std::fs::create_dir(&here).ok()?;
                Measuring::Own { here }
            }
            Self::Systemd { gate, .. } => {
                let gate = gate.clone().lock_owned().await;
                let here = self.home();
                // The slice exists only once systemd has been asked for it, so
                // the first run of a Runner's life finds nothing here — and is
                // also the only run whose peak is the slice's own history.
                let fresh = !here.is_dir();
                let opened = if fresh { None } else { reset_peak(&here) };
                Measuring::Shared {
                    cpu_before: usage_usec(&here).unwrap_or(0),
                    memory_before: opened.as_ref().map_or(0, |(_, at_reset)| *at_reset),
                    peak: opened.map(|(file, _)| file),
                    fresh,
                    here,
                    _gate: gate,
                }
            }
        };
        Some((measuring, parent))
    }

    /// Makes what this backend needs up front, and proves the Runner can use it.
    ///
    /// Under `cgroupfs` that is creation **and removal**. The check this
    /// replaced was `create_dir_all`, which succeeds without proving anything
    /// once the directory exists — so a tree remounted read-only under a
    /// directory a previous Runner had left passed preflight and then failed
    /// every job it claimed.
    ///
    /// Under `systemd` the Runner creates nothing, so there is nothing left to
    /// prove beyond a readable hierarchy, which [`Self::resolve`] proved.
    ///
    /// **Named for the Runner and not fixed**, because several of them share one
    /// host and one `algojudge` directory, and `docker compose up` starts them
    /// together. With one name they raced: the second saw `AlreadyExists`,
    /// removed the first one's probe, and the first then failed its own
    /// `remove_dir` and refused to start blaming the host. Reproduced on the
    /// first attempt by `two_runners_preparing_at_once_do_not_collide`.
    pub(crate) fn prepare(&self, instance: &str) -> Result<()> {
        let Self::Cgroupfs { root } = self else {
            return Ok(());
        };
        let home = root.join(OURS);
        let probe = home.join(probe_name(instance));
        std::fs::create_dir_all(&home)
            .and_then(|()| match std::fs::create_dir(&probe) {
                Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
                _ => std::fs::remove_dir(&probe),
            })
            .map_err(|e| {
                Error::Refused(format!(
                    "this Runner cannot make and remove a cgroup under {}: {e}. A time limit is \
                     decided on processor time, read from cpu.stat in a cgroup this Runner makes \
                     for each run, so without one it would register and then fail every job it \
                     claimed. Mount the host's cgroup tree writable and share its namespace — \
                     --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup, or in Compose `cgroup: \
                     host` on the service plus the same volume — and run the container as root, \
                     because the tree's directories are root's. It costs write permission, not a \
                     capability",
                    home.display()
                ))
            })
    }

    /// Why peak memory cannot be reported on this host, where it cannot.
    /// `None` means it can.
    ///
    /// Only the `systemd` backend can lose it: one slice serves every run, so a
    /// per-run peak needs `memory.peak` reset — a write to a root-owned file,
    /// and a kernel interface that arrived in Linux 6.8. Processor time is
    /// unaffected, so this is said at start rather than refused: every verdict
    /// still stands, and only the number beside it is missing.
    pub fn without_peak_memory(&self) -> Option<String> {
        let Self::Systemd { root, .. } = self else {
            return None;
        };
        let peak = own_cgroup(root)?.join("memory.peak");
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&peak)
            .err()
            .map(|e| {
                format!(
                    "{} cannot be opened for writing: {e}. Under the systemd cgroup driver one \
                     slice serves every run, so a per-run peak is taken by resetting \
                     memory.peak, which needs Linux 6.8 or later and a cgroup tree mounted \
                     writable in a container running as root",
                    peak.display()
                )
            })
    }
}

/// One run's measurement, held from before its container starts until after it
/// is gone.
pub(crate) enum Measuring {
    /// A directory of this run's own: read it, then take it away.
    Own { here: PathBuf },
    /// A slice shared by every run of this Runner: what changed in it while
    /// this run was the only thing there.
    Shared {
        here: PathBuf,
        cpu_before: u64,
        /// What the slice already held when the run began, and **the reason a
        /// peak here is a subtraction.**
        ///
        /// A reset does not zero the mark; it sets it to the usage of the
        /// moment. And a slice that has judged for a while is not empty even
        /// with no processes in it: page cache charged to a container is
        /// **reparented to the parent** when that container dies, so the slice
        /// accumulates file cache from every run it has ever hosted. Measured
        /// 2026-09-03 after 48 submissions: 328 MB held, `cgroup.procs` empty,
        /// 326 MB of it `file`. Reporting the mark as it stands charges a
        /// program 106 MiB for using 7.6.
        memory_before: u64,
        /// A descriptor whose `memory.peak` was reset when the run began. The
        /// reset is per descriptor — a fresh open still reports the slice's
        /// whole history — so this file is the only way back to the number.
        peak: Option<std::fs::File>,
        /// The slice did not exist yet, so its own history is this run.
        fresh: bool,
        _gate: tokio::sync::OwnedMutexGuard<()>,
    },
}

impl Measuring {
    /// What the run cost: peak memory, then processor time.
    ///
    /// Either may be absent and neither is ever guessed. This is measurement,
    /// not enforcement — the limits were applied by the runtime and hold
    /// whether or not this succeeds.
    pub(crate) fn finish(self) -> (Option<u64>, Option<Duration>) {
        match self {
            Self::Own { here } => {
                let peak = read_number(&here.join("memory.peak"));
                let cpu = usage_usec(&here).map(Duration::from_micros);
                // Read before the directory goes, and removed here rather than
                // left: the child's own cgroup is taken away with its container,
                // and this one is nobody else's to collect.
                let _ = std::fs::remove_dir(&here);
                (peak, cpu)
            }
            Self::Shared {
                here,
                cpu_before,
                memory_before,
                mut peak,
                fresh,
                ..
            } => {
                let cpu = usage_usec(&here).map(|after| {
                    // A slice systemd removed and remade counts from zero again,
                    // and then the whole counter is this run's.
                    Duration::from_micros(after.checked_sub(cpu_before).unwrap_or(after))
                });
                let peak = match (&mut peak, fresh) {
                    // How far above what the slice already held it climbed —
                    // see `memory_before`.
                    (Some(file), _) => reread(file).map(|p| p.saturating_sub(memory_before)),
                    // Nothing was reset because nothing was there: the slice was
                    // made for this run, so its history is this run.
                    (None, true) => read_number(&here.join("memory.peak")),
                    (None, false) => None,
                };
                (peak, cpu)
            }
        }
    }
}

/// Where the cgroup tree is mounted, from `AJ_Sandbox__CgroupRoot`.
pub(crate) fn root_from_environment() -> Result<PathBuf> {
    root_from(std::env::var("AJ_Sandbox__CgroupRoot").ok())
}

/// The same decision as a function of what the environment held, so every case
/// of it is testable.
///
/// **Empty is unset**, and the trap that closes is not hypothetical: an empty
/// value became a relative path the Runner wrote through while telling the
/// daemon an absolute one, so the measurement read an empty directory rather
/// than failing.
pub(crate) fn root_from(value: Option<String>) -> Result<PathBuf> {
    let root = PathBuf::from(
        value
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_ROOT.to_owned()),
    );
    if !root.is_absolute() {
        return Err(Error::Refused(format!(
            "AJ_Sandbox__CgroupRoot is {}, which is not an absolute path. This Runner reads \
             through this path while the daemon resolves a cgroup parent against its own root, \
             so a relative one names a directory the daemon has never heard of — and a time \
             limit is decided on processor time read from exactly that directory. Give it the \
             mount point of the unified hierarchy, normally {DEFAULT_ROOT}, or unset it",
            root.display()
        )));
    }
    Ok(root)
}

/// Refuses a root that is not a mounted, readable cgroup v2 hierarchy.
///
/// **The content of `cgroup.controllers`, not its size.** A cgroup pseudo-file
/// reports `st_size` zero however much it holds, so a size test calls every host
/// v1. Without this check the symptom is an empty read at judging time, which is
/// what a missing mount and a missing `cgroup: host` both look like.
fn readable_hierarchy(root: &Path) -> Result<()> {
    let listed = std::fs::read_to_string(root.join("cgroup.controllers"));
    match &listed {
        Ok(controllers) if !controllers.trim().is_empty() => return Ok(()),
        _ => {}
    }
    Err(Error::Refused(format!(
        "{} is not a readable cgroup v2 hierarchy ({}). A time limit is decided on processor \
         time read from a cgroup under it, so this Runner cannot judge without one. Mount the \
         host's tree and share its namespace: --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup, \
         or in Compose `cgroup: host` on the service plus the same volume. \
         AJ_Sandbox__CgroupRoot moves it",
        root.display(),
        match listed {
            Ok(_) => "it lists no controllers".to_owned(),
            Err(e) => e.to_string(),
        }
    )))
}

/// The version refusal, here so that every refusal the measurement path can
/// produce is in one module and one test holds all of them to one standard.
pub(crate) fn unsupported_version(reported: &str) -> Error {
    Error::Refused(format!(
        "this host reports cgroup version {reported}, and the Runner requires v2. The limits are \
         enforced on v1 — a container over its memory limit is OOM-killed there, measured rather \
         than assumed — but a time limit is decided on processor time read from cpu.stat, which \
         is a v2 interface, so what cannot be done here is reach a verdict at all. A hybrid host \
         boots unified with the kernel parameter systemd.unified_cgroup_hierarchy=1, which is a \
         reboot rather than a switch; docs/CGROUP_V2.md lists what the hierarchy then has to \
         carry"
    ))
}

/// Where systemd puts a slice, from its name alone.
///
/// **The naming rule is the path**: a name is split on `-` and every prefix is a
/// slice of its own, nested. So `algojudge-a.slice` is
/// `<root>/algojudge.slice/algojudge-a.slice`, and `a-b-c.slice` is
/// `<root>/a.slice/a-b.slice/a-b-c.slice`. Measured on WSL2, kernel 6.18.
///
/// `None` for a name systemd would not accept, because the path would then be a
/// guess about somebody else's directory.
fn slice_path(root: &Path, slice: &str) -> Option<PathBuf> {
    // `-.slice` is systemd's own name for the root of the hierarchy.
    let stem = slice
        .strip_suffix(".slice")
        .filter(|s| !s.is_empty() && *s != "-")?;

    let mut path = root.to_path_buf();
    let mut prefix = String::new();
    for part in stem.split('-') {
        if part.is_empty() || part == "." || part == ".." || part.contains(['/', '\\']) {
            return None;
        }
        if !prefix.is_empty() {
            prefix.push('-');
        }
        prefix.push_str(part);
        path.push(format!("{prefix}.slice"));
    }
    Some(path)
}

/// A probe directory of this Runner's own, inside the shared `algojudge` one.
///
/// **A dot rather than `algojudge-`**, so nothing this leaves can be mistaken
/// for a per-run cgroup, which is exactly what the suites and CI count. A crash
/// between the two calls leaves **at most one per Runner**, and that Runner's
/// own next start removes it — the same reasoning that makes `sweep`'s instance
/// label survive a restart.
fn probe_name(instance: &str) -> String {
    format!(".probe.{}", unit_safe(instance))
}

/// An instance id as one component of a systemd unit name.
///
/// A `-` in a unit name is a level of nesting rather than a character, so one
/// here would put this Runner's slice under an intermediate slice nobody asked
/// for. Production passes a key fingerprint, which is hex; a test passes its own
/// name, which is not.
fn unit_safe(instance: &str) -> String {
    let safe: String = instance
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "_".to_owned()
    } else {
        safe
    }
}

/// This process's own cgroup, as `/proc/self/cgroup` names it, under `root`.
///
/// The honest place to test a permission the Runner will need elsewhere in the
/// same tree: it exists, it is on the same mount, and it has the same owner.
fn own_cgroup(root: &Path) -> Option<PathBuf> {
    let listed = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = listed.lines().find_map(|l| l.strip_prefix("0::/"))?.trim();
    // The root cgroup carries no memory files, so there is nothing to test on.
    (!path.is_empty()).then(|| root.join(path))
}

/// `usage_usec` from `cpu.stat`: user plus system, for the whole subtree.
fn usage_usec(dir: &Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("cpu.stat"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

fn read_number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Opens `memory.peak`, resets it, and says what the slice held at that moment.
///
/// Writing any non-empty string resets the high-water mark **for that
/// descriptor only** — a fresh open still reports the cgroup's whole history —
/// so the file has to be carried to the end of the run. Linux 6.8 and later;
/// before that it is mode 0444 and this fails, which
/// [`Cgroups::without_peak_memory`] reports at start.
fn reset_peak(dir: &Path) -> Option<(std::fs::File, u64)> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join("memory.peak"))
        .ok()?;
    file.write_all(b"0").ok()?;
    // Read after the write, so the two describe the same moment. This is what
    // the mark was just set to, and what the run's own peak is measured from.
    Some((file, read_number(&dir.join("memory.current")).unwrap_or(0)))
}

fn reread(file: &mut std::fs::File) -> Option<u64> {
    let mut text = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(DEFAULT_ROOT)
    }

    fn cgroupfs() -> Cgroups {
        Cgroups::choose("cgroupfs", root(), "abc").expect("a backend")
    }

    fn systemd() -> Cgroups {
        Cgroups::choose("systemd", root(), "abc").expect("a backend")
    }

    #[test]
    fn a_slice_lives_where_its_name_says_it_does() {
        let cases = [
            (
                "algojudge-abc.slice",
                "/sys/fs/cgroup/algojudge.slice/algojudge-abc.slice",
            ),
            ("algojudge.slice", "/sys/fs/cgroup/algojudge.slice"),
            (
                "a-b-c.slice",
                "/sys/fs/cgroup/a.slice/a-b.slice/a-b-c.slice",
            ),
        ];
        for (slice, expected) in cases {
            assert_eq!(
                slice_path(&root(), slice),
                Some(PathBuf::from(expected)),
                "{slice}",
            );
        }
    }

    #[test]
    fn a_name_systemd_would_not_accept_yields_no_path() {
        for slice in [
            "-.slice",
            ".slice",
            "algojudge",
            "a--b.slice",
            "-a.slice",
            "a-.slice",
            "a-..-b.slice",
            "a/b.slice",
            "..-x.slice",
        ] {
            assert_eq!(slice_path(&root(), slice), None, "{slice}");
        }
    }

    #[test]
    fn the_configured_root_is_where_everything_hangs_from() {
        let elsewhere = PathBuf::from("/mnt/cgroup2");
        assert_eq!(
            slice_path(&elsewhere, "algojudge-abc.slice"),
            Some(PathBuf::from(
                "/mnt/cgroup2/algojudge.slice/algojudge-abc.slice"
            )),
        );
        assert_eq!(
            Cgroups::choose("cgroupfs", elsewhere, "abc")
                .expect("a backend")
                .home(),
            PathBuf::from("/mnt/cgroup2/algojudge"),
        );
    }

    #[test]
    fn cgroupfs_is_told_a_path_and_systemd_is_told_a_slice() {
        assert_eq!(
            cgroupfs().parent("algojudge-7-9"),
            "/algojudge/algojudge-7-9"
        );
        assert_eq!(systemd().parent("algojudge-7-9"), "algojudge-abc.slice");
    }

    /// Each daemon refuses the other's string, so swapping them does not
    /// degrade a measurement — it refuses to start the container.
    #[test]
    fn the_two_parent_strings_could_never_be_swapped() {
        let path = cgroupfs().parent("run");
        assert!(path.starts_with('/') && !path.ends_with(".slice"));

        let slice = systemd().parent("run");
        assert!(slice.ends_with(".slice") && !slice.contains('/'));
    }

    /// What makes a leftover count mean anything: a run under `cgroupfs` is a
    /// child of `home`, and under `systemd` there is no child to leave.
    #[test]
    fn a_cgroupfs_run_sits_inside_the_home_a_suite_counts() {
        let Cgroups::Cgroupfs { root } = cgroupfs() else {
            unreachable!()
        };
        assert_eq!(
            root.join(OURS).join("run").parent(),
            Some(cgroupfs().home().as_path())
        );
        assert_eq!(
            systemd().home(),
            PathBuf::from("/sys/fs/cgroup/algojudge.slice/algojudge-abc.slice")
        );
    }

    #[test]
    fn the_driver_the_daemon_reports_chooses_the_backend() {
        assert_eq!(cgroupfs().driver(), "cgroupfs");
        assert_eq!(systemd().driver(), "systemd");
    }

    #[test]
    fn a_driver_this_runner_does_not_know_is_refused() {
        for driver in ["", "none", "cgroupfs2"] {
            let refusal = Cgroups::choose(driver, root(), "abc").expect_err("a refusal");
            assert!(
                refusal.to_string().contains("cgroupfs and systemd"),
                "{driver}"
            );
        }
    }

    /// A `-` in a unit name is a level of nesting rather than a character, so
    /// a Runner whose instance carries one must not nest its own slice.
    #[test]
    fn an_instance_becomes_one_component_of_a_unit_name() {
        let Cgroups::Systemd { slice, .. } =
            Cgroups::choose("systemd", root(), "test-judging").expect("a backend")
        else {
            unreachable!()
        };
        assert_eq!(slice, "algojudge-test_judging.slice");
        assert_eq!(
            slice_path(&root(), &slice),
            Some(PathBuf::from(
                "/sys/fs/cgroup/algojudge.slice/algojudge-test_judging.slice"
            )),
        );
    }

    /// **Several Runners share one host and one cgroup tree**, and they start
    /// together: `docker compose up` brings a second one up alongside the first.
    /// Nothing either of them does at start may depend on the other not doing it
    /// at the same moment.
    #[test]
    fn two_runners_preparing_at_once_do_not_collide() {
        let root = std::env::temp_dir().join(format!("aj-prepare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a scratch root");

        // Enough attempts that the window cannot be missed by luck alone.
        for round in 0..200 {
            let outcomes: Vec<_> = ["first", "second"]
                .map(|who| {
                    let backend = Cgroups::Cgroupfs { root: root.clone() };
                    let who = who.to_owned();
                    std::thread::spawn(move || backend.prepare(&who))
                })
                .into_iter()
                .map(|t| t.join().expect("the thread"))
                .collect();

            for outcome in &outcomes {
                assert!(
                    outcome.is_ok(),
                    "round {round}: a Runner refused to start because another was                      preparing at the same moment: {:?}",
                    outcome.as_ref().err().map(ToString::to_string),
                );
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_probe_is_this_runners_own_and_is_never_read_as_a_run() {
        assert_ne!(probe_name("first"), probe_name("second"));
        for instance in ["abc", "test-judging", "a/b", "../x", ""] {
            let name = probe_name(instance);
            assert!(name.starts_with(".probe."), "{name}");
            assert!(!name.starts_with(OURS), "{name}");
            assert_eq!(Path::new(&name).components().count(), 1, "{name}");
        }
    }

    #[test]
    fn an_empty_cgroup_root_is_unset_rather_than_honoured() {
        for value in [None, Some(String::new()), Some("   ".to_owned())] {
            assert_eq!(root_from(value).expect("the default"), root());
        }
        assert_eq!(
            root_from(Some(" /mnt/x ".to_owned())).expect("trimmed"),
            PathBuf::from("/mnt/x")
        );
    }

    #[test]
    fn a_relative_cgroup_root_is_refused_and_named() {
        let refusal = root_from(Some("cgroup".to_owned())).expect_err("a refusal");
        assert!(refusal.to_string().contains("AJ_Sandbox__CgroupRoot"));
    }

    /// One consequence, so one standard: every refusal says what it costs, and
    /// none of them still tells an operator to reconfigure the daemon.
    #[test]
    fn every_refusal_says_what_it_costs_and_none_asks_for_a_driver() {
        let refusals = [
            unsupported_version("Some(_1)").to_string(),
            root_from(Some("cgroup".to_owned()))
                .expect_err("a refusal")
                .to_string(),
            Cgroups::choose("none", root(), "abc")
                .expect_err("a refusal")
                .to_string(),
            readable_hierarchy(Path::new("/nowhere/at/all"))
                .expect_err("a refusal")
                .to_string(),
        ];
        for refusal in refusals {
            assert!(refusal.contains("processor time"), "{refusal}");
            assert!(!refusal.contains("native.cgroupdriver"), "{refusal}");
        }
    }
}
