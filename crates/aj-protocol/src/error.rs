//! What the Server says when it says no.

use serde::Deserialize;

pub type Result<T> = std::result::Result<T, Error>;

/// The Server's error envelope.
///
/// Not RFC 7807 — there is no `type` — and the field that matters is `code`.
/// It is stable across releases, which is why every decision here switches on
/// it rather than on the status: a `403` is six different situations and only
/// one of them means "give up".
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Refusal {
    pub title: Option<String>,
    pub detail: Option<String>,
    pub status: Option<u16>,
    pub code: Option<String>,
}

impl Refusal {
    pub fn code(&self) -> &str {
        self.code.as_deref().unwrap_or("")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request never got an answer. Distinct from a refusal on purpose: a
    /// refusal is the Server's decision, and this is the network's.
    #[error("could not reach the Server: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("the Server refused with {status}: {}", refusal.code())]
    Refused { status: u16, refusal: Box<Refusal> },

    /// The bytes did not hash to what the job said they would. Never a verdict
    /// — the submission was not judged, and scoring it would be a lie about
    /// the solution.
    #[error("{what} is {actual}, and the job said {expected}")]
    ChecksumMismatch {
        what: String,
        expected: String,
        actual: String,
    },

    #[error("the identity key at {path} is {actual} bytes; an Ed25519 secret key is 32")]
    MalformedKey { path: String, actual: usize },

    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// An I/O failure that names what it was touching.
    ///
    /// Added after a bare `Permission denied (os error 13)` sent somebody
    /// reading a container log looking for a path the message did not contain.
    /// A Runner's two writable places are a mounted volume each, and getting
    /// their ownership wrong is the ordinary first-run mistake — so the message
    /// has to say which one.
    #[error("{path} could not be {doing}: {source}")]
    Storage {
        path: String,
        doing: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("the Server answered something this Runner cannot read: {0}")]
    Unreadable(String),
}

impl Error {
    fn refusal(&self) -> Option<(&u16, &Refusal)> {
        match self {
            Error::Refused { status, refusal } => Some((status, refusal)),
            _ => None,
        }
    }

    /// The code the Server named, or `""` for anything that is not a refusal.
    pub fn code(&self) -> &str {
        self.refusal().map(|(_, r)| r.code()).unwrap_or("")
    }

    pub fn status(&self) -> Option<u16> {
        self.refusal().map(|(s, _)| *s)
    }

    /// The token has gone. Tokens live in the Server's memory, so this happens
    /// on an ordinary Server restart and means "shake hands again", not "stop".
    pub fn needs_handshake(&self) -> bool {
        self.status() == Some(401)
    }

    /// Registered, or revoked, and either way not evaluating anything. Waiting
    /// is the right answer: an administrator has not got to it yet.
    pub fn not_approved(&self) -> bool {
        self.code() == "runner.notApproved"
    }

    /// The key is finished. There is no rotation, so this is terminal: a new
    /// key means a new configuration and a new registration, which is a
    /// person's decision and not a retry.
    pub fn revoked(&self) -> bool {
        self.code() == "runner.revoked"
    }

    /// Somebody else has the job now. Whatever this Runner computed is no
    /// longer wanted, and pushing it would overwrite a newer attempt.
    pub fn lease_lost(&self) -> bool {
        matches!(
            self.code(),
            "runner.lease.stale" | "runner.lease.foreign" | "job.state"
        )
    }

    /// Worth trying the same request again. A refusal is the Server having
    /// decided; only transport trouble and its own faults are transient.
    pub fn retryable(&self) -> bool {
        match self {
            Error::Transport(_) => true,
            Error::Refused { status, .. } => *status >= 500,
            _ => false,
        }
    }
}
