//! Why a package was refused.
//!
//! Every variant here is an **infrastructure failure**, never a verdict. A
//! package the Runner cannot read says nothing about the solution somebody
//! submitted, and scoring it as a zero would be a fabricated statement about
//! their work.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the archive could not be opened: {0}")]
    NotAnArchive(#[from] zip::result::ZipError),

    /// One of the extraction defences said no. Each carries what it saw,
    /// because "the package is bad" is not something an author can act on.
    #[error("{0}")]
    Refused(String),

    #[error("config.yml is missing from the package")]
    NoConfig,

    #[error("config.yml could not be read: {0}")]
    Malformed(#[from] serde_yaml_ng::Error),

    /// A Runner that does not know the version **refuses the package** rather
    /// than guessing at what changed.
    #[error("this package is {format}@{version}, and this Runner reads standard-io@1")]
    UnknownFormat { format: String, version: u32 },

    #[error("{0}")]
    Invalid(String),

    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    pub(crate) fn refused(what: impl Into<String>) -> Self {
        Error::Refused(what.into())
    }

    pub(crate) fn invalid(what: impl Into<String>) -> Self {
        Error::Invalid(what.into())
    }
}
