//! What a run is allowed to do, and what it did.

use std::path::PathBuf;
use std::time::Duration;

/// Where a language image carries the measuring shim.
///
/// **The sandbox owns this path and not the problem type**, because it is what
/// decides who a container starts as, and that is a confinement question rather
/// than a language one.
pub const SHIM: &str = "/usr/local/bin/aj-shim";

/// A read-only or read-write path handed into the sandbox.
///
/// **Never writable and executable at once.** A directory a submission can
/// write to and then execute from is the shortest route from "produced output"
/// to "ran something we did not compile".
#[derive(Debug, Clone)]
pub struct Mount {
    pub from: PathBuf,
    pub to: String,
    pub writable: bool,
}

impl Mount {
    pub fn read_only(from: impl Into<PathBuf>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            writable: false,
        }
    }

    pub fn writable(from: impl Into<PathBuf>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            writable: true,
        }
    }
}

/// One run: what to start, and every limit it is held to.
///
/// There is no "no limit" here on purpose. Each field has a value or the profile
/// does not compile, so a step cannot be added that quietly runs unbounded.
#[derive(Debug, Clone)]
pub struct Profile {
    pub image: String,
    pub command: Vec<String>,
    pub working_directory: String,

    pub memory_bytes: u64,
    pub pids: i64,
    /// Whole cores: how much processor time a run may spend per second,
    /// wherever the host chooses to spend it. On every container, always, and
    /// independent of [`Self::cpuset`].
    pub cpus: f64,

    /// **A deadline to reap by, and not a limit anybody is judged against.**
    ///
    /// A time limit is processor time (2026-09-02), so nothing here decides a
    /// verdict. What this catches is a program that is not *spending* processor
    /// time — one wedged in an uninterruptible syscall, or one that waits rather
    /// than computes — which a limit on the processor would never reach.
    ///
    /// **It counts only while the processor time is not growing**, where there
    /// is a [`Self::cpu_limit`] to measure that against. A program that computes
    /// therefore never reaches it, however long the host makes it wait for a
    /// processor — which on a busy host is most of its wall clock. Elsewhere,
    /// where nothing is being timed, it is the plain timeout it reads as.
    pub wall_clock: Duration,

    /// The processor time this step is judged against, where there is one.
    ///
    /// **Two things follow from it, and neither is the verdict.** The deadline
    /// above stops counting while the program is spending processor time, so a
    /// program that computes is never reaped for being descheduled — and a
    /// program that spends far past this is stopped rather than left to burn a
    /// Runner until the deadline.
    ///
    /// `None` for every step nobody is timed on — a build, a checker — and
    /// those keep a plain wall-clock timeout, which is the whole of what they
    /// need.
    pub cpu_limit: Option<Duration>,

    pub max_output_bytes: u64,

    pub mounts: Vec<Mount>,

    /// A writable scratch area, mounted `noexec`.
    pub tmpfs_bytes: Option<u64>,

    /// How many files it may hold open, and how large one may get.
    ///
    /// Neither is the main defence — the tmpfs size bounds what can be written
    /// and the memory limit bounds the rest — but both are cheap, and `fsize` is
    /// the one that turns "wrote a hundred gigabytes to scratch" from a slow
    /// failure into an immediate one.
    pub max_open_files: i64,
    pub max_file_bytes: i64,

    /// The processors a timed run may use: **the ones the Runner itself was
    /// given**, and absent when it was given the whole machine.
    ///
    /// [`crate::affinity`] holds the decision and the measurements behind it.
    /// The short of it: a job container inherits no affinity from the Runner --
    /// the daemon starts it, not the Runner -- so an operator's split has to be
    /// carried here explicitly or jobs escape it; and where there is no split, a
    /// pin chosen without coordination is worse than none, because several
    /// Runners choose the same processor while others idle and the kernel is
    /// then forbidden from repairing it.
    ///
    /// **Capping CPU is not the same as pinning it.** `--cpus=1` limits how much
    /// processor time a program may spend per second; it does not stop two
    /// threads running on two cores and finishing in half the wall-clock time.
    /// Neither does it need to: `cpu.stat` sums the whole subtree, so threads
    /// spend the budget faster rather than escaping it, and a limit is processor
    /// time.
    pub cpuset: Option<String>,

