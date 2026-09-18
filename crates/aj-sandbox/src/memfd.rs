//! The test's input, as a file that exists only in memory.
//!
//! **A judged submission is handed a descriptor and never a path.** What is
//! behind it is a `memfd` — an anonymous file in tmpfs — filled from the
//! package's `<test>.in` and then **sealed**, so the program on the other end
//! can read it, seek in it and map it privately, and can do nothing else to it.
//!
//! Three things it buys, and each of them was a choice with an alternative:
//!
//! - **Seekable**, where a pipe is not. A solution that reads its input twice —
//!   `rewind`, a second pass, `mmap` in a fast-input template — works on its
//!   author's machine, and a pipe would fail it here for a reason the
//!   participant cannot see. That objection is why the input was a mounted file
//!   for so long.
//! - **Nothing on disk, and nothing copied per job.** The bytes are read once
//!   out of the shared cache and never written back down; a test file of any
//!   size costs one copy into memory rather than a copy into every job's
//!   scratch.
//! - **No shared inode.** A mounted file out of the cache would be one inode
//!   held by every submission to that problem at once, which is a channel
//!   between contestants — file locks, `F_NOTIFY` — that `docs/SECURITY.md` §6
//!   names and refuses. Each run gets a memory file of its own.
//!
//! **The seals are the defense.** A memfd's inode is created world-writable and
//! owned by whoever made it, so a submission can reopen its own standard input
//! through `/proc/self/fd/0` and ask for `O_RDWR`; what stops it writing is
//! `F_SEAL_WRITE`, and what stops it changing the length is `F_SEAL_SHRINK`
//! and `F_SEAL_GROW`. The read-only re-open below is not a second lock — it
//! only gives the descriptor an offset of its own.

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;

/// One run's input, sealed, waiting to be handed over.
#[derive(Debug)]
pub struct SealedInput {
    file: std::fs::File,
}

impl SealedInput {
    /// Reads a file into memory and seals what it made.
    ///
    /// **`io::copy` on a blocking thread**, rather than anything cleverer:
    /// `copy_file_range` into tmpfs answers `EXDEV` on every maintained kernel,
    /// and a test file may be a quarter of a gigabyte — which is not an amount
    /// of work to do on a thread that is also driving containers.
    pub async fn from_file(path: &Path) -> io::Result<Self> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::read(&path))
            .await
            .map_err(io::Error::other)?
    }

    fn read(path: &Path) -> io::Result<Self> {
        let name = c"aj-input";
        // SAFETY: `name` is a valid C string that outlives the call.
        let made = unsafe {
            libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
        };
        if made < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `made` is a fresh descriptor this call owns.
        let mut file = unsafe { std::fs::File::from_raw_fd(made) };

        let mut source = std::fs::File::open(path)?;
        io::copy(&mut source, &mut file)?;

        // **Before the descriptor is shared, and it cannot be undone.**
        // `F_SEAL_SEAL` closes the door behind the other three, so nothing that
        // holds this file afterwards can lift them.
        let seals =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        // SAFETY: the descriptor is open and owned by `file`.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(io::Error::last_os_error());
        }

        // **A description of its own, read-only and at offset zero.** The one
        // above was written through, so it is at the end of the file; a
        // submission handed it would read nothing at all.
        let read_only = std::fs::File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        Ok(Self { file: read_only })
    }

    /// Gives it to whoever opens the socket, and lets go of it.
    ///
    /// **One connection, and then the socket is closed.** The far end is the
    /// measuring shim, which is still root when it connects and is the only
    /// thing that can reach the directory the socket sits in; the submission it
    /// goes on to start is `nobody` and finds neither the socket nor a second
    /// chance to ask for one.
    ///
    /// The Runner's own copy goes with this task, so once the container has the
    /// descriptor the pages belong to the container alone and are freed with
    /// it.
    pub fn hand_over(
        self,
        listener: tokio::net::UnixListener,
    ) -> tokio::task::JoinHandle<io::Result<()>> {
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            // Blocking, and it cannot block: this is one control message onto a
            // socket buffer nothing has written to yet.
            let stream = stream.into_std()?;
            stream.set_nonblocking(false)?;
            // SAFETY: both descriptors are open and owned for the whole call.
            unsafe { send(stream.as_raw_fd(), self.file.as_raw_fd()) }
        })
    }
}

