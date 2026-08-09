//! The two loops: getting admitted, and doing the work.

use std::sync::Arc;
use std::time::Duration;

use aj_protocol::wire::{
    AttachToJob, ClaimedJob, ClaimedTrial, Register, ReportResult, TrialReport,
};
use aj_protocol::{Backoff, Cache, Identity, Server};
use aj_standard_io::{Evaluated, Pipeline, Places};

use crate::config::Config;
use crate::pause;

/// Registers, waits to be approved, and comes back holding a token.
///
/// None of the waiting here is a failure. A Runner announces itself and an
/// administrator approves it, possibly tomorrow; a process that exited because
/// nobody had got to it yet would have to be watched by something else.
pub async fn admitted(server: &Server, identity: &Identity, config: &Config) -> anyhow::Result<()> {
    let register = Register {
        name: config.name.clone(),
        product: "AlgoJudge-Runner".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        public_key: identity.public_key(),
        problem_types: config.problem_types.clone(),
        machine: Some(machine()),
    };

    // **Backed off and jittered, where this used to ask every five seconds for
    // ever.** This loop is what a room of Runners runs on a cold start and after
    // every token expiry, so an unjittered five seconds is the worst herd in the
    // product — pointed at a Server that is, by the time it matters, deliberately
    // trying to be down.
    let mut backoff = Backoff::new(config.poll_min, config.poll_max);

    loop {
        match server.register(&register).await {
            Ok(registered) => {
                if registered.fingerprint != identity.fingerprint() {
                    // Both sides hash the same 32 bytes. Disagreeing means
                    // something re-encoded the key in transit, and every
                    // signature after this would fail with nothing to explain
                    // it.
                    anyhow::bail!(
                        "the Server calls this key {}, this Runner calls it {} — \
                         the public key was re-encoded somewhere in between",
                        registered.fingerprint,
                        identity.fingerprint(),
                    );
                }
                tracing::info!(
                    runner_id = %registered.runner_id,
                    state = %registered.state,
                    "registered",
                );
                break;
            }
            Err(e) if e.revoked() => {
                // Terminal by design: there is no rotation, so coming back
                // means a new key, a new registration and a new approval —
                // decisions for a person, not a retry loop.
                anyhow::bail!("this key has been revoked; register a new one: {e}");
            }
            Err(e) if e.unavailable() => wait_out(server, &e, &mut backoff).await,
            Err(e) if e.retryable() => {
                pause(
                    &format!("the Server is not answering ({e})"),
                    how_long(&e, &mut backoff),
                )
                .await
            }
            Err(e) => anyhow::bail!("registration was refused: {e}"),
        }
    }

    backoff.reset();

    loop {
        match server.authenticate(identity).await {
            Ok(()) => return Ok(()),
            Err(e) if e.not_approved() => {
                // A person has to act, so this is measured in minutes rather
                // than seconds — but it still backs off, because a room of
                // Runners registered together would otherwise ask in one burst
                // for however long it takes somebody to get to the panel.
                pause(
                    "this Runner has not been approved yet",
                    how_long(&e, &mut backoff),
                )
                .await;
            }
            Err(e) if e.revoked() => anyhow::bail!("this key has been revoked: {e}"),
            Err(e) if e.unavailable() => wait_out(server, &e, &mut backoff).await,
            Err(e) if e.retryable() => {
                pause(
                    &format!("the handshake failed ({e})"),
                    how_long(&e, &mut backoff),
                )
                .await
            }
            Err(e) => anyhow::bail!("the handshake was refused: {e}"),
        }
    }
}

/// How long to wait after a refusal worth retrying.
///
/// **The Server's `Retry-After` wins only where it asked for longer.** This
/// Runner's own jittered backoff is the floor, so an operator asking for five
/// minutes is honoured, while a proxy answering `Retry-After: 0` cannot turn a
/// retry into a spin against a Server that is trying to be down.
fn how_long(e: &aj_protocol::Error, backoff: &mut Backoff) -> Duration {
    let mine = backoff.next_delay();
    e.retry_after()
        .filter(|asked| *asked > mine)
        .unwrap_or(mine)
}

