//! A submission, judged by a Runner that nobody is helping.
//!
//! Everything else in this repository tests a part. This drives the **Server**
//! over HTTP and never touches the Runner at all: a manager publishes a problem
//! with the committed package, a participant submits, and the Runner running in
//! the development stack claims the job, judges it and reports. What is asserted
//! is what the participant would read.
//!
//! ```text
//! docker build -t algojudge/lang-gcc:local    images/gcc
//! docker build -t algojudge/lang-clang:local  images/clang
//! docker build -t algojudge/lang-python:local images/python
//! docker build -t algojudge/lang-pypy:local   images/pypy
//! docker compose -f example-runner-development-docker-compose.yaml up -d --build --wait
//!
//! AJ_TEST_SERVER=http://host.docker.internal:8080/api/v1 \
//!   ./x test -p aj-runner --test end_to_end -- --include-ignored --test-threads=1
//! ```
//!
//! **One of them needs a different network.** The maintenance case has to reach
//! the Server's own loopback interface, because that is the whole of the
//! switch's authorization, so it shares the container's network namespace
//! instead of joining the stack's network:
//!
//! ```text
//! AJ_DOCKER_NETWORK=container:algojudge-runner-dev-server-1 \
//! AJ_TEST_SERVER=http://127.0.0.1:8080/api/v1 \
//!   ./x test -p aj-runner --test end_to_end -- --include-ignored --test-threads=1
//! ```
//!
//! Which is also why CI runs it as a step of its own.
//!
//! **Six outcomes, one run.** Five are verdicts and the sixth is not: an
//! infrastructure failure means the submission was never judged, and it is the
//! one that must never be scored as a wrong answer.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use aj_protocol::wire::ReportResult;
use aj_protocol::{Identity, Server};
use aj_runner::config::Config;
use aj_runner::keeper::Keeper;
use aj_runner::run;

fn api() -> String {
    std::env::var("AJ_TEST_SERVER").unwrap_or_else(|_| "http://localhost:8080/api/v1".into())
}

fn unique(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("{prefix}-{now}")
}

struct Session {
    http: reqwest::Client,
}

