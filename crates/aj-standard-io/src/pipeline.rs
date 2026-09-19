//! Build, run, check, score — in that order, each step in its own container.
//!
//! Three isolated steps rather than one, because they need different things:
//! the build wants a compiler and room, a test wants neither and wants the
//! limits the problem states, and a checker is **package-authored code** that
//! gets its own sandbox rather than the Runner's process.
//!
//! Every failure of the machinery is an infrastructure failure. Only what the
//! program itself did becomes a verdict.

use std::path::{Path, PathBuf};
use std::time::Duration;

use aj_package::{Config, Test, TestSet};
use aj_sandbox::pipes::{open_for_reading, open_for_writing, release, release_writer, Fifo};
use aj_sandbox::{Beside, Enough, Mount, Pipes, Profile, Sandbox, Stopped};
use futures_util::StreamExt as _;

use crate::checker::{checker_said, Broken};
use crate::compare::{Comparing, Comparison};
use crate::details::{compiled, failed_to_compile, Compilation, Details, Limits};
use crate::language::{
    self, Images, ANSWER, BUILD_OUTPUT, FROM_THE_JUDGE, INPUT, OUTPUT, PROGRAM, SOURCE,
    TO_THE_JUDGE, VERDICT,
};
use crate::score::{judge, Judgment, Reason, Status, TestOutcome};

/// The scratch a compiler is given, for **every** build.
///
/// GCC's driver writes its intermediate `.s` and `.o` under `/tmp`, so this is
/// a compiler's working set rather than a guess. One constant because there is
/// one number: the checker is built by the same compiler, in the same image,
/// with the same command as a submission.
///
/// It was 64 MiB for a submission and 64 KiB for a checker — a dropped
/// `* 1024`, and not a difference anybody would read as one. The consequence
/// was silent and total: any checker whose assembly ran past 64 KiB, which is
/// most of them once `<iostream>` is included, failed to build with "No space
/// left on device", and that is an **infrastructure failure on every
/// submission to the problem**, reported in words that blame the author's
/// checker.
const BUILD_TMPFS_BYTES: u64 = 64 * 1024 * 1024;

/// What a build may write, and what the Runner will hold of what it wrote.
///
/// **The `fsize` limit is the one that matters, and it belongs on the
/// container.** `char pad[240*1024*1024] = {1};` is one line of source and a
/// binary that size; the profile's default is 256 MiB and neither build
/// overrode it, so the artifact was read into the trusted process — twice, at
/// the moment of joining — and `unpack` wrote a third copy into the job's
/// scratch, where it is mounted into every test container.
///
/// Applied to the container rather than caught afterwards, because `SIGXFSZ`
/// makes an oversized artifact the participant's **compilation error**, which
/// is a verdict they can act on, instead of an infrastructure failure that
/// claims the system broke. A statically linked C++ binary with heavy
/// templates is tens of megabytes, so this refuses what is not a program.
const BUILD_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

/// What a build may say, for **both** builds.
///
/// One constant for the reason the tmpfs above is one: the submission's build
/// capped its output at 256 KiB and the checker's capped nothing, so a checker
/// that would not stop talking put 64 MiB — the profile's default — into an
/// infrastructure-failure message and into the uploaded log.
const BUILD_LOG_BYTES: u64 = 256 * 1024;

/// What a judged submission may print before it is stopped.
///
/// **Counted here and nowhere else, which is the point of the number.** It used
/// to be `RLIMIT_FSIZE` on a file, and before that a count of what the daemon
/// had already written to its log — 76 MB of it, measured, for one flooding
/// submission against a 64 MiB cap. Now the only reader is the relay, so the
/// bytes are counted as they cross and the program is stopped on the chunk that
/// crosses the line.
///
/// Generous on purpose: it is a runaway `while (1) printf` this stops, not a
/// verbose solution. A problem whose answer genuinely approaches it wants a
/// checker, because nothing this size is compared token by token usefully.
const OUTPUT_CAP: u64 = 64 * 1024 * 1024;

/// How long a checker may run, and how long the Runner waits for it to open.
///
/// One constant for those two because they are the same wait seen from two
/// sides: a checker that has not opened its answer within its own wall clock is
/// a checker that is never going to.
///
/// **Waiting for a *container* is a different question** and has its own
/// constant below. The two are the same length today and mean different things,
/// which is why they are not one name.
const CHECKER_WALL_CLOCK: Duration = Duration::from_secs(30);

/// How long a channel waits for the container that is supposed to open it.
///
/// **A bound rather than a rescue.** Every one of these pipes is read by a
/// blocking thread, and a blocking open waits for a writer forever — which was
/// safe only while every path out of a run remembered to call `release`. That
/// rule failed twice: the interactor's verdict, and a judged run's own output,
/// each time as a Runner that never reported and re-claimed its job until it was
/// restarted. `release` is still called, and still ends a wait at once; this is
/// what makes it an optimization rather than the only way out.
///
/// Thirty seconds is the checker's own wall clock, and two orders of magnitude
/// above what it is bounding: `tests/judging.rs` records a container's own start
/// as "some 374 ms on the machine it was written on".
const CHANNEL_WALL_CLOCK: Duration = Duration::from_secs(30);

/// The largest submission this problem type will look at — **the outer wall,
/// and not a rule of anybody's activity.**
///
/// **The manager's limit is not this one and is not enforced here.** It is
/// `Activity.MaxUploadBytes`, narrowed per problem by `SeriesProblem`, and the
/// Server applies it to the bytes as they arrive. That is a decision, made
/// 2026-08-04: *a limit the Server must enforce is an explicit column, never
/// part of the opaque configuration — the Server cannot police what it cannot
/// read*, and it rejects the request before anything runs. Time and memory stay
/// in the configuration chain because they only become knowable while the
/// solution is running. So the number a manager sets deliberately never reaches
/// this crate, and nothing here should pretend otherwise.
///
/// **This was 1 MiB, and that was a defect.** The Server's own ceiling is 8 MiB
/// and an activity ships with it, so a 2 MiB submission was accepted by the
/// Server, stored, claimed, and then refused here as a `PolicyViolation` citing
/// a rule no manager had set — the Runner overriding the manager, which is the
/// opposite of what the split above is for.
///
/// Set to the Server's `UploadLimits.Submission` so it cannot contradict a
/// manager at any setting they are able to choose. What it still buys is that
/// this crate's own work — the policy scan above all — is bounded by something
/// this crate states, rather than by trusting that whatever handed us a job
/// bounded it first. A rejudge of a submission stored under an older, larger
/// wall is the case where it is not merely theoretical.
const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;

/// How many broken rules a participant is told about at once.
///
/// **Measured 2026-08-31**: one megabyte of a denied identifier is about
/// 150 000 violations and seven megabytes of text — written *twice*, into the
/// result document and into the uploaded `log`, neither of which was bounded.
/// The compiler's own log has been capped at 256 KiB since it existed; this
/// path had nothing.
///
/// A cap on the count rather than on the joined text, so a participant reads
/// whole lines, and how many were dropped is **said** rather than the text
/// being quietly cut off mid-rule.
const MAX_REPORTED_VIOLATIONS: usize = 100;

/// A directory as this process sees it, and as the container runtime does.
///
/// The two differ whenever the Runner is itself in a container, and a bind
/// mount is resolved by the **daemon** — so a path that is real here and
/// meaningless there produces an empty directory rather than an error. This
/// type exists so that difference cannot be forgotten at a call site.
#[derive(Debug, Clone)]
pub struct Places {
    pub here: PathBuf,
    pub on_host: PathBuf,
}

/// Everything a judged submission's container is given: **the program, and
/// nothing else.**
///
/// **No test file is mounted at all**, interactive or not. The input arrives as
/// a descriptor the Runner hands the measuring shim over a socket — a sealed
/// copy of `<test>.in` in memory — so the submission's container holds no path
/// into the package and no inode any other job holds. `crate::pipeline`'s
/// caller keeps the unpacked package in a cache several submissions share, and
/// that is exactly the arrangement `docs/SECURITY.md` §6 refuses to expose a
/// submission to.
///
/// **It was one mounted file until 2026-09-15**, and before 2026-08-09 it was
/// the whole `tests/` directory — which holds `<name>.out` beside `<name>.in`,
/// so `cat /in/1a.out` printed what the program had been asked to compute. The
/// only thing that had stood in front of that was the forbidden-identifier
/// dictionary, which `docs/SECURITY.md` §4 defines as a **policy** control
/// whose every rule is expected to be bypassable. An answer key must not rest
/// on a control the project itself calls bypassable, and now nothing rests on
/// one: there is no file to reach.
///
/// An interactive run is unchanged by any of this and always was given none:
/// everything it reads is what the interactor decided to send in answer to what
/// it wrote.
///
/// A function rather than a chain at the call site, because this is the one
/// place the rule is stated and it can then be asserted without a container.
fn judged_mounts(artifacts: &Path) -> Vec<Mount> {
    vec![Mount::read_only(artifacts, PROGRAM)]
}

impl Places {
    pub fn same(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            here: path.clone(),
            on_host: path,
        }
    }

    /// Both views, kept in step. Public because the Runner lays out a job's
    /// directories and has to keep the daemon's view of every one of them.
    pub fn join(&self, part: &str) -> Places {
        Places {
            here: self.here.join(part),
            on_host: self.on_host.join(part),
        }
    }
}

pub struct Job<'a> {
    pub config: &'a Config,
    pub tests: &'a TestSet,
    pub language: &'a str,
    /// **The name the participant uploaded it under**, which is the only thing
    /// that can say whether they picked the language they meant to. It reached
    /// `output-only@1` and stopped short of here, because this type selects its
    /// file by role and had no use for the name until now.
    pub file_name: &'a str,
    pub source: &'a [u8],
    /// The unpacked package, in the shared cache and read-only.
    ///
    /// **Not a copy of this job's own anymore.** It is unpacked once per
    /// archive and read by every submission to that problem; nothing here
    /// writes into it, and the judged container is given none of it at all.
    pub package: Places,
    /// The checker or interactor this package declares, already built.
    ///
    /// `None` where it declares neither. A package that declares one and
    /// arrives here without it is an infrastructure failure: the caller is the
    /// one that prepares it, under the cache's lock, and nothing is going to
    /// judge this submission correctly without it.
    pub judge: Option<&'a Judge>,
    /// Scratch for this job alone, and empty. Removed by the caller afterwards.
    pub work: Places,
    /// Where this job's per-test stdout files go, when that is not the scratch.
    ///
    /// **A tmpfs of the host's, where an operator has one.** The file is
    /// written by the submission and read once by the Runner, and nothing about
    /// it needs to survive the test — so it is the one thing in the loop that
    /// can be kept out of a disk entirely. `None` puts it in the job's own
    /// scratch, which is where it was before anybody could choose.
    ///
    /// It has to be the **host's** tmpfs and not the Runner's own: the daemon
    /// resolves the bind mount, and a path only the Runner's mount namespace
    /// knows produces an empty directory rather than an error — every test
    /// would then be compared against nothing.
    pub pipes: Option<Places>,
}

/// The package's checker or interactor, built and ready to be run over a test.
///
/// Carries its image and how it is started rather than assuming both. The
/// assumption held while `cpp` was the only compiled language there was; it
/// stopped holding the moment a package could name `cpp17-clang` — or `python3`,
/// where starting `/program/program` would try to execute a `.py` file.
///
/// **Built once per package and handed in**, rather than built here per job.
/// Compiling a checker is the same work for every submission to one problem, so
/// it belongs beside the unpacked package in the shared cache — see
/// `aj_runner::prepare`. What this type is, is the answer to "where did that
/// land, and how is it started".
#[derive(Debug, Clone)]
pub struct Judge {
    pub at: Places,
    pub image: String,
    pub start: Vec<String>,
}