/// Waits out a Server that is up and declining to serve, and says which it is.
///
/// Asks `/health` rather than guessing, because that is the one path a window
/// leaves open and because the answer carries the operator's own words. Waiting
/// in silence would be indistinguishable from a Runner that had stopped.
///
/// Returns as soon as the window has ended, without sleeping: the question is
/// asked before the wait, so a window that closed in between costs nothing.
async fn wait_out(server: &Server, e: &aj_protocol::Error, backoff: &mut Backoff) {
    let delay = how_long(e, backoff);

    match server.health().await {
        Ok(health) if health.open() => {
            tracing::info!("the Server is serving again");
            backoff.reset();
            return;
        }
        Ok(health) => tracing::info!(
            level = health.level(),
            reason = health.reason().unwrap_or("none given"),
            ?delay,
            "the Server is under maintenance; waiting",
        ),
        // Health is anonymous and answers at every level, so failing here means
        // the Server is not merely withdrawn — it is unreachable. Same wait,
        // different sentence, because they are fixed by different people.
        Err(unreachable) => tracing::warn!(
            %unreachable,
            ?delay,
            "the Server is not serving, and health could not be read either; waiting",
        ),
    }

    tokio::time::sleep(delay).await;
}

/// Claim, evaluate, report. For ever.
pub async fn work(
    server: &Arc<Server>,
    cache: &Arc<Cache>,
    pipeline: &Pipeline<aj_sandbox::Docker>,
    config: &Config,
) -> anyhow::Result<()> {
    let mut backoff = Backoff::new(config.poll_min, config.poll_max);
    let mut last_beat = tokio::time::Instant::now();

    loop {
        match server.claim(Some(config.lease_seconds)).await {
            Ok(Some(job)) => {
                backoff.reset();
                handle(server, cache, pipeline, config, job).await;
            }
            // An empty queue is the ordinary state of a Runner, not a fault.
            Ok(None) => {
                // **Only when there is no marking to do.** A trial produces
                // timings for somebody who asked; a job decides somebody's
                // grade. Asking for trials first would let a busy trial queue
                // delay a verdict, which is the one thing the separate table
                // exists to prevent.
                match server.claim_trial(Some(config.lease_seconds)).await {
                    Ok(Some(trial)) => {
                        backoff.reset();
                        run_trial(server, cache, pipeline, config, trial).await;
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(%e, "could not ask for a trial"),
                }

                if last_beat.elapsed() >= config.heartbeat {
                    match server.heartbeat().await {
                        Ok(()) => {}
                        // Expected during a window rather than worth a warning:
                        // the Server has withdrawn and already knows.
                        Err(e) if e.unavailable() => {
                            tracing::debug!(%e, "the Server is not taking heartbeats")
                        }
                        Err(e) => tracing::warn!(%e, "the heartbeat did not land"),
                    }
                    last_beat = tokio::time::Instant::now();
                }
                backoff.wait().await;
            }
            Err(e) if e.needs_handshake() => {
                // Tokens live in the Server's memory, so this is what an
                // ordinary Server restart looks like from here.
                tracing::info!("the token is no longer known; shaking hands again");
                server.forget_token();
                let identity = Identity::load_or_create(&config.key_path)?;
                admitted(server, &identity, config).await?;
            }
            // **Not a fault, and not a reason to exit.** At `draining` the
            // Server answers an empty queue instead, so this is the deeper
            // level — everything closed but health.
            Err(e) if e.unavailable() => wait_out(server, &e, &mut backoff).await,
            Err(e) if e.retryable() => {
                tracing::warn!(%e, "could not ask for work");
                tokio::time::sleep(how_long(&e, &mut backoff)).await;
            }
            Err(e) => anyhow::bail!("claiming was refused: {e}"),
        }
    }
}

/// Measures a package's own model solutions and says what they cost.
///
/// **No verdict, no score, no attachments.** A trial answers "how long does
/// this take" — reporting anything shaped like a result would invite a screen
/// to render it as one.
async fn run_trial(
    server: &Arc<Server>,
    cache: &Arc<Cache>,
    pipeline: &Pipeline<aj_sandbox::Docker>,
    config: &Config,
    trial: ClaimedTrial,
) {
    tracing::info!(
        trial = %trial.trial_id,
        problem_type = %trial.problem_type,
        lease_expires_at = %trial.lease_expires_at,
        "claimed a trial",
    );

    let report = match measure_trial(server, cache, pipeline, config, &trial).await {
        Ok(measured) => TrialReport {
            lease_token: trial.lease_token.clone(),
            measurement: Some(measured),
            failure_reason: None,
        },
        Err(Trouble::Machinery(reason)) => {
            tracing::error!(trial = %trial.trial_id, reason, "the trial failed");
            TrialReport {
                lease_token: trial.lease_token.clone(),
                measurement: None,
                failure_reason: Some(reason),
            }
        }
        // The same rule as a job, for the same reason: a trial that failed
        // because the Server withdrew did not fail. Its lease brings it back,
        // and the manager waiting on a measurement gets one instead of a
        // failure they would have to work out was not theirs.
        Err(Trouble::Away(e)) => {
            tracing::warn!(
                trial = %trial.trial_id,
                %e,
                maintenance = e.in_maintenance(),
                "the Server went away mid-trial; leaving it to the lease",
            );
            return;
        }
    };

    // Reported either way: the Server deletes the package when it hears, so a
    // trial nobody reports leaves bytes behind until the reaper gives up on it.
    // And held on to through a window, exactly as a verdict is — a measurement
    // costs the same machine time to produce whoever asked for it.
    let mut keep = KeepTrying::for_a_lease(config);

    loop {
        match server.report_trial(&trial.trial_id, &report).await {
            Ok(accepted) => {
                tracing::info!(
                    trial = %trial.trial_id,
                    state = %accepted.state,
                    duplicate = accepted.duplicate,
                    "trial reported",
                );
                return;
            }
            Err(e) if e.lease_lost() => {
                tracing::warn!(trial = %trial.trial_id, %e, "the trial's lease was gone");
                return;
            }
            Err(e) => match keep.or_give_up(&e) {
                Some(delay) => {
                    tracing::warn!(
                        trial = %trial.trial_id,
                        %e,
                        ?delay,
                        "the trial report did not land; holding on to it",
                    );
                    tokio::time::sleep(delay).await;
                }
                None => {
                    tracing::error!(
                        trial = %trial.trial_id,
                        %e,
                        "the trial report could not be delivered",
                    );
                    return;
                }
            },
        }
    }
}

/// Unpacks, measures, and hands back the document as the format states it.
async fn measure_trial(
    server: &Server,
    cache: &Arc<Cache>,
    pipeline: &Pipeline<aj_sandbox::Docker>,
    config: &Config,
    trial: &ClaimedTrial,
) -> Result<String, Trouble> {
    let work = Scratch::new(&config.work_path, &config.work_host_path, &trial.trial_id)?;

    let archive = cache
        .fetch(server, &trial.package_file_id, &trial.package_sha256)
        .await
        .map_err(Trouble::from)?;

    let package = unpacked_into(&work.places);
    aj_package::extract(
        archive.path(),
        &package.here,
        &aj_package::ArchiveLimits::default(),
    )
    .map_err(|e| e.to_string())?;

    let declared = std::fs::read_to_string(package.here.join("config.yml"))
        .map_err(|e| format!("config.yml could not be read: {e}"))?;

    // The same dispatch as judging, on the same string. A type this Runner does
    // not know is refused rather than guessed at.
    match trial.problem_type.as_str() {
        "standard-io@1" => {
            let package_config = aj_package::Config::parse(&declared).map_err(|e| e.to_string())?;
            let tests = aj_package::TestSet::read(&package.here, &package_config)
                .map_err(|e| e.to_string())?;

            let measured =
                aj_standard_io::measure(pipeline, &package_config, &tests, &package, &work.places)
                    .await?;

            serde_json::to_string(&measured).map_err(|e| e.to_string().into())
        }
        other => Err(format!("this Runner does not measure {other}").into()),
    }
}

async fn handle(
    server: &Arc<Server>,
    cache: &Arc<Cache>,
    pipeline: &Pipeline<aj_sandbox::Docker>,
    config: &Config,
    job: ClaimedJob,
) {
    tracing::info!(
        job = %job.job_id,
        submission = %job.submission_id,
        attempt = job.attempt,
        problem_type = %job.problem_type,
        lease_expires_at = %job.lease_expires_at,
        "claimed",
    );

    let (report, attachments) = match evaluate(server, cache, pipeline, config, &job).await {
        Finished::Say(report, attachments) => (*report, attachments),
        Finished::Abandon => return,
    };

    // Upload, attach, **then** report — the Server requires the job to be
    // `Running` to accept an attachment, and reporting ends that. Get the order
    // wrong and the log explaining a failure is the thing that goes missing.
    attach(server, &job, attachments).await;

    report_with_retries(server, &job, &report, config).await;
}

/// What a finished attempt leaves the Runner holding.
enum Finished {
    /// An answer, whether a verdict or an honest infrastructure failure. It has
    /// to reach the Server, and the report loop keeps trying until it does.
    ///
    /// Boxed only to keep the two variants a similar size, which `clippy`
    /// insists on; there is one of these per job and the indirection costs
    /// nothing that can be measured.
    Say(Box<ReportResult>, Attachments),

    /// Nothing that should be said. Left to the lease on purpose.
    Abandon,
}

/// Unpack, judge, and say what it was worth.
///
/// Everything that goes wrong with the **machinery** — a package that will not
/// open, a config that will not read, a sandbox that will not start — comes back
/// as an infrastructure failure. Only what the submission itself did becomes a
/// verdict.
///
/// And a Server that is away comes back as neither. See [`Trouble`].
async fn evaluate(
    server: &Server,
    cache: &Arc<Cache>,
    pipeline: &Pipeline<aj_sandbox::Docker>,
    config: &Config,
    job: &ClaimedJob,
) -> Finished {
    match judge(server, cache, pipeline, config, job).await {
        Ok((report, attachments)) => Finished::Say(Box::new(report), attachments),

        Err(Trouble::Machinery(reason)) => {
            tracing::error!(job = %job.job_id, reason, "the evaluation failed");
            Finished::Say(
                Box::new(ReportResult::failed(&job.lease_token, reason.clone())),
                Attachments {
                    log: format!("{reason}\n"),
                    details: None,
                },
            )
        }

        Err(Trouble::Away(e)) => {
            // Warning rather than error, and worded for whoever reads the log
            // afterwards: nothing was lost and nothing needs doing.
            tracing::warn!(
                job = %job.job_id,
                submission = %job.submission_id,
                %e,
                maintenance = e.in_maintenance(),
                "the Server went away mid-job; leaving it to the lease rather than failing it",
            );
            Finished::Abandon
        }
    }
}

/// What is uploaded beside a result. `log` is the build's own words; `details`
/// is the per-test table the Client's renderer draws.
struct Attachments {
    log: String,
    details: Option<Vec<u8>>,
}

/// Why there is no verdict — and the distinction the whole feature rests on.
///
/// **These two must never be flattened together.** Before this existed, every
/// failure inside `judge` became one `String` and every `String` became an
/// infrastructure failure reported against the attempt. That is right for a
/// package that will not open and wrong for a Server that went away mid-job:
/// the second one closes somebody's attempt over an outage they had nothing to
/// do with, and a backup starting at the wrong moment costs a participant their
/// submission.
enum Trouble {
    /// Ours, and permanent as far as this attempt is concerned. The submission
    /// cannot be judged, so the Server is told exactly that and the attempt is
    /// closed honestly.
    Machinery(String),

    /// The Server's, and temporary. **Say nothing at all.** The lease is
    /// already the mechanism for this: it expires, the reaper puts the job back
    /// on the queue, and whichever Runner is up when the window ends judges it
    /// properly. Reporting anything here would replace a working retry with a
    /// permanent wrong answer.
    Away(aj_protocol::Error),
}

impl From<aj_protocol::Error> for Trouble {
    /// Classified once, here, so no call site has to remember to.
    ///
    /// `retryable()` is the right question: it is already what tells a rate
    /// limit and an outage from a refusal the Server means. A checksum that does
    /// not match is not retryable and so lands where it belongs — the bytes
    /// arrived and were wrong, which is ours to report.
    fn from(e: aj_protocol::Error) -> Self {
        if e.retryable() {
            Trouble::Away(e)
        } else {
            Trouble::Machinery(e.to_string())
        }
    }
}

impl From<String> for Trouble {
    fn from(reason: String) -> Self {
        Trouble::Machinery(reason)
    }
}

impl From<&str> for Trouble {
    fn from(reason: &str) -> Self {
        Trouble::Machinery(reason.to_owned())
    }
}

async fn judge(
    server: &Server,
    cache: &Arc<Cache>,
    pipeline: &Pipeline<aj_sandbox::Docker>,
    config: &Config,
    job: &ClaimedJob,
) -> Result<(ReportResult, Attachments), Trouble> {
    if !job.has_package() {
        // Empty strings, not absent — there is nothing to judge against.
        return Err("the problem version carries no package".into());
    }

    // One directory per job, removed on every path out including a panic
    // upstream, because a Runner that leaks scratch fills its own disk.
    let work = Scratch::new(&config.work_path, &config.work_host_path, &job.job_id)?;

    // **Not `to_string()`.** These two lines are where a maintenance window
    // reaches a job that has already been claimed: the package and the
    // submission are downloaded from the Server, and flattening the error to a
    // string is exactly how an outage used to become somebody's zero.
    let archive = cache
        .fetch(server, &job.package_file_id, &job.package_sha256)
        .await
        .map_err(Trouble::from)?;

    let package = unpacked_into(&work.places);
    aj_package::extract(
        archive.path(),
        &package.here,
        &aj_package::ArchiveLimits::default(),
    )
    .map_err(|e| e.to_string())?;

    let declared = std::fs::read_to_string(package.here.join("config.yml"))
        .map_err(|e| format!("config.yml could not be read: {e}"))?;

    // The submission itself, by the name the Server gives it.
    let submitted = job
        .files
        .iter()
        .find(|f| f.name == "source")
        .or_else(|| job.files.first())
        .ok_or("the submission carries no file")?;
    let source = cache
        .fetch(server, &submitted.file_id, &submitted.sha256)
        .await
        .map_err(Trouble::from)?;

    // **The whole of what a second problem type costs.** Everything above and
    // below is shared; this is the dispatch, and adding a third type is another
    // arm plus a crate. Nothing about it reaches the Server.
    match job.problem_type.as_str() {
        "standard-io@1" => {
            let package_config = aj_package::Config::parse(&declared).map_err(|e| e.to_string())?;
            let tests = aj_package::TestSet::read(&package.here, &package_config)
                .map_err(|e| e.to_string())?;
            let bytes = std::fs::read(source.path()).map_err(|e| e.to_string())?;
            let language = job.language.as_deref().unwrap_or("cpp");

            let evaluated = pipeline
                .evaluate(&aj_standard_io::Job {
                    config: &package_config,
                    tests: &tests,
                    language,
                    source: &bytes,
                    package,
                    work: work.places.join("scratch"),
                })
                .await;
            finish(evaluated, &job.lease_token).map_err(Trouble::from)
        }

        "output-only@1" => {
            let package_config = aj_package::Config::parse_as(&declared, "output-only")
                .and_then(|c| c.overlaid(job.config.as_ref()))
                .map_err(|e| e.to_string())?;
            let tests = aj_package::TestSet::read(&package.here, &package_config)
                .map_err(|e| e.to_string())?;

            let into = work.places.join("answers").here;
            let answers = if submitted.file_name.ends_with(".zip") {
                // Untrusted, and the only untrusted archive the product opens.
                aj_output_only::Answers::unpack(source.path(), &into)?
            } else if tests.len() == 1 {
                let bytes = std::fs::read(source.path()).map_err(|e| e.to_string())?;
                let only = tests.iter().next().expect("one test").name.clone();
                aj_output_only::Answers::single(&bytes, &only, &into)?
            } else {
                return Err(format!(
                    "this problem has {} tests, so the answers arrive as an archive,                      and {} is not one",
                    tests.len(),
                    submitted.file_name,
                )
                .into());
            };

            let judgement = aj_output_only::mark(&package.here, &package_config, &tests, &answers);
            let details = aj_output_only::details(&judgement, &package_config);

            Ok((
                ReportResult::judged(
                    &job.lease_token,
                    judgement.score,
                    judgement.max_score,
                    &judgement.verdict,
                ),
                Attachments {
                    log: String::new(),
                    details: Some(details.to_bytes()),
                },
            ))
        }

        other => Err(format!("this Runner does not evaluate {other}").into()),
    }
}

/// Turns what the pipeline decided into what the Server is told.
fn finish(evaluated: Evaluated, lease_token: &str) -> Result<(ReportResult, Attachments), String> {
    match evaluated {
        Evaluated::Failed(reason) => Err(reason),
        Evaluated::Judged(verdict) => Ok((
            ReportResult::judged(
                lease_token,
                verdict.judgement.score,
                verdict.judgement.max_score,
                &verdict.judgement.verdict,
            ),
            Attachments {
                log: verdict.log.clone(),
                details: Some(verdict.details.to_bytes()),
            },
        )),
    }
}

/// A per-job directory that removes itself.
struct Scratch {
    places: Places,
}

/// Where a job's package is unpacked: **inside that job's own scratch**.
///
/// A named function rather than a `join` at two call sites, because what it
/// carries is not visible from either — and because the alternative is the sort
/// of change nobody would recognise as a security decision.
///
/// **Every path a sandbox mounts comes from `job.work` or `job.package`**
/// (`pipeline.rs`), and `Scratch` is per job. So while the package is unpacked
/// *into* the scratch, two concurrently judged submissions share no mounted
/// inode — and file locks, which are shared across mount namespaces on one
/// inode, cannot pass between them.
///
/// Unpacking straight from the cache instead would save the copy and look like
/// a performance change. It would also give every submission of a problem the
/// same inodes, and with them a channel between contestants that nothing in a
/// review would flag. `docs/SECURITY.md` §6 records the measurement; the test
/// below is what stops it.
fn unpacked_into(work: &Places) -> Places {
    work.join("package")
}

impl Scratch {
    fn new(
        here: &std::path::Path,
        on_host: &std::path::Path,
        job_id: &str,
    ) -> Result<Self, String> {
        let name = format!("job-{job_id}");
        let places = Places {
            here: here.join(&name),
            on_host: on_host.join(&name),
        };
        // Refused rather than reused: whatever is in there is from a previous
        // attempt, and judging against somebody else's leftovers is worse than
        // failing.
        let _ = std::fs::remove_dir_all(&places.here);
        std::fs::create_dir_all(&places.here).map_err(|e| e.to_string())?;
        Ok(Self { places })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.places.here) {
            tracing::warn!(path = %self.places.here.display(), %e, "scratch was left behind");
        }
    }
}

