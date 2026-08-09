//! A submission, judged by a Runner that nobody is helping.
//!
//! Everything else in this repository tests a part. This drives the **Server**
//! over HTTP and never touches the Runner at all: a manager publishes a problem
//! with the committed package, a participant submits, and the Runner running in
//! the development stack claims the job, judges it and reports. What is asserted
//! is what the participant would read.
//!
//! ```text
//! docker build -t algojudge/lang-cpp:local    images/cpp
//! docker build -t algojudge/lang-python:local images/python
//! docker compose -f example-runner-development-docker-compose.yaml up -d --build --wait
//! ./x test -p aj-runner --test end_to_end -- --include-ignored --test-threads=1
//! ```
//!
//! **Six outcomes, one run.** Five are verdicts and the sixth is not: an
//! infrastructure failure means the submission was never judged, and it is the
//! one that must never be scored as a wrong answer.

use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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
        let checksum = hex::encode(Sha256::digest(source.as_bytes()));

        let response = self
            .http
            .post(format!(
                "{}/activities/{activity}/problems/A/submissions",
                api()
            ))
            .multipart(
                reqwest::multipart::Form::new()
                    .text("language", language.to_owned())
                    .text("code", source.to_owned())
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
                "languages": ["cpp", "python"],
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

    admin
        .post(
            &format!("/series/{}/problems", series["id"].as_str().unwrap()),
            json!({ "problemId": problem["id"], "slug": "A" }),
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
    assert_eq!(judged["verdict"], "Błąd kompilacji", "{judged}");

    let judged = settled(&participant, &activity, &forbidden).await;
    assert_eq!(judged["verdict"], "PolicyViolation", "{judged}");
    assert_eq!(judged["score"], 0.0, "{judged}");
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