/// The program beside the submission, and which of the two things it is.
///
/// **One build and two wirings.** A checker and an interactor are the same
/// artifact — a program the package author wrote, compiled in a language image,
/// run in its own container — and they differ only in what is connected to it.
/// A checker is handed the answer and asked about it; an interactor is handed
/// the submission's questions and produces the answers to them.
enum Aside<'a> {
    Checker(&'a Judge),
    Interactor(&'a Judge),
}

/// A submission that was actually judged.
pub struct Verdict {
    pub judgment: Judgment,
    pub details: Details,
    pub log: String,
}

pub enum Evaluated {
    /// The submission was judged. **Boxed**, because the other variant is a
    /// short string and every `Evaluated` in the program would otherwise carry
    /// the whole result document's worth of space.
    Judged(Box<Verdict>),
    /// The evaluation failed. **Never a verdict** — the submission was not
    /// judged, and scoring it would be a fabricated statement about somebody's
    /// work.
    Failed(String),
}

/// The lanes this Runner judges in, and which of them nobody is holding.
///
/// One lane is one place a test can be judged: a piece of the Runner's
/// processors and a measurement home of its own. There are as many as an
/// operator asked for tests at once, for the life of the Runner.
struct Lanes {
    /// The processors each lane may use, in lane order. `None` in a lane is a
    /// Runner that was given the whole machine -- see `aj_sandbox::affinity`.
    cpus: Vec<Option<String>>,
    /// One permit per lane.
    free: tokio::sync::Semaphore,
    /// Which lanes nobody holds. A `std` mutex because it is never held across
    /// an await, and because a permit has already decided there is one to take.
    idle: std::sync::Mutex<Vec<usize>>,
}

/// One lane, held for the length of one test.
///
/// **Not `Clone`, and that is the whole of the rule that a test's judge runs
/// where the test runs.** Exactly one of these is in scope inside a test, and
/// it is the only thing in this file that can confine a container to anything
/// narrower than the whole Runner -- so a checker cannot be given a different
/// one without somebody deliberately taking a second lane, which would
/// serialize the fan-out and show up the moment it was measured.
struct Lane<'a> {
    pool: &'a Lanes,
    index: usize,
    _permit: tokio::sync::SemaphorePermit<'a>,
}

impl Lanes {
    fn new(cpus: Vec<Option<String>>) -> Self {
        Self {
            free: tokio::sync::Semaphore::new(cpus.len()),
            // Reversed, so that the first lane taken is lane zero: a Runner
            // judging one test at a time then measures where it always did.
            idle: std::sync::Mutex::new((0..cpus.len()).rev().collect()),
            cpus,
        }
    }

    fn width(&self) -> usize {
        self.cpus.len()
    }

    /// Waits for a lane nobody is in, and holds it until the value is dropped.
    async fn take(&self) -> Lane<'_> {
        let permit = self
            .free
            .acquire()
            .await
            .expect("the lanes are never closed");
        let index = self
            .idle
            .lock()
            .expect("a lane is never held across a panic")
            .pop()
            .expect("a permit is a lane nobody holds");
        Lane {
            pool: self,
            index,
            _permit: permit,
        }
    }
}

impl Lane<'_> {
    fn index(&self) -> usize {
        self.index
    }

    fn cpus(&self) -> Option<&str> {
        self.pool.cpus.get(self.index).and_then(Option::as_deref)
    }
}

impl Drop for Lane<'_> {
    /// **The index goes back before the permit does.** A value's own `drop`
    /// runs before its fields are dropped, and the permit is a field -- so the
    /// waiter this releases finds a lane in `idle` rather than a `pop` on an
    /// empty list.
    fn drop(&mut self) {
        if let Ok(mut idle) = self.pool.idle.lock() {
            idle.push(self.index);
        }
    }
}

/// Where a container this pipeline starts is placed.
enum On<'a> {
    /// **The whole of what this Runner was given, and only for the builds.** A
    /// build is not the step being timed, nothing else of this job is running
    /// while it happens, and it is the one step that gains from every processor
    /// there is -- cutting a compile into one lane would make it N times slower
    /// to buy nothing.
    TheWholeRunner,
    /// A judged run, and whatever is judging beside it. Both go in the same
    /// lane, because they are one test.
    Lane(&'a Lane<'a>),
}

pub struct Pipeline<S> {
    sandbox: S,
    images: Images,
    /// The lanes this Runner judges in, as many as its sandbox has homes for.
    lanes: Lanes,
    /// The processors a timed run may use, taken once from this Runner's own
    /// affinity.
    ///
    /// `None` — the default — means it was given the whole machine, and then
    /// nothing is pinned at all. `aj_sandbox::affinity` holds that decision and
    /// what was measured to reach it.
    ///
    /// The build is not pinned either way: it is not the step being timed, and
    /// it is the one that benefits from more.
    cpus: Option<String>,
}

impl<S: Sandbox> Pipeline<S> {
    pub fn new(sandbox: S, images: Images) -> Self {
        Self {
            // One number, and it lives on the sandbox: a pipeline that fanned
            // out wider than the sandbox has measurement homes would have two
            // tests sharing one reading.
            lanes: Lanes::new(aj_sandbox::affinity::cut(sandbox.lanes())),
            sandbox,
            images,
            cpus: aj_sandbox::affinity::allowed(),
        }
    }

    /// What this pipeline runs its steps in.
    ///
    /// Public so a Runner being stopped can clear up after itself: the job
    /// containers are the daemon's children, not the Runner's, and nothing else
    /// would end them before the next start swept them up.
    pub fn sandbox(&self) -> &S {
        &self.sandbox
    }

    /// A timed run, confined to the processors this Runner was given.
    ///
    /// **Given, not chosen.** Where the Runner may use the whole machine this
    /// adds nothing and the host's scheduler places the job.
    /// **Every container this pipeline starts goes through here.**
    ///
    /// Five do: the submission's build, the judged run, a checker's or an
    /// interactor's build, and the checker or interactor itself. Only the judged
    /// run did until 2026-09-05, which left the other four ignoring an
    /// operator's division of the host — on a machine cut into twelve, every
    /// compiler and every judge floated across all sixteen processors while the
    /// program being measured sat on one. `docs/SECURITY.md` said the division
    /// was carried to the job containers, and it was carried to one of five.
    ///
    /// The two builds are the ones that mattered most: a compiler is the most
    /// processor-hungry thing here, and a checker mostly waits on a pipe.
    fn pinned(&self, on: On<'_>, profile: Profile) -> Profile {
        placed(profile, on, self.cpus.as_deref())
    }

    pub async fn evaluate(&self, job: &Job<'_>) -> Evaluated {
        self.judged(job, self.lanes.width()).await
    }

    /// One submission, judged one test at a time.
    ///
    /// **What a trial uses, and it is not a preference.** The limits a trial
    /// derives are written into the package and paid by every future submission
    /// to it -- so a limit inflated by contention this Runner inflicted on
    /// itself would be permanent, and nobody reading the number afterwards
    /// could tell. A trial is slower than judging on the same host, on purpose.
    pub async fn evaluate_one_at_a_time(&self, job: &Job<'_>) -> Evaluated {
        self.judged(job, 1).await
    }

    async fn judged(&self, job: &Job<'_>, width: usize) -> Evaluated {
        match self.attempt(job, width).await {
            Ok(evaluated) => evaluated,
            Err(reason) => Evaluated::Failed(reason),
        }
    }

    async fn attempt(&self, job: &Job<'_>, width: usize) -> Result<Evaluated, String> {
        let language = language::for_id(job.language, &self.images)
            .ok_or_else(|| format!("this Runner does not evaluate {}", job.language))?;

        // ── the languages this assignment allows ────────────────────────────
        //
        // **The Server used to refuse this and cannot anymore**: the language
        // is one member of a document it does not read. The set travels with the
        // job instead, in the assignment's `config`, and the refusal happens
        // here — where a language id means something.
        //
        // A **verdict**, and `PolicyViolation` rather than a compilation error:
        // nothing was offered to a compiler, the code may be perfect, and what
        // was broken is a rule of the activity. That is exactly what this verdict
        // is for, and it leaves the submission rejudgeable if a manager widens
        // the set afterwards.
        //
        // An empty list means the assignment said nothing, which allows anything
        // this Runner can build. It is not a way of allowing none: an assignment
        // that meant none would have nothing to submit to.
        if !job.config.languages.is_empty()
            && !job
                .config
                .languages
                .iter()
                .any(|allowed| allowed == language.id || allowed == language.family.as_str())
        {
            return Ok(policy_violation(
                job,
                &language,
                &[format!(
                    "This problem does not accept {}. It accepts: {}.",
                    language.label,
                    job.config.languages.join(", "),
                )],
            ));
        }

        // **A verdict, not an infrastructure failure.** Choosing C++ and
        // uploading `main.py` is the participant's own doing, the compiler
        // would have said so thirty seconds later, and this says it in words
        // they can act on instead of as a parse error in a language they did
        // not think they were writing.
        if !language.accepts(job.file_name) {
            return Ok(compilation_failed(
                job,
                &language,
                &format!(
                    "{} is not a file {} accepts. Expected one of: {}.",
                    job.file_name,
                    language.label,
                    language.extensions.join(", "),
                ),
            ));
        }

        // ── the size of what was uploaded ───────────────────────────────────
        //
        // Checked before the bytes are written anywhere, and before the policy
        // scan reads them — the scan's cost is what this bounds.
        //
        // **The activity's own limit is the Server's and was applied before the
        // submission was ever stored.** This is the wall behind it; see
        // [`MAX_SOURCE_BYTES`], which is also where the reason it must not be
        // set below the Server's is written down.
        //
        // A **verdict** rather than an infrastructure failure, for the reason
        // the language check above gives: nothing was offered to a compiler and
        // the file has the size the participant chose. The submission stays
        // rejudgeable.
        if job.source.len() > MAX_SOURCE_BYTES {
            return Ok(policy_violation(
                job,
                &language,
                &[format!(
                    "The submission is {} KiB, and a solution may be at most {} KiB.",
                    job.source.len() / 1024,
                    MAX_SOURCE_BYTES / 1024,
                )],
            ));
        }

        let source = job.work.join("src");
        let built_into = job.work.join("build");
        let artifacts = built_into.join("out");
        for place in [&source, &built_into] {
            std::fs::create_dir_all(&place.here).map_err(|e| e.to_string())?;
        }
        std::fs::write(source.here.join(language.source_name), job.source)
            .map_err(|e| e.to_string())?;

        // ── the activity's rules, before anything is built ──────────────────
        //
        // **Before the build, on the raw source** (D-7). A violating submission
        // is never compiled and never run, the participant is told which rule
        // matched, and the state stays rejudgeable. It is a policy control and
        // not a security boundary: a bypass is expected, and containment is the
        // sandbox's job.
        let broken = crate::policy::Dictionary::built_in()
            .check(&language, &String::from_utf8_lossy(job.source));
        if !broken.is_empty() {
            return Ok(policy_violation(
                job,
                &language,
                &listed(&broken, MAX_REPORTED_VIOLATIONS),
            ));
        }

        // ── build ───────────────────────────────────────────────────────────
        let mut log = String::new();
        if let Some(command) = language.build.clone() {
            let built = self
                .sandbox
                .run(
                    &self.pinned(
                        On::TheWholeRunner,
                        Profile::new(&language.image, command)
                            .memory_bytes(512 * 1024 * 1024)
                            .pids(128)
                            .wall_clock(Duration::from_secs(60))
                            .max_output_bytes(BUILD_LOG_BYTES)
                            .max_file_bytes(BUILD_ARTIFACT_BYTES as i64)
                            .tmpfs_bytes(BUILD_TMPFS_BYTES)
                            .writable_root()
                            .collect(BUILD_OUTPUT, BUILD_ARTIFACT_BYTES)
                            .mount(Mount::read_only(&source.on_host, SOURCE)),
                    ),
                )
                .await
                .map_err(|e| format!("the build could not be run: {e}"))?;

            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&built.stdout),
                String::from_utf8_lossy(&built.stderr),
            );

