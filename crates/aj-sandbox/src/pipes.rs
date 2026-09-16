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
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Lets go of a reader waiting for a writer that will never come.
///
/// Opening the far end for an instant is what ends the wait: the open succeeds
/// only if somebody has the pipe open on the other side, and the close that
/// follows immediately reaches them as an ordinary end of file.
///
/// **This used to be the only way out, and that is what made it dangerous.**
/// Every path out of a run had to remember to call it, or a blocking thread
/// waited for ever — a rule kept in comments across four channels, which failed
/// twice in as many days: the interactor's verdict, and a judged run's own
/// output. The second was worse than a leaked thread, because `release` was
/// called and *landed on nobody*: the thread was still inside a bounded wait for
/// something else, and reached the open it needed rescuing from afterwards.
///
/// [`open_for_reading`] has its own deadline now, so this is an optimisation —
/// it ends a wait at once when the Runner already knows nothing is coming —
/// rather than the thing correctness rests on. Calling it is still right on
/// every path out; forgetting it is no longer fatal.
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

/// Opens a pipe for reading, waiting — with a deadline — for a writer to exist.
///
/// **The mirror of [`open_for_writing`], and it exists so that a reader needs no
/// rescue.** A blocking `O_RDONLY` open waits for a writer for ever, which is
/// safe only as long as every path out of every run remembers to call
/// [`release`]. That rule held in comments across four channels and failed
/// twice: once on the interactor's verdict, once on a judged run's own output,
/// each time as a Runner that never reported and re-claimed its job for ever.
/// A reader that cannot wait for ever needs nobody to remember anything.
///
/// **Why a read and not a `poll`.** Measured on Linux 6.18: `poll` reports
/// nothing at all both when no writer has opened the pipe and when one has and
/// is silent, and raises `POLLHUP` only once a writer has been and gone — so it
/// cannot tell the two apart. A non-blocking `read` can: **zero** means no
/// writer holds it, `EAGAIN` means one does and has said nothing. That is the
/// whole of the wait.
///
/// The read is issued only where `poll` has just said there is nothing to read,
/// so it consumes nothing in the ordinary case. Data can still race in between
/// the two, and what it consumed then comes back beside the descriptor rather
/// than being dropped — the alternative is a lost first line, which would be a
/// wrong verdict.
///
/// `O_NONBLOCK` is cleared before returning, so the reads that follow **block**
/// as they always did. A reader that spun on `EAGAIN` would turn the pipe's
/// back-pressure into a busy loop, which is the thing the whole arrangement
/// rests on not doing.
pub fn open_for_reading(at: &Path, waiting: Duration) -> io::Result<(std::fs::File, Vec<u8>)> {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::io::AsRawFd as _;

    // Never blocks in either direction, whoever is or is not at the far end.
    let open = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(at)?;

    let until = std::time::Instant::now() + waiting;
    let mut first = Vec::new();
    loop {
        let mut watched = libc::pollfd {
            fd: open.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one descriptor, owned by `open` and open for the call.
        let ready = unsafe { libc::poll(&mut watched, 1, 5) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        // Something to read, or a writer that has already finished: either way
        // one arrived, and nothing here had to touch the stream to learn it.
        if watched.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            break;
        }

        let mut buffer = [0u8; 64 * 1024];
        // SAFETY: `buffer` is owned here and its length is its length.
        let read = unsafe {
            libc::read(
                open.as_raw_fd(),
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len(),
            )
        };
        if read > 0 {
            first.extend_from_slice(&buffer[..read as usize]);
            break;
        }
        if read < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                // A writer holds it and has said nothing yet. It has arrived,
                // which is all this was waiting for.
                Some(libc::EAGAIN) => break,
                Some(libc::EINTR) => continue,
                _ => return Err(e),
            }
        }
        // Zero: nobody holds the writing end. Not yet, or not ever.
        if std::time::Instant::now() >= until {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "nothing opened {} for writing within {waiting:?}",
                    at.display()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    // SAFETY: the descriptor is open and owned by `open`.
    unsafe {
        let flags = libc::fcntl(open.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(open.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((open, first))
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

/// One Unix socket, removed when this is dropped.
///
/// **A socket where the neighbours are pipes**, because what travels on it is
/// not bytes but a descriptor: the test's input, as a sealed file in memory.
/// See [`crate::memfd`] for what is on the other end of it and why.
pub struct Socket {
    at: PathBuf,
}

impl Socket {
    /// Makes one, and hands back something to accept on.
    ///
    /// **Bound through the directory rather than by its own path**, and that is
    /// not a flourish: a socket address is `sun_path`, **108 bytes including
    /// the terminator**, where a pipe has the filesystem's own limit. A job's
    /// channels sit at `<work>/job-<id>/out/<test>/run/`, which is already
    /// about ninety bytes with a two-character test name — and an operator
    /// choosing a longer `AJ_Pipes__Path`, or a package naming a test `12ab`,
    /// would push every batch test past it. Opening the directory and binding
    /// at `/proc/self/fd/<n>/<name>` makes the address about twenty-five bytes
    /// whatever the directory is called.
    ///
    /// `mode` is set afterwards for the reason [`Fifo::make`] gives: `bind`
    /// applies the process umask, so the mode asked for is not the mode made.
    /// The submission's own channels are `0o600` in a directory that is root's,
    /// so the program — which is `nobody` by the time it runs — cannot open
    /// them by name even though it can walk to them.
    pub fn make(at: impl Into<PathBuf>, mode: u32) -> io::Result<(Self, tokio::net::UnixListener)> {
        let at = at.into();
        // Whatever is there is from an attempt that did not finish, and a
        // socket somebody else may still hold an end of is worse than none.
        let _ = std::fs::remove_file(&at);

        let (directory, name) = match (at.parent(), at.file_name()) {
            (Some(directory), Some(name)) => (directory, name),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} does not name a socket in a directory", at.display()),
                ))
            }
        };

        let held = opened(directory)?;
        // SAFETY: `held` is open and owned for the whole of this scope.
        let short = format!(
            "/proc/self/fd/{}/{}",
            held.as_raw_fd(),
            name.to_string_lossy()
        );

        let listener = std::os::unix::net::UnixListener::bind(&short).map_err(|why| {
            io::Error::new(
                why.kind(),
                format!(
                    "could not make the socket {}: {why}. It has to be on a \
                     filesystem that supports one, which is what AJ_Pipes__Path \
                     is for",
                    at.display()
                ),
            )
        })?;
        std::fs::set_permissions(&short, std::fs::Permissions::from_mode(mode))?;
        listener.set_nonblocking(true)?;

        Ok((Self { at }, tokio::net::UnixListener::from_std(listener)?))
    }

    pub fn path(&self) -> &Path {
        &self.at
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.at) {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::warn!(path = %self.at.display(), %e, "a socket was left behind");
            }
        }
    }
}