    /// Whether this step is one a participant is judged on the time of.
    ///
    /// **It decides who the container starts as.** A measured step goes through
    /// the shim, which needs to be root for as long as it takes to put the
    /// submission back to `nobody` — so the sandbox starts it as root and hands
    /// back `SETUID` and `SETGID`, and nothing else. Every other step, and any
    /// measured one whose image carries no shim, starts unprivileged as before:
    /// a fallback that ran a submission as root would be a far worse bargain
    /// than a coarser number.
    pub measured: bool,

    /// Lets the container write to its **own** layer — never to the host.
    ///
    /// Off for anything that runs a submission. On for a build, which has to
    /// put the program it made somewhere `collect` can read it back from: a
    /// tmpfs cannot serve, because it is destroyed with the container and the
    /// archive endpoint then finds nothing. The layer is discarded when the
    /// container is removed, which is immediately.
    pub writable_root: bool,

    /// A path inside the container to read back after it exits.
    ///
    /// **This is how a build hands over what it made**, instead of being given
    /// a writable host directory. A bind mount would have to be writable by
    /// whatever user the container runs as, which is a permission problem on
    /// every host and a hole on the ones where it is solved by opening the
    /// directory to everybody. Reading it back through the runtime API means
    /// the build container gets no writable host path at all.
    pub collect: Option<String>,

    /// The most the Runner will hold while reading that path back.
    ///
    /// **Set together with `collect`, and it has to be**: what comes back is
    /// whatever compiling untrusted code produced, it arrives in the *trusted*
    /// process, and a bound nobody stated is a bound nobody has. A submission
    /// declaring a 240 MiB initialised array is a one-line source and a binary
    /// that size.
    ///
    /// The container's own `fsize` is the first bound and the better one — it
    /// makes an oversized artefact the participant's compilation error rather
    /// than the machinery refusing after the fact. This is the second, and it
    /// exists because the first is a limit the *runtime* applies and this one
    /// is a limit **we** apply.
    pub max_collected_bytes: u64,
}

// Standard input is deliberately not a field here. A test's input is mounted
// read-only and the caller redirects it in its own command — `sh -c 'exec ./a.out
// < /in/1a.in'`. Doing it in the sandbox would mean the sandbox knowing that
// every image has a shell, which is a requirement it has no business having,
// and `exec` keeps the process tree the same size either way.

impl Profile {
    /// A profile with everything closed, to be opened deliberately.
    ///
    /// The default is the restrictive one so that a new pipeline step starts
    /// safe and each allowance is a visible line of code.
    pub fn new(image: impl Into<String>, command: Vec<String>) -> Self {
        Self {
            image: image.into(),
            command,
            working_directory: "/work".into(),
            memory_bytes: 256 * 1024 * 1024,
            pids: 64,
            cpus: 1.0,
            wall_clock: Duration::from_secs(10),
            cpu_limit: None,
            max_output_bytes: 64 * 1024 * 1024,
            mounts: Vec::new(),
            measured: false,
            tmpfs_bytes: None,
            max_open_files: 256,
            max_file_bytes: 256 * 1024 * 1024,
            cpuset: None,
            writable_root: false,
            collect: None,
            max_collected_bytes: 0,
        }
    }

    pub fn memory_bytes(mut self, bytes: u64) -> Self {
        self.memory_bytes = bytes;
        self
    }

    pub fn pids(mut self, pids: i64) -> Self {
        self.pids = pids;
        self
    }

    pub fn wall_clock(mut self, wall_clock: Duration) -> Self {
        self.wall_clock = wall_clock;
        self
    }

    pub fn cpu_limit(mut self, limit: Duration) -> Self {
        self.cpu_limit = Some(limit);
        self
    }

    pub fn max_output_bytes(mut self, bytes: u64) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    pub fn mount(mut self, mount: Mount) -> Self {
        self.mounts.push(mount);
        self
    }

    pub fn tmpfs_bytes(mut self, bytes: u64) -> Self {
        self.tmpfs_bytes = Some(bytes);
        self
    }