            if !built.succeeded() {
                // A build that did not produce a program is the participant's
                // answer being unbuildable, which is a verdict — not the
                // machinery failing.
                //
                // Unless the build was **stopped**, which is a different thing
                // and one a compiler never says out loud: a build killed at its
                // own limits produces no output at all, and reporting that as
                // an empty compilation error tells nobody anything.
                let said = match built.stopped {
                    Stopped::OnItsOwn => said,
                    stopped => format!(
                        "{said}\nthe build was stopped: {stopped:?} after {:?}, exit {}",
                        built.wall_time, built.exit_code,
                    ),
                };
                return Ok(compilation_failed(job, &language, &said));
            }
            unpack(&built.collected, &built_into.here)?;
            log.push_str(&said);
        }

        // ── the checker, which is also untrusted-adjacent ───────────────────
        //
        // **Built before this, once for the package, and handed in.** Refused
        // together in `Config::validated`, so at most one arm is taken; a
        // package that declares one and arrives without it is the caller having
        // skipped the preparation, which is an infrastructure failure and not
        // something to paper over by building it here per submission.
        let aside = match (&job.config.checker, &job.config.interactor) {
            (Some(_), _) => Some(Aside::Checker(job.judge.ok_or(
                "this package declares a checker and none was prepared for this job",
            )?)),
            (_, Some(_)) => Some(Aside::Interactor(job.judge.ok_or(
                "this package declares an interactor and none was prepared for this job",
            )?)),
            (None, None) => None,
        };

        // ── each test, in a lane of its own ─────────────────────────────────
        //
        // **Nothing is ever dropped, and that is what `scheduling` is for.** A
        // test whose machinery failed must not be answered by dropping the
        // tests beside it: a run future dropped inside `run_beside` leaves the
        // container alive, the measurement gate held and the relay thread
        // blocked on an open that nothing will answer -- sixteen minutes of a
        // CI suite producing no output, measured 2026-09-15. So what stops is
        // the *scheduling*. A test that has not begun returns at once, a test
        // that has begun runs to its end, and the caller is told the first
        // failure in test order.
        let scheduling = std::sync::atomic::AtomicBool::new(true);
        let done: Vec<(usize, Result<Option<TestOutcome>, String>)> =
            futures_util::stream::iter(job.tests.iter().enumerate().map(|(at, test)| {
                let scheduling = &scheduling;
                let language = &language;
                let artifacts = &artifacts;
                let aside = &aside;
                async move {
                    if !scheduling.load(std::sync::atomic::Ordering::Relaxed) {
                        return (at, Ok(None));
                    }
                    // **Taken before anything is made**, so that the channels,
                    // the sealed input and the blocking threads of a test are
                    // bounded by the lanes exactly as its containers are.
                    let lane = self.lanes.take().await;
                    let outcome = self
                        .one_test(job, language, artifacts, aside, test, &lane)
                        .await;
                    if outcome.is_err() {
                        scheduling.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    (at, outcome.map(Some))
                }
            }))
            .buffer_unordered(width.max(1))
            .collect()
            .await;

        let outcomes = in_test_order(done)?;

        let judgment = judge(job.config, job.tests, &outcomes);
        let details = Details::of(&judgment, limits_of(job, &language), compiled());

        Ok(Evaluated::Judged(Box::new(Verdict {
            judgment,
            details,
            log,
        })))
    }

    /// One test, from its channels to its outcome.
    ///
    /// **Every path out of here releases what it made.** That was already true
    /// when this was the body of a loop -- it is why `release(output.path())`
    /// comes before either error rather than after it. What is new is that
    /// several of these are in flight at once, so a path that leaked would leak
    /// one per test rather than one.
    ///
    /// A `TestOutcome` is what the program deserved, including every way it can
    /// fail; an `Err` is the machinery having failed instead, which is nobody's
    /// verdict and abandons the job.
    #[allow(clippy::too_many_arguments)]
    async fn one_test(
        &self,
        job: &Job<'_>,
        language: &crate::language::Language,
        artifacts: &Places,
        aside: &Option<Aside<'_>>,
        test: &Test,
        lane: &Lane<'_>,
    ) -> Result<TestOutcome, String> {
        let limits = job.config.effective(test.group, &language.keys());

        // **Where this test's stdout goes, instead of the daemon's log.**
        // One directory per test, made by the Runner and root's, so the
        // submission — which runs as `nobody` — cannot create, rename or
        // read anything in it. The shim opens the file inside it before it
        // drops privileges and hands over the descriptor alone.
        let per_test = job
            .pipes
            .clone()
            .unwrap_or_else(|| job.work.join("out"))
            .join(&test.name);

        // **Two directories and not one, because two containers.** The
        // submission's own channels are in `run/`; anything a checker is
        // given is in `beside/`. Both run as the same unprivileged user, so
        // a single directory would be a place each could reach the other's
        // — and the submission could then read the answer it is being
        // compared against, or write into it.
        let channels = per_test.join("run");
        let beside_them = per_test.join("beside");
        for at in [&channels.here, &beside_them.here] {
            std::fs::create_dir_all(at).map_err(|e| {
                format!(
                    "test {}: the channel directory could not be made: {e}",
                    test.name
                )
            })?;
        }
        // **A pipe, and the whole change is in that word.** The bytes go
        // from the program to this process and stop there: nothing is
        // written down, nothing is collected, and the answer is known while
        // the program is still running rather than after it has finished
        // producing an answer that was wrong at its first token.
        //
        // Made here, because the shim creates nothing — it opens what it is
        // given, so a channel that is not there is the Runner's failure to
        // prepare rather than something for the far end to invent.
        let output = Fifo::make(channels.here.join(Pipes::OUTPUT), 0o600).map_err(|e| {
            format!(
                "test {}: the output channel could not be made: {e}",
                test.name
            )
        })?;

        // **Who compares decides what the relay does with the bytes**, and
        // in both arms nothing is stored. With no checker the Runner
        // tokenizes them itself; with one it passes them straight on to a
        // second pipe the checker is reading.
        //
        // 0666, where the submission's own channels are 0600: whatever is
        // beside it runs unprivileged and has to open these, and there is
        // nothing in them to hide from the program on the other end.
        let channel = |at: &std::path::Path| {
            Fifo::make(at, 0o666)
                .map_err(|e| format!("test {}: a channel could not be made: {e}", test.name))
        };

        // What the far side is given, by role. A checker reads one channel
        // and is done; an interactor reads one, writes another, and says
        // what it decided on a third.
        let beside_channels = match &aside {
            Some(Aside::Checker(_)) => vec![channel(
                &beside_them.here.join(format!("{}.out", test.name)),
            )?],
            Some(Aside::Interactor(_)) => vec![
                channel(&beside_them.here.join(TO_THE_JUDGE))?,
                channel(&beside_them.here.join(FROM_THE_JUDGE))?,
                channel(&beside_them.here.join(VERDICT))?,
            ],
            None => Vec::new(),
        };

        let watching = match &aside {
            Some(_) => Watching::Relay(beside_channels[0].path().to_path_buf()),
            // **Unreachable without a `.out`, and the reader is what makes
            // that so.** `TestSet::read` refuses a package that has neither
            // a judge nor an expected output, so arriving here with `None`
            // means the two have disagreed — an infrastructure failure and
            // not a verdict, because nothing about it is the submission's.
            None => Watching::Against(
                String::from_utf8_lossy(&match &test.expected {
                    Some(at) => std::fs::read(at)
                        .map_err(|e| format!("test {}: the expected output: {e}", test.name))?,
                    None => {
                        return Err(format!(
                            "test {}: nothing decides this test — the package declares \
                             no checker and no interactor, and ships no expected output",
                            test.name
                        ))
                    }
                })
                .into_owned(),
            ),
        };

        // **The submission's own standard input, and it is always in this
        // directory.** What differs is what is behind it: for an
        // interactive problem a pipe with the interactor at the far end,
        // and for every other one a socket the Runner hands a descriptor
        // over — a sealed copy of `<test>.in`, in memory, which the program
        // can read, seek in and map privately. Nothing of the package is
        // mounted into a judged container either way.
        let feeding = match &aside {
            Some(Aside::Interactor(_)) => Some(channel(&channels.here.join(Pipes::INPUT))?),
            _ => None,
        };

        // **Made before the container is started**, so the shim's connect
        // never waits: it is served by a task holding the listener and the
        // memory file, and nothing else can reach either.
        let handing = match &feeding {
            Some(_) => None,
            None => {
                let at = test.input.as_ref().ok_or_else(|| {
                    format!(
                        "test {}: the package ships no {}.in, and only an interactive \
                         problem judges without one",
                        test.name, test.name,
                    )
                })?;
                let input = aj_sandbox::SealedInput::from_file(at)
                    .await
                    .map_err(|e| format!("test {}: the input could not be read: {e}", test.name))?;
                let (socket, listener) =
                    aj_sandbox::pipes::Socket::make(channels.here.join(Pipes::INPUT), 0o600)
                        .map_err(|e| {
                            format!(
                                "test {}: the input channel could not be made: {e}",
                                test.name
                            )
                        })?;
                Some((input.hand_over(listener), socket))
            }
        };
        let beside = Beside::new();
        let reading = relay(
            output.path().to_path_buf(),
            watching,
            OUTPUT_CAP,
            beside.clone(),
            CHANNEL_WALL_CLOCK,
        );
        let feeding = feeding.map(|stdin| {
            (
                feed(
                    beside_them.here.join(FROM_THE_JUDGE),
                    stdin.path().to_path_buf(),
                    beside.clone(),
                ),
                stdin,
            )
        });

        // Bound rather than passed as a temporary: the future below holds
        // a reference to it for as long as it runs.
        let judged = Profile::new(
            &language.image,
            language::with_channels(
                &language.start,
                &format!("{OUTPUT}/{}", Pipes::INPUT),
                &format!("{OUTPUT}/{}", Pipes::OUTPUT),
            ),
        )
        .memory_bytes(limits.memory_bytes)
        .pids(16)
        // The one step a participant is judged on the time of, and
        // so the one that goes through the shim.
        .measured()
        // **Nothing leaves this container on its stdio.** The
        // output travels on the pipe and the shim's report on
        // its own, so the daemon has nothing to write down and
        // no log driver to write it with.
        .silent()
        .wall_clock(reaping_deadline(limits.time_ms))
        // What the deadline above measures progress against,
        // and what "plainly past its budget" is measured from.
        .cpu_limit(Duration::from_millis(limits.time_ms))
        .pipes(&channels.here, &channels.on_host, OUTPUT);

        let judged = match handing.is_some() {
            // The sandbox has to know, because an image whose shim predates
            // the arrangement would open the socket as a file and report a
            // run that measured nothing.
            true => judged.reading_a_socket(),
            false => judged,
        };
        let judged = judged_mounts(&artifacts.on_host)
            .into_iter()
            .fold(judged, Profile::mount);
        let judged = self.pinned(On::Lane(lane), judged);

        // **The checker runs beside the submission, not after it.** It is
        // reading the answer as the program writes it, which is what makes
        // the pipe worth having: the checker's exit is the only thing a
        // separate program can say *while* it is running, and the closed
        // pipe that exit produces is what stops the submission.
        //
        // The pair is `join`ed rather than raced. Both have their own wall
        // clock, both are removed by the sandbox on every path out, and a
        // failure in either has to be reported rather than abandoned.
        let running = self.sandbox.run_beside(&judged, &beside);
        let (run, said) = match &aside {
            Some(Aside::Checker(built)) => {
                let checking = self.check(lane, job, built, &beside_them, test);
                let (run, said) = tokio::join!(running, checking);
                (run, Some(said))
            }
            Some(Aside::Interactor(built)) => {
                let interacting = self.interact(lane, job, built, &beside_them, test);
                let (run, said) = tokio::join!(running, interacting);
                (run, Some(said))
            }
            None => (running.await, None),
        };

        // **Ends the relay's wait at once where it is already waiting.** It
        // is not what keeps that thread from hanging anymore -- its opens
        // have their own deadline -- and this call is deliberately unreliable
        // in one direction: the relay may still be inside its wait for the
        // checker's answer channel, in which case this lands on nobody and the
        // deadline is what ends the thread. That ordering is the whole reason
        // the deadline exists.
        release(output.path());
        // **The hand-over is a task, never an arm of the `select!` above.**
        // It finishes the moment the shim connects, which in the ordinary
        // case is long before the program does — raced against the run it
        // would end it, leaving the container, the measurement gate and the
        // relay thread behind. Aborted here because a container that never
        // started never connected, and the task would otherwise wait for a
        // shim that is not coming; the socket goes with `per_test` below.
        if let Some((handing, _socket)) = handing {
            handing.abort();
        }
        if let Some((feeding, stdin)) = feeding {
            // Nothing is going to write the far end now, and nothing is
            // going to read this one. Both halves of the thread's wait are
            // ended here rather than waited out: `feed` opens its reading end
            // first and under a deadline, so this shortens the wait rather
            // than being the only thing that ends it.
            release(&beside_them.here.join(FROM_THE_JUDGE));
            release_writer(stdin.path());
            let _ = feeding.await;
        }
        let produced = reading
            .await
            .map_err(|e| format!("test {}: the output was not read: {e}", test.name))?;
        let run = run.map_err(|e| format!("a test could not be run: {e}"))?;
        // **The judge's own words, before the symptom below.** A checker or
        // interactor that could not be started says so with the daemon's
        // message — which names the image, the path and the reason — and an
        // empty channel is only what that failure looks like from here.
        // Hoisted out of the verdict below so the more specific account is the
        // one reported; what remains there is the judge's answer, not its
        // machinery.
        let said = said.transpose()?;
        // **A channel nobody opened, with nothing else to explain it.** The
        // shim opens this pipe before the program runs, so one still unopened
        // at the deadline means the container did not start — an
        // infrastructure failure. Reported as output it would be a wrong
        // answer pinned on a submission that never ran.
        if produced.never_opened {
            return Err(format!(
                "test {}: nothing opened the run's output channel within {CHANNEL_WALL_CLOCK:?}",
                test.name,
            ));
        }

        // The channels go with it; nothing in here outlives a test.
        let _ = std::fs::remove_dir_all(&per_test.here);
        let measured = Measured::of(&run).map_err(|e| format!("test {}: {e}", test.name))?;
        let time_ms = measured.time_ms;

        // **The wall clock survives here and nowhere else.** It is not
        // reported and it decides nothing, but the gap between the two is
        // the container's own start — the one number that explains why a
        // participant waited longer than their program ran, and the first
        // thing anybody diagnosing a slow judge wants.
        tracing::debug!(
            test = %test.name,
            cpu_ms = time_ms,
            wall_ms = run.wall_time.as_millis() as u64,
            limit_ms = limits.time_ms,
            "judged a test",
        );

        // What the machinery did to it comes first: none of these is the
        // program having answered wrongly.
        // **Not a verdict, so it never becomes one.** No processor time
        // was ever recorded against this run, which makes it a statement
        // about the host and not about the submission — the same class as a
        // test that could not be run at all.
        if run.stopped == Stopped::NeverStarted {
            return Err(format!(
                "test {}: the program never started; the sandbox recorded no \
                 processor time for it before the deadline",
                test.name
            ));
        }

        // **The judge's own answer, taken before the machinery's findings**,
        // because two of the things it can say outrank them.
        let said = match said {
            // **A judge that broke its contract is a system failure**, and
            // saying so outranks whatever stopped the run. Asked here rather
            // than below because a stopped run would otherwise report a judge
            // that exited non-zero to a participant as a time limit.
            Some(Err(broken)) => return Err(broken.to_string()),
            Some(Ok(said)) => Some(said),
            None => None,
        };

        // **A judge that refused outranks what stopped the run. One that
        // accepted does not.**
        //
        // A submission has to end by itself to be accepted, so being stopped
        // takes an `OK` away and that is deliberate. But where the judge has
        // already said *why* the answer is wrong, that sentence is worth more
        // to a participant than "Output limit exceeded" — and on an
        // interactive problem it is the only feedback there is, because there
        // is no expected output to be shown a difference against.
        //
        // **Two stops it does not outrank, because they know something the
        // judge could not.** A deadlock is the case: a submission holding its
        // question in an unflushed buffer makes the judge refuse for want of
        // anything to read, so the judge's *stopped asking* is a description of
        // the silence rather than of the answer — while "no processor time
        // for 12 s" is the one sentence that tells a participant their program
        // is stuck rather than slow. The kernel's memory kill is the other, and
        // for the same reason: it names a cause the judge only saw the shadow
        // of.
        let refused = said.as_ref().is_some_and(|said| !said.accepted)
            && !matches!(run.stopped, Stopped::WallClock | Stopped::Memory);

        let stopped = match run.stopped {
            // Stopped for being plainly past its budget rather than left to
            // run. An ordinary time limit, and it reads as one: what it
            // spent is measured and shown beside the limit like any other.
            Stopped::TimeLimit => Some(("Time limit exceeded".to_owned(), Reason::TimeLimit)),

            // **Reaped rather than over its limit, and the note says so.**
            // The deadline is four times the limit and four seconds
            // **without the processor time growing**, so a program that
            // reaches it has stopped spending any — waiting, or wedged in
            // an uninterruptible call.
            // The table would otherwise read "Time limit exceeded — 4 ms of
            // 1000 ms" and teach a participant nothing. The verdict and the
            // `reason` are deliberately the same: the vocabulary is shared
            // with the Client, the documentation and every package on disk,
            // and a program stopped here has failed a time limit whether it
            // spent the time computing or not.
            Stopped::WallClock => Some((
                format!(
                    "Time limit exceeded: no processor time for {:.1} s",
                    reaping_deadline(limits.time_ms).as_secs_f64()
                ),
                Reason::TimeLimit,
            )),
            // **The other deadline, and it says something else.** This
            // run never stopped spending processor time; it simply never
            // finished, waking for a moment in every window it was given.
            // No figure here: the cap is the sandbox's arithmetic, and
            // restating it would be a second copy to drift.
            Stopped::Overall => Some((
                "Time limit exceeded: the program kept running without finishing".to_owned(),
                Reason::TimeLimit,
            )),
            // Refused above, before this match, because it is a statement
            // about the host rather than a verdict about a submission.
            Stopped::NeverStarted => unreachable!("a run that never started is not judged"),
            // **Stopped because the answer was already known**, which is
            // not a failure of anything and so has no note of its own. What
            // it does change is the ordering below: a run stopped in the
            // middle of a `write` has an exit code that says nothing about
            // the program, so that check has to skip it.
            Stopped::Decided => None,

            Stopped::Memory => Some(("Memory limit exceeded".to_owned(), Reason::MemoryLimit)),
            Stopped::Output => Some(("Output limit exceeded".to_owned(), Reason::OutputLimit)),

            // **The limit is processor time**, decided here, on the
            // measurement (2026-09-02). It was the wall clock until then,
            // which charged the participant for the container's own start
            // and was the one arrangement no other judge in this space
            // uses; `docs/audits/TIME_LIMIT_QUANTITY_2026-09-02.md` in the
            // workspace is the whole of that history.
            //
            // The comparison is against `Measured::time_ms`, which is
            // rounded up, so the number a participant reads is exactly the
            // number this compared — with truncation the two could disagree
            // at the boundary and the table would look like a lie.
            Stopped::OnItsOwn if time_ms > limits.time_ms => {
                Some(("Time limit exceeded".to_owned(), Reason::TimeLimit))
            }
            Stopped::OnItsOwn => None,
        };
        if let Some((note, reason)) = stopped.filter(|_| !refused) {
            return Ok(failed(test, Some(measured), &note, reason));
        }

        // **After the sandbox's own findings and before the exit code.** A
        // memory limit outranks this — the program was stopped by the kernel
        // for a reason the participant can act on — but flooding then
        // exiting non-zero is flooding, and the non-zero is a consequence of
        // being cut off.
        if produced.capped && !refused {
            return Ok(failed(
                test,
                Some(measured),
                "Output limit exceeded",
                Reason::OutputLimit,
            ));
        }

        // **Only a run that ended by itself has an exit code worth reading.**
        // Anything the sandbox stopped died of a signal it was given rather
        // than one it earned, and reading that as a runtime error would turn
        // every early wrong answer into a crash. A run that finished on its own
        // and then reported a failure is a different matter, and still outranks
        // whatever the comparison found: wrong output followed by a segfault is
        // a segfault.
        //
        // **Written as `== OnItsOwn` rather than `!= Decided`**, which said the
        // same thing only while `Decided` was the sole stop that could reach
        // here. A judge's refusal now carries a stopped run this far, and its
        // `SIGKILL` is exactly the exit code this paragraph is about.
        if run.stopped == Stopped::OnItsOwn && run.exit_code != 0 {
            return Ok(failed(
                test,
                Some(measured),
                &how_it_died(run.exit_code),
                Reason::RuntimeError,
            ));
        }

        let (status, percentage, note) = match said {
            // A broken judge was already refused above, so what is left here is
            // an answer.
            Some(said) => (
                if said.accepted {
                    Status::Ok
                } else {
                    Status::Error
                },
                said.percentage,
                said.comment,
            ),
            None => {
                // Settled while the program was running, and possibly long
                // before it stopped. Nothing is compared here.
                let found = produced
                    .found
                    .expect("with no checker the relay is the one comparing");
                if found.equal() {
                    (Status::Ok, 100, String::new())
                } else {
                    (Status::Error, 0, found.note())
                }
            }
        };

        Ok(TestOutcome {
            name: test.name.clone(),
            group: test.group,
            status,
            percentage,
            time_ms,
            // From the cgroup holding the submission alone, and **absent
            // when the host gave the Runner nowhere to measure from** —
            // which is the answer `PACKAGE_FORMAT.md` asks for rather than
            // a number that is sometimes wrong. **No container floor is in
            // it since 2026-09-06**; what is left is the image's own
            // resident pages — about 1 MiB for a compiled binary, 4 MiB for
            // CPython, 24 MiB for PyPy — and that is not corrected for
            // either, because a model solution's measurement carries the
            // same floor and it cancels.
            memory_bytes: measured.memory_bytes,
            note,
            // Everything the machinery could do to it was handled above, so
            // a failure this far down is the answer itself.
            reason: (!status.passed()).then_some(Reason::WrongAnswer),
        })
    }

    /// Which image a declared judge is built and run in, and how it is
    /// started.
    ///
    /// Split out of the build so that a caller which finds one **already**
    /// built — the ordinary case, once a package has been judged once — can
    /// still say how to run it without compiling anything.
    pub fn judge_in(
        &self,
        declared: &aj_package::config::Source,
    ) -> Result<(String, Vec<String>), String> {
        let language = language::for_id(&declared.language, &self.images).ok_or_else(|| {
            format!(
                "the checker is in {}, which this Runner does not build",
                declared.language
            )
        })?;
        Ok((language.image.clone(), language.start.clone()))
    }

    /// What the runtime calls an image right now.
    ///
    /// **Half of what a built judge is filed under in the cache**, so that
    /// republishing a language image rebuilds it rather than serving a program
    /// compiled against the image that tag used to name.
    pub async fn image_id(&self, image: &str) -> Result<String, String> {
        self.sandbox
            .image_id(image)
            .await
            .map_err(|e| format!("the image {image} could not be read: {e}"))
    }

    /// Builds the package's checker or interactor into `into`. Its failure is
    /// the **package** being broken, which is an infrastructure failure and not
    /// a verdict.
    ///
    /// **Called once per package rather than once per submission**, by whoever
    /// holds the cache entry's lock — compiling a checker is the same work for
    /// every submission to one problem. `into` is where the result is
    /// assembled; what a caller hands back to [`Job::judge`] afterwards is the
    /// published location.
    ///
    /// Hands back the image it was built in as well as where it landed. Running
    /// it used to be hard-coded to the C++ image, which was true only while
    /// there was one — a checker built by Clang and run in the GCC image is a
    /// coincidence away from working, and a Python checker would have been
    /// started as though it were a binary.
    pub async fn build_judge(
        &self,
        declared: &aj_package::config::Source,
        package: &Places,
        into: &Places,
    ) -> Result<Judge, String> {
        let language = language::for_id(&declared.language, &self.images).ok_or_else(|| {
            format!(
                "the checker is in {}, which this Runner does not build",
                declared.language
            )
        })?;

        let source = into.join("src");
        let output = into.join("out");
        std::fs::create_dir_all(&source.here).map_err(|e| e.to_string())?;

        let declared_at = package.here.join(&declared.source);
        let bytes = std::fs::read(&declared_at).map_err(|e| {
            format!(
                "{} is named as the checker and could not be read: {e}",
                declared.source
            )
        })?;
        std::fs::write(source.here.join(language.source_name), bytes).map_err(|e| e.to_string())?;

        let built = self
            .sandbox
            .run(
                &self.pinned(
                    On::TheWholeRunner,
                    Profile::new(&language.image, language.build.clone().unwrap_or_default())
                        .memory_bytes(512 * 1024 * 1024)
                        .pids(128)
                        .wall_clock(Duration::from_secs(60))
                        .max_output_bytes(BUILD_LOG_BYTES)
                        .max_file_bytes(BUILD_ARTIFACT_BYTES as i64)
                        .tmpfs_bytes(BUILD_TMPFS_BYTES)
                        .writable_root()
                        .collect(BUILD_OUTPUT, BUILD_ARTIFACT_BYTES)
                        // Required: the source it compiles is in the shared
                        // cache, and an empty `/src` is a checker that does not
                        // build for a reason the message would blame on its
                        // author.
                        .mount(Mount::read_only(&source.on_host, SOURCE).required()),
                ),
            )
            .await
            .map_err(|e| format!("the checker could not be built: {e}"))?;

        if !built.succeeded() {
            return Err(format!(
                "the package's checker does not build: {}",
                String::from_utf8_lossy(&built.stderr),
            ));
        }
        unpack(&built.collected, &into.here)?;
        // **The source goes, and what was built stays.** This directory is
        // published into the cache and mounted into a container per test for as
        // long as the package lives there; a copy of the author's source in it
        // would be carried by every one of those mounts for nothing.
        let _ = std::fs::remove_dir_all(&source.here);

        Ok(Judge {
            at: output,
            image: language.image.clone(),
            start: language.start.clone(),
        })
    }

    /// Runs the checker over one test, in **its own sandbox**.
    ///
    /// It comes from a package a manager authored, not from the platform, so it
    /// is untrusted-adjacent: it gets limits and no network like anything else,
    /// and it never runs in the Runner's process.
    /// Runs an interactor beside the submission and reads what it decided.
    ///
    /// **The same contract as a checker, seen from the other side.** `argv[1]`
    /// is still the test's input and `argv[3]` still the reference answer; what
    /// changes is `argv[2]`, which a checker *reads* — the participant's output
    /// — and an interactor *writes*: the `OK`/`WRONG` document. So
    /// [`checker_said`] and [`Broken`] are reused unchanged, and the rule that a
    /// non-zero exit is a broken package rather than a wrong answer covers an
    /// interactor for free.
    ///
    /// The conversation itself is this program's own standard input and output,
    /// redirected by the shell onto two channels the Runner made. It never has
    /// the submission's own descriptors: everything is copied by [`relay`] one
    /// way and [`feed`] the other, which is what keeps the byte counter, the
    /// output cap and the early kill in trusted code.
    async fn interact(
        &self,
        lane: &Lane<'_>,
        job: &Job<'_>,
        interactor: &Judge,
        beside_them: &Places,
        test: &Test,
    ) -> Result<Result<crate::checker::Checked, Broken>, String> {
        let group = test.group;
        let test = &test.name;
        let mut command = interactor.start.clone();
        command.extend([
            format!("{INPUT}/{test}.in"),
            format!("{ANSWER}/{VERDICT}"),
            format!("{INPUT}/{test}.out"),
        ]);
        let spoken = command
            .iter()
            .map(|part| format!("'{}'", part.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");

        // Read before the run is awaited, because the interactor may write its
        // verdict and exit while the submission is still being stopped.
        let reading = read_channel(beside_them.here.join(VERDICT));

        let run = self
            .sandbox
            .run(
                &self.pinned(
                    On::Lane(lane),
                    Profile::new(
                    &interactor.image,
                    vec![
                        "/bin/sh".to_owned(),
                        "-c".to_owned(),
                        format!(
                            "exec {spoken} < '{ANSWER}/{TO_THE_JUDGE}' > '{ANSWER}/{FROM_THE_JUDGE}'"
                        ),
                    ],
                )
                .memory_bytes(256 * 1024 * 1024)
                .pids(16)
                .wall_clock(CHECKER_WALL_CLOCK)
                // The same two a checker is given, for the same reason.
                .env(format!("AJ_TEST={test}"))
                .env(format!("AJ_GROUP={group}"))
                // **Nothing here reads this container's streams**, so it keeps
                // no log: the conversation is on two FIFOs and the verdict on a
                // third, and all that is left on stderr is whatever the author
                // chose to print for themselves.
                //
                // **The output cap went with it, and that is a repair.** It was
                // 64 KiB, it bounded only that stderr — and crossing it gave
                // `Stopped::Output`, which the check below turns into "the
                // interactor was stopped", a broken-package error for an
                // interactor that was working and merely talkative. What bounds
                // this run now is `CHECKER_WALL_CLOCK` alone: an interactor that
                // never stops costs thirty seconds **per test**, because this is
                // `run` rather than `run_beside` and nothing ends it when the
                // submission does.
                .silent()
                // See `check`: this is what keeps the systemd cgroup driver's
                // gate from deadlocking two runs against each other.
                .alongside()
                // **Writable, where a checker's is read-only.** An interactor
                // opens two of these for writing, and a read-only bind refuses
                // that even for a pipe. What is in the directory is three
                // channels the Runner made and nothing else.
                .mount(Mount::writable(&beside_them.on_host, ANSWER))
                // **Out of the shared cache, and required.** Both were unpacked
                // or built once for this package; a path the daemon cannot open
                // would otherwise be an empty directory and an interactor
                // deciding against nothing.
                .mount(Mount::read_only(&interactor.at.on_host, PROGRAM).required())
                .mount(Mount::read_only(job.package.on_host.join("tests"), INPUT).required()),
                ),
            )
            .await;

        // **Released before the run's own failure is looked at.** An interactor
        // whose container never starts — a mount the daemon will not make, an
        // image that is not there — used to return through `?` with the verdict
        // channel still being read by a blocking thread nothing would ever
        // write to. That thread outlives the job, and dropping a runtime waits
        // for it: a Runner told to stop would wait out its grace and be killed,
        // and a test binary would hang instead of failing. Measured 2026-09-15,
        // on CI, as fifteen minutes of a suite producing no output at all.
        release(&beside_them.here.join(VERDICT));
        let verdict = reading
            .await
            .map_err(|e| format!("the interactor's verdict was not read: {e}"))?;
        let run = run.map_err(|e| format!("the interactor could not be run: {e}"))?;

        if run.stopped != Stopped::OnItsOwn {
            return Err(format!("the interactor was stopped: {:?}", run.stopped));
        }
        Ok(checker_said(run.exit_code, &verdict))
    }

    async fn check(
        &self,
        lane: &Lane<'_>,
        job: &Job<'_>,
        checker: &Judge,
        beside_them: &Places,
        test: &Test,
    ) -> Result<Result<crate::checker::Checked, Broken>, String> {
        let group = test.group;
        let test = &test.name;
        let mut command = checker.start.clone();
        command.extend([
            format!("{INPUT}/{test}.in"),
            format!("{ANSWER}/{test}.out"),
            format!("{INPUT}/{test}.out"),
        ]);

        let run = self
            .sandbox
            .run(
                &self.pinned(
                    On::Lane(lane),
                    Profile::new(&checker.image, command)
                        .memory_bytes(256 * 1024 * 1024)
                        .pids(16)
                        .wall_clock(CHECKER_WALL_CLOCK)
                        .max_output_bytes(64 * 1024)
                        // **Which test this is, said rather than parsed.** It is in
                        // `argv[1]` already — `/in/2a.in` — so this adds nothing a
                        // checker could not work out. What it removes is the working
                        // out: the split of `2a` into a group and a letter is a rule of
                        // the package format, and a checker deriving it again is that
                        // rule copied into code we do not control and cannot correct.
                        //
                        // **Variables rather than a fourth argument**, because
                        // `argv[1..3]` is SIO2's contract taken verbatim and a checker
                        // moved from there must keep working untouched.
                        .env(format!("AJ_TEST={test}"))
                        .env(format!("AJ_GROUP={group}"))
                        // **Out of the shared cache, and required**: see the
                        // interactor's, which says why an empty directory here
                        // would be worse than a container that does not start.
                        .mount(Mount::read_only(&checker.at.on_host, PROGRAM).required())
                        .mount(
                            Mount::read_only(job.package.on_host.join("tests"), INPUT).required(),
                        )
                        // **Alongside, and this is the flag that stops a hard
                        // deadlock.** The judged run holds the measurement gate for
                        // its whole length, and on the systemd cgroup driver that
                        // gate is an owned mutex — a checker asking for one of its
                        // own would wait for a run that is waiting for it.
                        .alongside()
                        // Read-only, and a pipe opened for reading is a read: what
                        // the mount refuses is creating or replacing the name.
                        .mount(Mount::read_only(&beside_them.on_host, ANSWER)),
                ),
            )
            .await
            .map_err(|e| format!("the checker could not be run: {e}"))?;

        // A checker killed by a limit is a broken checker, not a wrong answer.
        if run.stopped != Stopped::OnItsOwn {
            return Err(format!("the checker was stopped: {:?}", run.stopped));
        }
        Ok(checker_said(run.exit_code, &run.stdout))
    }
}

/// What the Runner does with a submission's output while it is being produced.
///
/// **The Runner is the only reader of it, always.** Where there is no checker it
/// tokenizes the bytes itself; where there is one it keeps them for it. The
/// program is never wired to anything the package brought with it.
enum Watching {
    /// Compare against the reference answer, token by token, as it arrives.
    Against(String),
    /// Pass it straight to a checker, which is reading the far end as it goes.
    ///
    /// **The checker never touches the program's own pipe.** It is handed a
    /// second one the Runner writes into, which is what keeps the byte counter,
    /// the cap and the early kill in trusted code, and what lets the two
    /// containers run as the same unprivileged user without sharing a
    /// directory.
    Relay(PathBuf),
}

/// What came out of one test, and what was made of it on the way.
struct Produced {
    /// The comparison, where the Runner was the one comparing.
    ///
    /// **Absent means a checker had it**, and a checker's answer is its exit
    /// code rather than anything this could carry. There is no third case:
    /// somebody is always reading, which is why nothing here holds the bytes.
    found: Option<Comparison>,
    /// The wait for somebody to open the run's own output channel ran out.
    ///
    /// **Not "nobody opened it", which is a wider claim than this can make.**
    /// The Runner's own `release` is a writer that opens and goes, so a thread
    /// woken by it comes back with this `false` and an empty stream — reported,
    /// correctly, as a run that printed nothing, because by then the Runner
    /// already knows the run is over.
    ///
    /// What it does record is the deadline passing with no writer at all.
    /// **Not the same as printing nothing, and reporting it as that would judge
    /// a participant on a container that never ran:** the shim opens this pipe
    /// before the program does anything, so a channel still unopened at the
    /// deadline means the container did not start. A test that could not be
    /// read is not a verdict.
    never_opened: bool,
    /// It printed more than it was allowed to.
    ///
    /// **Kept here rather than read off `Stopped::Output`**, because the two
    /// disagree in one direction: a program that floods and exits in the same
    /// breath can cross the cap after the sandbox has already stopped watching,
    /// and it still printed more than it was allowed to.
    capped: bool,
}

/// Reads one run's output as it is written, and decides what can be decided.
///
/// **On a thread of its own, and it waits for a writer rather than assuming
/// one.** A reader must not mistake *nothing has been written yet* for *nothing
/// will be* — a bare non-blocking open reports the second as an immediate end of
/// file, and it would arrive here as a program that printed nothing. This used
/// to be a blocking open, which cannot make that mistake and cannot end either;
/// `open_for_reading` keeps the distinction and adds a deadline, and what
/// happens when that deadline passes is `Produced::never_opened`.
///
/// **It goes on draining after it has decided.** The verdict is settled and the
/// bytes are thrown away, but a reader that stops reading is a full pipe, and a
/// full pipe is a program blocked in `write` rather than a program being
/// stopped — the participant would be charged for the judge's own tidiness.
fn relay(
    at: PathBuf,
    watching: Watching,
    cap: u64,
    beside: Beside,
    waiting: Duration,
) -> tokio::task::JoinHandle<Produced> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read as _;

        let mut comparing = match &watching {
            Watching::Against(expected) => Some(Comparing::against(expected)),
            _ => None,
        };
        // **Opened before a byte is read, and that ordering is the point.** The
        // checker is already running and blocked on its own open; leaving this
        // until the program has produced something would hold it there for as
        // long as the program thinks.
        let mut far = match &watching {
            // **A checker that never opens its answer is not an error, and
            // giving up on the run because of it was one.** Reading argv[2] is
            // the ordinary thing to do and not a requirement: a checker may
            // decide on the input alone, or refuse on its arguments and exit.
            //
            // This used to return here, which left nothing reading the
            // submission's pipe — so the shim blocked opening it, the run was
            // reaped, and a correct submission came back **Time limit
            // exceeded**. The verdict belonged to a checker that had already
            // answered. Now the bytes are read and dropped, exactly as they are
            // after a checker exits early.
            //
            // The wait stays the checker's whole wall clock, and it has to: a
            // shorter one could give up on a checker that was going to read,
            // and *that* would be a wrong verdict rather than a slow one.
            Watching::Relay(to) => match open_for_writing(to, waiting) {
                Ok(open) => Some(open),
                Err(e) => {
                    tracing::warn!(
                        path = %to.display(), %e,
                        "nothing opened the answer channel; the run is drained and discarded",
                    );
                    None
                }
            },
            _ => None,
        };
        let mut capped = false;
        let mut never_opened = false;
        let mut total: u64 = 0;

        // **Bounded, and that is the whole of the fix.** This was a blocking
        // open, which waits for a writer forever — and the one thing that
        // could end that wait, `release`, had already been called by the time
        // this thread reached here whenever the judged run finished inside the
        // wait above. A container that never started then held the job for
        // ever.
        match open_for_reading(&at, waiting) {
            Err(e) => {
                never_opened = true;
                tracing::warn!(
                    path = %at.display(), %e,
                    "nothing opened the run's output within the deadline",
                );
            }
            Ok((mut channel, first)) => {
                let mut buffer = vec![0u8; 64 * 1024];
                // What the open had to take from the stream to learn that a writer
                // had arrived. Normally nothing; never dropped.
                if buffer.len() < first.len() {
                    buffer.resize(first.len(), 0);
                }
                buffer[..first.len()].copy_from_slice(&first);
                let mut carried = first.len();
                loop {
                    let read = if carried > 0 {
                        std::mem::take(&mut carried)
                    } else {
                        match channel.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => read,
                        }
                    };
                    let chunk = &buffer[..read];
                    // What the reaper watches: a run talking to its checker is
                    // working, however little processor time it is spending.
                    beside.moved(read);
                    total += read as u64;

                    // **The cap covers the comparing side too**, and not only the
                    // checker's. A program printing one enormous token with no
                    // whitespace in it is holding that token in *this* process
                    // until it ends, because a token is only whole once something
                    // follows it.
                    if !capped && total > cap {
                        capped = true;
                        // The checker is reading a stream that is not going to end
                        // on its own. Closing this is what lets it finish rather
                        // than wait out its own wall clock.
                        far = None;
                        beside.enough(Enough::Output);
                    }
                    if capped {
                        continue;
                    }

                    match (&mut comparing, &mut far) {
                        (Some(comparing), _) => {
                            if comparing.feed(chunk).is_some() {
                                beside.enough(Enough::Decided);
                            }
                        }
                        // **A judge that has stopped reading is done with this
                        // run, and that is not a reason to stop the run.**
                        //
                        // It used to be, until 2026-09-16, and the reason it
                        // changed is that it made the verdict depend on
                        // scheduling. Both endings live in this one loop: the
                        // chunk that crosses the cap and the chunk whose write
                        // fails. Measured on that day, a submission that prints
                        // its answer and then floods was stopped by the judge
                        // after a median 59 ms idle and by the cap after 142 ms
                        // under load — so 24% of loaded runs said *output limit*
                        // and every idle run said *accepted*, for the same
                        // program. The same submission earned two verdicts.
                        //
                        // **A submission has to end by itself to be accepted.**
                        // Nothing here stops it anymore: the far end is dropped,
                        // the bytes go on being drained and counted, and whatever
                        // limit the program reaches is what ends it. One that
                        // keeps writing reaches the cap; one that waits for input
                        // that will never come reaches the reaper. Both are the
                        // same answer every time, which is the whole point.
                        (None, Some(open)) => {
                            use std::io::Write as _;
                            if open.write_all(chunk).is_err() {
                                far = None;
                            }
                        }
                        // Nothing is listening anymore — the judge exited, or
                        // the cap closed its end. Draining is still what keeps the
                        // program out of a blocking `write` until it is stopped,
                        // and it is now also what lets a submission that will not
                        // end run into a limit of its own.
                        (None, None) => {}
                    }
                }
            }
        }

        Produced {
            found: comparing.map(|comparing| comparing.finish()),
            capped,
            never_opened,
        }
    })
}

