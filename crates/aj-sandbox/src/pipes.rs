//! Named pipes, made by the Runner and opened by whoever is at each end.
//!
//! **A pipe rather than a file, because nothing needs to keep what travels on
//! it.** A judged run's output exists to decide a verdict and is then thrown
//! away — it reaches no screen, no document and no attachment. Written to a
//! file it costs a write, a read and a delete; left on the container's own
//! stdout it costs the daemon a JSON-escaped copy of every byte, measured at
//! 76 MB for one flooding submission against a 64 MiB cap. On a pipe it costs
//! nothing at all, and the reader sees it while the program is still running,
//! which is what lets a wrong answer be found at its first differing token.
//!
//! **The Runner makes them; the shim opens what it is given.** A container
//! cannot make one where the daemon can see it — a mount inside its own
//! namespace is invisible outside, measured — so the directory is the Runner's
//! and the naming is the Runner's, and a missing pipe means the Runner did not
//! do its half rather than something for the far end to paper over.

use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Lets go of a reader waiting for a writer that will never come.
///
/// **Every path out of a run has to pass through this**, because the reader is
/// a blocking thread and the thing it is waiting for is a container that failed
/// to start. Opening the far end for an instant is what ends its wait: the open
/// succeeds only if somebody is blocked on the other side, and the close that
/// follows immediately reaches them as an ordinary end of file.
pub fn release(at: &Path) {
    use std::os::unix::fs::OpenOptionsExt as _;
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(at);
}

/// Opens a pipe for writing, waiting for the far end to arrive.
///
/// **Non-blocking and retried, rather than a blocking open, and the asymmetry
/// with the reader is deliberate.** A reader blocked on an open can be let go
/// by opening the writing end for an instant — see [`release`] — because that
/// is a thing the Runner can do on its own. A writer blocked on an open needs
/// somebody to open the *reading* end, and the only candidate is the container
/// that failed to start. So the wait is bounded here instead of relying on a
/// rescue that would have to come from the thing that went wrong.
///
/// `ENXIO` is the whole of the retry: it is what a non-blocking `O_WRONLY` open
/// says when no reader has the pipe open yet, and it is indistinguishable from
/// success arriving a millisecond later.
///
/// `O_NONBLOCK` is cleared once it is open, so the writes that follow **block**
/// when the pipe is full. That is the back-pressure the whole arrangement rests
/// on: a full pipe must stop the producer, not spin the Runner on `EAGAIN`.
pub fn open_for_writing(at: &Path, waiting: Duration) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::io::AsRawFd as _;

    let until = std::time::Instant::now() + waiting;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(at)
        {
            Ok(open) => {
                // SAFETY: the descriptor is open and owned by `open`.
                unsafe {
                    let flags = libc::fcntl(open.as_raw_fd(), libc::F_GETFL);
                    if flags < 0
                        || libc::fcntl(open.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK)
                            < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                return Ok(open);
            }
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {
                if std::time::Instant::now() >= until {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "nothing opened {} for reading within {waiting:?}",
                            at.display()
                        ),
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Lets go of a writer waiting for a reader that will never come.
///
/// **The mirror of [`release`], and the asymmetry is only in the flags.** A
/// non-blocking `O_RDONLY` open of a pipe succeeds whether or not anybody is
/// writing — there is no `ENXIO` in this direction — so this both wakes a
/// blocked writer and returns at once. Which matters: the caller is an async
/// task, and a blocking open here would be the whole runtime waiting on a
/// container that has already gone.
pub fn release_writer(at: &Path) {
    use std::os::unix::fs::OpenOptionsExt as _;
    let _ = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(at);
}

/// One named pipe, removed when this is dropped.
///
/// **Ownership is what the type is for.** A FIFO left behind is a few bytes,
/// but one per test for the life of an installation is a directory nothing
/// prunes — and the run that leaves it is exactly the run that failed, which is
/// when nobody is looking.
#[derive(Debug)]
pub struct Fifo {
    at: PathBuf,
}

impl Fifo {
    /// Makes one, or says why not.
    ///
    /// `mode` is the permission the far end is opened under, and it is not a
    /// detail: the submission's pipes are `0o600` in a directory that is root's,
    /// so a program running as `nobody` cannot open them by name even though it
    /// can walk to them. What it gets is the descriptor the shim opened before
    /// it dropped privileges.
    ///
    /// **Refuses rather than reuses.** Whatever is already at that path is from
    /// a previous attempt, and a pipe somebody else may still hold an end of is
    /// worse than no pipe: the two runs would see each other's bytes.
    pub fn make(at: impl Into<PathBuf>, mode: u32) -> io::Result<Self> {
        let at = at.into();
        let _ = std::fs::remove_file(&at);

        let path = CString::new(at.as_os_str().as_encoded_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a pipe's path may not contain a zero byte: {}",
                    at.display()
                ),
            )
        })?;

        // SAFETY: `path` is a valid C string that outlives the call, and
        // `mkfifo` touches nothing else.
        let made = unsafe { libc::mkfifo(path.as_ptr(), mode as libc::mode_t) };
        if made != 0 {
            let why = io::Error::last_os_error();
            return Err(io::Error::new(
                why.kind(),
                format!(
                    "could not make the pipe {}: {why}. It has to be on a \
                     filesystem that supports one — a bind mount of a Windows or \
                     macOS directory does not",
                    at.display()
                ),
            ));
        }

        // `mkfifo` is masked by the process umask, so the mode asked for is not
        // necessarily the mode made. Said plainly rather than left to surprise
        // somebody reading `0o600` and finding `0o644`.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&at, std::fs::Permissions::from_mode(mode))?;
        }

        Ok(Self { at })
    }

    pub fn path(&self) -> &Path {
        &self.at
    }
}

