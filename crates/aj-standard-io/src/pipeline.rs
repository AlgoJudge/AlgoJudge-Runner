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

use aj_package::{Config, TestSet};
use aj_sandbox::{Mount, Profile, Sandbox, Stopped};

use crate::checker::{checker_said, Broken};
use crate::compare::compare;
use crate::details::{compiled, failed_to_compile, Compilation, Details, Limits};
use crate::language::{self, Images, BUILD_OUTPUT, INPUT, PROGRAM, SOURCE};
use crate::score::{judge, Judgement, Reason, Status, TestOutcome};

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
/// overrode it, so the artefact was read into the trusted process — twice, at
/// the moment of joining — and `unpack` wrote a third copy into the job's
/// scratch, where it is mounted into every test container.
///
/// Applied to the container rather than caught afterwards, because `SIGXFSZ`
/// makes an oversized artefact the participant's **compilation error**, which
/// is a verdict they can act on, instead of an infrastructure failure that
/// claims the system broke. A statically linked C++ binary with heavy
/// templates is tens of megabytes, so this refuses what is not a program.
const BUILD_ARTEFACT_BYTES: u64 = 64 * 1024 * 1024;

/// What a build may say, for **both** builds.
///
/// One constant for the reason the tmpfs above is one: the submission's build
/// capped its output at 256 KiB and the checker's capped nothing, so a checker
/// that would not stop talking put 64 MiB — the profile's default — into an
/// infrastructure-failure message and into the uploaded log.
const BUILD_LOG_BYTES: u64 = 256 * 1024;

/// The largest submission this problem type will look at.
///
/// **The Runner validates the participant's input, because nothing above it
/// can.** The Server stores a submission without opening it, and what makes one
/// well formed belongs to the problem type — which is exactly why
/// `output-only@1` bounds its own archive here rather than asking for that to
/// be done upstream. A Server that validated this would need a rule per type,
/// which is the thing the product's boundary exists to prevent.
///
/// Generous on purpose. A solution is a few kilobytes and a generated one
/// carrying lookup tables is tens of them, so this bounds what is plainly not a
/// program rather than judging anybody's style.
const MAX_SOURCE_BYTES: usize = 1024 * 1024;

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

/// What a running submission is given of the package: **one input file.**
///
/// A named function with a test rather than an expression at the call site,
/// because getting it wrong is silent and expensive. `tests/` holds
/// `<name>.in` beside `<name>.out`, so mounting the directory whole hands the
/// submission the answer key — `cat /in/1a.out` prints what it was asked to
/// compute, and the run still looks like an ordinary correct solution.
///
/// That was the arrangement until 2026-08-09. The only thing standing in front
/// of it was the forbidden-word dictionary catching `fopen`, `ifstream` and
/// `open`; `docs/SECURITY.md` §4 states that the dictionary is a **policy**
/// control and that every rule in it is expected to be bypassable. An answer
/// key must not rest on a control the project itself calls bypassable.
///
/// The program is started as `exec … < /in/<name>.in`, so one file is all it
/// ever needed. The checker still receives both, because comparing them is its
/// job and it is not the participant's code.
fn input_mount(package: &Path, test: &str) -> Mount {
    Mount::read_only(
        package.join("tests").join(format!("{test}.in")),
        format!("{INPUT}/{test}.in"),
    )
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
    /// The unpacked package.
    pub package: Places,
    /// Scratch for this job alone, and empty. Removed by the caller afterwards.
    pub work: Places,
}

/// The package's checker, built and ready to be run over a test.
///
/// Carries its image and how it is started rather than assuming both. The
/// assumption held while `cpp` was the only compiled language there was; it
/// stopped holding the moment a package could name `cpp17-clang` — or `python3`,
/// where starting `/program/program` would try to execute a `.py` file.
struct Checker {
    at: Places,
    image: String,
    start: Vec<String>,
}