/// Reads one channel to its end, on a thread of its own.
///
/// **Bounded like every other reader here.** A blocking open of a pipe waits
/// for a writer to *open* it, and an interactor that could not start never
/// will. `release` is called on every path out of `interact`, which is what made
/// this survivable; the deadline is what makes it safe without that.
fn read_channel(at: PathBuf) -> tokio::task::JoinHandle<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read as _;
        let Ok((mut open, mut said)) = open_for_reading(&at, CHANNEL_WALL_CLOCK) else {
            return Vec::new();
        };
        let _ = open.read_to_end(&mut said);
        said
    })
}

/// Carries what the interactor says back to the submission.
///
/// **The mirror of [`relay`], and it exists for the same reason.** The
/// submission is never wired to package-authored code in either direction: what
/// reaches its standard input is copied here, by the Runner, out of a second
/// pipe. That is what makes both halves of a conversation countable, and
/// countable is what the reaper needs — an interactive run spends most of its
/// time with neither side on a processor, and only a byte moving tells that
/// apart from a run that has wedged.
fn feed(from: PathBuf, to: PathBuf, beside: Beside) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        use std::io::{Read as _, Write as _};

        // The far end first, exactly as the forward relay does: the submission
        // is blocked opening its input, and it is the interactor's arrival that
        // has to be waited for, not the submission's.
        let Ok((mut said, first)) = open_for_reading(&from, CHANNEL_WALL_CLOCK) else {
            return;
        };
        let Ok(mut onward) = open_for_writing(&to, CHECKER_WALL_CLOCK) else {
            return;
        };

        // Whatever the open had to take to learn the far end had arrived goes
        // onward first: this is one half of a conversation, and a lost first
        // line is a different conversation.
        if !first.is_empty() {
            beside.moved(first.len());
            if onward.write_all(&first).is_err() {
                return;
            }
        }

        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = match said.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            beside.moved(read);
            // The submission is gone, or has closed its input. Neither is this
            // side's business to report.
            if onward.write_all(&buffer[..read]).is_err() {
                break;
            }
        }
    })
}