/// Uploads what a participant and a manager read beside the verdict.
///
/// Two conventional names on an attempt: `log`, the build and sandbox output,
/// and `details`, the per-test table. Who may read either is the **activity's**
/// answer, one row per name, and a name the activity does not list is
/// managers-only — so attaching something new does not publish it by arriving.
///
/// Losing an attachment is worth a warning and not worth failing over: the
/// verdict is still the useful part, and a report that never lands because a
/// log did not upload is a job that has to be judged again.
async fn attach(server: &Server, job: &ClaimedJob, attachments: Attachments) {
    let mut named: Vec<(&str, &str, Vec<u8>)> = Vec::new();
    if !attachments.log.trim().is_empty() {
        named.push(("log", "text/plain", attachments.log.into_bytes()));
    }
    if let Some(details) = attachments.details {
        named.push(("details", "application/json", details));
    }

    for (name, mime, bytes) in named {
        let uploaded = match server.upload(&format!("{name}.txt"), mime, bytes).await {
            Ok(uploaded) => uploaded,
            Err(e) => {
                tracing::warn!(name, %e, "an attachment could not be uploaded");
                continue;
            }
        };

        if let Err(e) = server
            .attach_to_job(
                &job.job_id,
                &AttachToJob {
                    lease_token: job.lease_token.clone(),
                    file_id: uploaded.id,
                    name: name.to_owned(),
                },
            )
            .await
        {
            tracing::warn!(name, %e, "an attachment could not be named on the attempt");
        }
    }
}