/// A submission that was actually judged.
pub struct Verdict {
    pub judgement: Judgement,
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

pub struct Pipeline<S> {
    sandbox: S,
    images: Images,
    /// Which core the next timed run is pinned to.
    ///
    /// Rotated rather than fixed: pinning every job to core 0 would make a
    /// machine with sixteen of them evaluate on one, and two Runners sharing a
    /// host would fight over it. The build is not pinned — it is not the step
    /// being timed, and it is the one that benefits from more.
    next_core: std::sync::atomic::AtomicUsize,
}

impl<S: Sandbox> Pipeline<S> {
    pub fn new(sandbox: S, images: Images) -> Self {
        Self {
            sandbox,
            images,
            next_core: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn core(&self) -> usize {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        self.next_core
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % cores
    }

    pub async fn evaluate(&self, job: &Job<'_>) -> Evaluated {
        match self.attempt(job).await {
            Ok(evaluated) => evaluated,
            Err(reason) => Evaluated::Failed(reason),
        }
    }

    async fn attempt(&self, job: &Job<'_>) -> Result<Evaluated, String> {
        let language = language::for_id(job.language, &self.images)
            .ok_or_else(|| format!("this Runner does not evaluate {}", job.language))?;

        // ── the languages this assignment allows ────────────────────────────
        //
        // **The Server used to refuse this and cannot any more**: the language
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
        // scan reads them. See [`MAX_SOURCE_BYTES`] for why this is the
        // Runner's job and not the Server's.
        //
        // A **verdict**, and `PolicyViolation` for the same reason the language
        // check above is one: nothing was offered to a compiler, the code may
        // be perfect, and what was broken is a rule of the activity. The
        // submission stays rejudgeable.
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
        let artefacts = built_into.join("out");
        let answers = job.work.join("answers");
        for place in [&source, &built_into, &answers] {
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
                    &Profile::new(&language.image, command)
                        .memory_bytes(512 * 1024 * 1024)
                        .pids(128)
                        .wall_clock(Duration::from_secs(60))
                        .max_output_bytes(BUILD_LOG_BYTES)
                        .max_file_bytes(BUILD_ARTEFACT_BYTES as i64)
                        .tmpfs_bytes(BUILD_TMPFS_BYTES)
                        .writable_root()
                        .collect(BUILD_OUTPUT, BUILD_ARTEFACT_BYTES)
                        .mount(Mount::read_only(&source.on_host, SOURCE)),
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
        let checker = match &job.config.checker {
            None => None,
            Some(declared) => Some(self.build_checker(job, declared).await?),
        };

        // ── each test, in its own container ─────────────────────────────────
        let mut outcomes = Vec::new();
        for test in job.tests.iter() {
            let limits = job.config.effective(test.group, &language.keys());

            let run = self
                .sandbox
                .run(
                    &Profile::new(
                        &language.image,
                        language::with_input(&language.start, &test.name),
                    )
                    .memory_bytes(limits.memory_bytes)
                    .pids(16)
                    .cpuset(self.core())
                    .max_output_bytes(64 * 1024 * 1024)
                    // **The limit plus a grace.** A program stuck in an
                    // uninterruptible syscall has to be reaped from
                    // outside, and the verdict comes from the measurement
                    // rather than from this deadline.
                    .wall_clock(Duration::from_millis(limits.time_ms) + Duration::from_secs(1))
                    .mount(Mount::read_only(&artefacts.on_host, PROGRAM))
                    .mount(input_mount(&job.package.on_host, &test.name)),
                )
                .await
                .map_err(|e| format!("a test could not be run: {e}"))?;

            let time_ms = run.wall_time.as_millis() as u64;

            // What the machinery did to it comes first: none of these is the
            // program having answered wrongly.
            let stopped = match run.stopped {
                Stopped::WallClock => Some(("Time limit exceeded", Reason::TimeLimit)),
                Stopped::Memory => Some(("Memory limit exceeded", Reason::MemoryLimit)),
                Stopped::Output => Some(("Output limit exceeded", Reason::OutputLimit)),

                // **The limit is decided here, on the measurement, and until
                // 2026-08-22 it was decided nowhere.** The deadline above is the
                // limit plus a second of grace, because a program wedged in an
                // uninterruptible syscall has to be reaped from outside — and
                // the comment beside it has always said the verdict comes from
                // the measurement rather than from that deadline. No such
                // comparison existed. A solution that overran its limit by
                // anything less than the grace finished on its own, was never
                // reaped, and was marked correct: at a one-second limit, a
                // program taking 1.9 s was `Accepted`.
                //
                // The wall clock is what decides it, deliberately —
                // `Outcome::cpu_time` says so where it is declared, because a
                // limit is stated in what a participant waits through. It
                // includes the container's start, and so do the numbers a trial
                // measures, which is what keeps a calibrated limit honest
                // against the thing it is compared with.
                Stopped::OnItsOwn if time_ms > limits.time_ms => {
                    Some(("Time limit exceeded", Reason::TimeLimit))
                }
                Stopped::OnItsOwn => None,
            };
            if let Some((note, reason)) = stopped {
                outcomes.push(failed(test, time_ms, note, reason));
                continue;
            }
            if run.exit_code != 0 {
                outcomes.push(failed(
                    test,
                    time_ms,
                    &how_it_died(run.exit_code),
                    Reason::RuntimeError,
                ));
                continue;
            }

            let answer = answers.here.join(format!("{}.out", test.name));
            std::fs::write(&answer, &run.stdout).map_err(|e| e.to_string())?;

            let (status, percentage, note) = match &checker {
                Some(built) => {
                    match self.check(job, built, &answers, &test.name).await? {
                        Ok(said) => (
                            if said.accepted {
                                Status::Ok
                            } else {
                                Status::Error
                            },
                            said.percentage,
                            said.comment,
                        ),
                        // The load-bearing rule: a checker that exited non-zero
                        // means the **system** failed. Reporting it as a wrong
                        // answer turns a bug in the checker into a rejected
                        // submission.
                        Err(broken) => return Err(broken.to_string()),
                    }
                }
                None => {
                    let found = compare(
                        &std::fs::read(&test.expected).map_err(|e| e.to_string())?,
                        &run.stdout,
                    );
                    if found.equal() {
                        (Status::Ok, 100, String::new())
                    } else {
                        (Status::Error, 0, found.note())
                    }
                }
            };

            outcomes.push(TestOutcome {
                name: test.name.clone(),
                group: test.group,
                status,
                percentage,
                time_ms,
                // From the run's own cgroup, and **absent when the host gave
                // the Runner nowhere to measure from** — which is the answer
                // `PACKAGE_FORMAT.md` asks for rather than a number that is
                // sometimes wrong. It carries about 2 MiB of container floor
                // that does not scale, and it is not corrected for: a
                // participant's run has the same floor, so subtracting it would
                // make every calibrated limit too tight for the sake of a
                // number nobody meets.
                memory_bytes: run.peak_memory_bytes,
                note,
                // Everything the machinery could do to it was handled above, so
                // a failure this far down is the answer itself.
                reason: (!status.passed()).then_some(Reason::WrongAnswer),
            });
        }

        let judgement = judge(job.config, job.tests, &outcomes);
        let details = Details::of(&judgement, limits_of(job, &language), compiled());

        Ok(Evaluated::Judged(Box::new(Verdict {
            judgement,
            details,
            log,
        })))
    }

    /// Builds the package's checker. Its failure is the **package** being
    /// broken, which is an infrastructure failure and not a verdict.
    ///
    /// Hands back the image it was built in as well as where it landed. Running
    /// it used to be hard-coded to the C++ image, which was true only while
    /// there was one — a checker built by Clang and run in the GCC image is a
    /// coincidence away from working, and a Python checker would have been
    /// started as though it were a binary.
    async fn build_checker(
        &self,
        job: &Job<'_>,
        declared: &aj_package::config::Source,
    ) -> Result<Checker, String> {
        let language = language::for_id(&declared.language, &self.images).ok_or_else(|| {
            format!(
                "the checker is in {}, which this Runner does not build",
                declared.language
            )
        })?;

        let source = job.work.join("checker-src");
        let built_into = job.work.join("checker-build");
        let output = built_into.join("out");
        for place in [&source, &built_into] {
            std::fs::create_dir_all(&place.here).map_err(|e| e.to_string())?;
        }

        let declared_at = job.package.here.join(&declared.source);
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
                &Profile::new(&language.image, language.build.clone().unwrap_or_default())
                    .memory_bytes(512 * 1024 * 1024)
                    .pids(128)
                    .wall_clock(Duration::from_secs(60))
                    .max_output_bytes(BUILD_LOG_BYTES)
                    .max_file_bytes(BUILD_ARTEFACT_BYTES as i64)
                    .tmpfs_bytes(BUILD_TMPFS_BYTES)
                    .writable_root()
                    .collect(BUILD_OUTPUT, BUILD_ARTEFACT_BYTES)
                    .mount(Mount::read_only(&source.on_host, SOURCE)),
            )
            .await
            .map_err(|e| format!("the checker could not be built: {e}"))?;

        if !built.succeeded() {
            return Err(format!(
                "the package's checker does not build: {}",
                String::from_utf8_lossy(&built.stderr),
            ));
        }
        unpack(&built.collected, &built_into.here)?;
        Ok(Checker {
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
    async fn check(
        &self,
        job: &Job<'_>,
        checker: &Checker,
        answers: &Places,
        test: &str,
    ) -> Result<Result<crate::checker::Checked, Broken>, String> {
        let mut command = checker.start.clone();
        command.extend([
            format!("{INPUT}/{test}.in"),
            format!("/answers/{test}.out"),
            format!("{INPUT}/{test}.out"),
        ]);

        let run = self
            .sandbox
            .run(
                &Profile::new(&checker.image, command)
                    .memory_bytes(256 * 1024 * 1024)
                    .pids(16)
                    .wall_clock(Duration::from_secs(30))
                    .max_output_bytes(64 * 1024)
                    .mount(Mount::read_only(&checker.at.on_host, PROGRAM))
                    .mount(Mount::read_only(job.package.on_host.join("tests"), INPUT))
                    .mount(Mount::read_only(&answers.on_host, "/answers")),
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

fn failed(test: &aj_package::Test, time_ms: u64, note: &str, reason: Reason) -> TestOutcome {
    TestOutcome {
        name: test.name.clone(),
        group: test.group,
        status: Status::Error,
        percentage: 0,
        time_ms,
        memory_bytes: None,
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
        .map(|test| failed(test, 0, "Policy violation", Reason::PolicyViolation))
        .collect();

    let judgement = judge(job.config, job.tests, &outcomes);
    let details = Details::of(
        &judgement,
        limits_of(job, language),
        Compilation {
            // Not an error: nothing failed to compile, because nothing was
            // offered to a compiler.
            status: Status::Warning,
            log: listed.join("\n"),
        },
    );

    Evaluated::Judged(Box::new(Verdict {
        judgement: Judgement {
            verdict: "PolicyViolation".into(),
            ..judgement.clone()
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
        .map(|test| failed(test, 0, "Compilation error", Reason::CompilationError))
        .collect();

    let judgement = judge(job.config, job.tests, &outcomes);
    let details = Details::of(&judgement, limits_of(job, language), failed_to_compile(log));

    Evaluated::Judged(Box::new(Verdict {
        judgement: Judgement {
            verdict: "Compilation error".into(),
            ..judgement.clone()
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
    use super::*;

    use crate::policy::Violation;

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

    /// The answer key is not in the container the submission runs in.
    ///
    /// Asserted on the mount rather than by trying to read it from a
    /// submission, because every way of reading a file is already refused by
    /// the word dictionary — so a test written that way would pass for the
    /// wrong reason and keep passing after the dictionary was relaxed.
    #[test]
    fn a_running_submission_is_given_its_input_and_not_the_answers() {
        let mount = input_mount(Path::new("/cache/pkg"), "1a");

        assert_eq!(mount.from, Path::new("/cache/pkg/tests/1a.in"));
        assert_eq!(mount.to, "/in/1a.in");
        assert!(!mount.writable);

        // The shape that leaked: the directory itself, which carries `1a.out`.
        assert_ne!(
            mount.from,
            Path::new("/cache/pkg/tests"),
            "mounting the directory hands over the answer key",
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
}
