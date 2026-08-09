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
use crate::score::{judge, Judgement, Status, TestOutcome};

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
    pub source: &'a [u8],
    /// The unpacked package.
    pub package: Places,
    /// Scratch for this job alone, and empty. Removed by the caller afterwards.
    pub work: Places,
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
            .check(job.language, &String::from_utf8_lossy(job.source));
        if !broken.is_empty() {
            return Ok(policy_violation(job, &broken));
        }

        // ── build ───────────────────────────────────────────────────────────
        let mut log = String::new();
        if let Some(command) = language.build.clone() {
            let built = self
                .sandbox
                .run(
                    &Profile::new(&language.image, command)
                        .memory_kib(512 * 1024)
                        .pids(128)
                        .wall_clock(Duration::from_secs(60))
                        .max_output_bytes(256 * 1024)
                        .tmpfs_kib(64 * 1024)
                        .writable_root()
                        .collect(BUILD_OUTPUT)
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
                return Ok(compilation_failed(job, &said));
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
            let limits = job.config.effective(test.group, job.language);

            let run = self
                .sandbox
                .run(
                    &Profile::new(
                        &language.image,
                        language::with_input(&language.start, &test.name),
                    )
                    .memory_kib(limits.memory_kib)
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
                Stopped::WallClock => Some("Przekroczenie limitu czasu"),
                Stopped::Memory => Some("Przekroczenie limitu pamięci"),
                Stopped::Output => Some("Przekroczenie limitu wyjścia"),
                Stopped::OnItsOwn => None,
            };
            if let Some(note) = stopped {
                outcomes.push(failed(test, time_ms, note));
                continue;
            }
            if run.exit_code != 0 {
                outcomes.push(failed(
                    test,
                    time_ms,
                    &format!("Błąd wykonania, kod {}", run.exit_code),
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
                // Absent until something can measure it honestly — cgroup v2,
                // or `isolate` underneath.
                memory_kib: None,
                note,
            });
        }

        let judgement = judge(job.config, job.tests, &outcomes);
        let details = Details::of(&judgement, limits_of(job), compiled());

        Ok(Evaluated::Judged(Box::new(Verdict {
            judgement,
            details,
            log,
        })))
    }

    /// Builds the package's checker. Its failure is the **package** being
    /// broken, which is an infrastructure failure and not a verdict.
    async fn build_checker(
        &self,
        job: &Job<'_>,
        declared: &aj_package::config::Source,
    ) -> Result<Places, String> {
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
                    .memory_kib(512 * 1024)
                    .pids(128)
                    .wall_clock(Duration::from_secs(60))
                    .tmpfs_kib(64 * 1024)
                    .writable_root()
                    .collect(BUILD_OUTPUT)
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
        Ok(output)
    }

    /// Runs the checker over one test, in **its own sandbox**.
    ///
    /// It comes from a package a manager authored, not from the platform, so it
    /// is untrusted-adjacent: it gets limits and no network like anything else,
    /// and it never runs in the Runner's process.
    async fn check(
        &self,
        job: &Job<'_>,
        checker: &Places,
        answers: &Places,
        test: &str,
    ) -> Result<Result<crate::checker::Checked, Broken>, String> {
        let run = self
            .sandbox
            .run(
                &Profile::new(
                    &self.images.cpp,
                    vec![
                        format!("{PROGRAM}/program"),
                        format!("{INPUT}/{test}.in"),
                        format!("/answers/{test}.out"),
                        format!("{INPUT}/{test}.out"),
                    ],
                )
                .memory_kib(256 * 1024)
                .pids(16)
                .wall_clock(Duration::from_secs(30))
                .max_output_bytes(64 * 1024)
                .mount(Mount::read_only(&checker.on_host, PROGRAM))
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

fn failed(test: &aj_package::Test, time_ms: u64, note: &str) -> TestOutcome {
    TestOutcome {
        name: test.name.clone(),
        group: test.group,
        status: Status::Error,
        percentage: 0,
        time_ms,
        memory_kib: None,
        note: note.to_owned(),
    }
}

/// The submission broke the activity's rules, so nothing was built or run.
///
/// Distinct from a compilation error and from an internal error, because those
/// are three different things to tell a participant: their code is wrong, their
/// code does not build, or the system failed. This one is none of the three —
/// their code may be perfect and still not allowed.
fn policy_violation(job: &Job<'_>, broken: &[crate::policy::Violation]) -> Evaluated {
    let listed: Vec<String> = broken.iter().map(|v| v.note()).collect();

    let outcomes: Vec<TestOutcome> = job
        .tests
        .iter()
        .map(|test| failed(test, 0, "Naruszenie regulaminu"))
        .collect();

    let judgement = judge(job.config, job.tests, &outcomes);
    let details = Details::of(
        &judgement,
        limits_of(job),
        Compilation {
            // Not an error: nothing failed to compile, because nothing was
            // offered to a compiler.
            status: Status::Warning,
            log: listed.join(
                "
",
            ),
        },
    );

    Evaluated::Judged(Box::new(Verdict {
        judgement: Judgement {
            verdict: "PolicyViolation".into(),
            ..judgement.clone()
        },
        details,
        log: listed.join(
            "
",
        ),
    }))
}

/// Every test failed for the same reason, and the reason is worth stating once.
fn compilation_failed(job: &Job<'_>, log: &str) -> Evaluated {
    let outcomes: Vec<TestOutcome> = job
        .tests
        .iter()
        .map(|test| failed(test, 0, "Błąd kompilacji"))
        .collect();

    let judgement = judge(job.config, job.tests, &outcomes);
    let details = Details::of(&judgement, limits_of(job), failed_to_compile(log));

    Evaluated::Judged(Box::new(Verdict {
        judgement: Judgement {
            verdict: "Błąd kompilacji".into(),
            ..judgement.clone()
        },
        details,
        log: log.to_owned(),
    }))
}

/// The problem's stated limits, for the document a participant reads. Kibibytes
/// in the package, mebibytes here — see `details`.
fn limits_of(job: &Job<'_>) -> Limits {
    Limits {
        time_ms: job.config.limits.time_ms,
        memory_mb: job.config.limits.memory_kib / 1024,
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
