//! What a run is allowed to do, and what it did.

use std::path::PathBuf;
use std::time::Duration;

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

    pub memory_kib: u64,
    pub pids: i64,
    /// Whole cores. Pinning as well as capping is what stops threads buying
    /// wall-clock time that a single-threaded rule says they may not have.
    pub cpus: f64,

    /// **The limit plus a grace, not the limit.** A program stuck in an
    /// uninterruptible syscall has to be reaped by something outside it, and the
    /// verdict a participant reads comes from the measured time rather than from
    /// this deadline.
    pub wall_clock: Duration,

    pub max_output_bytes: u64,

    pub mounts: Vec<Mount>,

    /// A writable scratch area, mounted `noexec`.
    pub tmpfs_kib: Option<u64>,

    /// One core, by number, for the step that is being timed.
    ///
    /// **Capping CPU is not the same as pinning it.** `--cpus=1` limits how much
    /// CPU time a program may use per second; it does not stop two threads
    /// running on two cores and finishing in half the wall-clock time. A problem
    /// that states a one-second limit means one second of one core, and the
    /// verdict a participant reads comes from the wall clock — so without this,
    /// threads buy time the single-thread rule says they may not have.
    pub cpuset: Option<String>,

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
            memory_kib: 256 * 1024,
            pids: 64,
            cpus: 1.0,
            wall_clock: Duration::from_secs(10),
            max_output_bytes: 64 * 1024 * 1024,
            mounts: Vec::new(),
            tmpfs_kib: None,
            cpuset: None,
            writable_root: false,
            collect: None,
        }
    }

    pub fn memory_kib(mut self, kib: u64) -> Self {
        self.memory_kib = kib;
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

    pub fn max_output_bytes(mut self, bytes: u64) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    pub fn mount(mut self, mount: Mount) -> Self {
        self.mounts.push(mount);
        self
    }

    pub fn tmpfs_kib(mut self, kib: u64) -> Self {
        self.tmpfs_kib = Some(kib);
        self
    }

    pub fn cpuset(mut self, core: usize) -> Self {
        self.cpuset = Some(core.to_string());
        self
    }

    pub fn writable_root(mut self) -> Self {
        self.writable_root = true;
        self
    }

    pub fn collect(mut self, path: impl Into<String>) -> Self {
        self.collect = Some(path.into());
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
    /// The wall-clock deadline passed.
    WallClock,
    /// The kernel killed it at the memory limit.
    Memory,
    /// It produced more than it was allowed to.
    Output,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub exit_code: i64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub wall_time: Duration,
    pub stopped: Stopped,

    /// **Absent for now, and deliberately not guessed.** The container runtime
    /// reports peak memory inconsistently across cgroup versions, and a number
    /// that is sometimes wrong is worse than no number: it would be shown to a
    /// participant beside a verdict. Getting it right is one of the reasons
    /// `isolate` is on the roadmap as a deeper supervisor.
    pub peak_memory_kib: Option<u64>,

    /// Absent for the same reason. The verdict for a time limit comes from the
    /// wall clock until something can measure CPU time honestly.
    pub cpu_time: Option<Duration>,

    /// A tar archive of whatever `Profile::collect` named, if anything did.
    pub collected: Option<Vec<u8>>,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        self.stopped == Stopped::OnItsOwn && self.exit_code == 0
    }
}
