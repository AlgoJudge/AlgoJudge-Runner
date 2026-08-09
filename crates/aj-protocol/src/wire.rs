//! The shapes on the wire.
//!
//! Two rules govern every type here, and both come from the Server's
//! serializer rather than from taste:
//!
//! - **camelCase**, because the Server uses `JsonSerializerDefaults.Web`.
//! - **Absent, never `null`.** `DefaultIgnoreCondition = WhenWritingNull` means
//!   an optional value the Server does not have is simply not in the document.
//!   So every optional field is `Option<T>` with `#[serde(default)]`, and
//!   nothing here may send an explicit `null` either.

use serde::{Deserialize, Serialize};

// ── Whether the Server is serving ───────────────────────────────────────────

/// What `/health` answers, at every level.
///
/// **It answers `200` even while the Server is refusing everything else**, which
/// is what makes a window escapable: this is the one path a Runner can ask
/// during one, and the answer says how far the Server has withdrawn and why.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub status: String,
    /// **Absent while the Server is open**, so a Runner built before maintenance
    /// existed reads exactly the document it always read.
    #[serde(default)]
    pub maintenance: Option<Maintenance>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Maintenance {
    /// `open` | `draining` | `closed`.
    ///
    /// A string rather than an enum, and matched rather than parsed: a level
    /// this Runner has never heard of must not be a parse failure, because the
    /// safe reading of an unknown level is "not open" and an error would be
    /// "the Server is broken".
    pub level: String,
    /// When the operator asked, as the Server states it. Shown, never computed
    /// with — the two clocks are not the same clock.
    #[serde(default)]
    pub since: Option<String>,
    /// What the operator typed. Repeated into the log verbatim.
    #[serde(default)]
    pub reason: Option<String>,
}

impl Health {
    /// Whether there is any point asking for work.
    ///
    /// `draining` counts as closed here even though the Server would still take
    /// a report: it hands out nothing new, so a Runner with empty hands has
    /// nothing to do but wait.
    pub fn open(&self) -> bool {
        match &self.maintenance {
            None => true,
            Some(maintenance) => maintenance.level == "open",
        }
    }

    pub fn level(&self) -> &str {
        self.maintenance
            .as_ref()
            .map(|m| m.level.as_str())
            .unwrap_or("open")
    }

    pub fn reason(&self) -> Option<&str> {
        self.maintenance.as_ref()?.reason.as_deref()
    }
}

// ── Registration ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Register {
    pub name: String,
    pub product: String,
    pub version: String,
    /// The raw 32 bytes of the Ed25519 public key, base64. **Not** an SPKI
    /// wrapper — the Server takes the bytes straight into
    /// `Ed25519PublicKeyParameters`, and an SPKI blob is 44 bytes and refused.
    pub public_key: String,
    /// Matched by string equality and never parsed, which is what keeps
    /// "adding a problem type is not a Server change" true.
    pub problem_types: Vec<String>,
    /// Host facts, stored opaquely and only ever shown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registered {
    pub runner_id: String,
    pub fingerprint: String,
    /// `pendingApproval` | `approved` | `revoked`.
    pub state: String,
}

impl Registered {
    pub fn approved(&self) -> bool {
        self.state == "approved"
    }
}

// ── The handshake ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeRequest {
    pub fingerprint: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Challenge {
    /// Single-use and short-lived. Both matter: without single use a captured
    /// exchange replays for ever, and without expiry one captured today works
    /// next year.
    pub nonce: String,
    pub expires_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenRequest {
    pub fingerprint: String,
    pub nonce: String,
    /// Ed25519 over the UTF-8 bytes of the nonce, base64.
    pub signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    pub token: String,
    pub expires_at: String,
}

// ── Claiming ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_seconds: Option<u32>,
}

/// One trial: a package to time, belonging to no problem.
///
/// Deliberately not `ClaimedJob` with empty fields. A trial has no submission,
/// no attempt and no problem version, and a shared type would carry four
/// `Option`s that mean "this is the other kind" — which every reader would then
/// have to check, and one would forget.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimedTrial {
    pub trial_id: String,
    pub lease_token: String,
    /// **Authoritative**, as for a job: the request was a suggestion.
    pub lease_expires_at: String,
    pub problem_type: String,
    pub package_file_id: String,
    pub package_sha256: String,
}

