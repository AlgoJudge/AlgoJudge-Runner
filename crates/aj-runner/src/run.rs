//! The two loops: getting admitted, and doing the work.

use std::sync::Arc;
use std::time::Duration;

use aj_protocol::wire::{AttachToJob, ClaimedJob, Register, ReportResult};
use aj_protocol::{Backoff, Cache, Error, Identity, Server};

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
            Err(e) if e.retryable() => {
                pause(&format!("the Server is not answering ({e})"), FIVE).await
            }
            Err(e) => anyhow::bail!("registration was refused: {e}"),
        }
    }

    loop {
        match server.authenticate(identity).await {
            Ok(()) => return Ok(()),
            Err(e) if e.not_approved() => {
                pause("this Runner has not been approved yet", THIRTY).await;
            }
            Err(e) if e.revoked() => anyhow::bail!("this key has been revoked: {e}"),
            Err(e) if e.retryable() => pause(&format!("the handshake failed ({e})"), FIVE).await,
            Err(e) => anyhow::bail!("the handshake was refused: {e}"),
        }
    }
}

/// Claim, evaluate, report. For ever.
pub async fn work(server: &Arc<Server>, cache: &Arc<Cache>, config: &Config) -> anyhow::Result<()> {
    let mut backoff = Backoff::new(config.poll_min, config.poll_max);
    let mut last_beat = tokio::time::Instant::now();

    loop {
        match server.claim(Some(config.lease_seconds)).await {
            Ok(Some(job)) => {
                backoff.reset();
                handle(server, cache, job).await;
            }
            // An empty queue is the ordinary state of a Runner, not a fault.
            Ok(None) => {
                if last_beat.elapsed() >= config.heartbeat {
                    if let Err(e) = server.heartbeat().await {
                        tracing::warn!(%e, "the heartbeat did not land");
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
            Err(e) if e.retryable() => {
                tracing::warn!(%e, "could not ask for work");
                backoff.wait().await;
            }
            Err(e) => anyhow::bail!("claiming was refused: {e}"),
        }
    }
}

async fn handle(server: &Arc<Server>, cache: &Arc<Cache>, job: ClaimedJob) {
    tracing::info!(
        job = %job.job_id,
        submission = %job.submission_id,
        attempt = job.attempt,
        problem_type = %job.problem_type,
        lease_expires_at = %job.lease_expires_at,
        "claimed",
    );

    let (report, log) = evaluate(server, cache, &job).await;

    // Upload, attach, **then** report — the Server requires the job to be
    // `Running` to accept an attachment, and reporting ends that. Get the order
    // wrong and the log explaining a failure is the thing that goes missing.
    attach_log(server, &job, log).await;

    report_with_retries(server, &job, &report).await;
}

/// What this becomes in M1. Today it downloads, verifies, and makes the answer
/// up.
async fn evaluate(server: &Server, cache: &Arc<Cache>, job: &ClaimedJob) -> (ReportResult, String) {
    if !job.has_package() {
        // Empty strings, not absent — there is nothing to judge against. Not
        // the solution's fault, so not a verdict.
        return (
            ReportResult::failed(&job.lease_token, "the problem version carries no package"),
            "no package was attached to this problem version\n".into(),
        );
    }

    match cache
        .fetch(server, &job.package_file_id, &job.package_sha256)
        .await
    {
        Ok(entry) => {
            let log = format!(
                "package {} verified at {}\nthis Runner does not evaluate yet; the verdict below is fabricated\n",
                job.package_sha256,
                entry.path().display(),
            );
            (
                ReportResult::judged(&job.lease_token, 100.0, 100.0, "Accepted"),
                log,
            )
        }
        Err(e @ Error::ChecksumMismatch { .. }) => {
            // A corrupt package says nothing about the solution. Scoring it
            // would be a fabricated verdict about somebody's work.
            tracing::error!(%e, "the package did not verify");
            (
                ReportResult::failed(&job.lease_token, e.to_string()),
                format!("{e}\n"),
            )
        }
        Err(e) => {
            tracing::error!(%e, "the package could not be fetched");
            (
                ReportResult::failed(
                    &job.lease_token,
                    format!("the package could not be fetched: {e}"),
                ),
                format!("{e}\n"),
            )
        }
    }
}

async fn attach_log(server: &Server, job: &ClaimedJob, log: String) {
    let uploaded = match server
        .upload("log.txt", "text/plain", log.into_bytes())
        .await
    {
        Ok(uploaded) => uploaded,
        Err(e) => {
            // Losing the log is worth saying and not worth failing over: the
            // verdict is still the useful part.
            tracing::warn!(%e, "the log could not be uploaded");
            return;
        }
    };

    if let Err(e) = server
        .attach_to_job(
            &job.job_id,
            &AttachToJob {
                lease_token: job.lease_token.clone(),
                file_id: uploaded.id,
                name: "log".into(),
            },
        )
        .await
    {
        tracing::warn!(%e, "the log could not be attached");
    }
}

/// Reporting is idempotent on the lease token, so resending after a dropped
/// connection is safe — and it is the only way a Runner that computed an answer
/// and then lost the network does not throw that work away.
async fn report_with_retries(server: &Server, job: &ClaimedJob, report: &ReportResult) {
    for attempt in 1..=REPORT_ATTEMPTS {
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
            Err(e) if e.retryable() && attempt < REPORT_ATTEMPTS => {
                tracing::warn!(%e, attempt, "the report did not land; sending it again");
                tokio::time::sleep(FIVE).await;
            }
            Err(e) => {
                tracing::error!(job = %job.job_id, %e, "the report was refused");
                return;
            }
        }
    }
}

/// Host facts for the panel.
///
/// **Stored opaquely, but not shown opaquely.** The Server keeps this as a
/// string it never parses, and then projects it to a manager through a closed
/// shape — `os`, `cpu`, `cores`, `memoryMb` (`Api/Contracts/ManagerPanel.cs`).
/// Anything else is stored and then silently dropped on the way out, with no
/// error to say so: a first version of this reported `arch` and it simply never
/// appeared. So the architecture goes in `cpu`, which is the field that can
/// actually be read.
fn machine() -> serde_json::Value {
    serde_json::json!({
        "os": std::env::consts::OS,
        "cpu": std::env::consts::ARCH,
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "memoryMb": total_memory_mb(),
    })
}

/// Total memory, from `/proc/meminfo`, in mebibytes.
///
/// `MemTotal` is stated in kibibytes. Absent rather than zero where it cannot
/// be read — a zero would show in the panel as a machine with no memory.
fn total_memory_mb() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse::<u64>().ok())
        .map(|kib| kib / 1024)
}

const FIVE: Duration = Duration::from_secs(5);
const THIRTY: Duration = Duration::from_secs(30);
const REPORT_ATTEMPTS: u32 = 5;