/// Writes out what a build container produced.
///
/// The archive comes from the runtime API, so its paths are the container's and
/// its entries are whatever the build wrote. Unpacked by the **Runner**, which
/// owns the directory — which is the whole point of collecting rather than
/// bind-mounting.
fn unpack(collected: &Option<Vec<u8>>, into: &Path) -> Result<(), String> {
    let bytes = collected
        .as_ref()
        .ok_or("the build reported success and produced nothing")?;

    tar::Archive::new(&bytes[..])
        .unpack(into)
        .map_err(|e| format!("what the build produced could not be read: {e}"))
}

/// What to tell a participant whose program did not exit cleanly.
///
/// A shell reports a fatal signal as `128 + n`, and `kod 139` is not something
/// anybody can act on. Naming the signal is the difference between "look at
/// your memory access" and "look at everything".
///
/// **A time limit never reaches here.** That is decided by the Runner killing
/// the container itself and reported as `Stopped::WallClock`, before the exit
/// code is consulted at all — so a crash and a timeout come from two different
/// places and cannot be mistaken for one another.
///
/// **One caveat, measured 2026-08-09.** The program runs as **PID 1**, and the
/// kernel does not deliver a signal with a default disposition to PID 1 unless
/// it was generated by a hardware fault. `SIGSEGV` and `SIGFPE` are faults and
/// arrive correctly; `SIGABRT` is raised in software, so glibc's `abort()` —
/// which is also a failed `assert`, an uncaught C++ exception and a detected
/// double free — is refused and then dies as `SIGSEGV` instead. Under
/// `docker --init` the same programs report `134` as they should. So `139` is
/// honest about "it crashed" and can be wrong about *how*, and until the init
/// question is decided this wording must not promise more than it knows.
fn how_it_died(exit_code: i64) -> String {
    let signal = match exit_code - 128 {
        4 => "illegal instruction",
        6 => "aborted",
        7 => "bus error",
        8 => "arithmetic error, such as division by zero",
        9 => "killed",
        11 => "segmentation fault",
        13 => "wrote to a closed stream",
        15 => "terminated",
        _ => {
            return format!("Runtime error, exit code {exit_code}");
        }
    };
    format!("Runtime error: {signal} (exit code {exit_code})")
}