/// What a trial produced, or why it produced nothing.
///
/// **Never both, and never a score.** A trial that failed says so; a trial that
/// worked carries a measurement the Server stores without reading.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrialReport {
    pub lease_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrialLease {
    pub trial_id: String,
    pub lease_token: String,
    pub lease_expires_at: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrialReportAccepted {
    pub trial_id: String,
    pub state: String,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimedJob {
    pub job_id: String,
    pub submission_id: String,
    pub attempt: i32,
    pub lease_token: String,
    /// **Authoritative.** `leaseSeconds` was a request; the Server clamps it to
    /// `[60, 3600]`, so a Runner that renews on its own arithmetic renews on a
    /// number the Server never agreed to.
    pub lease_expires_at: String,

    pub problem_type: String,
    pub problem_version_id: String,
    /// The **empty string**, not absent, when the problem version carries no
    /// package. Nothing to judge against — an infrastructure failure, not a
    /// verdict, and never a download of an empty id.
    pub package_file_id: String,
    pub package_sha256: String,
    pub files: Vec<SubmissionFile>,
    #[serde(default)]
    pub language: Option<String>,
    /// The merged chain — package, then problem version, then assignment. The
    /// Server merged documents it never read.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

impl ClaimedJob {
    /// Whether there is a package at all. See `package_file_id`.
    pub fn has_package(&self) -> bool {
        !self.package_file_id.is_empty() && !self.package_sha256.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionFile {
    /// The role within the submission — `source`.
    pub name: String,
    pub file_name: String,
    #[serde(default)]
    pub language: Option<String>,
    pub file_id: String,
    pub sha256: String,
    pub size_bytes: i64,
}

// ── The lease ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRequest {
    pub lease_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_seconds: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub job_id: String,
    pub lease_token: String,
    pub lease_expires_at: String,
}

// ── Reporting ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportResult {
    pub lease_token: String,
    /// The evaluation itself failed — a package whose checksum did not match, a
    /// sandbox that would not start. **Not a wrong answer, and it must never be
    /// scored as one.**
    pub infrastructure_failure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,

    /// The Runner's own scale, before the assignment's rescaling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_score: Option<f64>,
    /// Opaque to the Server, which never branches on it — which is what lets a
    /// problem type introduce a verdict without a Server release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_version: Option<String>,
    /// Bounded at 2 kB by the Server, and **refused rather than truncated**.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

impl ReportResult {
    pub fn judged(lease_token: &str, score: f64, max_score: f64, verdict: &str) -> Self {
        Self {
            lease_token: lease_token.to_owned(),
            infrastructure_failure: false,
            failure_reason: None,
            score: Some(score),
            max_score: Some(max_score),
            verdict: Some(verdict.to_owned()),
            runner_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            extra: None,
        }
    }

    /// The submission was never judged. No score is sent, because a zero here
    /// would read as a wrong answer on every board that shows it.
    pub fn failed(lease_token: &str, reason: impl Into<String>) -> Self {
        Self {
            lease_token: lease_token.to_owned(),
            infrastructure_failure: true,
            failure_reason: Some(reason.into()),
            score: None,
            max_score: None,
            verdict: None,
            runner_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            extra: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportAccepted {
    pub result_id: String,
    pub state: String,
    /// This report was a repeat and the stored result came back unchanged. Not
    /// an error — it is the whole point of reporting being idempotent.
    pub duplicate: bool,
}

// ── Files ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadedFile {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub created_at: String,
}

/// Naming a file already uploaded, on the attempt this Runner holds.
///
/// The order is fixed and cannot be worked around: the Server requires the job
/// to be `Running`, and reporting ends that. **Upload, attach, then report.**
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachToJob {
    pub lease_token: String,
    pub file_id: String,
    /// `log`, `details`. The role within the attempt, not the file name.
    pub name: String,
}

/// Naming a file the Runner uploaded about itself. Replaces the name.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachToSelf {
    pub file_id: String,
    /// `runner.log`, `lscpu.txt`.
    pub name: String,
}
