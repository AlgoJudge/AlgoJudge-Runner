//! What a run is allowed to do, and what it did.

use std::path::PathBuf;
use std::time::Duration;

/// Where a language image carries the measuring shim.
///
/// **The sandbox owns this path and not the problem type**, because it is what
/// decides who a container starts as, and that is a confinement question rather
/// than a language one.
pub const SHIM: &str = "/usr/local/bin/aj-shim";

/// What a shim says about itself when it can take its input as a descriptor.
///
/// **Read out of the binary in the image**, so a Runner can tell an image built
/// before the arrangement from one built after without starting anything. An
/// image whose shim predates it would open the socket as a file, fail with
/// `ENXIO`, and report every test as a run that measured nothing.
pub const SOCKET_INPUT: &str = "aj-shim-features: socket-input";

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
    /// Whether the container runtime must **refuse** a source that is not
    /// there, instead of making an empty directory in its place.
    ///
    /// **The default is the legacy bind, which creates one**, and that is the
    /// failure mode every path-as-the-daemon-sees-it mistake ends in: the
    /// container starts, the mount is empty, and a submission is judged against
    /// nothing. A mount the Runner knows must exist — the package unpacked in
    /// the shared cache, the judge built beside it — says so here, and a
    /// misconfigured `AJ_Cache__HostPath` is then a container that does not
    /// start and a sentence naming the path.
    pub required: bool,
}

