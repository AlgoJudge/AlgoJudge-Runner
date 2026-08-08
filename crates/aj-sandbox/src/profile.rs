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
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        self.stopped == Stopped::OnItsOwn && self.exit_code == 0
    }
}