impl Drop for Fifo {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.at) {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::warn!(path = %self.at.display(), %e, "a pipe was left behind");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    fn somewhere(name: &str) -> PathBuf {
        let at = std::env::temp_dir().join(format!("aj-pipes-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&at).expect("a place to make pipes in");
        at
    }

    #[test]
    fn it_is_a_pipe_and_not_a_file() {
        let at = somewhere("kind").join("stdout");
        let fifo = Fifo::make(&at, 0o600).expect("a pipe");
        let kind = std::fs::metadata(fifo.path())
            .expect("it is there")
            .file_type();
        assert!(
            kind.is_fifo(),
            "a file here would be read as an empty answer"
        );
    }

    #[test]
    fn the_mode_asked_for_is_the_mode_made() {
        // The umask would otherwise decide this, and the whole reason the
        // submission cannot open its own pipes by name is that they are 0600.
        let at = somewhere("mode").join("stdout");
        let fifo = Fifo::make(&at, 0o600).expect("a pipe");
        let mode = std::fs::metadata(fifo.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the umask must not have a say in this");
    }

    #[test]
    fn a_leftover_is_replaced_rather_than_reused() {
        let at = somewhere("stale").join("stdout");
        std::fs::write(&at, b"from a run that failed").expect("a leftover");
        let fifo = Fifo::make(&at, 0o600).expect("a pipe");
        assert!(std::fs::metadata(fifo.path())
            .unwrap()
            .file_type()
            .is_fifo());
    }

    #[test]
    fn dropping_it_takes_it_away() {
        let at = somewhere("drop").join("stdout");
        {
            let _fifo = Fifo::make(&at, 0o600).expect("a pipe");
            assert!(at.exists());
        }
        assert!(!at.exists(), "a pipe per test would otherwise accumulate");
    }

    #[test]
    fn a_path_that_cannot_hold_one_says_where_to_put_it_instead() {
        let at = somewhere("nowhere")
            .join("no-such-directory")
            .join("stdout");
        let why = Fifo::make(&at, 0o600).expect_err("a pipe cannot be made there");
        let said = why.to_string();
        assert!(
            said.contains("filesystem that supports one"),
            "the message has to name the likeliest cause: {said}",
        );
    }
}