    pub fn max_file_bytes(mut self, bytes: i64) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    /// A processor set, spelled the way the kernel spells one: `3`, `0-1`,
    /// `0-3,8`.
    ///
    /// **A set rather than a number, because what is passed on is what the
    /// Runner was given.** See [`crate::affinity`]: the only caller hands over
    /// its own allowed processors verbatim, and hands over nothing when it was
    /// allowed everything.
    pub fn cpuset(mut self, set: impl Into<String>) -> Self {
        self.cpuset = Some(set.into());
        self
    }

    pub fn measured(mut self) -> Self {
        self.measured = true;
        self
    }

    pub fn writable_root(mut self) -> Self {
        self.writable_root = true;
        self
    }

    /// What to read back, and the most of it that will be held. **One call for
    /// both**, so a caller cannot ask for the first and forget the second.
    pub fn collect(mut self, path: impl Into<String>, max_bytes: u64) -> Self {
        self.collect = Some(path.into());
        self.max_collected_bytes = max_bytes;
        self
    }
}

/// Why a run ended early, if it did.
///
/// Distinct from the exit code, because a program killed at its memory limit
/// and one that returned a non-zero status are different things to tell a
/// participant, and the exit code alone cannot tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// It finished on its own.
    OnItsOwn,
    /// It went **so far past its processor-time limit** that there was no
    /// reason to keep waiting — see [`Profile::cpu_limit`]. The verdict is
    /// still decided afterwards, on the precise measurement; this only stops a
    /// program that is plainly over budget from holding a Runner.
    TimeLimit,
    /// The reaping deadline passed — see [`Profile::wall_clock`]. It is not the
    /// time limit: a program reaching it has stopped spending processor time
    /// altogether, which means waiting, or wedged in an uninterruptible call.
    WallClock,
    /// The kernel killed it at the memory limit.
    Memory,
    /// It produced more than it was allowed to.
    Output,
    /// **The deadline passed and the program never ran at all.** Not a verdict
    /// about anybody's code: no processor time was ever recorded against this
    /// run, so what took the time was the container, the image or the host —
    /// never the submission.
    ///
    /// Told apart from [`Stopped::WallClock`] by a single fact, and it is worth
    /// the extra variant because the two are opposites. A run that spent
    /// processor time and then stopped spending it has failed; a run that never
    /// spent any was never given the chance, and reporting that to a
    /// participant as "no processor time" reads as an accusation.
    NeverStarted,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub exit_code: i64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub wall_time: Duration,
    pub stopped: Stopped,

    /// Read from the run's own cgroup on a cgroup v2 host, and **absent rather
    /// than guessed** where the Runner was given nowhere to measure from.
    ///
    /// A number that is sometimes wrong is worse than no number, because it is
    /// shown to a participant beside a verdict — so absence is a real answer
    /// here and `PACKAGE_FORMAT.md` treats it as one.
    ///
    /// The runtime API is not the source: it reports no peak on cgroup v2, and
    /// a container's own cgroup does not outlive it. See `Docker::cgroup_root`
    /// for how the measurement is actually taken.
    pub peak_memory_bytes: Option<u64>,

    /// From `cpu.stat` in the same cgroup: user plus system, for the whole
    /// subtree.
    ///
    /// **What decides a time limit, since 2026-09-02.** It was the wall clock
    /// until then — which charged a participant for the container's own start
    /// and made a verdict as much a property of the host as of the submission,
    /// and which was the one arrangement no other judge in this space used.
    ///
    /// **Still `Option` here, deliberately.** This layer measures and stays
    /// honest about a host that gave it nowhere to measure from; it is
    /// `aj-standard-io` that refuses to make a verdict without one, and
    /// `Sandbox::preflight` that refuses to start such a Runner at all. Making
    /// it required here would fail the adversarial suite, which runs without a
    /// cgroup mount because what it asserts is enforcement.
    pub cpu_time: Option<Duration>,

    /// A tar archive of whatever `Profile::collect` named, if anything did.
    pub collected: Option<Vec<u8>>,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        self.stopped == Stopped::OnItsOwn && self.exit_code == 0
    }
}