impl Mount {
    pub fn read_only(from: impl Into<PathBuf>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            writable: false,
            required: false,
        }
    }

    pub fn writable(from: impl Into<PathBuf>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            writable: true,
            required: false,
        }
    }

    /// A source the daemon must already be able to open. See [`Mount::required`].
    pub fn required(mut self) -> Self {
        self.required = true;
        self
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

    /// **What the submission is held to, which is not what the container is.**
    /// A judged run puts the submission in a cgroup of its own and this is the
    /// `memory.max` written on it, so it holds the program, everything it forks
    /// and every tmpfs page it writes -- and nothing the container spent
    /// existing. Everywhere else, and on a host that could not make that cgroup,
    /// it is the container's own limit as it always was.
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

    /// A directory the shim writes the submission's stdout into, instead of
    /// leaving it on the container's own stream.
    ///
    /// **This is a disk measurement, not a preference.** Left on the container's
    /// stdout, every byte a submission prints is written by the daemon to its
    /// `*-json.log` — JSON-escaped, stamped per line — and read back through a
    /// socket. Measured 2026-09-05 across a twelve-Runner burst: a single
    /// flooding submission left a **76 MB** log against a 64 MiB cap, and the
    /// daemon wrote at 72 MB/s for the length of the run.
    ///
    /// **The submission is handed a descriptor and never a path.** The shim runs
    /// as root, opens the file before it forks, and only then drops to the
    /// submission's user — which cannot create, rename or read anything in a
    /// directory that is root's. It is what is already done for the input,
    /// pointing the other way.
    ///
    /// `None` leaves stdout where it was, which is what every step that is not
    /// judging a submission wants: a build's output *is* its compiler log.
    pub pipes: Option<Pipes>,

    pub mounts: Vec<Mount>,

    /// A writable scratch area, mounted `noexec`.
    pub tmpfs_bytes: Option<u64>,

    /// How many files it may hold open, and how large one may get.
    ///
    /// Neither is the main defense — the tmpfs size bounds what can be written
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

    /// Which of the Runner's measurement lanes this run is measured in.
    ///
    /// **Not a processor, and not a set of them** -- that is [`Self::cpuset`],
    /// and the two are set together by whoever holds the lane. This is the
    /// index of the home a reading comes out of: under `cgroupfs` every run
    /// makes a directory of its own and this changes nothing, and under
    /// `systemd` it picks which of the Runner's slices the run is started
    /// under, where *one run at a time* is the whole reason a reading is a
    /// difference rather than a number.
    ///
    /// Zero where nobody said, which is every caller that runs one thing at a
    /// time.
    pub lane: usize,

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

    /// Whether this run's standard input is a socket the Runner will hand a
    /// descriptor over, rather than a path to open.
    ///
    /// Stated rather than guessed at, because the sandbox is the one that has
    /// to refuse: an image whose shim predates the arrangement opens the socket
    /// as a file and reports a run that measured nothing, which reads as a
    /// broken host rather than as an image that needs rebuilding.
    pub socket_input: bool,

    /// Lets the container write to its **own** layer — never to the host.
    ///
    /// Off for anything that runs a submission. On for a build, which has to
    /// put the program it made somewhere `collect` can read it back from: a
    /// tmpfs cannot serve, because it is destroyed with the container and the
    /// archive endpoint then finds nothing. The layer is discarded when the
    /// container is removed, which is immediately.
    pub writable_root: bool,

    /// Nothing reaches this container's stdio, and the daemon keeps no log.
    ///
    /// **A disk measurement, not tidiness.** The `json-file` driver writes every
    /// byte a container prints, JSON-escaped and stamped per line, and one
    /// flooding submission left a **76 MB** file against a 64 MiB cap while the
    /// daemon wrote at 72 MB/s — measured 2026-09-05. A run whose output travels
    /// on a pipe the Runner holds has no use for a second copy of it on a disk.
    ///
    /// Off for a build and for a checker: those two are read through
    /// [`Outcome::stdout`] and [`Outcome::stderr`], and a driver of `none`
    /// refuses the endpoint that reads them.
    ///
    /// **It also takes [`Profile::max_output_bytes`] with it**, which is the
    /// consequence a caller is likeliest to miss. That cap is counted by the
    /// collector, and the collector is not started for a silent run at all — so
    /// a profile that is silent and states a cap is stating one nothing applies.
    /// Set one or the other.
    pub silent: bool,

    /// This container runs **beside** a measured one, and opens no measurement.
    ///
    /// **Without it a checker deadlocks against the submission it is checking**,
    /// and only where Docker puts most installations. Under the `systemd` cgroup
    /// driver one slice serves every run, so a second run in it waits for a gate
    /// the first holds — and the first is waiting for the second to read its
    /// output. It would pass every unit test and the whole `cgroupfs` leg of CI.
    ///
    /// Such a container is placed wherever the daemon puts it and is measured by
    /// nothing. It needs no measurement: anything that stops a checker other
    /// than its own exit already makes it a *broken* checker rather than a
    /// verdict.
    pub alongside: bool,

    /// What to put in this container's environment, as `NAME=value`.
    ///
    /// **For a program the package brought, and never for a submission.** The
    /// shim's own variables are added by the sandbox and are not these; a
    /// submission's container gets nothing here, because everything it is told
    /// arrives as an argument or on a channel, and an environment is a place to
    /// leak something into by accident.
    pub env: Vec<String>,

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
    /// declaring a 240 MiB initialized array is a one-line source and a binary
    /// that size.
    ///
    /// The container's own `fsize` is the first bound and the better one — it
    /// makes an oversized artifact the participant's compilation error rather
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

/// The directory a measured run's channels live in, on both sides of the mount.
///
/// **One directory per run and the names are fixed**, because the directory is
/// already one run's: a name carried from the test would be a second thing to
/// keep in step for nothing.
///
/// Two channels live here today and a third is coming. The Runner makes each of
/// them and owns it; the shim opens what it is given and creates nothing, so a
/// channel that is missing means the Runner did not do its half rather than
/// something for the far end to invent.
#[derive(Debug, Clone)]
pub struct Pipes {
    /// Where **this process** sees the directory.
    ///
    /// **Three views and not two, and the third is the one that bites.** The
    /// daemon resolves the bind mount, so `on_host` is its view; the command
    /// names `at`, so that is the container's. But the channels are *made and
    /// read by the Runner*, which where it is itself containerized sees neither
    /// of those — and a `mkfifo` against the daemon's path fails with "no such
    /// file or directory" while naming a path that plainly exists, which is a
    /// confusing hour for whoever meets it.
    pub here: PathBuf,
    /// Where the **daemon** sees it, for the bind mount.
    pub on_host: PathBuf,
    /// Where the **container** sees it, for the command.
    pub at: String,
}

impl Pipes {
    /// What the submission's own output travels on.
    pub const OUTPUT: &'static str = "stdout";

    /// What a submission's standard input travels on, where it has one.
    ///
    /// **Every judged run has one**, and what is behind it differs: for an
    /// interactive problem a pipe with the interactor at the far end, and for
    /// every other one a socket the Runner hands a descriptor over — the
    /// package's `<test>.in`, sealed into a file in memory. Nothing of the
    /// package is mounted into a judged container either way, and the
    /// descriptor is seekable, which is what a batch problem needs and a pipe
    /// could not give.
    pub const INPUT: &'static str = "stdin";

    /// What the shim's measurement report travels on.
    ///
    /// **Its own channel, and that is the point of it.** The report used to
    /// share the container's stderr with whatever the submission printed there,
    /// picked back out by a nonce and by being written last. Nothing else can
    /// write here, so the nonce is now belt to a brace.
    pub const REPORT: &'static str = "report";

    /// Where a channel is, as the container sees it.
    pub fn inside(&self, channel: &str) -> String {
        format!("{}/{channel}", self.at)
    }

    /// Where a channel is, as the daemon sees it.
    pub fn on_host(&self, channel: &str) -> PathBuf {
        self.on_host.join(channel)
    }

    /// Where a channel is, as this process sees it: what to make and read.
    pub fn here(&self, channel: &str) -> PathBuf {
        self.here.join(channel)
    }
}

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
            pipes: None,
            mounts: Vec::new(),
            measured: false,
            socket_input: false,
            tmpfs_bytes: None,
            max_open_files: 256,
            max_file_bytes: 256 * 1024 * 1024,
            cpuset: None,
            lane: 0,
            writable_root: false,
            silent: false,
            alongside: false,
            env: Vec::new(),
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

    /// Give the run a directory of channels, made by the caller and mounted
    /// at `at`.
    ///
    /// The caller makes every channel in it before the run starts, and names
    /// them with [`Pipes::inside`] when building the command.
    pub fn pipes(
        mut self,
        here: impl Into<PathBuf>,
        on_host: impl Into<PathBuf>,
        at: impl Into<String>,
    ) -> Self {
        self.pipes = Some(Pipes {
            here: here.into(),
            on_host: on_host.into(),
            at: at.into(),
        });
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

    /// The measurement lane this run belongs to. See [`Profile::lane`].
    ///
    /// Set beside [`Self::cpuset`] and by the same caller: a run placed on one
    /// lane's processors and measured in another's would be two halves of two
    /// different arrangements.
    pub fn lane(mut self, index: usize) -> Self {
        self.lane = index;
        self
    }

    /// Its standard input arrives as a descriptor. See [`Profile::socket_input`].
    pub fn reading_a_socket(mut self) -> Self {
        self.socket_input = true;
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

    /// Keep no log of this container, and read nothing back from it.
    ///
    /// For a run whose output travels somewhere the Runner already holds. See
    /// [`Profile::silent`] for what it costs, including the output cap.
    pub fn silent(mut self) -> Self {
        self.silent = true;
        self
    }

    /// Adds one `NAME=value` to the container's environment.
    pub fn env(mut self, entry: impl Into<String>) -> Self {
        self.env.push(entry.into());
        self
    }

    pub fn alongside(mut self) -> Self {
        self.alongside = true;
        self
    }

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
    /// The kernel killed it at the memory limit -- **the submission's own**,
    /// written on the cgroup that holds the submission and nothing else, so
    /// neither the container's floor nor the page cache of what the container
    /// read is inside what it was compared against.
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
    /// **The absolute cap passed while the program was still making progress.**
    /// Not [`Stopped::WallClock`]: that one says the processor time stopped
    /// growing, and here it never did — this is the program that wakes for a
    /// millisecond every tick, resetting the no-progress window for ever
    /// without ever approaching its limit.
    ///
    /// A variant of its own because the note a participant reads states which
    /// happened, and "no processor time for 8 s" is plainly false about a run
    /// that spent some in every one of those seconds.
    Overall,
    /// **Whatever was reading this run's output had already decided.**
    ///
    /// The exit code and any signal are the kill's doing and say nothing about
    /// the program — a run stopped here must be reported as what the reader
    /// decided, never as the runtime error a `SIGKILL` would otherwise look
    /// like. It is the one stop whose meaning lives outside the sandbox.
    Decided,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub exit_code: i64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub wall_time: Duration,
    pub stopped: Stopped,

    /// Read from a cgroup on a cgroup v2 host, and **absent rather than
    /// guessed** where the Runner was given nowhere to measure from.
    ///
    /// A number that is sometimes wrong is worse than no number, because it is
    /// shown to a participant beside a verdict — so absence is a real answer
    /// here and `PACKAGE_FORMAT.md` treats it as one.
    ///
    /// **Which cgroup decides what this contains.** A judged run has one holding
    /// the submission alone, and this is that one: the same number the limit was
    /// enforced against, so what a participant reads and what they were judged
    /// on cannot differ. Anywhere else it is the whole run's, and where a shim
    /// reported and there was no such cgroup it is the shim's `ru_maxrss` --
    /// one process's resident set, which counts neither a forked child nor a
    /// tmpfs page.
    ///
    /// The runtime API is not the source: it reports no peak on cgroup v2, and
    /// a container's own cgroup does not outlive it.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A profile says where it is measured as well as where it runs, and says
    /// the first lane when nobody said -- which is what every caller that runs
    /// one thing at a time means.
    #[test]
    fn a_profile_says_which_lane_it_is_measured_in() {
        let bare = Profile::new("image", vec!["true".to_owned()]);
        assert_eq!(bare.lane, 0);
        assert_eq!(bare.lane(2).lane, 2);
    }
}