/// Sends one descriptor over a Unix socket, with a byte to carry it.
///
/// `SCM_RIGHTS` needs something in the ordinary payload — a control message on
/// its own is not delivered — so one byte travels with it and the far end
/// throws it away.
///
/// # Safety
///
/// Both descriptors must be open for the duration of the call.
unsafe fn send(socket: RawFd, payload: RawFd) -> io::Result<()> {
    let mut byte = [0u8; 1];
    let mut carrier = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };

    // Eight-byte aligned, which is what a control message header needs, and
    // larger than the one descriptor this ever carries.
    let mut control = [0u64; 4];
    let mut message: libc::msghdr = std::mem::zeroed();
    message.msg_iov = &mut carrier;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as _;

    let header = libc::CMSG_FIRSTHDR(&message);
    if header.is_null() {
        return Err(io::Error::other("no room for the control message"));
    }
    (*header).cmsg_level = libc::SOL_SOCKET;
    (*header).cmsg_type = libc::SCM_RIGHTS;
    (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
    std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), payload);

    // **`MSG_NOSIGNAL`, so a shim that died between connecting and reading is
    // an error rather than a signal.** Rust ignores `SIGPIPE` at start, which
    // would cover it; a raw syscall should not depend on that.
    let sent = libc::sendmsg(socket, &message, libc::MSG_NOSIGNAL);
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Seek as _, Write as _};

    fn a_file(name: &str, body: &[u8]) -> std::path::PathBuf {
        let at = std::env::temp_dir().join(format!("aj-memfd-{}-{name}", std::process::id()));
        std::fs::write(&at, body).expect("an input file");
        at
    }

    #[tokio::test]
    async fn it_carries_what_the_file_held() {
        let input = SealedInput::from_file(&a_file("read", b"42 the input line\n"))
            .await
            .expect("a sealed input");

        let mut read = String::new();
        (&input.file).read_to_string(&mut read).expect("read it");
        assert_eq!(read, "42 the input line\n");
    }

    /// **The whole reason this is not a pipe.** A solution that reads its input
    /// twice works on its author's machine, and would fail here for a reason
    /// the participant could not see.
    #[tokio::test]
    async fn it_can_be_read_again_from_the_start() {
        let input = SealedInput::from_file(&a_file("seek", b"7 9\n"))
            .await
            .expect("a sealed input");
        let mut file = &input.file;

        let mut first = String::new();
        file.read_to_string(&mut first).unwrap();
        file.rewind().expect("a memory file seeks");
        let mut again = String::new();
        file.read_to_string(&mut again).unwrap();

        assert_eq!(first, again);
        assert_eq!(first, "7 9\n");
    }

    /// **The seals are what hold, and this is the descriptor that tests them.**
    ///
    /// A submission is handed the read-only one, on which a `write` fails with
    /// `EBADF` whether or not anything is sealed — so a test written against it
    /// passes with the sealing deleted. What a submission can actually do is
    /// reopen its own standard input through `/proc/self/fd`, asking for
    /// `O_RDWR`: a memfd's inode is created world-writable and the open
    /// succeeds. Every way of changing it from there has to fail.
    #[tokio::test]
    async fn a_submission_cannot_change_its_own_input() {
        let input = SealedInput::from_file(&a_file("seal", b"1 2\n"))
            .await
            .expect("a sealed input");

        let mut writable = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/proc/self/fd/{}", input.file.as_raw_fd()))
            .expect("a memfd may be reopened for writing; the seals are what refuse");

        let refused = writable.write_all(b"0 0\n").expect_err("F_SEAL_WRITE");
        assert_eq!(refused.raw_os_error(), Some(libc::EPERM), "{refused}");

        assert_eq!(
            writable
                .set_len(0)
                .expect_err("F_SEAL_SHRINK")
                .raw_os_error(),
            Some(libc::EPERM),
        );
        assert_eq!(
            writable
                .set_len(4096)
                .expect_err("F_SEAL_GROW")
                .raw_os_error(),
            Some(libc::EPERM),
        );

        // And a shared writable mapping, which is a write by another road.
        // SAFETY: the descriptor is open, and the call is expected to fail.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                writable.as_raw_fd(),
                0,
            )
        };
        assert_eq!(
            mapped,
            libc::MAP_FAILED,
            "a shared writable mapping is a write"
        );

        // A private one still works, which is what a fast-input template does.
        // SAFETY: the descriptor is open; the mapping is unmapped below.
        let private = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                writable.as_raw_fd(),
                0,
            )
        };
        assert_ne!(
            private,
            libc::MAP_FAILED,
            "a solution may still map its input"
        );
        // SAFETY: `private` is a mapping this test made.
        unsafe { libc::munmap(private, 4) };
    }

    /// The hand-over, with this test standing in for the shim.
    #[tokio::test]
    async fn the_descriptor_arrives_on_the_socket() {
        let at = std::env::temp_dir().join(format!("aj-memfd-{}-hand.socket", std::process::id()));
        let _ = std::fs::remove_file(&at);
        let listener = tokio::net::UnixListener::bind(&at).expect("a socket");

        let input = SealedInput::from_file(&a_file("hand", b"99\n"))
            .await
            .expect("a sealed input");
        let handing = input.hand_over(listener);

        let received = tokio::task::spawn_blocking(move || {
            let socket = std::os::unix::net::UnixStream::connect(&at).expect("connect");
            // SAFETY: the socket is open and owned for the whole call.
            let fd = unsafe { receive(socket.as_raw_fd()) }.expect("a descriptor");
            // SAFETY: `fd` came from `SCM_RIGHTS` and is owned here.
            let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut said = String::new();
            file.read_to_string(&mut said).expect("read it");
            said
        })
        .await
        .expect("the receiver");

        handing.await.expect("the task").expect("the hand-over");
        assert_eq!(received, "99\n");
    }

    /// The other half of [`send`], for the test above alone. The shim's own is
    /// in C.
    ///
    /// # Safety
    ///
    /// The socket must be open for the duration of the call.
    unsafe fn receive(socket: RawFd) -> io::Result<RawFd> {
        let mut byte = [0u8; 1];
        let mut carrier = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut control = [0u64; 4];
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut carrier;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as _;

        if libc::recvmsg(socket, &mut message, libc::MSG_CMSG_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() || (*header).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::other("nothing was handed over"));
        }
        Ok(std::ptr::read_unaligned(
            libc::CMSG_DATA(header).cast::<RawFd>(),
        ))
    }
}
