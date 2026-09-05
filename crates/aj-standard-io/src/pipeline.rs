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
use aj_sandbox::pipes::{release, Fifo};
use aj_sandbox::{Beside, Enough, Mount, Pipes, Profile, Sandbox, Stopped};

use crate::checker::{checker_said, Broken};
use crate::compare::{Comparing, Comparison};
use crate::details::{compiled, failed_to_compile, Compilation, Details, Limits};
use crate::language::{self, Images, BUILD_OUTPUT, INPUT, OUTPUT, PROGRAM, SOURCE};
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

/// The largest submission this problem type will look at — **the outer wall,
/// and not a rule of anybody's activity.**
///
/// **The manager's limit is not this one and is not enforced here.** It is
/// `Activity.MaxUploadBytes`, narrowed per problem by `SeriesProblem`, and the
/// Server applies it to the bytes as they arrive. That is a decision, taken
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
    pub outputs: Option<Places>,
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
    fn pinned(&self, profile: Profile) -> Profile {
        pin(profile, self.cpus.as_deref())
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

            // **Where this test's stdout goes, instead of the daemon's log.**
            // One directory per test, made by the Runner and root's, so the
            // submission — which runs as `nobody` — cannot create, rename or
            // read anything in it. The shim opens the file inside it before it
            // drops privileges and hands over the descriptor alone.
            let outputs = job
                .outputs
                .clone()
                .unwrap_or_else(|| job.work.join("out"))
                .join(&test.name);
            std::fs::create_dir_all(&outputs.here).map_err(|e| {
                format!(
                    "test {}: the output directory could not be made: {e}",
                    test.name
                )
            })?;
            // **A pipe, and the whole change is in that word.** The bytes go
            // from the program to this process and stop there: nothing is
            // written down, nothing is collected, and the answer is known while
            // the program is still running rather than after it has finished
            // producing an answer that was wrong at its first token.
            //
            // Made here, because the shim creates nothing — it opens what it is
            // given, so a channel that is not there is the Runner's failure to
            // prepare rather than something for the far end to invent.
            let output = Fifo::make(outputs.here.join(Pipes::OUTPUT), 0o600).map_err(|e| {
                format!(
                    "test {}: the output channel could not be made: {e}",
                    test.name
                )
            })?;

            // **Who compares decides what the relay does with the bytes.** With
            // no checker the Runner tokenises them itself and can stop the
            // program the moment they diverge; with one it has to keep them,
            // because the checker is a separate program that has not started.
            // Wiring the checker to the same pipe is the next step, and it is
            // what makes this arm disappear.
            let watching = match &checker {
                Some(_) => Watching::Keep,
                None => Watching::Against(
                    String::from_utf8_lossy(
                        &std::fs::read(&test.expected).map_err(|e| e.to_string())?,
                    )
                    .into_owned(),
                ),
            };
            let beside = Beside::new();
            let reading = relay(
                output.path().to_path_buf(),
                watching,
                OUTPUT_CAP,
                beside.clone(),
            );

            let run = self
                .sandbox
                .run_beside(
                    &self.pinned(
                        Profile::new(
                            &language.image,
                            language::with_input(
                                &language.start,
                                &test.name,
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
                        .pipes(&outputs.here, &outputs.on_host, OUTPUT)
                        .mount(Mount::read_only(&artefacts.on_host, PROGRAM))
                        .mount(input_mount(&job.package.on_host, &test.name)),
                    ),
                    &beside,
                )
                .await;

            // **Before the run's own error, and that is not a preference.** The
            // relay is a thread blocked on an open that a container which never
            // started will never answer; leaving by the `?` below would leak one
            // per failed test for the life of the Runner.
            release(output.path());
            let produced = reading
                .await
                .map_err(|e| format!("test {}: the output was not read: {e}", test.name))?;
            let run = run.map_err(|e| format!("a test could not be run: {e}"))?;

            // The channels are gone with it; nothing in here outlives a test.
            let _ = std::fs::remove_dir_all(&outputs.here);
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
            if let Some((note, reason)) = stopped {
                outcomes.push(failed(test, Some(measured), &note, reason));
                continue;
            }

            // **After the sandbox's own findings and before the exit code.** A
            // memory limit outranks this — the program was stopped by the kernel
            // for a reason the participant can act on — but flooding then
            // exiting non-zero is flooding, and the non-zero is a consequence of
            // being cut off.
            if produced.capped {
                outcomes.push(failed(
                    test,
                    Some(measured),
                    "Output limit exceeded",
                    Reason::OutputLimit,
                ));
                continue;
            }

            // **A decided run's exit code is not evidence.** It was stopped
            // mid-write, so it died of a signal it was given rather than one it
            // earned, and reading that as a runtime error would turn every early
            // wrong answer into a crash. A run that finished **on its own** and
            // then reported a failure is a different matter, and still outranks
            // whatever the comparison found: wrong output followed by a segfault
            // is a segfault.
            if run.stopped != Stopped::Decided && run.exit_code != 0 {
                outcomes.push(failed(
                    test,
                    Some(measured),
                    &how_it_died(run.exit_code),
                    Reason::RuntimeError,
                ));
                continue;
            }

            let (status, percentage, note) = match &checker {
                Some(built) => {
                    let answer = answers.here.join(format!("{}.out", test.name));
                    std::fs::write(&answer, &produced.kept).map_err(|e| e.to_string())?;
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
                memory_bytes: measured.memory_bytes,
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

/// What the Runner does with a submission's output while it is being produced.
///
/// **The Runner is the only reader of it, always.** Where there is no checker it
/// tokenises the bytes itself; where there is one it keeps them for it. The
/// program is never wired to anything the package brought with it.
enum Watching {
    /// Compare against the reference answer, token by token, as it arrives.
    Against(String),
    /// Hold it for a checker to be given afterwards.
    Keep,
}

/// What came out of one test, and what was made of it on the way.
struct Produced {
    /// The comparison, where the Runner was the one comparing.
    found: Option<Comparison>,
    /// The bytes, where a checker is going to want them.
    kept: Vec<u8>,
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
/// **Blocking, on a thread of its own, and the blocking is the point.** Opening
/// a pipe for reading waits for a writer, so the reader cannot mistake *nothing
/// has been written yet* for *nothing will be* — which is what a non-blocking
/// open reports, as an immediate end of file, and it would arrive here as a
/// program that printed nothing.
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
) -> tokio::task::JoinHandle<Produced> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read as _;

        let mut comparing = match &watching {
            Watching::Against(expected) => Some(Comparing::against(expected)),
            Watching::Keep => None,
        };
        let mut kept = Vec::new();
        let mut capped = false;
        let mut total: u64 = 0;

        if let Ok(mut channel) = std::fs::File::open(&at) {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let read = match channel.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
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
                    kept = Vec::new();
                    beside.enough(Enough::Output);
                }
                if capped {
                    continue;
                }

                match &mut comparing {
                    Some(comparing) => {
                        if comparing.feed(chunk).is_some() {
                            beside.enough(Enough::Decided);
                        }
                    }
                    None => kept.extend_from_slice(chunk),
                }
            }
        }

        Produced {
            found: comparing.map(|comparing| comparing.finish()),
            kept,
            capped,
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
        .map(|test| failed(test, None, "Compilation error", Reason::CompilationError))
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
    /// inventing one would be a judgement about a participant made out of a
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
            input: PathBuf::from("1a.in"),
            expected: PathBuf::from("1a.out"),
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
}
