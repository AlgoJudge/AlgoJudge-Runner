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
//! — and removing its directory does not release the unit, `CollectMode` being
//! `inactive`. So a slice per test would be a **permanent systemd unit per
//! test**, growing for as long as the installation judges anything. Nothing
//! this Runner can reach takes them away — only `systemctl stop` as root does,
//! and not asking for them in the first place.
//!
//! **The cost of one is small, and that is not the argument.** This said two
//! hundred cost 49 MB in pid 1; re-measured 2026-09-04, stopping 238 of them
//! released nothing measurable, in a pid 1 whose whole resident set was 17 MB
//! while it held 250. Unbounded growth is disqualifying whatever the constant,
//! which is why the constant was never load-bearing.
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

/// The cgroup a submission is judged in, under the run it belongs to.
const BOX: &str = "box";

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

    /// The cgroup the submission itself is judged in, in this process's view.
    ///
    /// **A sibling of the container's own cgroup, not a child of it**, and both
    /// sit under what this Runner reads. So the processor time a run is charged
    /// still comes off one file covering everything the run did, while memory
    /// comes off a cgroup that holds the submission and nothing else — no shim,
    /// no container start, no page cache of anything the container read.
    ///
    /// A child of the container's would have inherited every control instead of
    /// copying four of them, which is the one thing to be said for it. It also
    /// needs the cgroup found from inside the container and this process
    /// migrated out of its own before `memory` can be handed to children at all
    /// — cgroup v2 refuses a cgroup that has both processes and controlled
    /// children. More shim, the same directory bound writable either way.
    pub(crate) fn bound_at(&self, name: &str) -> PathBuf {
        match self {
            // The run's own directory, beside the container the daemon puts
            // there.
            Self::Cgroupfs { root } => root.join(OURS).join(name).join(BOX),
            // Inside the slice, beside the container's scope. The run's name
            // carries this Runner's prefix, so a leftover says whose it is.
            Self::Systemd { .. } => self.home().join(name),
        }
    }

    /// The same directory as the **daemon** would name it, for the bind mount.
    pub(crate) fn bound_for_daemon(&self, name: &str) -> Option<String> {
        let at = self.bound_at(name);
        let root = match self {
            Self::Cgroupfs { root } | Self::Systemd { root, .. } => root,
        };
        // Joined from components rather than printed: a path prints with the
        // separator of the machine this runs on, and the daemon's is always the
        // one below.
        let under: Vec<String> = at
            .strip_prefix(root)
            .ok()?
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        Some(format!("{DAEMON_ROOT}/{}", under.join("/")))
    }

    /// Makes that cgroup and states its limits, or answers `None`.
    ///
    /// `None` where none was asked for, and where one was and could not be made
    /// — the caller tells those apart by whether it asked, and refuses the run
    /// in the second case. **A judged run whose limits were not applied must not
    /// be judged**: it would be a submission held to no memory limit at all,
    /// reported as an ordinary verdict, with nothing on the screen to say so.
    fn make_the_box(&self, name: &str, bounds: Option<&Bounds>) -> Option<PathBuf> {
        let bounds = bounds?;
        let at = self.bound_at(name);
        let parent = at.parent()?;

        // **systemd makes the slice when it is first asked for a container in
        // it**, which is after this. Without this the first run of a Runner's
        // life has nowhere to put the box and every later one is fine, which is
        // the worst shape a defect can have.
        std::fs::create_dir_all(parent).ok()?;
        delegate(&self.home());
        delegate(parent);

        match std::fs::create_dir(&at).and_then(|()| bounds.applied_to(&at)) {
            Ok(()) => Some(at),
            Err(e) => {
                tracing::warn!(
                    at = %at.display(),
                    error = %e,
                    "could not make the cgroup a submission is judged in",
                );
                // Half a cgroup is worse than none: it would hold the submission
                // under whatever limits did get written.
                let _ = std::fs::remove_dir(&at);
                None
            }
        }
    }

    /// Opens a measurement for one run, and says what the daemon should be told.
    ///
    /// `None` means this run goes unmeasured, which is not an error here: the
    /// limits are the runtime's and hold either way. It is `aj-standard-io` that
    /// refuses to reach a verdict without a reading.
    pub(crate) async fn begin(
        &self,
        name: &str,
        bounds: Option<&Bounds>,
    ) -> Option<(Measuring, String)> {
        let parent = self.parent(name);
        let measuring = match self {
            Self::Cgroupfs { root } => {
                let here = root.join(OURS).join(name);
                std::fs::create_dir(&here).ok()?;
                let bound = self.make_the_box(name, bounds);
                Measuring::Own { here, bound }
            }
            Self::Systemd { gate, .. } => {
                let gate = gate.clone().lock_owned().await;
                let here = self.home();
                // The slice exists only once systemd has been asked for it, so
                // the first run of a Runner's life finds nothing here — and is
                // also the only run whose peak is the slice's own history.
                let fresh = !here.is_dir();
                // After `fresh` and before `reset_peak`: this may be what
                // creates the slice, and a Runner's first run must not read
                // its own creation as the slice's history.
                let bound = self.make_the_box(name, bounds);
                let opened = if fresh { None } else { reset_peak(&here) };
                Measuring::Shared {
                    bound,
                    cpu_before: usage_usec(&here).unwrap_or(0),
                    oom_before: memory_kills(&here).unwrap_or((0, 0)),
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
                    "this Runner cannot make, limit and remove a cgroup under {}: {e}. A time \
                     limit is decided on processor time read from cpu.stat in a cgroup this \
                     Runner makes for each run, and a memory limit is enforced on a second one \
                     inside it holding the submission alone, so without them it would register \
                     and then fail every job it claimed. Mount the host's cgroup tree writable and share its namespace — \
                     --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup, or in Compose `cgroup: \
                     host` on the service plus the same volume — and run the container as root, \
                     because the tree's directories are root's. It costs write permission, not a \
                     capability",
                    home.display()
                ))
            })
    }

    /// Removes the cgroups of runs that ended without anything removing theirs,
    /// and answers how many.
    ///
    /// **A Runner stopped in the middle of a job leaves one.** The evaluation is
    /// cancelled where it stands, so [`Measuring::finish`] — which is what
    /// removes a run's directory — is never reached, and the directory sits in
    /// the tree until somebody takes it away. This is that somebody, called
    /// once the containers are gone, because a cgroup with a live child cannot
    /// be removed at all.
    ///
    /// **This Runner's only**, matched on the name every run carries. Several
    /// Runners share one `algojudge` directory, and inside a container every one
    /// of them is pid 1 — so the name is the only thing that says whose a
    /// leftover is.
    ///
    /// **Under `systemd` there is now something to sweep and there was not
    /// before.** The slice itself is the Runner's for its whole life and is
    /// never any one run's — but the cgroup a submission is judged in is made
    /// per run, inside that slice, and a run cancelled where it stands leaves
    /// one exactly as `cgroupfs` does. A scope the daemon made is not touched:
    /// it carries the daemon's name and not this Runner's prefix.
    pub(crate) fn abandoned(&self, instance: &str) -> usize {
        let prefix = run_prefix(instance);
        let Ok(entries) = std::fs::read_dir(self.home()) else {
            return 0;
        };

        entries
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            // `rmdir` refuses a cgroup that still holds a process or a child,
            // and that refusal is the safety rather than a race to be avoided:
            // what goes is what nothing was using.
            .filter(|entry| {
                // The submission's own cgroup is *inside* the run's under
                // `cgroupfs`, and `rmdir` refuses a cgroup that has a child — so
                // a sweep that did not take it first would leave both for ever.
                let _ = std::fs::remove_dir(entry.path().join(BOX));
                std::fs::remove_dir(entry.path()).is_ok()
            })
            .count()
    }

    /// Why peak memory cannot be reported on this host, where it cannot.
    /// `None` means it can.
    ///
    /// Only the `systemd` backend can lose it: one slice serves every run, so a
    /// peak taken from the slice needs `memory.peak` reset — a write to a
    /// root-owned file, and a kernel interface that arrived in **Linux 6.12**,
    /// commit `c6f53ed8f213`.
    ///
    /// **A judged submission's peak does not come from there**, so it is not
    /// what this is about: that one is read from a cgroup made for the
    /// submission and holding it alone, which is fresh and needs no reset. What
    /// a host like this loses is the peak of the runs that are nobody's
    /// submission — a build, a checker, an interactor — which no screen shows.
    /// Processor time is unaffected either way, so this is said at start rather
    /// than refused.
    ///
    /// **The write is attempted and not merely the open**, because an open
    /// tests a file mode rather than the interface. Before 6.12 the file is
    /// expected to be read-only, and a host where that expectation is wrong
    /// would be promised a number it cannot deliver. The write costs nothing:
    /// the reset is per descriptor, and this descriptor is dropped.
    pub fn without_peak_memory(&self) -> Option<String> {
        let Self::Systemd { root, .. } = self else {
            return None;
        };
        let peak = own_cgroup(root)?.join("memory.peak");
        let attempt = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&peak)
            .and_then(|mut file| file.write_all(b"0"));
        attempt.err().map(|e| {
            format!(
                "{} cannot be reset: {e}. Under the systemd cgroup driver one slice serves \
                 every run, so a peak taken from it needs memory.peak reset, which needs Linux \
                 6.12 or later and a cgroup tree mounted writable in a container running as \
                 root. A judged submission is unaffected: its peak is read from a cgroup made \
                 for it alone. What is lost is the peak of builds, checkers and interactors, \
                 which no screen shows",
                peak.display()
            )
        })
    }
}