/// The deadline a test container is killed at — **not the limit**.
///
/// **Four times the limit and four seconds.** A limit is processor time, and
/// the only thing this deadline exists for is reaping something that is not
/// spending any: a program wedged in an uninterruptible syscall, or one that
/// waits rather than computes. A program that *is* computing may legitimately
/// spend rather more wall clock than processor time before it has used its
/// limit — the container's own start alone is a few hundred milliseconds — so a
/// deadline near the limit would reap correct solutions.
///
/// **Four times, because a starved program is indistinguishable from an idle
/// one.** This measures *consecutive* time without progress, so four times the
/// limit is roughly a host loaded four times past what it can carry. Measured
/// 2026-09-04: an unpinned twelve-Runner fleet on sixteen processors put 80% of
/// 8409 tests past the old three-times-and-a-second and produced five wrong
/// verdicts in 150. The four seconds are what makes this also the guard against
/// a program hung on input that never comes, at a limit small enough that four
/// times it would not be.
///
/// `saturating_mul` because nothing bounds `timeMs` above: `Config::validated`
/// refuses zero and nothing else, so four times a large one wraps.
fn reaping_deadline(time_ms: u64) -> Duration {
    Duration::from_millis(time_ms.saturating_mul(4)) + Duration::from_secs(4)
}

/// A timed run confined to the processors the Runner was given, and to nothing
/// where it was given the whole machine.
///
/// A function of its argument rather than of the process, so the decision this
/// makes is testable without a machine that has been divided up.
/// `aj_sandbox::affinity` decides which of the two a Runner is in.
/// Where a container goes, as a function of its arguments, so that every case
/// of it is testable without a daemon.
fn placed(profile: Profile, on: On<'_>, whole: Option<&str>) -> Profile {
    match on {
        On::TheWholeRunner => pin(profile, whole),
        // The processors and the measurement home together: a run placed on one
        // lane's processors and measured in another's would be a reading taken
        // where a second run was also making one.
        On::Lane(lane) => pin(profile, lane.cpus()).lane(lane.index()),
    }
}

/// A job's outcomes in the order its tests are declared, or the first failure
/// in that same order.
///
/// **Not the order they finished in.** `judge` keeps the order it is given
/// within a group and `verdict` names the first thing that went wrong in it, so
/// a table assembled as tests completed would give one submission different
/// verdicts on different days -- and the table is what a participant reads. A
/// test that never began carries no outcome and is not one.
fn in_test_order(
    mut done: Vec<(usize, Result<Option<TestOutcome>, String>)>,
) -> Result<Vec<TestOutcome>, String> {
    done.sort_by_key(|(at, _)| *at);
    let mut outcomes = Vec::with_capacity(done.len());
    for (_, outcome) in done {
        if let Some(outcome) = outcome? {
            outcomes.push(outcome);
        }
    }
    Ok(outcomes)
}