/// How long an answer already computed is worth carrying.
///
/// **Bounded by the lease, not by a count.** Five fixed attempts five seconds
/// apart gave up after twenty-odd seconds, which is shorter than any restart and
/// far shorter than a backup — so an evaluation that had already finished was
/// thrown away and the submission waited out a ten-minute lease to be judged
/// again from nothing.
///
/// The bound that means something is the lease this Runner asked for: once it
/// has certainly elapsed, another Runner may hold the job, and this answer is no
/// longer wanted. Sending past it is not dangerous — the Server refuses it as a
/// stale lease — but holding a Runner off the queue for an answer nobody wants
/// is.
struct KeepTrying {
    backoff: Backoff,
    until: tokio::time::Instant,
}

impl KeepTrying {
    fn for_a_lease(config: &Config) -> Self {
        Self {
            // From five seconds rather than from the poll floor: this is not
            // polling an empty queue, it is a Server that just refused, and a
            // one-second retry against one that is deliberately down is noise.
            backoff: Backoff::new(FIVE, THIRTY),
            until: tokio::time::Instant::now()
                + Duration::from_secs(u64::from(config.lease_seconds)),
        }
    }

    /// How long to wait before sending again, or `None` when it is time to stop.
    fn or_give_up(&mut self, e: &aj_protocol::Error) -> Option<Duration> {
        if !e.retryable() {
            return None;
        }
        let delay = how_long(e, &mut self.backoff);
        (tokio::time::Instant::now() + delay < self.until).then_some(delay)
    }
}