/// Where the **daemon** finds the cgroup tree.
///
/// Not [`Cgroups::home`], which is this process's own view and may be wherever a
/// `-v` put it. That the two name the same tree is already assumed — `parent()`
/// hands the daemon a path relative to its root and says nothing about where the
/// root is — and this is the one place the assumption has to become an absolute
/// path, because a bind mount is resolved by the daemon.
const DAEMON_ROOT: &str = "/sys/fs/cgroup";

/// One period of `cpu.max`, in microseconds. The kernel's own default, and the
/// one the runtime uses when it converts `--cpus`.
const CPU_PERIOD_US: i64 = 100_000;

/// What one submission is held to, in the cgroup that holds it and nothing else.
///
/// **Every field here is a control the kernel applies to a cgroup**, and the
/// submission is in a cgroup of its own — so each has to be stated twice, once
/// on the container and once on the submission's own. That is the whole risk of
/// giving the submission a cgroup: a control set on the container alone stops
/// reaching the program it was for, silently, and a fork bomb or an unpinned run
/// looks exactly like a working one.
///
/// The mechanism against that is not vigilance. Both places destructure this
/// struct **without `..`**, so a fifth control cannot be added here without the
/// compiler stopping at each of them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bounds {
    pub(crate) memory_bytes: u64,
    pub(crate) pids: i64,
    pub(crate) cpus: f64,
    pub(crate) cpuset: Option<String>,
}