fn pin(profile: Profile, cpus: Option<&str>) -> Profile {
    match cpus {
        Some(cpus) => profile.cpuset(cpus),
        None => profile,
    }
}

/// What one test cost, where anything was run at all.
///
/// **One value for both numbers, because they are one reading.** They come out
/// of one cgroup, after one run, or they come out of nothing — a signature that
/// took them apart would let a caller pass a time from a run beside a memory
/// from nowhere, which is how `failed` came to discard a measurement it had.
#[derive(Debug, Clone, Copy)]
struct Measured {
    /// Processor time, user plus system, **rounded up to the millisecond**.
    time_ms: u64,
    memory_bytes: Option<u64>,
}

impl Measured {
    /// **The one place a run becomes a number.**
    ///
    /// A run with no processor time is a machine fault rather than a
    /// measurement: `Sandbox::preflight` refuses a host that cannot read
    /// `cpu.stat` at all, so an absence here means the cgroup went away under
    /// us. There is nothing to compare against a limit, and inventing one would
    /// be a verdict about a participant made out of a broken judge.
    fn of(run: &aj_sandbox::Outcome) -> Result<Self, String> {
        let cpu = run.cpu_time.ok_or(
            "produced no processor time. The cgroup this Runner started it under could \
             not be read, and a time limit is decided on processor time",
        )?;
        Ok(Self {
            // **Up, not down.** A run that did any work must not report none:
            // zero is what a calibration then multiplies by three, and a
            // `limits.timeMs` of zero is a limit the format refuses — so
            // truncation would make calibration die on a fast model solution.
            // It also keeps the number a participant reads identical to the one
            // the verdict was made on.
            time_ms: cpu.as_micros().div_ceil(1000) as u64,
            memory_bytes: run.peak_memory_bytes,
        })
    }
}

/// **`None` only where nothing ran.** A test that was reaped, or that crashed,
/// still has a reading — the cgroup is read after the container is gone and
/// whatever stopped it — and this used to throw it away on the belief that
/// nobody had measured it. A compilation error and a policy violation are the
/// two callers for which that belief is true.
fn failed(
    test: &aj_package::Test,
    measured: Option<Measured>,
    note: &str,
    reason: Reason,
) -> TestOutcome {
    TestOutcome {
        name: test.name.clone(),
        group: test.group,
        status: Status::Error,
        percentage: 0,
        time_ms: measured.map_or(0, |m| m.time_ms),
        memory_bytes: measured.and_then(|m| m.memory_bytes),
        note: note.to_owned(),
        reason: Some(reason),
    }
}

/// The submission broke the activity's rules, so nothing was built or run.
///
/// Distinct from a compilation error and from an internal error, because those
/// are three different things to tell a participant: their code is wrong, their
/// code does not build, or the system failed. This one is none of the three —
/// their code may be perfect and still not allowed.
/// The broken rules a participant is shown, and how many were left out.
///
/// A named function with a test rather than a `map` at the call site, because
/// what it bounds is not visible from there: every line here is written twice
/// into documents somebody stores, and the number of lines is chosen by whoever
/// wrote the submission.
fn listed(broken: &[crate::policy::Violation], cap: usize) -> Vec<String> {
    let mut listed: Vec<String> = broken.iter().take(cap).map(|v| v.note()).collect();
    if broken.len() > cap {
        // Said rather than left to be inferred from a list that stops. A
        // participant who fixes a hundred rules and finds a hundred more was
        // not told the first time.
        listed.push(format!(
            "and {} more, not listed. Fix these first.",
            broken.len() - cap,
        ));
    }
    listed
}

fn policy_violation(job: &Job<'_>, language: &language::Language, listed: &[String]) -> Evaluated {
    let outcomes: Vec<TestOutcome> = job
        .tests
        .iter()
        .map(|test| failed(test, None, "Policy violation", Reason::PolicyViolation))
        .collect();

    let judgment = judge(job.config, job.tests, &outcomes);
    let details = Details::of(
        &judgment,
        limits_of(job, language),
        Compilation {
            // Not an error: nothing failed to compile, because nothing was
            // offered to a compiler.
            status: Status::Warning,
            log: listed.join("\n"),
        },
    );

    Evaluated::Judged(Box::new(Verdict {
        judgment: Judgment {
            verdict: "PolicyViolation".into(),
            ..judgment.clone()
        },
        details,
        log: listed.join("\n"),
    }))
}

/// Every test failed for the same reason, and the reason is worth stating once.
fn compilation_failed(job: &Job<'_>, language: &language::Language, log: &str) -> Evaluated {
    let outcomes: Vec<TestOutcome> = job
        .tests
        .iter()
        .map(|test| failed(test, None, "Compilation error", Reason::CompilationError))
        .collect();

    let judgment = judge(job.config, job.tests, &outcomes);
    let details = Details::of(&judgment, limits_of(job, language), failed_to_compile(log));

    Evaluated::Judged(Box::new(Verdict {
        judgment: Judgment {
            verdict: "Compilation error".into(),
            ..judgment.clone()
        },
        details,
        log: log.to_owned(),
    }))
}

/// The limits this submission was actually held to, for the document a
/// participant reads.
///
/// **This used to report the package's global pair**, so a Python submission
/// judged under `overrideLimits.python` was shown a `timeMs` it was never held
/// to — the document contradicting the run it describes. The language override
/// holds for the whole submission, because a submission has one language, so it
/// belongs in the one pair of numbers this document carries.
///
/// A group's own limits still are not in here, and cannot be: they vary across
/// the tests of one submission and there is one slot. The per-group table
/// belongs on the problem's own page, from the configuration, rather than in a
/// result document that has no shape for it.
/// The conversion is between two types of the same name — the package's and the
/// document's — which is why this cannot simply hand the one back.
fn limits_of(job: &Job<'_>, language: &language::Language) -> Limits {
    let held_to = job.config.for_language(&language.keys());
    Limits {
        time_ms: held_to.time_ms,
        memory_bytes: held_to.memory_bytes,
    }
}

/// Where a package's `tests/` are, for a caller assembling mounts.
pub fn tests_of(package: &Places) -> PathBuf {
    package.on_host.join("tests")
}

/// A scratch directory for one job, named after it.
pub fn scratch(root: &Path, job_id: &str) -> PathBuf {
    root.join(format!("job-{job_id}"))
}

