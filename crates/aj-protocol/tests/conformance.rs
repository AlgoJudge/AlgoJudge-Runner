//! The accepted specification, exercised **with this Runner as the client**.
//!
//! `AlgoJudge.Server.Tests/RunnerConformanceTests.cs` proves the Server keeps
//! the contract; it does so against a stub written in the same language, in the
//! same process, by the same person. That is worth having and is not the same
//! claim as "a second implementation passes". This file is the second
//! implementation.
//!
//! Every test here needs a running Server, so every one is `#[ignore]`d:
//!
//! ```text
//! docker compose -f example-runner-development-docker-compose.yaml up -d --build --wait
//! ./x test --test conformance -- --include-ignored --test-threads=1
//! ```
//!
//! Serial by requirement rather than by habit — several of these drain the
//! queue or read a submission's state, and two running at once would take each
//! other's jobs.

use std::sync::Arc;
use std::time::Duration;

use aj_protocol::wire::{AttachToJob, Register, ReportResult};
use aj_protocol::{Cache, Identity, Server};

/// The development stack's Server. `AJ_TEST_SERVER` points these somewhere else.
fn base_url() -> String {
    std::env::var("AJ_TEST_SERVER").unwrap_or_else(|_| "http://localhost:8080/api/v1".into())
}

/// A fresh identity in a directory nothing else uses.
///
/// Per test, because a Runner is its key: sharing one would mean the second
/// test inherits the first's approval and stops proving that approval is
/// required.
fn identity(name: &str) -> Identity {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "aj-conformance-{name}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    path.push("identity.key");
    Identity::load_or_create(path).expect("a scratch identity")
}

fn registration(identity: &Identity, name: &str) -> Register {
    Register {
        name: name.into(),
        product: "AlgoJudge-Runner-Conformance".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        public_key: identity.public_key(),
        external: false,
        problem_types: vec!["standard-io@1".into()],
        tags: vec![],
        machine: None,
    }
}

/// The manager half. Approving a Runner is an ordinary session-authenticated
/// act — a Runner token authorizes nothing a manager may do, by design.
struct Administrator {
    http: reqwest::Client,
    base: String,
}

impl Administrator {
    async fn sign_in() -> Self {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .expect("a client");
        let base = base_url();

        let response = http
            .post(format!("{base}/identity/login?useSessionCookies=true"))
            .json(&serde_json::json!({
                "email": "admin",
                "password": "admin-development-only",
            }))
            .send()
            .await
            .expect("the Server is up — is the compose stack running?");
        assert!(
            response.status().is_success(),
            "the development administrator could not sign in: {}",
            response.status(),
        );

        Self { http, base }
    }

    async fn approve(&self, runner_id: &str) {
        let response = self
            .http
            .post(format!("{}/runners/{runner_id}/approve", self.base))
            .send()
            .await
            .expect("approval");
        assert!(
            response.status().is_success(),
            "approving {runner_id} answered {}",
            response.status(),
        );
    }
}

/// A registered, approved Runner holding a token — the state every clause after
/// §4 assumes.
async fn approved(name: &str) -> (Server, Identity) {
    let identity = identity(name);
    let server = Server::new(&base_url()).expect("a client");

    let registered = server
        .register(&registration(&identity, name))
        .await
        .expect("registration");

    Administrator::sign_in()
        .await
        .approve(&registered.runner_id)
        .await;
    server.authenticate(&identity).await.expect("the handshake");

    (server, identity)
}

// ── §3 registration ─────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "needs a Server"]
async fn a_key_that_is_not_thirty_two_bytes_is_refused() {
    let server = Server::new(&base_url()).unwrap();

    for (public_key, expected) in [
        ("dG9vLXNob3J0", "runner.key.length"),
        ("not base64 at all !!", "runner.key.malformed"),
    ] {
        let error = server
            .register(&Register {
                name: "bad-key".into(),
                product: "AlgoJudge-Runner-Conformance".into(),
                version: "0.0.1".into(),
                public_key: public_key.into(),
                external: false,
                problem_types: vec!["standard-io@1".into()],
                tags: vec![],
                machine: None,
            })
            .await
            .expect_err("a key that is not an Ed25519 key must not be accepted");

        assert_eq!(error.status(), Some(422));
        assert_eq!(error.code(), expected);
    }
}