impl Bounds {
    /// Everything in a profile that a cgroup enforces, and nothing else.
    pub(crate) fn of(profile: &crate::Profile) -> Self {
        Self {
            memory_bytes: profile.memory_bytes,
            pids: profile.pids,
            cpus: profile.cpus,
            cpuset: profile.cpuset.clone(),
        }
    }

    /// Writes every one of them into a cgroup that already exists.
    pub(crate) fn applied_to(&self, at: &Path) -> std::io::Result<()> {
        let Self {
            memory_bytes,
            pids,
            cpus,
            cpuset,
        } = self;

        std::fs::write(at.join("memory.max"), memory_bytes.to_string())?;
        // Equal to the limit, exactly as on the container and for the same
        // reason: without it the program swaps instead of being killed. Absent
        // is a kernel with no swap controller, where there is nothing to evade.
        write_if_present(&at.join("memory.swap.max"), "0")?;
        // **The cgroup dies together.** One process of a submission killed while
        // the others carry on is a program whose answer came out of a run the
        // kernel had already stopped; and the participant is told about the
        // whole submission either way.
        write_if_present(&at.join("memory.oom.group"), "1")?;
        std::fs::write(at.join("pids.max"), pids.to_string())?;
        std::fs::write(
            at.join("cpu.max"),
            format!("{} {CPU_PERIOD_US}", (cpus * CPU_PERIOD_US as f64) as i64),
        )?;
        if let Some(cpuset) = cpuset {
            std::fs::write(at.join("cpuset.cpus"), cpuset)?;
        }
        Ok(())
    }
}

/// Writes a control that a kernel may simply not carry, and treats its absence
/// as nothing to do. A file that **is** there and refuses the write is an error:
/// the difference between a control this host has not got and one it would not
/// apply is the difference between a limit that needs no enforcing and one that
/// silently is not.
fn write_if_present(at: &Path, value: &str) -> std::io::Result<()> {
    if !at.exists() {
        return Ok(());
    }
    std::fs::write(at, value)
}

