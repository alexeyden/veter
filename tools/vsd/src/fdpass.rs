//! `SCM_RIGHTS` helpers for handing stdio fds from the CLI to the
//! daemon over the Unix-domain socket used by the IPC layer.
//!
//! The session manager design (`doc/session-manager.md` §2.4) calls
//! for the `vsd attach <name>` CLI to:
//!
//! 1. Connect to the daemon socket.
//! 2. Send `Request::Attach { name }` over it.
//! 3. Pass its stdin and stdout fds to the daemon as one
//!    `SCM_RIGHTS` ancillary-data message (in that order).
//! 4. Wait for the `Response::Ok` reply.
//! 5. Exit.
//!
//! From step 3 onward the daemon owns the renderer's stdio fds. The
//! CLI exit is the desired "no long-lived attach middleman" property —
//! the SSH PTY is glued straight to the daemon.
//!
//! Most BSD/Linux socket implementations require `sendmsg(2)` with
//! ancillary data to also carry at least one ordinary data byte, so
//! both helpers transmit a single sentinel byte (`b'F'`, "fds")
//! alongside the cmsg.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use nix::sys::socket::{
    recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags,
};

/// Send a pair of fds (stdin, stdout) to the peer of `sock` as a
/// single `SCM_RIGHTS` ancillary-data message, prefixed with a
/// one-byte filler so the kernel accepts the cmsg.
pub fn send_stdio<S: AsRawFd>(sock: &S, stdin: RawFd, stdout: RawFd) -> io::Result<()> {
    let fds = [stdin, stdout];
    let cmsgs = [ControlMessage::ScmRights(&fds)];
    let buf = [b'F'];
    let iov = [io::IoSlice::new(&buf)];
    sendmsg::<()>(sock.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
        .map(|_| ())
        .map_err(io::Error::other)
}

/// Receive a pair of fds (stdin, stdout) from the peer of `sock`,
/// expecting the [`send_stdio`] framing. The two `OwnedFd`s returned
/// are dup'd by the kernel (the sender's originals are independent of
/// these copies; either side can `close(2)` without affecting the
/// other).
///
/// Returns an error if no fds arrived, the wrong count arrived, or
/// the peer closed before sending.
pub fn recv_stdio<S: AsRawFd>(sock: &S) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut buf = [0u8; 1];
    let mut iov = [io::IoSliceMut::new(&mut buf)];
    let mut cmsg_buf = nix::cmsg_space!([RawFd; 2]);
    let msg = recvmsg::<()>(
        sock.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg_buf),
        MsgFlags::empty(),
    )
    .map_err(io::Error::other)?;
    if msg.bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed before sending stdio fds",
        ));
    }
    let mut received: Vec<RawFd> = Vec::new();
    for cmsg in msg.cmsgs().map_err(io::Error::other)? {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            received.extend(fds);
        }
    }
    if received.len() != 2 {
        // The kernel hands out fds even when the cmsg is malformed; if
        // we got the wrong count we still need to close the extras so
        // they don't leak.
        for fd in received {
            // SAFETY: each fd was just allocated by recvmsg.
            unsafe { drop(OwnedFd::from_raw_fd(fd)) };
        }
        return Err(io::Error::other(
            "expected 2 stdio fds via SCM_RIGHTS, got a different count",
        ));
    }
    // SAFETY: recvmsg returned fresh fds we now own.
    let stdin = unsafe { OwnedFd::from_raw_fd(received[0]) };
    let stdout = unsafe { OwnedFd::from_raw_fd(received[1]) };
    Ok((stdin, stdout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    #[test]
    fn round_trip_stdio_over_socketpair() {
        let (a, b) = UnixStream::pair().expect("socketpair");

        // Create a real pair of file descriptors to round-trip: a pipe.
        // We send the pipe's read and write ends through SCM_RIGHTS.
        let (pipe_r, pipe_w) = nix::unistd::pipe().expect("pipe");

        send_stdio(&a, pipe_r.as_raw_fd(), pipe_w.as_raw_fd())
            .expect("send_stdio");
        let (got_r, got_w) = recv_stdio(&b).expect("recv_stdio");

        // The originals stay valid in this thread; close them so the
        // duped fds in got_r/got_w are the only references. Otherwise
        // the pipe stays open and the read below blocks.
        drop(pipe_r);
        drop(pipe_w);

        // Sanity: writing to got_w shows up on got_r.
        let mut writer = std::fs::File::from(got_w);
        writer.write_all(b"ping").expect("write to fd-passed pipe");
        drop(writer);
        let mut reader = std::fs::File::from(got_r);
        let mut s = String::new();
        reader.read_to_string(&mut s).expect("read fd-passed pipe");
        assert_eq!(s, "ping");
    }
}