/// **The key this Runner publishes is the raw 32 bytes.** An SPKI wrapper is 44
/// and would be refused — which is the single easiest way to build a Runner
/// that cannot register and cannot say why.
#[tokio::test]
#[ignore = "needs a Server"]
async fn the_server_and_the_runner_name_the_key_the_same_way() {
    let identity = identity("fingerprint");
    let server = Server::new(&base_url()).unwrap();

    let registered = server
        .register(&registration(&identity, "fingerprint"))
        .await
        .expect("registration");

    assert_eq!(registered.fingerprint, identity.fingerprint());
    assert_eq!(registered.state, "pendingApproval");
}

/// Registering again is how a Runner reports a restart: same identity, updated
/// capabilities, and **not** a way to shed an approval decision.
#[tokio::test]
#[ignore = "needs a Server"]
async fn registering_again_is_the_same_runner() {
    let identity = identity("restart");
    let server = Server::new(&base_url()).unwrap();

    let first = server
        .register(&registration(&identity, "restart"))
        .await
        .expect("registration");
    Administrator::sign_in()
        .await
        .approve(&first.runner_id)
        .await;

    let second = server
        .register(&registration(&identity, "restart-renamed"))
        .await
        .expect("re-registration");

    assert_eq!(first.runner_id, second.runner_id);
    assert_eq!(second.state, "approved", "a restart does not undo approval");
}

// ── §4 authentication ───────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "needs a Server"]
async fn a_registered_but_unapproved_runner_is_refused_a_token() {
    let identity = identity("unapproved");
    let server = Server::new(&base_url()).unwrap();

    server
        .register(&registration(&identity, "unapproved"))
        .await
        .expect("registration");

    let error = server
        .authenticate(&identity)
        .await
        .expect_err("nothing is evaluated before an administrator approves the key");

    assert_eq!(error.code(), "runner.notApproved");
    assert!(
        error.not_approved(),
        "the Runner has to be able to tell this apart from a failure"
    );
    assert!(
        !error.retryable(),
        "waiting is right; retrying immediately is not"
    );
}