impl Session {
    async fn as_(login: &str, password: &str) -> Self {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .expect("a client");

        let response = http
            .post(format!("{}/identity/login?useSessionCookies=true", api()))
            .json(&json!({ "email": login, "password": password }))
            .send()
            .await
            .expect("the Server is up — is the development stack running?");
        assert!(
            response.status().is_success(),
            "{login} could not sign in: {}",
            response.status(),
        );

        Self { http }
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let response = self
            .http
            .post(format!("{}{path}", api()))
            .json(&body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("POST {path}: {e}"));

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        assert!(status.is_success(), "POST {path} answered {status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    async fn get(&self, path: &str) -> Value {
        let response = self
            .http
            .get(format!("{}{path}", api()))
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {path}: {e}"));

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        assert!(status.is_success(), "GET {path} answered {status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    /// Every byte the product stores travels this way, with a checksum the
    /// Server recomputes before it stores anything.
    async fn upload(&self, name: &str, mime: &str, bytes: Vec<u8>) -> String {
        let checksum = hex::encode(Sha256::digest(&bytes));

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(name.to_owned())
            .mime_str(mime)
            .expect("a media type");

        let response = self
            .http
            .post(format!("{}/files", api()))
            .multipart(
                reqwest::multipart::Form::new()
                    .part("file", part)
                    .text("sha256", checksum),
            )
            .send()
            .await
            .expect("the upload");

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "uploading {name} answered {status}: {text}"
        );

        serde_json::from_str::<Value>(&text).unwrap()["id"]
            .as_str()
            .expect("a file id")
            .to_owned()
    }

    /// Whether a path answers at all, without asserting that it does.
    async fn reaches(&self, path: &str) -> bool {
        self.http
            .get(format!("{}{path}", api()))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// A submission that is a **file** rather than source.
    ///
    /// The Server already takes either — "one of the two, never both: the form
    /// offers an editor or a file field, and which it offers is the problem
    /// type's business". That sentence was written before a second problem type
    /// existed, and it is why this needed nothing.
    async fn submit_file(&self, activity: &str, name: &str, bytes: Vec<u8>) -> String {
        let checksum = hex::encode(Sha256::digest(&bytes));

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(name.to_owned())
            .mime_str("application/zip")
            .expect("a media type");

        let response = self
            .http
            .post(format!(
                "{}/activities/{activity}/problems/A/submissions",
                api()
            ))
            .multipart(
                reqwest::multipart::Form::new()
                    .part("file", part)
                    .text("sha256", checksum),
            )
            .send()
            .await
            .expect("the submission");

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        assert!(
            status.is_success(),
            "submitting a file answered {status}: {text}"
        );

        serde_json::from_str::<Value>(&text).unwrap()["id"]
            .as_str()
            .expect("a submission id")
            .to_owned()
    }

    async fn submit(&self, activity: &str, source: &str, language: &str) -> String {
        /// A name the toolchain accepts. The Runner refuses a file whose
        /// extension its toolchain does not, so this is not decoration.
        fn file_named(language: &str) -> &'static str {
            if language.starts_with("py") || language.starts_with("python") {
                "main.py"
            } else if language.starts_with("cpp") || language == "c++" {
                "main.cpp"
            } else {
                "main.c"
            }
        }

        let checksum = hex::encode(Sha256::digest(source.as_bytes()));

        let response = self
            .http
            .post(format!(
                "{}/activities/{activity}/problems/A/submissions",
                api()
            ))
            .multipart(
                // **One opaque document, and a file name.** The language was a
                // field the Server read; it is a member of `props` now, and the
                // Server named pasted source from a table of seven extensions
                // it no longer has — so the sender names it or the submission
                // is refused.
                reqwest::multipart::Form::new()
                    .text(
                        "props",
                        format!(r#"{{"type":"standard-io@1","language":"{language}"}}"#),
                    )
                    .text("code", source.to_owned())
                    .text("fileName", file_named(language).to_owned())
                    .text("sha256", checksum),
            )
            .send()
            .await
            .expect("the submission");

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        assert!(status.is_success(), "submitting answered {status}: {text}");

        serde_json::from_str::<Value>(&text).unwrap()["id"]
            .as_str()
            .expect("a submission id")
            .to_owned()
    }
}

/// Approves whatever Runner the development stack registered.
///
/// Registering is not approval, by design — so a stack brought up fresh has a
/// Runner waiting, and nothing is evaluated until this runs.
async fn approve_the_runner(admin: &Session) {
    let listed = admin.get("/runners").await;
    let runners = listed["items"].as_array().cloned().unwrap_or_default();
    assert!(
        !runners.is_empty(),
        "no Runner has registered; is the development stack up?",
    );

    for runner in runners {
        if runner["state"] == "approved" {
            continue;
        }
        let id = runner["id"].as_str().expect("a runner id");
        admin
            .post(&format!("/runners/{id}/approve"), json!({}))
            .await;
    }
}

/// Publishes a problem carrying the committed package, and returns the
/// activity's slug.
async fn publish(admin: &Session, package: Vec<u8>) -> String {
    publish_of_type(admin, package, "standard-io@1").await
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures")
            .join(name),
    )
    .unwrap_or_else(|e| panic!("the committed {name}: {e}"))
}

async fn publish_of_type(admin: &Session, package: Vec<u8>, problem_type: &str) -> String {
    publish_with_config(admin, package, problem_type, None).await
}

/// The same, with a configuration on the **assignment** — the last link of the
/// chain, which is the one an activity uses to say what a shared problem costs
/// in its own round.
async fn publish_with_config(
    admin: &Session,
    package: Vec<u8>,
    problem_type: &str,
    assignment_config: Option<serde_json::Value>,
) -> String {
    let slug = unique("E2E");

    let activity = admin
        .post(
            "/activities",
            json!({
                "slug": slug,
                "name": "Od zgłoszenia do werdyktu",
                "type": "contest@1",
                "rankingType": "icpc",
                "timeZone": "Europe/Warsaw",
                "joinPolicy": "open",
                "attachmentVisibility": [
                    { "name": "source", "visibility": "participant" },
                    { "name": "log", "visibility": "participant" },
                    { "name": "details", "visibility": "participant" },
                ],
            }),
        )
        .await;

    // No start means started, which is what an untimed activity is.
    let series = admin
        .post(
            &format!("/activities/{}/series", activity["id"].as_str().unwrap()),
            json!({ "slug": "runda", "name": "Runda", "revealProblemCount": true }),
        )
        .await;

    let statement = "# Suma\n\nWczytaj dwie liczby i wypisz ich sumę.\n";
    let statement_id = admin
        .upload("content.md", "text/markdown", statement.as_bytes().to_vec())
        .await;
    let package_id = admin
        .upload("package.zip", "application/zip", package)
        .await;

    let problem = admin
        .post(
            "/problems",
            json!({
                "slug": unique("zadanie").to_lowercase(),
                "name": "Zadanie",
                "type": problem_type,
            }),
        )
        .await;

    // Pure JSON carrying ids. Not one byte of the package is in this request —
    // it went through the file API, with its checksum, like everything else.
    admin
        .post(
            &format!("/problems/{}/versions", problem["id"].as_str().unwrap()),
            json!({
                "note": "pierwsza",
                "statements": [{ "fileId": statement_id }],
                "package": { "fileId": package_id },
            }),
        )
        .await;

    let mut assignment = json!({ "problemId": problem["id"], "slug": "A" });
    if let Some(config) = assignment_config {
        assignment["config"] = config;
    }
    admin
        .post(
            &format!("/series/{}/problems", series["id"].as_str().unwrap()),
            assignment,
        )
        .await;

    slug
}

/// Waits for the round to open.
///
/// A round is opened by `SeriesScheduler` on a scan, not by the request that
/// created it — so a problem is genuinely absent for a moment after being
/// attached, and submitting into that moment is a 404 rather than a race the
/// Server hides. Waiting here is the honest way to write the test; retrying the
/// submission would be pretending the gap is not there.
async fn wait_until_open(participant: &Session, activity: &str) {
    for _ in 0..90 {
        if participant
            .reaches(&format!("/activities/{activity}/problems/A"))
            .await
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("the round never opened for {activity}");
}

/// Waits for a submission to stop moving, and returns what it became.
///
/// Generous, because the first submission also waits for the round to open on
/// the scheduler's scan and for the Runner's polling backoff. A Runner that is
/// slower than this is a Runner that is not working.
async fn settled(participant: &Session, activity: &str, submission: &str) -> Value {
    for _ in 0..120 {
        let seen = participant
            .get(&format!("/activities/{activity}/submissions/{submission}"))
            .await;

        match seen["state"].as_str().unwrap_or("") {
            "completed" | "failed" | "cancelled" => return seen,
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
    panic!("submission {submission} never settled");
}

// ── The milestone ───────────────────────────────────────────────────────────

/// Six outcomes, judged by the Runner in the development stack.
///
/// One test rather than six, because the expensive part is publishing the
/// problem and the interesting part is that **one Runner produces all six** —
/// six separate tests would each prove a Runner can do one thing.
#[tokio::test]
#[ignore = "needs the development stack and the language images"]
async fn a_runner_judges_every_outcome_a_participant_can_get() {
    let package = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sum.zip"),
    )
    .expect("the committed package");

    let admin = Session::as_("admin", "admin-development-only").await;
    approve_the_runner(&admin).await;

    let activity = publish(&admin, package).await;

    let participant = Session::as_("student", "student-development-only").await;
    participant
        .post(&format!("/activities/{activity}/enrolment"), json!({}))
        .await;

    wait_until_open(&participant, &activity).await;

    // Sent first, settled afterwards: the Runner takes them one at a time, and
    // queueing them together is also the only way this test exercises a queue.
    let correct = participant
        .submit(
            &activity,
            "#include <iostream>\nint main(){long long a,b;std::cin>>a>>b;std::cout<<a+b;}\n",
            "cpp",
        )
        .await;
    let wrong = participant
        .submit(
            &activity,
            "#include <iostream>\nint main(){long long a,b;std::cin>>a>>b;std::cout<<a-b;}\n",
            "cpp",
        )
        .await;
    let looping = participant
        .submit(
            &activity,
            "#include <iostream>\nint main(){long long a,b;std::cin>>a>>b;while(true){}}\n",
            "cpp",
        )
        .await;
    let broken = participant
        .submit(&activity, "int main() { this is not c++ }\n", "cpp")
        .await;
    let forbidden = participant
        .submit(
            &activity,
            "#include <iostream>\n#include <unistd.h>\nint main(){std::cout<<1;}\n",
            "cpp",
        )
        .await;
    let python = participant
        .submit(
            &activity,
            "a, b = input().split()\nprint(int(a) + int(b))\n",
            "python",
        )
        .await;

    // ── what each became ────────────────────────────────────────────────────

    let judged = settled(&participant, &activity, &correct).await;
    assert_eq!(judged["state"], "completed", "{judged}");
    assert_eq!(judged["verdict"], "Accepted", "{judged}");
    assert_eq!(judged["score"], 100.0, "{judged}");
    // Both attachments reached the attempt, which is the only way a participant
    // sees the per-test table and a manager sees the build's own words.
    let named: Vec<&str> = judged["attempts"][0]["files"]
        .as_array()
        .map(|files| files.iter().filter_map(|f| f["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(named.contains(&"details"), "{judged}");

    let judged = settled(&participant, &activity, &python).await;
    assert_eq!(judged["verdict"], "Accepted", "{judged}");

    let judged = settled(&participant, &activity, &wrong).await;
    assert_ne!(judged["verdict"], "Accepted", "{judged}");
    assert_eq!(judged["score"], 0.0, "{judged}");

    let judged = settled(&participant, &activity, &looping).await;
    assert_eq!(judged["score"], 0.0, "{judged}");

    let judged = settled(&participant, &activity, &broken).await;
    assert_eq!(judged["verdict"], "Compilation error", "{judged}");

    let judged = settled(&participant, &activity, &forbidden).await;
    assert_eq!(judged["verdict"], "PolicyViolation", "{judged}");
    assert_eq!(judged["score"], 0.0, "{judged}");
}

/// Two Runners on one cache volume judge a problem correctly, end to end.
///
/// **What this proves, and it is worth being exact.** The arrangement works: two
/// containers, one cache volume, separate identities and work directories, a
/// cold cache, six submissions of one problem — every one accepted. That is a
/// regression test for the deployment shape, and it is the only test here that
/// runs two real Runner processes at all.
///
/// **What it does not prove is the race.** Measured 2026-09-02: it passes just
/// as happily with the temporary-name fix reverted. The committed package is
/// **3309 bytes**, written in a single pass, so two writers never overlap on it
/// — there is no window to lose. Both Runners did claim work and both did read
/// the shared volume; the collision simply cannot happen at that size.
///
/// The race is proved in `aj_protocol::cache::tests`, by
/// `two_runners_fetching_one_entry_leave_it_whole`, which forces the window by
/// serving a body in halves and fails without the fix. Two processes were never
/// what the race needed: `File::create` hands every caller its own descriptor
/// with its own offset.
#[tokio::test]
#[ignore = "needs the development stack with two Runners sharing one cache"]
async fn two_runners_sharing_one_cache_judge_the_same_problem_at_once() {
    let admin = Session::as_("admin", "admin-development-only").await;
    approve_the_runner(&admin).await;

    // Otherwise this is the single-Runner test wearing a different name.
    let listed = admin.get("/runners").await;
    let approved = listed["items"]
        .as_array()
        .map(|items| items.iter().filter(|r| r["state"] == "approved").count())
        .unwrap_or(0);
    assert!(
        approved >= 2,
        "this test needs two approved Runners on one cache volume; saw {approved}",
    );

    let activity = publish(&admin, fixture("sum.zip")).await;

    let participant = Session::as_("student", "student-development-only").await;
    participant
        .post(&format!("/activities/{activity}/enrolment"), json!({}))
        .await;
    wait_until_open(&participant, &activity).await;

    // Sent before any is settled, so both Runners claim while the entry is
    // still missing from the volume they share.
    const CORRECT: &str = "#include <iostream>
int main(){long long a,b;std::cin>>a>>b;std::cout<<a+b;}
";
    let mut sent = Vec::new();
    for _ in 0..6 {
        sent.push(participant.submit(&activity, CORRECT, "cpp").await);
    }

    for submission in sent {
        let judged = settled(&participant, &activity, &submission).await;
        assert_eq!(
            judged["state"], "completed",
            "a submission did not complete; a `failed` here is the package the              cache handed over, not the program: {judged}",
        );
        assert_eq!(judged["verdict"], "Accepted", "{judged}");
        assert_eq!(judged["score"], 100.0, "{judged}");
    }
}

/// The sixth outcome, and the only one that is **not a verdict**.
///
/// A package the Runner cannot open says nothing about the solution. The
/// submission must end `failed` with **no score at all** — not zero, absent —
/// because a zero on a board reads as a wrong answer, and this participant's
/// program was never run.
#[tokio::test]
#[ignore = "needs the development stack and the language images"]
async fn a_package_that_will_not_open_is_not_scored_as_a_wrong_answer() {
    let admin = Session::as_("admin", "admin-development-only").await;
    approve_the_runner(&admin).await;

    // Stored happily: the Server recomputes the checksum and never opens the
    // archive, which is the whole point of the package being opaque to it.
    let activity = publish(&admin, b"this is not a zip file".to_vec()).await;

    let participant = Session::as_("student", "student-development-only").await;
    participant
        .post(&format!("/activities/{activity}/enrolment"), json!({}))
        .await;
    wait_until_open(&participant, &activity).await;

    let submission = participant
        .submit(
            &activity,
            "#include <iostream>\nint main(){std::cout<<1;}\n",
            "cpp",
        )
        .await;

    let judged = settled(&participant, &activity, &submission).await;

    assert_eq!(judged["state"], "failed", "{judged}");
    assert!(
        judged["score"].is_null(),
        "an evaluation that never happened must carry no score — absent, not zero: {judged}",
    );
    assert!(judged["verdict"].is_null(), "and no verdict: {judged}");
}

// ── M3: the invariant ───────────────────────────────────────────────────────

/// A second problem type, judged end to end, **with no Server change at all**.
///
/// "Adding a problem type does not require a Server change" has been asserted
/// from the beginning and nothing had ever checked it. This is the check. What
/// it exercises is everything that could plausibly have needed one:
///
/// - the submission is a **file**, not source in an editor;
/// - the package declares no language and needs no compiler;
/// - nothing untrusted is executed, so there is no sandbox in the path.
///
/// The mechanical half of the criterion — an empty `git diff` on the Server and
/// a byte-identical `openapi.json` — is asserted outside this test, because a
/// test cannot honestly assert something about a repository it is not in.
#[tokio::test]
#[ignore = "needs the development stack"]
async fn a_second_problem_type_is_judged_without_the_server_learning_about_it() {
    let admin = Session::as_("admin", "admin-development-only").await;
    approve_the_runner(&admin).await;

    let activity = publish_of_type(&admin, fixture("squares.zip"), "output-only@1").await;

    let participant = Session::as_("student", "student-development-only").await;
    participant
        .post(&format!("/activities/{activity}/enrolment"), json!({}))
        .await;
    wait_until_open(&participant, &activity).await;

    let right = participant
        .submit_file(&activity, "answers.zip", fixture("squares-answers.zip"))
        .await;
    let nearly = participant
        .submit_file(
            &activity,
            "answers.zip",
            fixture("squares-answers-wrong.zip"),
        )
        .await;

    let judged = settled(&participant, &activity, &right).await;
    assert_eq!(judged["state"], "completed", "{judged}");
    assert_eq!(judged["verdict"], "Accepted", "{judged}");
    assert_eq!(judged["score"], 100.0, "{judged}");

    // One wrong answer, in group 2. The group rule then takes that group and
    // leaves the other — the same scoring the other type gets, because scoring
    // is shared and only the evaluation differs.
    let judged = settled(&participant, &activity, &nearly).await;
    assert_eq!(judged["score"], 50.0, "{judged}");
    assert_ne!(judged["verdict"], "Accepted", "{judged}");
}

/// One trial, through the whole stack.
///
/// The seam nothing else covers. The Server's own suite proves a trial can be
/// stored, claimed and reported; the Runner's proves a package's model
/// solutions can be measured. **Neither proves the two halves meet** — and the
/// endpoints, the wire names and the deletion all live in that gap.
///
/// Asserts the two things a trial exists for and the one it must not do: a
/// measurement arrives, per group, and the package is **gone** afterwards.
#[tokio::test]
#[ignore = "needs the development stack and the language images"]
async fn a_trial_is_measured_end_to_end_and_the_package_does_not_survive() {
    let admin = Session::as_("admin", "admin-development-only").await;
    approve_the_runner(&admin).await;

    // The package that ships, so this fails if the format and the code part
    // company. It declares two model solutions, in two languages.
    let package = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sum.zip"),
    )
    .expect("the committed package");

    let file_id = admin.upload("sum.zip", "application/zip", package).await;

    // `/trials`, not `/activities/{id}/trials`: the whole surface moved when a
    // manager had to be able to measure a problem in the library, which belongs
    // to no activity (D-16). The activity is named in the body instead.
    let created = admin
        .post(
            "/trials",
            serde_json::json!({
                "problemType": "standard-io@1",
                "packageFileId": file_id,
                "activityIdOrSlug": "DEV-2026",
            }),
        )
        .await;

    let trial = created["id"].as_str().expect("a trial id").to_owned();
    assert_eq!(created["state"], "queued");
    assert_eq!(created["hasPackage"], true);

    // Trials are claimed only when there is no marking to do, so this waits on
    // an idle Runner rather than on a queue position.
    let mut settled = serde_json::Value::Null;
    for _ in 0..240 {
        let seen = admin.get(&format!("/trials/{trial}")).await;
        match seen["state"].as_str().unwrap_or("") {
            "completed" | "failed" => {
                settled = seen;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }

    assert_eq!(
        settled["state"], "completed",
        "the trial did not finish cleanly: {settled}",
    );

    // The measurement is opaque to the Server, so it arrives as a string and is
    // parsed here — by the only component that is allowed to know its shape.
    let measurement = settled["measurement"].as_str().expect("a measurement");
    let measured: Value = serde_json::from_str(measurement).expect("a measurement document");
    let rows = measured["measured"].as_array().expect("measured rows");

    assert!(!rows.is_empty(), "nothing was measured: {measurement}");
    assert!(
        rows.iter()
            .all(|row| row["timeMs"].as_u64().unwrap_or(0) > 0),
        "a measured group with no time did not run: {measurement}",
    );
    // Two model solutions, so every row names which language it is for.
    assert!(
        rows.iter().all(|row| row["language"].is_string()),
        "with two models every row names a language: {measurement}",
    );

    // **The bytes do not survive the trial** (D-12), and the row does.
    assert_eq!(settled["hasPackage"], false);
    assert!(
        !admin.reaches(&format!("/files/{file_id}")).await,
        "the package was still readable after the trial finished",
    );
}

// ── Maintenance ─────────────────────────────────────────────────────────────

/// The token `/admin/**` asks for, beside being called from inside the container.
///
/// Defaulted to the value the development compose file ships, so an ordinary run
/// needs no configuration; an operator whose stack reads a real one from `.env`
/// passes the same variable here.
fn admin_token() -> String {
    std::env::var("AJ_ADMIN_TOKEN").unwrap_or_else(|_| "admin-token-development-only".into())
}

/// Throws the maintenance switch.
///
/// **Two things are needed, and this test has to hold both.** The token above,
/// and a request from the Server's own loopback interface — proving that each
/// is required on its own is the Server's `AdminSurfaceTests`, not this. The way
/// a test becomes a local caller without a shell inside the container is to
/// share the container's network namespace:
///
/// ```text
/// AJ_DOCKER_NETWORK=container:algojudge-runner-dev-server-1 \
/// AJ_TEST_SERVER=http://127.0.0.1:8080/api/v1 \
///   ./x test -p aj-runner --test end_to_end -- --include-ignored --test-threads=1
/// ```
///
/// Run any other way this fails with a 404 and says so, which is the switch
/// working rather than the test being wrong.
async fn maintenance(http: &reqwest::Client, query: &str) -> Value {
    let response = http
        .post(format!("{}/admin/maintenance?{query}", api()))
        .header("X-AlgoJudge-Admin-Token", admin_token())
        .send()
        .await
        .expect("the switch — is the development stack up?");

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "the switch answered {status}: {text}\n\
         a 404 means the token was refused, or this test is not on the Server's \
         loopback interface — see the doc comment above",
    );
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// What `/health` says, from the host — it is anonymous and always answers.
async fn level(http: &reqwest::Client) -> String {
    let health: Value = http
        .get(format!("{}/health", api()))
        .send()
        .await
        .expect("health always answers")
        .json()
        .await
        .expect("a health document");

    health["maintenance"]["level"]
        .as_str()
        .unwrap_or("open")
        .to_owned()
}

/// **A backup must not cost somebody their submission.**
///
/// The case the whole feature exists for, driven end to end: a submission is in
/// flight, an operator takes the installation off the air, and the participant
/// still gets the verdict their solution earned. Before this, the Runner
/// flattened a 503 during the package download into a `String` and reported it
/// as an infrastructure failure against the attempt — so a backup started at the
/// wrong second closed somebody's submission for them.
///
/// Four things are asserted and each one failed differently before:
///
/// 1. the submission ends **judged**, never `failed`;
/// 2. a participant is refused **while the Runner is admitted** — the asymmetry
///    that is the whole reason there are two levels rather than a flag;
/// 3. the Server reaches `closed` on its own, which is what an operator waits
///    for before touching the database;
/// 4. the Runner is **still working afterwards** — it waited the window out
///    rather than exiting into whatever restarts it.
///
/// **What it does not exercise**, said plainly rather than left to be assumed:
/// the Runner never sees a `503` here, and that is the design working — at
/// `draining` it is allowed to finish and report, which is exactly why (1)
/// holds. The harsher path, a `closed` Server arriving mid-job, is covered by
/// the unit tests over `Trouble`, where the classification that decides it
/// lives.
#[tokio::test]
#[ignore = "needs the development stack and the language images"]
async fn a_window_does_not_cost_a_participant_their_submission() {
    let http = reqwest::Client::new();
    let admin = Session::as_("admin", "admin-development-only").await;
    approve_the_runner(&admin).await;

    let activity = publish(&admin, fixture("sum.zip")).await;
    let participant = Session::as_("student", "student-development-only").await;
    participant
        .post(&format!("/activities/{activity}/enrolment"), json!({}))
        .await;
    wait_until_open(&participant, &activity).await;

    // The same solution the six-outcome test calls accepted, so a failure here
    // is about the window and never about the C++.
    let correct = "#include <iostream>\nint main(){long long a,b;std::cin>>a>>b;std::cout<<a+b;}\n";
    let submission = participant.submit(&activity, correct, "cpp").await;

    // Thrown as close to the claim as this can honestly get: polled for the
    // moment the Runner takes it, and thrown anyway if that moment is missed.
    // The assertion below does not depend on winning the race — it is the same
    // invariant whether the window caught the job or only the queue.
    for _ in 0..100 {
        let seen = participant
            .get(&format!("/activities/{activity}/submissions/{submission}"))
            .await;
        if seen["state"] == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let thrown = maintenance(&http, "on=true&reason=an+end-to-end+window").await;
    // **Never straight to closed**: whatever is in flight is given its chance.
    assert_eq!(thrown["level"], "draining");

    // **The asymmetry, from the participant's side.** A Runner may still hand in
    // what it holds; nobody may start anything new. One flag could not express
    // both, which is why there are two levels.
    let refused = participant
        .http
        .post(format!(
            "{}/activities/{activity}/problems/A/submissions",
            api()
        ))
        .multipart(
            reqwest::multipart::Form::new()
                .text("props", r#"{"type":"standard-io@1","language":"cpp"}"#)
                .text("code", correct.to_owned())
                .text("fileName", "main.cpp")
                .text("sha256", hex::encode(Sha256::digest(correct.as_bytes()))),
        )
        .send()
        .await
        .expect("the Server answers, it just refuses");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "a participant was still able to submit during a window",
    );
    assert!(
        refused.headers().contains_key("retry-after"),
        "a refusal with no Retry-After leaves a caller guessing",
    );

    // It drains rather than closing on top of whatever is running, so this is
    // the wait an operator actually has before a backup is safe to start.
    let mut closed = false;
    for _ in 0..120 {
        if level(&http).await == "closed" {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(closed, "the Server never finished draining");

    // Nothing has been marked in the meantime, because nothing was allowed to
    // be — but nothing has been thrown away either.
    maintenance(&http, "on=false").await;
    assert_eq!(level(&http).await, "open");

    let seen = settled(&participant, &activity, &submission).await;
    assert_eq!(
        seen["state"], "completed",
        "the window turned a submission into a failure: {seen}",
    );
    assert_eq!(
        seen["verdict"], "Accepted",
        "a correct solution was not accepted after the window: {seen}",
    );

    // **And the Runner is still there.** A `503` used to be terminal, so this is
    // the assertion that it waited rather than exiting into its restart policy:
    // a second submission, judged by the same process, with nothing restarted.
    let second = participant.submit(&activity, correct, "cpp").await;
    let after = settled(&participant, &activity, &second).await;
    assert_eq!(
        after["state"], "completed",
        "the Runner did not come back on its own after the window: {after}",
    );
}

/// **A lease outliving its own deadline, watched from outside.**
///
/// The one thing about renewal that unit tests cannot reach. `keeper`'s own
/// tests drive a stopped clock and a stand-in for the Server; the conformance
/// suite proves the endpoint. Neither proves the two wired together, and a lease
/// being renewed looks exactly like one that has not expired yet — there is no
/// output to read, so the only honest proof is to hold a job past the point
/// where the Server would have taken it back, and then ask whose it is.
///
/// The shape:
///
/// - a problem of a type **no Runner in the stack handles**, so the job stays
///   queued for this test rather than being judged out from under it;
/// - a lease of sixty seconds, the shortest the Server grants, which the keeper
///   renews every fifteen;
/// - ninety-five seconds of waiting — past the deadline, and past the
///   thirty-second sweep that would have reclaimed it;
/// - `progress`, which the Server refuses with `runner.lease.stale` if this
///   Runner is no longer the holder.
///
/// **Sabotaged by dropping the keeper** immediately after it is made: the wait
/// then ends with `runner.lease.stale` and this fails, which is the whole point.
#[tokio::test]
#[ignore = "needs the development stack; deliberately spends about two minutes waiting"]
async fn a_renewed_lease_outlives_the_deadline_it_was_granted() {
    let admin = Session::as_("admin", "admin-development-only").await;

    let activity = publish_of_type(&admin, fixture("squares.zip"), "lease-probe@1").await;

    let participant = Session::as_("student", "student-development-only").await;
    participant
        .post(&format!("/activities/{activity}/enrolment"), json!({}))
        .await;
    wait_until_open(&participant, &activity).await;
    participant
        .submit_file(&activity, "answers.zip", fixture("squares-answers.zip"))
        .await;

    let config = probe_config();
    let server = Arc::new(Server::new(&config.base_url).expect("a Server"));
    let identity = Identity::load_or_create(&config.key_path).expect("an identity");

    // `admitted` registers and then waits to be approved, so approving has to
    // happen while it waits. Repeated rather than timed once: this races the
    // registration, and losing that race would hang the test rather than fail
    // it.
    let approving = tokio::spawn(async move {
        let admin = Session::as_("admin", "admin-development-only").await;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            approve_the_runner(&admin).await;
        }
    });
    run::admitted(&server, &identity, &config)
        .await
        .expect("this Runner was never admitted");
    approving.abort();

    let job = claim_the_probe(&server, &config).await;

    let keeper = Keeper::hold_job(
        Arc::clone(&server),
        job.job_id.clone(),
        job.lease_token.clone(),
        &config,
    );

    tokio::time::sleep(Duration::from_secs(95)).await;

    server
        .progress(&job.job_id, &job.lease_token)
        .await
        .expect("the job was reclaimed while this Runner was still holding it");

    drop(keeper);

    // Left tidy rather than abandoned: an unreported job sits `Running` until
    // its lease lapses and is then delivered four more times before the Server
    // gives up on it. Saying plainly that nothing judged it costs one call.
    server
        .report(
            &job.job_id,
            &ReportResult::failed(&job.lease_token, "lease probe, never evaluated".to_owned()),
        )
        .await
        .expect("reporting the probe");
}

/// A Runner that handles one type nothing else does, on the shortest lease the
/// Server will grant.
fn probe_config() -> Config {
    let scratch = std::env::temp_dir().join(unique("lease-probe"));
    Config {
        base_url: api(),
        name: unique("lease-probe"),
        key_path: scratch.join("identity.key"),
        problem_types: vec!["lease-probe@1".into()],
        tags: vec![],
        poll_min: Duration::from_secs(1),
        poll_max: Duration::from_secs(5),
        heartbeat: Duration::from_secs(60),
        // Sixty is the Server's floor, so this is the fastest a lease can be
        // made to matter — and it makes the keeper's interval fifteen seconds.
        lease_seconds: 60,
        cache_path: scratch.join("cache"),
        cache_max_bytes: 0,
        work_path: scratch.join("work"),
        work_host_path: scratch.join("work"),
        // Never used: nothing here evaluates anything.
        images: aj_standard_io::Images::default(),
        allow_unmeasured: false,
    }
}

/// The queue is not instant — the round opens on a scheduler's scan rather than
/// on the request that created the submission.
async fn claim_the_probe(server: &Server, config: &Config) -> aj_protocol::wire::ClaimedJob {
    for _ in 0..60 {
        match server.claim(Some(config.lease_seconds)).await {
            Ok(Some(job)) => return job,
            Ok(None) => tokio::time::sleep(Duration::from_secs(1)).await,
            Err(e) => panic!("claiming the probe was refused: {e}"),
        }
    }
    panic!("no lease-probe@1 job was queued within a minute");
}

/// An assignment's own limits are the ones the submission is judged under.
///
/// **This is the whole configuration chain, end to end, and until 2026-08-22 it
/// did nothing for the product's principal problem type.** The Server merged
/// `ProblemVersion.Config` over the package and `SeriesProblem.Config` over
/// that, sent the result with every job — and the `standard-io@1` arm parsed the
/// package and discarded it. One library problem attached to two activities with
/// different limits was judged under the package's limits in both, silently, and
/// every sentence `PACKAGE_FORMAT.md` writes about the chain was untrue here.
///
/// The committed package gives a C++ solution a full second. This activity
/// narrows that to ten milliseconds, which no container starts in — so the same
/// solution that is `Accepted` everywhere else in this file has to come back as
/// a time limit. If it does not, the overlay is being thrown away again.
///
/// **`limits` is restated whole, and that is the format's rule rather than this
/// test being careful.** The merge replaces top-level members, because a deeper
/// one would require knowing what they mean — so an assignment that narrows the
/// time limit and says nothing about memory does not inherit the package's
/// memory limit, it *removes* it, and the merged document then fails to read.
/// Writing `{ "timeMs": 10 }` alone here cost a run and produced
/// `missing field memoryBytes`, which is the error a manager would get too.
#[tokio::test]
#[ignore = "needs the development stack and the language images"]
async fn an_assignments_own_limits_are_what_the_submission_is_judged_under() {
    let admin = Session::as_("admin", "admin-development-only").await;
    approve_the_runner(&admin).await;

    let activity = publish_with_config(
        &admin,
        fixture("sum.zip"),
        "standard-io@1",
        Some(json!({ "limits": { "timeMs": 10, "memoryBytes": 268435456 } })),
    )
    .await;

    let participant = Session::as_("student", "student-development-only").await;
    participant
        .post(&format!("/activities/{activity}/enrolment"), json!({}))
        .await;
    wait_until_open(&participant, &activity).await;

    let sent = participant
        .submit(
            &activity,
            // **It spins for 1.2 s on purpose, and that is not padding.** A test
            // container is killed at its limit *plus a second of grace*, and
            // nothing compares the measured time against the limit itself — so a
            // solution that merely exceeds the limit is still accepted, and only
            // one that outlives the grace comes back as a time limit. This
            // finishes inside the package's second-plus-grace and outlives the
            // assignment's ten-milliseconds-plus-grace, which is the difference
            // under test.
            //
            // A first version of this source was written by hand and did not
            // compile, so the test passed on `Compilation error` in the
            // sabotaged build too. Anything here that is not the sibling tests'
            // own source is worth checking twice.
            "#include <iostream>\n#include <chrono>\nint main(){long long a,b;std::cin>>a>>b;auto t=std::chrono::steady_clock::now();while(std::chrono::steady_clock::now()-t<std::chrono::milliseconds(1200)){}std::cout<<a+b;}\n",
            "cpp",
        )
        .await;

    let judged = settled(&participant, &activity, &sent).await;

    assert_eq!(judged["state"], "completed", "{judged}");
    assert_ne!(
        judged["verdict"], "Accepted",
        "a correct solution passed under an assignment that allows it ten \
         milliseconds — the overlay was discarded: {judged}",
    );
}

/// **A Runner told to stop gives its job back, and the queue notices at once.**
///
/// The signal is the real one — `SIGTERM`, what `docker stop` and `compose
/// down` send — delivered to the Runner in the stack while it is judging. What
/// is asserted is what the Server sees: the submission is waiting again in
/// seconds, not in the ten minutes its lease had left.
///
/// **The lease is what makes the number meaningful.** A Runner that simply died
/// would also give the job back eventually; sixty seconds is a tenth of the
/// deadline it would have waited for, so passing this cannot be the reaper.
///
/// Needs the daemon as well as the stack — `AJ_DOCKER_SOCKET=1` — because
/// sending a process a signal is the one thing HTTP cannot do.
#[tokio::test]
#[ignore = "needs the development stack, the language images and the daemon"]
async fn a_runner_told_to_stop_hands_its_job_back_at_once() {
    let package = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sum.zip"),
    )
    .expect("the committed package");

    let admin = Session::as_("admin", "admin-development-only").await;
    approve_the_runner(&admin).await;
    let activity = publish(&admin, package).await;

    let participant = Session::as_("student", "student-development-only").await;
    participant
        .post(&format!("/activities/{activity}/enrolment"), json!({}))
        .await;
    wait_until_open(&participant, &activity).await;

    // Long enough to still be running when the signal arrives: every test in
    // the package outlives its limit, so the job spends its life being reaped
    // one test at a time.
    let sent = participant
        .submit(
            &activity,
            "#include <iostream>\n#include <chrono>\nint main(){long long a,b;std::cin>>a>>b;auto t=std::chrono::steady_clock::now();while(std::chrono::steady_clock::now()-t<std::chrono::seconds(30)){}std::cout<<a+b;}\n",
            "cpp",
        )
        .await;

    let running = wait_for(&participant, &activity, &sent, "running", 120).await;
    assert_eq!(running["state"], "running", "{running}");

    let daemon = bollard::Docker::connect_with_local_defaults().expect("the daemon");
    let container = std::env::var("AJ_TEST_RUNNER_CONTAINER")
        .unwrap_or_else(|_| "algojudge-runner-dev-runner-1".to_owned());
    daemon
        .kill_container(
            &container,
            Some(
                bollard::query_parameters::KillContainerOptionsBuilder::default()
                    .signal("SIGTERM")
                    .build(),
            ),
        )
        .await
        .expect("the signal reaches the Runner");

    let began = std::time::Instant::now();
    let back = wait_for(&participant, &activity, &sent, "queued", 120).await;
    let took = began.elapsed();

    assert_eq!(back["state"], "queued", "{back}");
    assert!(
        took < Duration::from_secs(60),
        "it took {took:?} to come back, which is the lease expiring rather than          the Runner giving it back",
    );

    // **And the stack is handed on working.** A signal sent with `docker kill`
    // counts as the operator stopping the container, so `restart: unless-stopped`
    // leaves it down -- measured, after this case took the Runner away from every
    // test that ran after it. Started again here, and the submission waited for,
    // because a Runner that comes back and judges it is the only proof that it
    // did come back. Generous: a start is a preflight and four image probes
    // before it claims anything.
    // **Asked until it is actually running.** A container that is still shutting
    // down answers a start with "nothing changed" rather than an error, so one
    // call is a coin toss on whether the Runner had finished exiting -- and a
    // lost toss leaves every test after this one without a Runner, which is
    // exactly how this was found.
    let mut up = false;
    for _ in 0..60 {
        let running = daemon
            .inspect_container(
                &container,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await
            .ok()
            .and_then(|seen| seen.state)
            .and_then(|state| state.running)
            .unwrap_or(false);
        if running {
            up = true;
            break;
        }
        let _ = daemon
            .start_container(
                &container,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(up, "the Runner never came back up");

    let judged = settled_within(&participant, &activity, &sent, 360).await;
    assert_eq!(
        judged["state"], "completed",
        "the Runner never came back to judge it: {judged}",
    );
}

/// `settled`, with the patience named by the caller.
async fn settled_within(
    participant: &Session,
    activity: &str,
    submission: &str,
    tries: usize,
) -> Value {
    for _ in 0..tries {
        let seen = participant
            .get(&format!("/activities/{activity}/submissions/{submission}"))
            .await;
        match seen["state"].as_str().unwrap_or("") {
            "completed" | "failed" | "cancelled" => return seen,
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
    panic!("submission {submission} never settled");
}

/// Polls until a submission reads as `want`, and answers with it.
async fn wait_for(
    participant: &Session,
    activity: &str,
    submission: &str,
    want: &str,
    tries: usize,
) -> Value {
    for _ in 0..tries {
        let seen = participant
            .get(&format!("/activities/{activity}/submissions/{submission}"))
            .await;
        if seen["state"] == want {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("submission {submission} never reached {want}");
}