/// Hands a cgroup's children the controllers this Runner sets on them.
///
/// A child's `memory.max` does not exist until its parent's
/// `cgroup.subtree_control` says `memory`. The runtime does this along the path
/// it creates — but it does it when it creates the container's cgroup, which is
/// after the submission's own has to be there.
///
/// **One controller per write.** A single write naming several is refused whole
/// if any one of them is unavailable, so asking for four on a kernel built
/// without `cpuset` would leave a run with none. Each is asked for separately
/// and only where the cgroup says it has it; whether it took is answered by
/// [`Bounds::applied_to`], which writes the files themselves.
fn delegate(dir: &Path) {
    let Ok(available) = std::fs::read_to_string(dir.join("cgroup.controllers")) else {
        return;
    };
    for controller in ["memory", "pids", "cpu", "cpuset"] {
        if available.split_whitespace().any(|it| it == controller) {
            let _ = std::fs::write(dir.join("cgroup.subtree_control"), format!("+{controller}"));
        }
    }
}

/// What one run cost, and whether the kernel killed anything in it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reading {
    pub(crate) peak_memory_bytes: Option<u64>,
    pub(crate) cpu_time: Option<Duration>,
    /// Processes the kernel killed here — `oom_kill` from `memory.events`.
    ///
    /// **The second opinion on a memory kill, and the one that comes from the
    /// kernel.** The runtime reports `OOMKilled` on the container, which is what
    /// the verdict was decided on until 2026-09-03 — and CI caught it reporting
    /// `false` for a container that exited 137 after 117 ms, which turns a
    /// memory limit into a runtime error in front of a participant.
    ///
    /// **Not enough on its own**, and the kernel's own wording is why: *"the
    /// number of processes belonging to this cgroup killed by **any kind of OOM
    /// killer**"*. That includes the system one, so a host that ran out of
    /// memory and killed a submission would be reported as the submission
    /// exceeding its own limit. See [`Self::over_limit`].
    pub(crate) oom_kills: u64,

    /// Times this cgroup reached **its own** limit — `oom` from `memory.events`.
    ///
    /// *"The number of time the cgroup's memory usage was reached the limit and
    /// allocation was about to fail."* That is the question a memory verdict
    /// asks, and it is what tells a submission over its limit from a host over
    /// its own.
    ///
    /// **Also not enough on its own**: it moves without anything dying —
    /// measured on one slice after a term of judging, `oom 845` against
    /// `oom_kill 843`. So a memory kill is both, and neither alone.
    pub(crate) over_limit: u64,
}

impl Reading {
    /// The memory half taken from the submission's own cgroup where there is
    /// one, and the processor time kept.
    ///
    /// **The two quantities are read from two different cgroups, deliberately.**
    /// Memory is the submission's own, because a container's floor and the page
    /// cache of everything it read are not the program's doing and a limit is
    /// compared against this. Processor time stays the whole run's: that is what
    /// the reaping deadline watches for progress and what a shim's report is
    /// disbelieved against, and the shim's own time is microseconds either way.
    fn about_the_submission(self, submission: Option<Reading>) -> Reading {
        match submission {
            None => self,
            Some(its_own) => Reading {
                cpu_time: self.cpu_time,
                ..its_own
            },
        }
    }
}

/// What the submission's own cgroup held, taking it away as it reads it.
///
/// Removed here and before the run's own directory it sits in: `rmdir` refuses
/// a cgroup that still has a child, so a finish that took the outer one first
/// would leave both behind for the sweep.
fn took_the_submissions_own(bound: Option<PathBuf>) -> Option<Reading> {
    let at = bound?;
    let peak = read_number(&at.join("memory.peak"));
    let (oom_kills, over_limit) = memory_kills(&at).unwrap_or((0, 0));
    let _ = std::fs::remove_dir(&at);
    Some(Reading {
        peak_memory_bytes: peak,
        cpu_time: None,
        oom_kills,
        over_limit,
    })
}