/// A nonce is spent by being used. Proven the only way it can be — by replaying
/// a whole exchange that worked a moment ago.
#[tokio::test]
#[ignore = "needs a Server"]
async fn a_nonce_is_single_use() {
    let identity = identity("replay");
    let server = Server::new(&base_url()).unwrap();
    let base = base_url();

    let registered = server
        .register(&registration(&identity, "replay"))
        .await
        .expect("registration");
    Administrator::sign_in()
        .await
        .approve(&registered.runner_id)
        .await;

    // The raw calls, because the client deliberately offers no way to hold a
    // nonce and use it twice.
    let http = reqwest::Client::new();
    let challenge: serde_json::Value = http
        .post(format!("{base}/runner/auth/challenge"))
        .json(&serde_json::json!({ "fingerprint": identity.fingerprint() }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let nonce = challenge["nonce"].as_str().expect("a nonce").to_owned();
    let body = serde_json::json!({
        "fingerprint": identity.fingerprint(),
        "nonce": nonce,
        "signature": identity.sign(&nonce),
    });

    let first = http
        .post(format!("{base}/runner/auth/token"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(first.status().is_success(), "the exchange must work once");

    let replay = http
        .post(format!("{base}/runner/auth/token"))
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(replay.status().as_u16(), 403);
    let refusal: serde_json::Value = replay.json().await.unwrap();
    assert_eq!(refusal["code"], "runner.nonce.unknown");
}

/// Holding a fingerprint is not holding the key. The fingerprint is public —
/// the Server shows it in its own panel — so this signature check is the whole
/// of what stops anybody who read one from getting a token.
#[tokio::test]
#[ignore = "needs a Server"]
async fn a_signature_from_the_wrong_key_is_refused() {
    let genuine = identity("impostor");
    let impostor = identity("impostor-other");
    let base = base_url();
    let server = Server::new(&base).unwrap();

    let registered = server
        .register(&registration(&genuine, "impostor"))
        .await
        .expect("registration");
    Administrator::sign_in()
        .await
        .approve(&registered.runner_id)
        .await;

    let http = reqwest::Client::new();
    let challenge: serde_json::Value = http
        .post(format!("{base}/runner/auth/challenge"))
        .json(&serde_json::json!({ "fingerprint": genuine.fingerprint() }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let nonce = challenge["nonce"].as_str().unwrap();

    let response = http
        .post(format!("{base}/runner/auth/token"))
        .json(&serde_json::json!({
            "fingerprint": genuine.fingerprint(),
            "nonce": nonce,
            // Somebody else's key over the right nonce.
            "signature": impostor.sign(nonce),
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status().as_u16(), 403);
    let refusal: serde_json::Value = response.json().await.unwrap();
    assert_eq!(refusal["code"], "runner.signature");
}

// ── §5 claiming ─────────────────────────────────────────────────────────────

/// An empty queue is `204`, and this client reports it as `None` rather than as
/// an error — the distinction the whole polling loop rests on.
#[tokio::test]
#[ignore = "needs a Server"]
async fn an_empty_queue_is_not_an_error() {
    let (server, _identity) = approved("empty-queue").await;

    for _ in 0..50 {
        match server.claim(Some(60)).await.expect("claiming") {
            None => return,
            // Drain whatever else is queued, then ask again.
            Some(job) => {
                let _ = server
                    .report(
                        &job.job_id,
                        &ReportResult::failed(&job.lease_token, "drained by a conformance test"),
                    )
                    .await;
            }
        }
    }

    panic!("the queue never emptied, so the 204 path was never reached");
}

/// The Server clamps `leaseSeconds`. A Runner that renewed on its own
/// arithmetic would be renewing against a deadline nobody agreed to, so the
/// answer carries the deadline and that is what must be believed.
#[tokio::test]
#[ignore = "needs a Server"]
async fn the_lease_deadline_comes_from_the_server_not_from_the_request() {
    let (server, _identity) = approved("lease-clamp").await;
    let participant = Participant::enrolled().await;
    participant.submit("print('lease')\n").await;

    let job = claim_ours(&server).await;

    assert!(
        !job.lease_expires_at.is_empty(),
        "a claim must carry the deadline the Server granted",
    );

    // **Renewing never shortens.** This test originally asserted the opposite —
    // that renewal only ever moves the deadline forward — and failed, because
    // the Server replaced the deadline outright: claimed for ten minutes,
    // renewed asking for one second (clamped to the sixty-second floor), and
    // the deadline moved eight minutes *closer*. The Server was changed on
    // 2026-08-09 to keep the later of the two, because a call named "renew"
    // must not be able to hand a job away from the Runner that is working on
    // it.
    let renewed = server
        .renew(&job.job_id, &job.lease_token, Some(1))
        .await
        .expect("renewal");
    assert_eq!(renewed.lease_token, job.lease_token);
    // Compared to the millisecond rather than exactly. PostgreSQL stores
    // microseconds and the Server's clock keeps hundred-nanosecond ticks, so the
    // deadline a claim returns straight from memory has more precision than the
    // same deadline read back afterwards — the tail differs by a couple of
    // hundred nanoseconds with nothing having moved.
    assert!(
        to_millis(&renewed.lease_expires_at) >= to_millis(&job.lease_expires_at),
        "renewing moved the deadline from {} back to {}",
        job.lease_expires_at,
        renewed.lease_expires_at,
    );

    // And asking for more than is held does move it out.
    let extended = server
        .renew(&job.job_id, &job.lease_token, Some(3600))
        .await
        .expect("renewal");
    assert!(extended.lease_expires_at > renewed.lease_expires_at);

    let _ = server
        .report(
            &job.job_id,
            &ReportResult::failed(&job.lease_token, "lease test"),
        )
        .await;
}

/// A lease this Runner does not hold is refused, and the client says so in a
/// way the loop can act on: drop the work rather than push it.
#[tokio::test]
#[ignore = "needs a Server"]
async fn a_lease_that_is_not_held_is_refused() {
    let (server, _identity) = approved("stale-lease").await;
    let participant = Participant::enrolled().await;
    participant.submit("print('stale')\n").await;

    let job = claim_ours(&server).await;

    let error = server
        .report(
            &job.job_id,
            &ReportResult::judged(
                "00000000-0000-0000-0000-000000000000",
                100.0,
                100.0,
                "Accepted",
            ),
        )
        .await
        .expect_err("a token nobody issued must not be able to write a verdict");

    assert!(error.lease_lost(), "got {} instead", error.code());

    let _ = server
        .report(
            &job.job_id,
            &ReportResult::failed(&job.lease_token, "stale-lease test"),
        )
        .await;
}

// ── §6 reporting ────────────────────────────────────────────────────────────

/// The property that lets a Runner resend after losing the network instead of
/// throwing away work it already did.
#[tokio::test]
#[ignore = "needs a Server"]
async fn reporting_twice_answers_the_same_result_and_says_it_is_a_repeat() {
    let (server, _identity) = approved("idempotent").await;
    let participant = Participant::enrolled().await;
    participant.submit("print('idempotent')\n").await;

    let job = claim_ours(&server).await;
    let report = ReportResult::judged(&job.lease_token, 100.0, 100.0, "Accepted");

    let first = server
        .report(&job.job_id, &report)
        .await
        .expect("the report");
    let again = server
        .report(&job.job_id, &report)
        .await
        .expect("the repeat");

    assert_eq!(first.result_id, again.result_id);
    assert!(!first.duplicate);
    assert!(
        again.duplicate,
        "a repeat has to say so, or a Runner cannot tell it from a first report",
    );
}

/// An infrastructure failure carries **no score**. A zero would read as a wrong
/// answer on every board that shows it, about a submission that was never
/// judged.
#[tokio::test]
#[ignore = "needs a Server"]
async fn an_infrastructure_failure_carries_no_score() {
    let (server, _identity) = approved("infrastructure").await;
    let participant = Participant::enrolled().await;
    participant.submit("print('infrastructure')\n").await;

    let job = claim_ours(&server).await;

    let accepted = server
        .report(
            &job.job_id,
            &ReportResult::failed(&job.lease_token, "package checksum mismatch"),
        )
        .await
        .expect("the report");

    // **The first one is a reason to try again, not a verdict** (§6): a Runner
    // cannot tell a broken package from a torn download. Nothing is stored, so
    // nothing is named.
    assert_eq!(accepted.state, "queued");
    assert!(accepted.result_id.is_none(), "{:?}", accepted.result_id);
    assert!(!accepted.duplicate);

    // It stops travelling once the deliveries run out, and only then is there a
    // result — which still carries no score, because a failure is not an answer.
    let mut last = accepted;
    for _ in 0..8 {
        if last.state == "failed" {
            break;
        }
        let again = claim_ours(&server).await;
        last = server
            .report(
                &again.job_id,
                &ReportResult::failed(&again.lease_token, "package checksum mismatch"),
            )
            .await
            .expect("the report");
    }
    assert_eq!(last.state, "failed", "the deliveries never ran out");

    let submission = participant.submission(&job.submission_id).await;
    assert!(
        submission["result"].get("score").is_none() || submission["result"]["score"].is_null(),
        "a failure must not be scored: {submission}",
    );
}

/// **A Runner being stopped gives the job back, and it is claimable at once.**
/// §5.1. The alternative is going quiet and letting the lease expire, which
/// costs a participant ten minutes while idle Runners wait for a deadline
/// nobody is going to miss.
#[tokio::test]
#[ignore = "needs a Server"]
async fn a_runner_that_is_stopping_gives_the_job_back() {
    let (server, _identity) = approved("release").await;
    let participant = Participant::enrolled().await;
    participant.submit("print('release')\n").await;

    let job = claim_ours(&server).await;

    server
        .release(&job.job_id, &job.lease_token)
        .await
        .expect("the release");

    // Back in the queue this instant, and it is the same job: nothing about it
    // was wrong and nothing was recorded against the submission.
    let again = claim_ours(&server).await;
    assert_eq!(again.job_id, job.job_id);
    assert_ne!(
        again.lease_token, job.lease_token,
        "a new lease, not the old"
    );

    // And the lease it was let go of is gone, so a late renewal from the Runner
    // that stopped cannot reach past the one now holding it.
    let stale = server.renew(&job.job_id, &job.lease_token, None).await;
    assert!(stale.is_err(), "the old lease still renewed");
}

// ── §7 files ────────────────────────────────────────────────────────────────

/// Upload, attach, **then** report. The Server requires the job to be
/// `Running`, and reporting ends that — so getting the order wrong loses the
/// log that explains the failure.
#[tokio::test]
#[ignore = "needs a Server"]
async fn a_log_attaches_before_the_report_and_not_after() {
    let (server, _identity) = approved("attach").await;
    let participant = Participant::enrolled().await;
    participant.submit("print('attach')\n").await;

    let job = claim_ours(&server).await;

    let uploaded = server
        .upload("log.txt", "text/plain", b"compiled nothing\n".to_vec())
        .await
        .expect("the upload");

    server
        .attach_to_job(
            &job.job_id,
            &AttachToJob {
                lease_token: job.lease_token.clone(),
                file_id: uploaded.id.clone(),
                name: "log".into(),
            },
        )
        .await
        .expect("attaching while the job is still running");

    server
        .report(
            &job.job_id,
            &ReportResult::judged(&job.lease_token, 100.0, 100.0, "Accepted"),
        )
        .await
        .expect("the report");

    // And now the same attachment is refused, which is why the order is not a
    // preference.
    let late = server
        .attach_to_job(
            &job.job_id,
            &AttachToJob {
                lease_token: job.lease_token.clone(),
                file_id: uploaded.id,
                name: "details".into(),
            },
        )
        .await
        .expect_err("a finished job takes no more attachments");
    assert!(late.lease_lost(), "got {} instead", late.code());
}

/// The package is verified **before** it is evaluated, and a mismatch is an
/// infrastructure failure rather than a verdict. Forced here by claiming the
/// package under a checksum it does not have.
#[tokio::test]
#[ignore = "needs a Server"]
async fn a_package_that_does_not_verify_is_refused_and_the_entry_is_discarded() {
    let (server, _identity) = approved("checksum").await;
    let participant = Participant::enrolled().await;
    participant.submit("print('checksum')\n").await;

    let job = claim_ours(&server).await;
    assert!(job.has_package(), "the development seed attaches one");

    let root = std::env::temp_dir().join(format!("aj-cache-conformance-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cache = Arc::new(Cache::new(&root, 1 << 30, "test-conformance"));

    let wrong = "0".repeat(64);
    let error = cache
        .fetch(&server, &job.package_file_id, &wrong)
        .await
        .expect_err("bytes that do not hash to what the job promised must not be evaluated");

    assert!(
        matches!(error, aj_protocol::Error::ChecksumMismatch { .. }),
        "got {error}",
    );
    assert_eq!(
        walk(&root).len(),
        0,
        "the corrupt entry has to go before the failure is reported, or a retry reads it again",
    );

    // The real checksum works, and lands in the cache.
    let entry = cache
        .fetch(&server, &job.package_file_id, &job.package_sha256)
        .await
        .expect("the package the job actually names");
    assert!(entry.path().exists());

    let _ = server
        .report(
            &job.job_id,
            &ReportResult::failed(&job.lease_token, "checksum test"),
        )
        .await;
}

// ── §8 heartbeat ────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "needs a Server"]
async fn a_heartbeat_is_accepted() {
    let (server, _identity) = approved("heartbeat").await;
    server.heartbeat().await.expect("the heartbeat");
}

// ── The participant half ────────────────────────────────────────────────────

/// The development seed's student, enrolled in the development activity.
///
/// Using the seeded activity rather than building one keeps this file about the
/// Runner's side of the contract. `DEV-2026` has an open round, one assignment
/// `A`, and a package with a real checksum.
struct Participant {
    http: reqwest::Client,
    base: String,
}

const ACTIVITY: &str = "DEV-2026";

impl Participant {
    async fn enrolled() -> Self {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .unwrap();
        let base = base_url();

        let response = http
            .post(format!("{base}/identity/login?useSessionCookies=true"))
            .json(&serde_json::json!({
                "email": "student",
                "password": "student-development-only",
            }))
            .send()
            .await
            .expect("the Server is up");
        assert!(
            response.status().is_success(),
            "the student could not sign in"
        );

        // Already enrolled from an earlier run is not a failure.
        let _ = http
            .post(format!("{base}/activities/{ACTIVITY}/enrolment"))
            .json(&serde_json::json!({}))
            .send()
            .await;

        Self { http, base }
    }

    async fn submit(&self, source: &str) -> String {
        use sha2::{Digest, Sha256};
        let checksum = hex::encode(Sha256::digest(source.as_bytes()));

        // One opaque document, and a file name. The language was a field the
        // Server read; it is a member of `props` now, and pasted source is named
        // by the sender because the Server has no extension table any more.
        let form = reqwest::multipart::Form::new()
            .text("props", r#"{"type":"standard-io@1","language":"python"}"#)
            .text("code", source.to_owned())
            .text("fileName", "main.py")
            .text("sha256", checksum);

        let response = self
            .http
            .post(format!(
                "{}/activities/{ACTIVITY}/problems/A/submissions",
                self.base
            ))
            .multipart(form)
            .send()
            .await
            .expect("the submission");
        // **The body, not only the status.** A refusal carries a `code` naming
        // which rule it was, and reading the status alone turns three different
        // causes into one number nobody can act on.
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        assert!(status.is_success(), "submitting answered {status}: {text}");

        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        body["id"].as_str().expect("a submission id").to_owned()
    }

    async fn submission(&self, id: &str) -> serde_json::Value {
        self.http
            .get(format!(
                "{}/activities/{ACTIVITY}/submissions/{id}",
                self.base
            ))
            .send()
            .await
            .expect("the submission")
            .json()
            .await
            .expect("a submission document")
    }
}

/// Claims until a job turns up, since another test may hold the queue for a
/// moment. Whatever comes back is ours to report on.
async fn claim_ours(server: &Server) -> aj_protocol::wire::ClaimedJob {
    for _ in 0..60 {
        if let Some(job) = server.claim(Some(600)).await.expect("claiming") {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("nothing was queued within fifteen seconds — did the submission fail?");
}

/// An ISO-8601 instant cut to the millisecond, so two of them compare on what
/// both sides actually agree about. Lexicographic order is date order for this
/// format, which is why these stay strings.
fn to_millis(at: &str) -> String {
    match at.find('.') {
        Some(dot) => at.chars().take(dot + 4).collect(),
        None => at.to_owned(),
    }
}

fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}
