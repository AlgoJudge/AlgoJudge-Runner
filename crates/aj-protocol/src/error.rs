//! What the Server says when it says no.

use std::time::Duration;

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
    Refused {
        status: u16,
        refusal: Box<Refusal>,
        /// What `Retry-After` asked for, where the Server sent one.
        ///
        /// A header rather than part of the envelope, which is why it sits on
        /// the variant instead of in [`Refusal`]: an edge proxy answering while
        /// the Server is unreachable sends this and no body at all.
        retry_after: Option<Duration>,
    },

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

    /// A cache entry was there when it was decided on and gone when it was
    /// used.
    ///
    /// **Retryable, and that is the whole reason it exists.** Eviction runs on
    /// every download and several Runners may share one cache volume, so an
    /// entry can be decided on by one and evicted by another between the two
    /// steps. Fetching it again is the correct and complete answer; the same
    /// bytes are still on the Server. Reported as `Io` this was terminal — the
    /// job landed in `Failed`, which needs a person and a rejudge to undo, for
    /// a race that heals itself.
    #[error("{what} was gone when it was used; it can be fetched again")]
    Vanished { what: String },

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
            Error::Refused {
                status, refusal, ..
            } => Some((status, refusal)),
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

    /// The Server is up and has withdrawn from service on purpose.
    ///
    /// Told apart from an ordinary outage because it means something different
    /// to a person reading the log: nothing is broken, somebody is doing
    /// maintenance, and it will end. The Server names the reason, and this
    /// Runner repeats it rather than inventing one.
    pub fn in_maintenance(&self) -> bool {
        self.code() == "server.maintenance"
    }

    /// The Server cannot serve this request now, whether it said so itself or
    /// an edge proxy said it for it.
    ///
    /// **502 and 504 belong here with 503.** From a Runner's side a proxy that
    /// cannot reach the Server behind it is the same situation as a Server that
    /// is declining to answer: wait, and come back. The distinction matters to
    /// whoever is fixing it, not to the loop that has to keep somebody's
    /// submission from being marked wrong in the meantime.
    pub fn unavailable(&self) -> bool {
        matches!(self.status(), Some(502) | Some(503) | Some(504))
    }

    /// How long the Server asked this Runner to wait, if it said.
    ///
    /// Advice, not an instruction: it is compared against this Runner's own
    /// backoff and the longer of the two wins, so an operator's `Retry-After: 300`
    /// is honoured while a proxy's `Retry-After: 0` cannot turn the retry into a
    /// spin.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Refused { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Worth trying the same request again. A refusal is the Server having
    /// decided; only transport trouble and its own faults are transient.
    pub fn retryable(&self) -> bool {
        match self {
            Error::Transport(_) => true,
            // **429 is retryable, and used not to be.** It is a rate limit — the
            // Server saying "later", which is the definition of transient — and
            // falling through to the terminal arm exited the process into a
            // `restart: unless-stopped` loop that asked harder each time.
            Error::Refused { status, .. } => *status >= 500 || *status == 429,
            // An entry that went away is the one filesystem outcome worth
            // trying again: the bytes are still on the Server, and the Runner
            // reached this point precisely because it wanted them.
            Error::Vanished { .. } => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(status: u16, code: &str) -> Error {
        Error::Refused {
            status,
            refusal: Box::new(Refusal {
                code: Some(code.to_owned()),
                ..Default::default()
            }),
            retry_after: None,
        }
    }

    #[test]
    fn a_rate_limit_is_worth_waiting_out_rather_than_dying_on() {
        assert!(refused(429, "rate.limited").retryable());
        assert!(refused(503, "server.maintenance").retryable());
        assert!(refused(502, "").retryable());

        // Still terminal: these are the Server having decided, and asking again
        // gets the same answer.
        assert!(!refused(403, "runner.revoked").retryable());
        assert!(!refused(409, "job.state").retryable());
        assert!(!refused(400, "").retryable());
    }

    /// The distinction the whole feature rests on. A submission whose package
    /// could not be downloaded because a backup started **was not judged**, and
    /// anything that reports it as judged is reporting a zero somebody did not
    /// earn.
    #[test]
    fn a_window_is_told_apart_from_a_verdict_and_from_a_broken_package() {
        let window = refused(503, "server.maintenance");
        assert!(window.unavailable());
        assert!(window.in_maintenance());

        // A proxy in front of a Server that is simply gone: same decision, and
        // it says nothing about the submission either.
        let gone = refused(504, "");
        assert!(gone.unavailable());
        assert!(!gone.in_maintenance(), "nobody planned this one");

        // The package arrived and was wrong. That is ours to report, and it is
        // not something waiting will fix.
        let corrupt = Error::ChecksumMismatch {
            what: "package.zip".into(),
            expected: "a".into(),
            actual: "b".into(),
        };
        assert!(!corrupt.unavailable());
        assert!(!corrupt.retryable());
    }

    #[test]
    fn an_entry_that_went_away_is_worth_fetching_again() {
        let vanished = Error::Vanished {
            what: "the download of file f".into(),
        };
        assert!(
            vanished.retryable(),
            "this race heals itself; as `Io` it ended the job instead",
        );
    }

    #[test]
    fn retry_after_is_carried_where_the_caller_can_reach_it() {
        let asked = Error::Refused {
            status: 503,
            refusal: Box::new(Refusal::default()),
            retry_after: Some(Duration::from_secs(300)),
        };
        assert_eq!(asked.retry_after(), Some(Duration::from_secs(300)));
        assert_eq!(refused(503, "server.maintenance").retry_after(), None);
    }
}