/// A directory, opened only to be named again.
///
/// `O_PATH` because nothing is read or written through it: it exists so that
/// `/proc/self/fd/<n>` can stand in for a path too long to be a socket address.
fn opened(directory: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd as _;

    let path = CString::new(directory.as_os_str().as_encoded_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} contains a zero byte", directory.display()),
        )
    })?;
    // SAFETY: `path` is a valid C string that outlives the call.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor this call owns.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
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

    /// Reading a whole channel the way every caller of `open_for_reading` does.
    fn drained(at: &Path, waiting: Duration) -> io::Result<Vec<u8>> {
        use std::io::Read as _;
        let (mut open, mut said) = open_for_reading(at, waiting)?;
        open.read_to_end(&mut said)?;
        Ok(said)
    }

    #[test]
    fn a_reader_gives_up_on_a_writer_that_never_comes() {
        let at = somewhere("never").join("stdout");
        let fifo = Fifo::make(&at, 0o600).expect("a pipe");

        let began = std::time::Instant::now();
        let answer = open_for_reading(fifo.path(), Duration::from_millis(200));
        let took = began.elapsed();

        // The whole point: this used to be a thread held for the life of the
        // Runner, and the job with it.
        assert_eq!(
            answer.map(|_| ()).unwrap_err().kind(),
            io::ErrorKind::TimedOut,
            "a pipe nobody ever writes has to end the wait, not outlast it",
        );
        assert!(took < Duration::from_secs(5), "it waited {took:?}");
    }

    #[test]
    fn a_reader_waits_for_a_writer_that_is_late() {
        use std::io::Write as _;
        let at = somewhere("late").join("stdout");
        let fifo = Fifo::make(&at, 0o600).expect("a pipe");
        let path = fifo.path().to_path_buf();

        let writing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let mut open = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("the far end");
            open.write_all(b"hello").expect("said");
        });

        let said = drained(fifo.path(), Duration::from_secs(10)).expect("a writer arrived");
        writing.join().expect("the writer finished");
        // Not merely "it did not hang": a first line lost here is a wrong verdict.
        assert_eq!(said, b"hello", "every byte the far end wrote");
    }

    #[test]
    fn a_reader_that_found_its_writer_waits_for_what_it_says_next() {
        use std::io::{Read as _, Write as _};
        let at = somewhere("quiet").join("stdout");
        let fifo = Fifo::make(&at, 0o600).expect("a pipe");
        let path = fifo.path().to_path_buf();

        // **No handshake before the reader, and that is not a style choice.** A
        // blocking `O_WRONLY` open waits for a reader, so a writer that signals
        // "I am open" before this thread opens is a test that deadlocks itself.
        // Opening at once and saying nothing for a while is the shim while the
        // program is still thinking, which is the case worth covering.
        let writing = std::thread::spawn(move || {
            let mut open = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("the far end");
            std::thread::sleep(Duration::from_millis(100));
            open.write_all(b"later").expect("said");
        });

        let (mut open, first) =
            open_for_reading(fifo.path(), Duration::from_secs(10)).expect("a writer holds it");
        assert!(
            first.is_empty(),
            "nothing was said yet, so nothing was taken"
        );

        // **`O_NONBLOCK` has to be off by now.** Left on, this read would answer
        // `EAGAIN` at once and the channel would be reported empty -- a
        // submission judged on output it had not finished writing.
        let mut said = Vec::new();
        open.read_to_end(&mut said).expect("read");
        writing.join().expect("the writer finished");
        assert_eq!(said, b"later");
    }

    #[test]
    fn a_writer_that_opens_and_says_nothing_is_an_empty_channel() {
        let at = somewhere("silent").join("stdout");
        let fifo = Fifo::make(&at, 0o600).expect("a pipe");
        let path = fifo.path().to_path_buf();

        std::thread::spawn(move || {
            let _ = std::fs::OpenOptions::new().write(true).open(&path);
        });

        let began = std::time::Instant::now();
        let said = drained(fifo.path(), Duration::from_secs(10)).expect("a writer arrived");
        assert_eq!(said, b"", "a program that printed nothing printed nothing");
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "it should not wait out the deadline"
        );
    }

    #[test]
    fn release_still_ends_a_wait_early() {
        let at = somewhere("released").join("stdout");
        let fifo = Fifo::make(&at, 0o600).expect("a pipe");
        let path = fifo.path().to_path_buf();

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            release(&path);
        });

        // `release` is no longer what keeps this from hanging -- the deadline is
        // -- but it is still what ends the wait at once when the Runner already
        // knows nothing will come.
        let began = std::time::Instant::now();
        let said = drained(fifo.path(), Duration::from_secs(30)).expect("released");
        assert_eq!(said, b"");
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "it waited {:?}",
            began.elapsed()
        );
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