/// Reporting is idempotent on the lease token, so resending after a dropped
/// connection is safe — and it is the only way a Runner that computed an answer
/// and then lost the network does not throw that work away.
async fn report_with_retries(
    server: &Server,
    job: &ClaimedJob,
    report: &ReportResult,
    config: &Config,
) {
    let mut keep = KeepTrying::for_a_lease(config);

    loop {
        match server.report(&job.job_id, report).await {
            Ok(accepted) => {
                tracing::info!(
                    job = %job.job_id,
                    result = %accepted.result_id,
                    state = %accepted.state,
                    duplicate = accepted.duplicate,
                    "reported",
                );
                return;
            }
            Err(e) if e.lease_lost() => {
                // Somebody else has the job now. Pushing this would overwrite a
                // newer attempt with an older answer.
                tracing::warn!(job = %job.job_id, %e, "the lease was gone; the work is dropped");
                return;
            }
            Err(e) => match keep.or_give_up(&e) {
                Some(delay) => {
                    tracing::warn!(
                        job = %job.job_id,
                        %e,
                        ?delay,
                        maintenance = e.in_maintenance(),
                        "the report did not land; holding on to it",
                    );
                    tokio::time::sleep(delay).await;
                }
                None => {
                    tracing::error!(
                        job = %job.job_id,
                        %e,
                        "the report could not be delivered; the lease will requeue the job",
                    );
                    return;
                }
            },
        }
    }
}

