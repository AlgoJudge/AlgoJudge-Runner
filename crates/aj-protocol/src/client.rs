//! Everything this Runner says to the Server.
//!
//! Twelve endpoints, and every one of them is the Runner dialling out. The
//! Server never calls back, which is what lets a Runner sit behind a domestic
//! router — and it is why there is no listener anywhere in this crate.

use std::path::Path;
use std::sync::RwLock;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::error::{Error, Refusal, Result};
use crate::identity::Identity;
use crate::wire::*;

pub struct Server {
    http: reqwest::Client,
    /// Normalised: no trailing slash, and **including `/api/v1`**. The Server
    /// serves nothing outside that prefix — a guard returns an empty 404 before
    /// routing — so a base URL without it fails on every call with no clue as
    /// to why.
    base: String,
    token: RwLock<Option<String>>,
}

/// Long enough for a busy Server, short enough that a hung one is noticed.
///
/// Without a connect timeout, a proxy that accepts the socket and then says
/// nothing holds the claim loop for as long as the kernel is willing to wait —
/// which is minutes, and looks from outside like a Runner that has stopped
/// working rather than one that is waiting.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// **Between bytes, not for the whole request.**
///
/// A total timeout would be wrong here: `download_to` streams a package that
/// may be large over a link that may be slow, and cutting it off at a fixed
/// duration would turn a big problem set into an infrastructure failure. What
/// must not be tolerated is *silence* — a connection that was accepted and then
/// abandoned — and that is what this bounds.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl Server {
    pub fn new(base_url: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("AlgoJudge-Runner/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .build()?,
            base: base_url.trim_end_matches('/').to_owned(),
            token: RwLock::new(None),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    fn token(&self) -> Option<String> {
        self.token
            .read()
            .expect("the token lock is never poisoned")
            .clone()
    }

    pub fn authenticated(&self) -> bool {
        self.token().is_some()
    }

    /// Forgets the token. Called when the Server says `401`, which happens on
    /// an ordinary Server restart because tokens live in its memory.
    pub fn forget_token(&self) {
        *self
            .token
            .write()
            .expect("the token lock is never poisoned") = None;
    }

    // ── Whether the Server is serving ───────────────────────────────────────

    /// Asks whether the Server is open, and if not, how far it has withdrawn.
    ///
    /// **Anonymous and never gated.** Both matter. A Runner whose token has
    /// expired during a window cannot shake hands again — the handshake is
    /// refused with everything else at the deepest level — so a call that needed
    /// one would be the door locking behind it. And this is the only request
    /// that answers `200` throughout, which is what makes waiting for the window
    /// to end distinguishable from waiting for a Server that is never coming
    /// back.
    pub async fn health(&self) -> Result<Health> {
        let response = accept(self.http.get(self.url("health")).send().await?).await?;
        read(response).await
    }

    // ── Registration and the handshake ──────────────────────────────────────

    /// Presents the public key. **Registering again is how a Runner reports a
    /// restart**: the Server updates the capabilities and keeps the identity,
    /// so this is safe to call on every start and is not a way to get a second
    /// approval.
    pub async fn register(&self, register: &Register) -> Result<Registered> {
        let response = self
            .http
            .post(self.url("runner/register"))
            .json(register)
            .send()
            .await?;
        read(accept(response).await?).await
    }

    /// Challenge, sign, token.
    ///
    /// Both calls are anonymous, because a Runner has no session to present.
    /// The nonce is fetched immediately before it is signed and used exactly
    /// once — it is spent by being used, and holding one for later is how a
    /// Runner earns `runner.nonce.unknown`.
    pub async fn authenticate(&self, identity: &Identity) -> Result<()> {
        let challenge: Challenge = read(
            accept(
                self.http
                    .post(self.url("runner/auth/challenge"))
                    .json(&ChallengeRequest {
                        fingerprint: identity.fingerprint(),
                    })
                    .send()
                    .await?,
            )
            .await?,
        )
        .await?;

        let token: Token = read(
            accept(
                self.http
                    .post(self.url("runner/auth/token"))
                    .json(&TokenRequest {
                        fingerprint: identity.fingerprint(),
                        nonce: challenge.nonce.clone(),
                        signature: identity.sign(&challenge.nonce),
                    })
                    .send()
                    .await?,
            )
            .await?,
        )
        .await?;

        tracing::info!(expires_at = %token.expires_at, "authenticated");
        *self
            .token
            .write()
            .expect("the token lock is never poisoned") = Some(token.token);
        Ok(())
    }

    // ── Work ────────────────────────────────────────────────────────────────

    /// Takes one job, or `None` when the queue holds nothing for this Runner.
    ///
    /// **`None` is the ordinary state and not an error.** The Server answers
    /// `204`, and a Runner that treats an empty queue as a fault spends its life
    /// logging that there is no work.
    pub async fn claim(&self, lease_seconds: Option<u32>) -> Result<Option<ClaimedJob>> {
        let response = accept(
            self.bearer(self.http.post(self.url("runner/jobs/claim")))
                .json(&ClaimRequest { lease_seconds })
                .send()
                .await?,
        )
        .await?;

        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        read(response).await.map(Some)
    }

    /// Takes one trial, or `None`.
    ///
    /// A second endpoint rather than a flag on `claim`, matching the Server: a
    /// Runner that has not been taught about trials keeps working, and a queue
    /// of trials can never delay the queue that decides somebody's mark.
    pub async fn claim_trial(&self, lease_seconds: Option<u32>) -> Result<Option<ClaimedTrial>> {
        let response = accept(
            self.bearer(self.http.post(self.url("runner/trials/claim")))
                .json(&ClaimRequest { lease_seconds })
                .send()
                .await?,
        )
        .await?;

        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        read(response).await.map(Some)
    }

    /// Says what a trial measured, or why it measured nothing.
    ///
    /// Idempotent on the lease token, as a job report is: a resend after a lost
    /// acknowledgement answers `duplicate: true` rather than recording a second
    /// measurement.
    pub async fn report_trial(
        &self,
        trial_id: &str,
        report: &TrialReport,
    ) -> Result<TrialReportAccepted> {
        let response = self
            .bearer(
                self.http
                    .post(self.url(&format!("runner/trials/{trial_id}/report"))),
            )
            .json(report)
            .send()
            .await?;
        read(accept(response).await?).await
    }

    /// Extends a **trial's** lease. Same deadline, same reason, its own path.
    pub async fn renew_trial(
        &self,
        trial_id: &str,
        lease_token: &str,
        lease_seconds: Option<u32>,
    ) -> Result<TrialLease> {
        let response = self
            .bearer(
                self.http
                    .post(self.url(&format!("runner/trials/{trial_id}/lease"))),
            )
            .json(&LeaseRequest {
                lease_token: lease_token.to_owned(),
                lease_seconds,
            })
            .send()
            .await?;
        read(accept(response).await?).await
    }

    /// Extends a lease this Runner still holds.
    ///
    /// The deadline exists so a job survives a Runner that died, not so it
    /// interrupts one that is working — so a long evaluation renews rather than
    /// being cut off.
    pub async fn renew(
        &self,
        job_id: &str,
        lease_token: &str,
        lease_seconds: Option<u32>,
    ) -> Result<Lease> {
        let response = self
            .bearer(
                self.http
                    .post(self.url(&format!("runner/jobs/{job_id}/lease"))),
            )
            .json(&LeaseRequest {
                lease_token: lease_token.to_owned(),
                lease_seconds,
            })
            .send()
            .await?;
        read(accept(response).await?).await
    }

    /// Says the Runner is still working. Renews the lease, which is all the
    /// Server can do with the news.
    pub async fn progress(&self, job_id: &str, lease_token: &str) -> Result<()> {
        let response = self
            .bearer(
                self.http
                    .post(self.url(&format!("runner/jobs/{job_id}/progress"))),
            )
            .json(&LeaseRequest {
                lease_token: lease_token.to_owned(),
                lease_seconds: None,
            })
            .send()
            .await?;
        accept(response).await.map(|_| ())
    }

    /// Records a verdict, once.
    ///
    /// **Safe to resend.** Idempotency is on the lease token and is enforced by
    /// a unique index rather than by a check the Server could forget, so a
    /// Runner that reported and then lost the connection gets the same result
    /// back with `duplicate: true`.
    pub async fn report(&self, job_id: &str, report: &ReportResult) -> Result<ReportAccepted> {
        let response = self
            .bearer(
                self.http
                    .post(self.url(&format!("runner/jobs/{job_id}/report"))),
            )
            .json(report)
            .send()
            .await?;
        read(accept(response).await?).await
    }

    /// Alive, and holding nothing in particular. It is what makes "last seen"
    /// mean anything.
    pub async fn heartbeat(&self) -> Result<()> {
        let response = self
            .bearer(self.http.post(self.url("runner/heartbeat")))
            .send()
            .await?;
        accept(response).await.map(|_| ())
    }

    // ── Files ───────────────────────────────────────────────────────────────

    /// Streams a file to disk and returns the checksum **of what actually
    /// arrived**, lowercase hex.
    ///
    /// Hashed while streaming rather than by re-reading the file: the bytes are
    /// in hand once, and a second pass would hash whatever is on disk now
    /// rather than what came off the wire. The caller compares it against the
    /// job — this function does not, because what a mismatch *means* is the
    /// caller's decision.
    pub async fn download_to(&self, file_id: &str, path: &Path) -> Result<String> {
        let mut response = accept(
            self.bearer(self.http.get(self.url(&format!("runner/files/{file_id}"))))
                .send()
                .await?,
        )
        .await?;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::File::create(path).await?;
        let mut digest = Sha256::new();

        while let Some(chunk) = response.chunk().await? {
            digest.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;

        Ok(hex::encode(digest.finalize()))
    }

    /// Stores bytes this Runner produced. The Server recomputes the checksum
    /// and refuses to store on a mismatch, exactly as it does for anybody else.
    pub async fn upload(
        &self,
        file_name: &str,
        mime_type: &str,
        bytes: Vec<u8>,
    ) -> Result<UploadedFile> {
        let sha256 = hex::encode(Sha256::digest(&bytes));

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name.to_owned())
            .mime_str(mime_type)
            .map_err(|e| Error::Unreadable(format!("{mime_type} is not a media type: {e}")))?;

        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("sha256", sha256);

        let response = self
            .bearer(self.http.post(self.url("runner/files")))
            .multipart(form)
            .send()
            .await?;
        read(accept(response).await?).await
    }

    /// Names an uploaded file on the attempt this Runner holds.
    ///
    /// **Before the report, never after.** The Server requires the job to be
    /// `Running`, and reporting ends that — so an attachment named afterwards
    /// is refused and the log a person needs to read the failure is the thing
    /// that goes missing.
    pub async fn attach_to_job(&self, job_id: &str, attach: &AttachToJob) -> Result<()> {
        let response = self
            .bearer(
                self.http
                    .post(self.url(&format!("runner/jobs/{job_id}/files"))),
            )
            .json(attach)
            .send()
            .await?;
        accept(response).await.map(|_| ())
    }

    /// Names an uploaded file on the Runner itself. Replaces the name rather
    /// than adding another, so a chatty Runner costs a fixed amount.
    pub async fn attach_to_self(&self, attach: &AttachToSelf) -> Result<()> {
        let response = self
            .bearer(self.http.post(self.url("runner/files/attach")))
            .json(attach)
            .send()
            .await?;
        accept(response).await.map(|_| ())
    }

    fn bearer(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.token() {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }
}

/// Turns a refusal into an error while keeping the `code`.
///
/// The status alone is not enough to act on — a `403` is "not approved yet",
/// "revoked", "wrong signature", "spent nonce" and "somebody else has the job",
/// and those want five different responses.
async fn accept(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    // Read before the body is consumed, and kept even when the body is not
    // ours: during a window the answer may come from an edge proxy that has
    // never heard of this product, and the header is then the only thing in it
    // worth acting on.
    let retry_after = retry_after(response.headers());

    // A body that will not parse is not a second failure: nginx answers a 413
    // in HTML, and the status is still the useful part.
    let refusal = response.json::<Refusal>().await.unwrap_or_default();
    Err(Error::Refused {
        status: status.as_u16(),
        refusal: Box::new(refusal),
        retry_after,
    })
}

/// `Retry-After`, seconds form only.
///
/// RFC 9110 also allows an HTTP date. Parsing one would mean trusting two
/// clocks to agree, and the difference between them is unbounded — so a date is
/// treated as "the Server did not say", which falls back to this Runner's own
/// backoff. That is the safe direction: the fallback waits, it does not spin.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(std::time::Duration::from_secs)
}

async fn read<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let bytes = response.bytes().await?;
    serde_json::from_slice(&bytes).map_err(|e| {
        Error::Unreadable(format!(
            "{e} in {}",
            String::from_utf8_lossy(&bytes[..bytes.len().min(256)])
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_url_keeps_its_prefix_however_it_was_written() {
        for written in [
            "http://server:8080/api/v1",
            "http://server:8080/api/v1/",
            "http://server:8080/api/v1///",
        ] {
            let server = Server::new(written).unwrap();
            assert_eq!(
                server.url("runner/register"),
                "http://server:8080/api/v1/runner/register",
            );
            // A leading slash on the path must not reset to the origin, which
            // is exactly what `Url::join` would do and what cost the Client a
            // round of 404s.
            assert_eq!(
                server.url("/runner/register"),
                "http://server:8080/api/v1/runner/register",
            );
        }
    }

    #[test]
    fn a_report_without_a_score_sends_no_score_at_all() {
        let failed = ReportResult::failed("a-lease", "package checksum mismatch");
        let json = serde_json::to_value(&failed).unwrap();

        assert_eq!(json["infrastructureFailure"], serde_json::json!(true));
        // Absent, not null. The Server omits nulls and so must this: an
        // explicit null is a different document, and `score: 0` would be a lie
        // about a solution that was never judged.
        assert!(json.get("score").is_none());
        assert!(json.get("maxScore").is_none());
        assert!(json.get("verdict").is_none());
    }

    fn header(value: &str) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
        headers
    }

    #[test]
    fn retry_after_is_read_in_seconds_and_ignored_in_any_other_form() {
        assert_eq!(
            retry_after(&header("300")),
            Some(std::time::Duration::from_secs(300)),
        );
        assert_eq!(retry_after(&header(" 30 ")), Some(FIVE_SECONDS * 6));

        // An HTTP date is legal and deliberately not read: acting on it means
        // trusting two clocks to agree. Absent means "the Server did not say",
        // and the caller's own backoff decides — which waits rather than spins.
        assert_eq!(retry_after(&header("Wed, 09 Aug 2026 18:00:00 GMT")), None);
        assert_eq!(retry_after(&header("")), None);
        assert_eq!(retry_after(&reqwest::header::HeaderMap::new()), None);
    }

    const FIVE_SECONDS: std::time::Duration = std::time::Duration::from_secs(5);

    #[test]
    fn a_health_document_without_maintenance_is_an_open_server() {
        let open: Health = serde_json::from_value(serde_json::json!({ "status": "ok" })).unwrap();
        assert!(open.open());
        assert_eq!(open.level(), "open");
        assert_eq!(open.reason(), None);

        let draining: Health = serde_json::from_value(serde_json::json!({
            "status": "ok",
            "maintenance": { "level": "draining", "reason": "nightly backup" }
        }))
        .unwrap();
        assert!(!draining.open(), "no new work while the Server drains");
        assert_eq!(draining.reason(), Some("nightly backup"));

        // A level this build has never heard of is **not open** and not an
        // error. Refusing to parse it would turn a Server that added a level
        // into a Runner that cannot read health at all — during a window, when
        // health is the only thing it can read.
        let later: Health = serde_json::from_value(serde_json::json!({
            "status": "ok",
            "maintenance": { "level": "hibernating" }
        }))
        .unwrap();
        assert!(!later.open());
        assert_eq!(later.level(), "hibernating");
    }

    #[test]
    fn a_job_without_a_package_is_recognised_rather_than_fetched() {
        let job: ClaimedJob = serde_json::from_value(serde_json::json!({
            "jobId": "j", "submissionId": "s", "attempt": 1,
            "leaseToken": "t", "leaseExpiresAt": "2026-08-08T10:00:00Z",
            "problemType": "standard-io@1", "problemVersionId": "v",
            "packageFileId": "", "packageSha256": "",
            "files": []
        }))
        .unwrap();

        assert!(!job.has_package());
        assert!(job.language.is_none(), "absent is not an error");
        assert!(job.config.is_none());
    }
}