/// One run's measurement, held from before its container starts until after it
/// is gone.
pub(crate) enum Measuring {
    /// A directory of this run's own: read it, then take it away.
    Own {
        here: PathBuf,
        /// The submission's own cgroup, inside this one - see
        /// [`Cgroups::bound_at`]. `None` for every run that is not a judged
        /// submission, and for a host that could not make one.
        bound: Option<PathBuf>,
    },
    /// A slice shared by every run of this Runner: what changed in it while
    /// this run was the only thing there.
    Shared {
        here: PathBuf,
        /// The submission's own cgroup, beside its container - see
        /// [`Cgroups::bound_at`].
        bound: Option<PathBuf>,
        cpu_before: u64,
        /// What the slice had already seen, both counters. Cumulative like
        /// `cpu_before`, and for the same reason: one slice serves every run.
        oom_before: (u64, u64),
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
    /// Whether the submission was given a cgroup of its own.
    ///
    /// The caller asked for one or it did not, and this says whether it got it.
    /// A judged run that asked and did not get one is refused rather than judged
    /// under limits that were never applied.
    pub(crate) fn bounded(&self) -> bool {
        match self {
            Self::Own { bound, .. } | Self::Shared { bound, .. } => bound.is_some(),
        }
    }

    /// Processor time so far, **without ending the measurement**.
    ///
    /// The same arithmetic as [`Self::finish`] does for processor time, and it
    /// has to stay the same: this is what decides whether a program is making
    /// progress, and a reading on a different basis would answer a different
    /// question. `cpu.stat` is live, so there is nothing to arrange — the file
    /// is simply read again.
    pub(crate) fn so_far(&self) -> Option<Duration> {
        match self {
            Self::Own { here, .. } => usage_usec(here).map(Duration::from_micros),
            Self::Shared {
                here, cpu_before, ..
            } => usage_usec(here)
                .map(|now| Duration::from_micros(now.checked_sub(*cpu_before).unwrap_or(now))),
        }
    }

    /// How long something in here has been **runnable and waiting for a
    /// processor**, cumulative, from `cpu.pressure`.
    ///
    /// **The one reading that tells starvation from idleness**, which
    /// [`Self::so_far`] cannot: a program the kernel never scheduled and a
    /// program that is not trying to run both spend no processor time, and the
    /// deadline exists to reap only the second. Pressure separates them —
    /// something waiting for a core is counted here and something asleep is
    /// not.
    ///
    /// **`some`, not `full`.** `some` is the time at least one task was
    /// stalled; `full` is the time they all were. A single-process submission
    /// makes them nearly equal, but a program with threads loses a verdict to
    /// the first long before the second, and it is the same program either way.
    ///
    /// Cumulative and unadjusted, because the caller compares one look with the
    /// next and a difference needs no origin. That also makes the two backends
    /// the same: under `systemd` the slice carries every run this Runner has
    /// done, and one run at a time is in it, so the change across a look is
    /// still this run's.
    ///
    /// `None` where the kernel carries no PSI — built without `CONFIG_PSI`, or
    /// booted with `psi=0`. The caller must then behave exactly as it did
    /// before this existed: an absent instrument may not change a verdict.
    pub(crate) fn stalled(&self) -> Option<Duration> {
        let here = match self {
            Self::Own { here, .. } => here,
            Self::Shared { here, .. } => here,
        };
        stalled_usec(here).map(Duration::from_micros)
    }

    /// What the run cost: peak memory, then processor time.
    ///
    /// Either may be absent and neither is ever guessed. This is measurement,
    /// not enforcement — the limits were applied by the runtime and hold
    /// whether or not this succeeds.
    pub(crate) fn finish(self) -> Reading {
        match self {
            Self::Own { here, bound } => {
                let peak = read_number(&here.join("memory.peak"));
                let cpu = usage_usec(&here).map(Duration::from_micros);
                // A directory of this run's own, so the counts are this run's.
                let (oom_kills, over_limit) = memory_kills(&here).unwrap_or((0, 0));
                let submission = took_the_submissions_own(bound);
                // Read before the directory goes, and removed here rather than
                // left: the child's own cgroup is taken away with its container,
                // and this one is nobody else's to collect.
                let _ = std::fs::remove_dir(&here);
                Reading {
                    peak_memory_bytes: peak,
                    cpu_time: cpu,
                    oom_kills,
                    over_limit,
                }
                .about_the_submission(submission)
            }
            Self::Shared {
                here,
                bound,
                cpu_before,
                oom_before,
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
                let (kills, limits) = memory_kills(&here).unwrap_or(oom_before);
                Reading {
                    peak_memory_bytes: peak,
                    cpu_time: cpu,
                    oom_kills: kills.saturating_sub(oom_before.0),
                    over_limit: limits.saturating_sub(oom_before.1),
                }
                .about_the_submission(took_the_submissions_own(bound))
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

/// What everything one Runner makes for a run is named after.
///
/// **The Runner, not the process.** The name was the pid, which says nothing
/// about whose a container or a cgroup is: several Runners share one host and
/// one `algojudge` directory, and inside a container every one of them is pid 1.
/// A sweep keyed on that would have removed a neighbour's live run. The
/// fingerprint is this Runner's and survives a restart, which is what lets a
/// Runner clear up after the process it used to be.
///
/// A leading `.` is what keeps [`probe_name`] out of this: the probe is made and
/// removed within one call, and a sweep that caught one mid-flight would be
/// racing `prepare` for no reason.
pub(crate) fn run_prefix(instance: &str) -> String {
    format!("{OURS}-{}-", unit_safe(instance))
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

/// `some total=` from `cpu.pressure`: microseconds with something stalled on a
/// processor.
///
/// The file's first line is `some avg10=… avg60=… avg300=… total=…`. The
/// averages are decayed and describe the recent past; `total` is a counter and
/// is the only field a difference can be taken from.
fn stalled_usec(dir: &Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("cpu.pressure"))
        .ok()?
        .lines()
        .find(|line| line.starts_with("some "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("total="))
        .and_then(|v| v.parse().ok())
}

/// `oom_kill` and `oom` from `memory.events`, in that order.
///
/// **`memory.events` and not `memory.events.local`**, because the container runs
/// in a *child* of the cgroup this reads and only the first is hierarchical:
/// *"all fields in this file are hierarchical"*, against `.local`, whose fields
/// are *"local to the cgroup i.e. not hierarchical"*.
///
/// Both fields or neither: they answer different questions, and [`Reading`]
/// says which is which.
fn memory_kills(dir: &Path) -> Option<(u64, u64)> {
    let events = std::fs::read_to_string(dir.join("memory.events")).ok()?;
    let field = |name: &str| {
        events
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    Some((field("oom_kill ")?, field("oom ")?))
}

fn read_number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Opens `memory.peak`, resets it, and says what the slice held at that moment.
///
/// Writing any non-empty string resets the high-water mark **for that
/// descriptor only** — a fresh open still reports the cgroup's whole history —
/// so the file has to be carried to the end of the run. **Linux 6.12 and
/// later** — commit `c6f53ed8f213`, *"memcg: memory.swap and memory.peak write
/// handlers"*; before that the file is read-only and this fails, which
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

    /// **A Runner stopped mid-job leaves its run's cgroup**, and clears it at
    /// the next sweep — its own, and nobody else's.
    ///
    /// Measured before it was written: CI's *No per-run cgroup was left behind*
    /// step went red the first time a test stopped a Runner while it was
    /// judging, on `algojudge/algojudge-1-14285993230047861089`. `finish` is
    /// what removes a run's directory, and a cancelled evaluation never reaches
    /// it.
    ///
    /// The neighbour is the other half. Several Runners share one `algojudge`
    /// directory, so a sweep that took every leftover would take a running
    /// evaluation's cgroup off a Runner that is busy.
    #[test]
    fn a_sweep_clears_this_runners_abandoned_runs_and_leaves_a_neighbours() {
        let root = std::env::temp_dir().join(format!("aj-abandoned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("algojudge");
        std::fs::create_dir_all(&home).expect("a scratch home");

        let ours = home.join(format!("{}dead", run_prefix("ours")));
        let neighbour = home.join(format!("{}live", run_prefix("theirs")));
        let probe = home.join(probe_name("ours"));
        for made in [&ours, &neighbour, &probe] {
            std::fs::create_dir(made).expect("a scratch run");
        }
        // **The submission's own cgroup is inside the run's**, and `rmdir`
        // refuses a directory that has one. A sweep that did not take it first
        // would report a number and leave everything where it was.
        let inner = ours.join(BOX);
        std::fs::create_dir(&inner).expect("a scratch box");

        let backend = Cgroups::Cgroupfs { root: root.clone() };
        assert_eq!(backend.abandoned("ours"), 1);
        assert!(!ours.is_dir(), "the abandoned run was left behind");
        assert!(neighbour.is_dir(), "another Runner's run was swept");
        assert!(probe.is_dir(), "a start-up probe was swept");
        assert!(
            !inner.is_dir(),
            "the cgroup the submission was judged in stayed"
        );

        // And nothing is left to find on the second pass, so a sweep at every
        // start and every stop stays quiet rather than reporting a number.
        assert_eq!(backend.abandoned("ours"), 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The `systemd` backend has one thing per run to sweep and exactly one.**
    /// Its slice is the Runner's for its whole life and removing it would take
    /// the measurement away from the run in it -- but the cgroup a submission is
    /// judged in is made per run inside that slice, and a Runner stopped mid-job
    /// leaves one there just as it does under `cgroupfs`.
    #[test]
    fn a_systemd_backend_sweeps_the_boxes_and_nothing_else() {
        let root = std::env::temp_dir().join(format!("aj-abandoned-slice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let slice = root.join("algojudge.slice").join("algojudge-ours.slice");
        std::fs::create_dir_all(&slice).expect("a scratch slice");

        // A run cancelled where it stood, a neighbour's, and the daemon's own
        // container. Only the first is this Runner's to take away.
        let ours = slice.join(format!("{}dead", run_prefix("ours")));
        let neighbour = slice.join(format!("{}live", run_prefix("theirs")));
        let scope = slice.join("docker-0123456789ab.scope");
        for made in [&ours, &neighbour, &scope] {
            std::fs::create_dir(made).expect("a scratch cgroup");
        }

        let backend = Cgroups::Systemd {
            root: root.clone(),
            slice: "algojudge-ours.slice".to_owned(),
            gate: Arc::new(tokio::sync::Mutex::new(())),
        };
        assert_eq!(backend.abandoned("ours"), 1);
        assert!(!ours.is_dir(), "the abandoned box was left behind");
        assert!(neighbour.is_dir(), "another Runner's box was swept");
        assert!(scope.is_dir(), "a container the daemon made was swept");
        assert!(slice.is_dir(), "the Runner's own slice was removed");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A run's own directory carries the run's own count, and a cgroup that
    /// never saw a kill answers zero rather than nothing.
    #[test]
    fn a_run_that_was_oom_killed_says_so_from_its_own_cgroup() {
        let here = std::env::temp_dir().join(format!("aj-oom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&here);
        std::fs::create_dir_all(&here).expect("a scratch cgroup");
        std::fs::write(
            here.join("cpu.stat"),
            "usage_usec 4056
user_usec 4000
",
        )
        .unwrap();
        std::fs::write(
            here.join("memory.peak"),
            "7655424
",
        )
        .unwrap();
        std::fs::write(
            here.join("memory.events"),
            "low 0
high 0
max 12
oom 1
oom_kill 2
oom_group_kill 0
",
        )
        .unwrap();

        let reading = Measuring::Own {
            here: here.clone(),
            bound: None,
        }
        .finish();
        assert_eq!(reading.oom_kills, 2);
        assert_eq!(
            reading.over_limit, 1,
            "`oom` is not `oom_kill`, and both are read"
        );
        assert_eq!(reading.peak_memory_bytes, Some(7_655_424));
        assert_eq!(reading.cpu_time, Some(Duration::from_micros(4056)));

        // Not asserted here: that `finish` gave the directory back. A cgroup's
        // files are not directory entries, so `rmdir` works on a real one and
        // not on this imitation. `a_measured_run_leaves_no_cgroup_behind` is
        // where that is checked, against a cgroup.
        let _ = std::fs::remove_dir_all(&here);
    }

    /// **Every control the container gets, the submission's own cgroup gets.**
    ///
    /// [`Bounds`] is destructured without `..` in both places that apply it, so
    /// a fifth control cannot be added without the compiler stopping at each —
    /// but nothing in that says the values arrive. This says it.
    #[test]
    fn every_bound_reaches_the_cgroup_the_submission_is_judged_in() {
        let at = std::env::temp_dir().join(format!("aj-bounds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch cgroup");
        // The kernel makes these files; a scratch directory has to be given them.
        for file in [
            "memory.max",
            "memory.swap.max",
            "memory.oom.group",
            "pids.max",
            "cpu.max",
            "cpuset.cpus",
        ] {
            std::fs::write(at.join(file), "unset").unwrap();
        }

        let bounds = Bounds {
            memory_bytes: 2 * 1024 * 1024,
            pids: 16,
            cpus: 1.0,
            cpuset: Some("3".to_owned()),
        };
        bounds.applied_to(&at).expect("the controls");

        let said = |file: &str| std::fs::read_to_string(at.join(file)).unwrap();
        // Two mebibytes, which is a third of what the container runtime accepts
        // as its own smallest limit — and the whole point of the cgroup being
        // the submission's rather than the container's.
        assert_eq!(said("memory.max"), "2097152");
        assert_eq!(
            said("memory.swap.max"),
            "0",
            "a limit that can be swapped past"
        );
        assert_eq!(said("memory.oom.group"), "1");
        assert_eq!(said("pids.max"), "16", "a fork bomb would hit nothing");
        assert_eq!(said("cpu.max"), "100000 100000");
        assert_eq!(said("cpuset.cpus"), "3", "the run would not be pinned");

        // **A kernel that carries neither is not a failure to apply them.**
        // Without a swap controller there is nothing to swap past, and without
        // `memory.oom.group` the kernel kills one process instead of all of
        // them. A control this host has not got and one it would not apply are
        // different things, and only the second is an error.
        for file in ["memory.swap.max", "memory.oom.group"] {
            std::fs::remove_file(at.join(file)).unwrap();
        }
        bounds
            .applied_to(&at)
            .expect("the controls, on a plainer kernel");

        let _ = std::fs::remove_dir_all(&at);
    }

    /// Memory comes off the cgroup that held the submission, processor time off
    /// the one that held the whole run, and both directories go.
    #[test]
    fn a_bounded_run_reports_the_submissions_memory_and_the_runs_time() {
        let here = std::env::temp_dir().join(format!("aj-bounded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&here);
        let bound = here.join(BOX);
        std::fs::create_dir_all(&bound).expect("a scratch pair");

        let events = |oom: u64, kills: u64| {
            format!("low 0\nhigh 0\nmax 3\noom {oom}\noom_kill {kills}\noom_group_kill 0\n")
        };
        // The run's own: the container's floor and the page cache of everything
        // it read, and a kill count of nobody.
        std::fs::write(here.join("memory.peak"), "98765432\n").unwrap();
        std::fs::write(here.join("memory.events"), events(0, 0)).unwrap();
        std::fs::write(here.join("cpu.stat"), "usage_usec 4056\n").unwrap();
        // The submission's own.
        std::fs::write(bound.join("memory.peak"), "2097152\n").unwrap();
        std::fs::write(bound.join("memory.events"), events(1, 2)).unwrap();

        let reading = Measuring::Own {
            here: here.clone(),
            bound: Some(bound.clone()),
        }
        .finish();

        assert_eq!(
            reading.peak_memory_bytes,
            Some(2_097_152),
            "the container's own peak was reported as the program's",
        );
        assert_eq!((reading.oom_kills, reading.over_limit), (2, 1));
        assert_eq!(
            reading.cpu_time,
            Some(Duration::from_micros(4056)),
            "processor time is the whole run's on purpose",
        );
        // Not asserted here, and for the reason the case above it gives: a
        // cgroup's files are not directory entries, so `rmdir` works on a real
        // one and refuses this one. That both directories go, and that the
        // inner one goes first, is proved against a real tree in
        // `adversarial.rs::a_measured_run_leaves_no_cgroup_behind`.
        let _ = std::fs::remove_dir_all(&here);
    }

    /// The daemon resolves the bind mount, and it does not share this process's
    /// view of where the tree is.
    #[test]
    fn the_daemon_is_told_an_absolute_path_under_its_own_root() {
        let elsewhere = PathBuf::from("/mnt/somebody-elses-mount");
        let cgroupfs = Cgroups::Cgroupfs {
            root: elsewhere.clone(),
        };
        assert_eq!(
            cgroupfs.bound_for_daemon("aj-run-ours-7").as_deref(),
            Some("/sys/fs/cgroup/algojudge/aj-run-ours-7/box"),
        );
        assert!(
            cgroupfs.bound_at("aj-run-ours-7").starts_with(&elsewhere),
            "this process reads it where this process can see it",
        );
    }

    #[test]
    fn a_cgroup_with_no_memory_events_is_not_a_kill() {
        let here = std::env::temp_dir().join(format!("aj-nooom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&here);
        std::fs::create_dir_all(&here).expect("a scratch cgroup");
        std::fs::write(
            here.join("memory.events"),
            "low 0
high 0
max 0
oom 0
oom_kill 0
",
        )
        .unwrap();
        assert_eq!(memory_kills(&here), Some((0, 0)));

        std::fs::remove_file(here.join("memory.events")).unwrap();
        assert_eq!(
            memory_kills(&here),
            None,
            "absent is not zero, and finish() decides which"
        );
        let _ = std::fs::remove_dir_all(&here);
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