/// Host facts for the panel.
///
/// **Stored opaquely, but not shown opaquely.** The Server keeps this as a
/// string it never parses, and then projects it to a manager through a closed
/// shape — `os`, `cpu`, `cores`, `memoryBytes` (`Api/Contracts/ManagerPanel.cs`).
/// Anything else is stored and then silently dropped on the way out, with no
/// error to say so: a first version of this reported `arch` and it simply never
/// appeared. So the architecture goes in `cpu`, which is the field that can
/// actually be read.
fn machine() -> serde_json::Value {
    serde_json::json!({
        "os": std::env::consts::OS,
        "cpu": std::env::consts::ARCH,
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "memoryBytes": total_memory_bytes(),
    })
}

/// Total memory, from `/proc/meminfo`, in mebibytes.
///
/// `MemTotal` is stated in kibibytes. Absent rather than zero where it cannot
/// be read — a zero would show in the panel as a machine with no memory.
fn total_memory_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse::<u64>().ok())
        // `MemTotal` is stated in kibibytes; everything here is bytes.
        .map(|kib| kib * 1024)
}

const FIVE: Duration = Duration::from_secs(5);
const THIRTY: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    use aj_protocol::error::Refusal;

    fn refused(status: u16, code: &str, retry_after: Option<Duration>) -> aj_protocol::Error {
        aj_protocol::Error::Refused {
            status,
            refusal: Box::new(Refusal {
                code: Some(code.to_owned()),
                ..Default::default()
            }),
            retry_after,
        }
    }

    /// **The rule a participant would feel.**
    ///
    /// A package that could not be downloaded because an operator started a
    /// backup is not a submission that failed. Before this split existed every
    /// error inside `judge` became one string and every string became an
    /// infrastructure failure written against the attempt — so the window closed
    /// somebody's submission for them.
    #[test]
    fn a_server_that_went_away_is_not_something_to_report() {
        assert!(matches!(
            Trouble::from(refused(503, "server.maintenance", None)),
            Trouble::Away(_),
        ));
        // A proxy in front of a Server that is simply gone reads the same way.
        assert!(matches!(
            Trouble::from(refused(504, "", None)),
            Trouble::Away(_),
        ));
        // And so does a rate limit, which used to end the process.
        assert!(matches!(
            Trouble::from(refused(429, "", None)),
            Trouble::Away(_),
        ));

        // What did arrive and was wrong stays ours to report. Waiting would not
        // fix it, and staying silent would leave the attempt hanging until its
        // lease ran out for no reason.
        assert!(matches!(
            Trouble::from(aj_protocol::Error::ChecksumMismatch {
                what: "package.zip".into(),
                expected: "aa".into(),
                actual: "bb".into(),
            }),
            Trouble::Machinery(_),
        ));
        assert!(matches!(
            Trouble::from(refused(409, "job.state", None)),
            Trouble::Machinery(_),
        ));
    }

    #[test]
    fn the_servers_own_wait_is_honoured_only_where_it_asks_for_longer() {
        let mut backoff = Backoff::new(FIVE, THIRTY);
        assert_eq!(
            how_long(
                &refused(503, "", Some(Duration::from_secs(300))),
                &mut backoff
            ),
            Duration::from_secs(300),
            "an operator asking for five minutes is taken at their word",
        );

        // **A proxy answering `Retry-After: 0` must not turn a retry into a
        // spin** against a Server that is deliberately trying to be down.
        let mut backoff = Backoff::new(FIVE, THIRTY);
        let floor = how_long(&refused(503, "", Some(Duration::ZERO)), &mut backoff);
        assert!(
            floor >= Duration::from_secs(4),
            "{floor:?} is faster than this Runner's own floor",
        );

        // And with nothing asked for, the backoff decides on its own.
        let mut backoff = Backoff::new(FIVE, THIRTY);
        assert!(how_long(&refused(503, "", None), &mut backoff) >= Duration::from_secs(4));
    }

    /// Nothing computed is thrown away inside a window, and nothing is held
    /// past the point where somebody else may already have the job.
    #[test]
    fn an_answer_is_carried_for_the_length_of_a_lease_and_no_longer() {
        let config = lease_of(600);
        let mut keep = KeepTrying::for_a_lease(&config);

        // The first refusals are worth waiting through — this is the case that
        // used to give up after five attempts, about twenty seconds, which is
        // shorter than any restart.
        for _ in 0..6 {
            assert!(
                keep.or_give_up(&refused(503, "server.maintenance", None))
                    .is_some(),
                "an answer already computed is worth holding on to",
            );
        }

        // A refusal the Server means is not waited on at all.
        assert!(keep
            .or_give_up(&refused(403, "runner.revoked", None))
            .is_none());

        // And once the lease has certainly gone, so has the point.
        let mut expired = KeepTrying::for_a_lease(&lease_of(0));
        assert!(
            expired
                .or_give_up(&refused(503, "server.maintenance", None))
                .is_none(),
            "holding a Runner off the queue for an answer nobody wants",
        );
    }

    fn lease_of(seconds: u32) -> Config {
        Config {
            base_url: "http://server/api/v1".into(),
            name: "test".into(),
            key_path: "/dev/null".into(),
            problem_types: vec!["standard-io@1".into()],
            poll_min: FIVE,
            poll_max: THIRTY,
            heartbeat: THIRTY,
            lease_seconds: seconds,
            cache_path: "/dev/null".into(),
            cache_max_bytes: 0,
            work_path: "/dev/null".into(),
            work_host_path: "/dev/null".into(),
            images: aj_standard_io::Images {
                cpp: String::new(),
                python: String::new(),
            },
            allow_cgroup_v1: false,
        }
    }

    /// **Two jobs share no mounted path.**
    ///
    /// The property, asserted directly rather than through the syscall that
    /// would exploit its absence. File locks pass between processes holding one
    /// inode across mount-namespace boundaries — demonstrated on 2026-08-09 —
    /// and denying `flock` would close that one road while leaving POSIX record
    /// locks, `F_NOTIFY` and anything else two processes can do to one inode.
    ///
    /// What actually protects us is that there is no shared inode to hold. So
    /// that is what is pinned here: every path a sandbox mounts comes from
    /// `job.work` or `job.package`, `Scratch` is per job, and the package is
    /// unpacked inside it.
    #[test]
    fn two_jobs_mount_nothing_in_common() {
        let root = std::path::Path::new("/var/lib/algojudge");
        let host = std::path::Path::new("/host/algojudge");

        let one = Scratch::new(root, host, "job-one").expect("scratch");
        let two = Scratch::new(root, host, "job-two").expect("scratch");

        assert_ne!(one.places.on_host, two.places.on_host, "scratch is per job");

        // As the daemon resolves them: a bind mount is resolved on the host, so
        // two jobs sharing an inode would share it through `on_host` whatever
        // this process sees.
        let first = unpacked_into(&one.places).on_host;
        let second = unpacked_into(&two.places).on_host;

        assert!(
            first.starts_with(&one.places.on_host),
            "a package unpacked outside its job's scratch is shared with every other job: {first:?}",
        );
        assert!(second.starts_with(&two.places.on_host));
        assert!(
            !first.starts_with(&second) && !second.starts_with(&first),
            "two jobs' packages must not nest: {first:?} and {second:?}",
        );
        assert_ne!(first, second);
    }
}