#[cfg(test)]
mod tests {
    /// A checker that never started, and the thread that used to wait for it.
    ///
    /// **This is the shape that held a Runner forever.** The judged run ends,
    /// `one_test` calls `release` on its way out — and this thread is still
    /// inside the wait for the checker's answer channel, so that release lands
    /// on nobody. It then opened the run's own output, for which no writer would
    /// ever come, and blocked with no deadline: the job was never reported, the
    /// lease was renewed, and the same job was claimed again until a restart.
    #[tokio::test]
    async fn a_relay_whose_channels_nobody_opens_ends_rather_than_waits() {
        let here = std::env::temp_dir().join(format!("aj-relay-{}", std::process::id()));
        std::fs::create_dir_all(&here).expect("somewhere to put pipes");
        let out = aj_sandbox::pipes::Fifo::make(here.join("stdout"), 0o600).expect("a pipe");
        let answer = aj_sandbox::pipes::Fifo::make(here.join("answer"), 0o600).expect("a pipe");

        let began = std::time::Instant::now();
        let reading = relay(
            out.path().to_path_buf(),
            Watching::Relay(answer.path().to_path_buf()),
            OUTPUT_CAP,
            Beside::new(),
            Duration::from_millis(200),
        );

        // No `release` anywhere: the point is that this ends without one.
        //
        // **The rescue is on the failing arm, and nowhere else.** `reading` is a
        // `spawn_blocking` task that nothing can cancel, so a timeout alone
        // would not make a regression fail: the panic drops the runtime,
        // `Runtime::drop` waits for the blocking pool, and the blocked thread
        // never finishes -- the binary wedges before libtest is told anything.
        // Measured by putting the blocking open back: "has been running for
        // over 60 seconds", and the run had to be killed. CI declares no
        // `timeout-minutes`, so that is six hours naming nothing.
        //
        // Releasing here and not before is what keeps the test honest: the wait
        // has already failed by the time this runs, so it cannot mask the thing
        // being tested -- it only lets the failure be reported.
        let produced = match tokio::time::timeout(Duration::from_secs(20), reading).await {
            Ok(joined) => joined.expect("the thread did not panic"),
            Err(_) => {
                aj_sandbox::pipes::release(out.path());
                panic!("the relay did not end on its own");
            }
        };

        // **Said, and not merely survived.** Reported as "it printed nothing"
        // this would be a wrong answer pinned on a participant whose container
        // never ran; the caller turns this flag into a failure with a reason.
        assert!(
            produced.never_opened,
            "a channel nobody opened has to say so"
        );
        assert!(
            produced.found.is_none(),
            "nothing was compared, because nothing was written"
        );
        assert!(!produced.capped);
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "it waited {:?}, which is a Runner holding a job",
            began.elapsed(),
        );
    }

    use super::*;

    use crate::policy::Violation;

    fn a_run() -> Profile {
        Profile::new("image", vec!["true".to_owned()])
    }

    /// **A Runner given the whole machine pins nothing**, and this is the case
    /// that has to keep working without anybody configuring it: several Runners
    /// choosing processors with nothing coordinating them is worse than letting
    /// the host place the work, and a pin also forbids the kernel from moving a
    /// job off a processor somebody else is using.
    #[test]
    fn a_runner_that_was_given_no_processors_in_particular_pins_none() {
        assert_eq!(pin(a_run(), None).cpuset, None);
    }

    /// **Every profile this file builds is pinned, and this is what says so.**
    ///
    /// A source check rather than a behavioral one, because the behavior is
    /// only observable on a host that has been divided up — which neither a
    /// developer's machine nor CI is, so a container test would pass on both
    /// while the property was false. Reading the source is what is left.
    ///
    /// It caught four of five call sites on 2026-09-05: only the judged run was
    /// pinned, so every compiler and every judge escaped the operator's cpuset.
    #[test]
    fn every_container_this_pipeline_starts_is_confined_to_the_runners_processors() {
        let source = include_str!("pipeline.rs");
        let production = &source[..source.find("#[cfg(test)]").expect("a test module")];

        let built = production.matches("Profile::new(").count();
        let pinned = production.matches("self.pinned(").count();
        assert_eq!(
            built, pinned,
            "{built} profiles are built and {pinned} are pinned; a container that \
             skips `pinned` ignores an operator's division of the host",
        );
    }

    /// And a Runner that *was* given a set hands that set on, because a job
    /// container is the daemon's child and inherits no affinity from the Runner
    /// that asked for it.
    #[test]
    fn a_runner_given_processors_confines_its_jobs_to_them() {
        assert_eq!(pin(a_run(), Some("0,1")).cpuset, Some("0,1".to_owned()));
        assert_eq!(pin(a_run(), Some("4-7")).cpuset, Some("4-7".to_owned()));
    }

    fn violations(how_many: usize) -> Vec<Violation> {
        (1..=how_many)
            .map(|line| Violation {
                rule: "forbidden call".into(),
                matched: "getenv".into(),
                line,
            })
            .collect()
    }

    /// **What a participant is shown is bounded, and the bound is stated.**
    ///
    /// Measured 2026-08-31: a megabyte of one denied identifier is about
    /// 150 000 violations and seven megabytes of text, written into the result
    /// document *and* into the uploaded log. Both are documents somebody
    /// stores, and how long they are was chosen by whoever wrote the
    /// submission.
    /// **One step is measured, and a test says which.** `Profile::measured` is
    /// what makes a container start as root so the shim can drop out of it, so a
    /// second one would hand privilege to a step nobody meant to -- a build,
    /// which runs a compiler over a participant's source, or a checker. Neither
    /// is judged on its processor time and neither has any business starting as
    /// root.
    ///
    /// Read out of this file rather than asserted against a built profile,
    /// because the profiles are assembled inline where they are used. The
    /// `.env.example` guard in `aj-runner` reads its sources the same way.
    #[test]
    fn exactly_one_step_is_measured() {
        let source = include_str!("pipeline.rs");
        // Split so that this line is not itself one of the occurrences.
        let marked = source.matches(concat!(".measu", "red()")).count();

        assert_eq!(
            marked, 1,
            "{marked} steps are marked measured; each one starts as root where              the image has a shim, so a second needs a reason"
        );
    }

    #[test]
    fn a_participant_is_told_the_first_rules_and_how_many_were_left() {
        let shown = listed(&violations(250), 100);

        assert_eq!(shown.len(), 101, "a hundred rules, and one line saying so");
        assert!(shown[99].contains("line 100"), "{}", shown[99]);
        assert_eq!(shown[100], "and 150 more, not listed. Fix these first.");
    }

    /// Nothing is appended when nothing was dropped: a list that ends because
    /// it ended must not read like a list that was cut.
    #[test]
    fn a_list_short_enough_to_show_whole_says_nothing_about_more() {
        let shown = listed(&violations(3), 100);

        assert_eq!(shown.len(), 3);
        assert!(shown.iter().all(|line| !line.contains("not listed")));
    }

    /// **No judged submission is given a file of the package at all.**
    ///
    /// Not the answer key, not its own input, not an empty directory where one
    /// would be: the program it was compiled into, and nothing else. What it
    /// reads arrives as a descriptor on a socket — see [`crate::pipeline`]'s
    /// per-test setup and `aj_sandbox::memfd`.
    ///
    /// **Asserted here rather than in a container**, because from inside there
    /// is no way to look: reading a file needs `fopen` or `ifstream`, and the
    /// forbidden-identifier dictionary refuses a submission both. That
    /// dictionary is why the answer key went unnoticed in the mount for so
    /// long, and it is also why it is not enough — it is a policy control by
    /// decision, and a package may turn it off. The containerized half is
    /// `judging.rs::a_judged_submission_cannot_read_the_answer_key`.
    #[test]
    fn a_judged_submission_is_given_its_program_and_nothing_else() {
        let artifacts = std::path::Path::new("/work/build/out");
        let mounts = judged_mounts(artifacts);

        assert_eq!(mounts.len(), 1, "the program, and nothing else: {mounts:?}");
        assert_eq!(mounts[0].to, PROGRAM);
        assert!(!mounts[0].writable);

        // The two shapes that leaked, in order: the whole `tests/` directory,
        // which carries `<test>.out` beside `<test>.in`, and then one input
        // file — which was correct and is still an inode shared with every
        // other submission to that problem now that the package is unpacked in
        // a cache they all read.
        assert!(
            !mounts.iter().any(|m| m.to.starts_with(INPUT)),
            "a judged submission was handed part of the package: {mounts:?}",
        );
    }

    /// A crash says what kind of crash, and an ordinary non-zero exit is left
    /// as the number it is rather than dressed up as a signal.
    #[test]
    fn a_fatal_signal_is_named_and_a_plain_exit_code_is_not() {
        assert!(how_it_died(139).contains("segmentation fault"));
        assert!(how_it_died(136).contains("division by zero"));
        assert!(how_it_died(134).contains("aborted"));

        // Not a signal: 3 is just what the program returned.
        assert_eq!(how_it_died(3), "Runtime error, exit code 3");
        // 128 itself is an exit code, not signal zero.
        assert_eq!(how_it_died(128), "Runtime error, exit code 128");
    }

    #[test]
    fn a_place_carries_both_views_through_a_join() {
        let places = Places {
            here: PathBuf::from("/work/cache"),
            on_host: PathBuf::from("C:\\repo\\cache"),
        };
        let inner = places.join("tests");

        assert_eq!(inner.here, PathBuf::from("/work/cache/tests"));
        assert!(
            inner.on_host.to_string_lossy().contains("repo"),
            "the daemon's view must not be replaced by this process's",
        );
    }

    /// **The deadline is not the limit, and nothing asserted that before.** It
    /// was an inline expression at one call site, so a change to it would have
    /// been caught by no test at all.
    #[test]
    fn the_reaping_deadline_is_four_times_the_limit_and_four_seconds() {
        assert_eq!(reaping_deadline(1000), Duration::from_millis(8000));
        assert_eq!(reaping_deadline(1), Duration::from_millis(4004));

        // Nothing bounds `timeMs` above: `Config::validated` refuses zero and
        // nothing else. Three times a large one has to saturate rather than
        // wrap, because wrapping would produce a deadline of a few milliseconds
        // and reap every correct solution to that problem.
        assert!(reaping_deadline(u64::MAX) > Duration::from_secs(86_400));
    }

    /// **A run that did work must not report none.** Truncating would: a
    /// program under a millisecond of processor time is ordinary for a test
    /// that reads two integers, and zero is what a calibration then multiplies
    /// by three to produce `limits.timeMs: 0` — which the format refuses. So
    /// calibration would die on a fast model solution, which is exactly the
    /// solution a package is calibrated from.
    #[test]
    fn a_run_that_did_work_never_reports_no_time() {
        let measured = |micros: u64| {
            Measured::of(&outcome_with(Some(Duration::from_micros(micros))))
                .unwrap()
                .time_ms
        };

        assert_eq!(measured(0), 0, "nothing spent is honestly nothing");
        assert_eq!(measured(1), 1, "a microsecond of work is not no work");
        assert_eq!(measured(1000), 1);
        assert_eq!(measured(1001), 2);
        assert_eq!(measured(4_056_000), 4056);
    }

    /// A verdict cannot be made without the number it is compared against, and
    /// inventing one would be a judgment about a participant made out of a
    /// broken judge.
    #[test]
    fn a_run_with_no_processor_time_is_not_a_verdict() {
        let refused = Measured::of(&outcome_with(None)).unwrap_err();
        assert!(
            refused.contains("processor time"),
            "the reason has to name what is missing: {refused}"
        );
    }

    /// **The defect this signature exists to prevent.** `failed` used to
    /// hard-code both numbers to nothing, on the stated belief that a stopped
    /// test spent time nobody measured — false for the two callers that had a
    /// run in hand, and true only for the two that never started one.
    #[test]
    fn a_test_that_ran_reports_what_it_cost_and_one_that_did_not_reports_nothing() {
        let test = aj_package::Test {
            name: "1a".into(),
            group: 1,
            letter: "a".into(),
            input: Some(PathBuf::from("1a.in")),
            expected: Some(PathBuf::from("1a.out")),
        };

        let ran = failed(
            &test,
            Some(Measured {
                time_ms: 4056,
                memory_bytes: Some(7_655_424),
            }),
            "Time limit exceeded",
            Reason::TimeLimit,
        );
        assert_eq!(ran.time_ms, 4056);
        assert_eq!(ran.memory_bytes, Some(7_655_424));

        let never_ran = failed(&test, None, "Compilation error", Reason::CompilationError);
        assert_eq!(never_ran.time_ms, 0, "nothing ran, so nothing was spent");
        assert_eq!(never_ran.memory_bytes, None);
    }

    fn outcome_with(cpu_time: Option<Duration>) -> aj_sandbox::Outcome {
        aj_sandbox::Outcome {
            exit_code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            wall_time: Duration::from_millis(1),
            stopped: Stopped::OnItsOwn,
            peak_memory_bytes: None,
            cpu_time,
            collected: None,
        }
    }

    fn an_outcome(name: &str) -> TestOutcome {
        TestOutcome {
            name: name.to_owned(),
            group: 1,
            status: Status::Ok,
            percentage: 100,
            time_ms: 1,
            memory_bytes: None,
            note: String::new(),
            reason: None,
        }
    }

    /// **A lane carries both halves of where a run goes**: the processors it is
    /// confined to and the home its reading comes out of. A run given one lane's
    /// processors and another's home would be a reading taken where a second run
    /// was also making one.
    #[tokio::test]
    async fn a_run_placed_in_a_lane_takes_its_processors_and_its_home() {
        let lanes = Lanes::new(vec![Some("0,1".to_owned()), Some("2,3".to_owned())]);
        let first = lanes.take().await;
        let second = lanes.take().await;

        let run = placed(a_run(), On::Lane(&second), Some("0-3"));
        assert_eq!(run.cpuset, Some("2,3".to_owned()));
        assert_eq!(run.lane, 1);
        assert_eq!(first.index(), 0);

        // A build is not the step being timed and gets the whole of what the
        // Runner was given, measured in the first home like everything that
        // names no lane.
        let build = placed(a_run(), On::TheWholeRunner, Some("0-3"));
        assert_eq!(build.cpuset, Some("0-3".to_owned()));
        assert_eq!(build.lane, 0);
    }

    /// **The judge of a test runs in the test's own lane.** A source check, for
    /// the reason the pin gate above is one: the property is only observable on
    /// a divided host. The type carries the other half -- a `Lane` is not
    /// `Clone`, and exactly one is in scope inside a test.
    #[test]
    fn a_test_and_the_judge_beside_it_are_confined_to_one_lane() {
        let source = include_str!("pipeline.rs");
        let production = &source[..source.find("#[cfg(test)]").expect("a test module")];

        assert_eq!(
            production.matches("On::Lane(").count(),
            production.matches("On::Lane(lane)").count(),
            "a container placed in a lane takes the one its test is holding, and              there is only ever one of those in scope to take",
        );
        // The judged run, the interactor and the checker -- and the arm of
        // `placed` that puts them there.
        assert_eq!(production.matches("On::Lane(lane)").count(), 4);
        // The two builds, and the arm of `placed` that gives them the whole of
        // what the Runner was given. A timed step here would escape its lane.
        assert_eq!(production.matches("On::TheWholeRunner").count(), 3);
    }

    /// **A lane comes back when the test that had it ends**, or the second
    /// submission to a Runner waits forever.
    #[tokio::test]
    async fn a_lane_is_given_back_when_the_test_that_had_it_ends() {
        let lanes = Lanes::new(vec![Some("0".to_owned()), Some("1".to_owned())]);
        let first = lanes.take().await;
        let second = lanes.take().await;
        assert_eq!((first.index(), second.index()), (0, 1));
        drop(second);

        // **The index is back before the permit is**, which is what this asks:
        // a waiter released by the permit finds a lane in `idle` rather than a
        // `pop` on an empty list.
        let third = lanes.take().await;
        assert_eq!(third.index(), 1);
        assert_eq!(third.cpus(), Some("1"));
    }

    /// The order a participant reads, which is the order the tests are
    /// declared in -- never the order they happened to finish in.
    #[test]
    fn results_are_read_back_in_test_order() {
        let done = vec![
            (2, Ok(Some(an_outcome("1c")))),
            (0, Ok(Some(an_outcome("1a")))),
            (1, Ok(Some(an_outcome("1b")))),
        ];
        let names: Vec<String> = in_test_order(done)
            .expect("no failure")
            .into_iter()
            .map(|outcome| outcome.name)
            .collect();
        assert_eq!(names, ["1a", "1b", "1c"]);
    }

    /// **The first failure in test order**, and not the first to arrive: a
    /// submission whose machinery failed twice must be reported the same way
    /// whichever test was slower.
    #[test]
    fn the_first_failure_in_test_order_is_the_one_reported() {
        let done = vec![
            (2, Err("the third".to_owned())),
            (0, Ok(Some(an_outcome("1a")))),
            (1, Err("the second".to_owned())),
        ];
        assert_eq!(in_test_order(done).unwrap_err(), "the second");
    }

    /// A test that never began is not an outcome. Scoring one would grade a
    /// submission on tests nobody ran.
    #[test]
    fn a_test_that_never_began_is_not_an_outcome() {
        let done = vec![
            (0, Ok(Some(an_outcome("1a")))),
            (1, Ok(None)),
            (2, Ok(Some(an_outcome("1c")))),
        ];
        let outcomes = in_test_order(done).expect("no failure");
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[1].name, "1c");
    }
}
