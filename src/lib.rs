#![doc = include_str!("../README.md")]
#![deny(missing_docs)]

use std::{
    collections::VecDeque,
    io::{IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::net::UnixStream,
    },
    pin::Pin,
    task::{Context, Poll, ready},
};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

/// Maximum number of file descriptors that can be passed in a single D-Bus message,
/// as defined by the [D-Bus specification].
///
/// [D-Bus specification]: https://dbus.freedesktop.org/doc/dbus-specification.html
pub const DBUS_FD_LIMIT: usize = 253;

/// Ancillary buffer size (bytes) required to send or receive up to [`DBUS_FD_LIMIT`] file descriptors
/// via `SCM_RIGHTS` in a single `recvmsg` / `sendmsg` call.
///
/// Equals `rustix::cmsg_space!(ScmRights(DBUS_FD_LIMIT))`. Use this as the `S` parameter of
/// [`AnchovyStream`] when working with D-Bus.
pub const DBUS_SCM_RIGHTS: usize = rustix::cmsg_space!(ScmRights(DBUS_FD_LIMIT));

/// Maximum number of file descriptors that can be passed in a single Wayland message,
/// as defined by the Wayland reference implementation (`WAYLAND_MAX_FDS_OUT`).
pub const WAYLAND_FD_LIMIT: usize = 28;

/// Ancillary buffer size (bytes) required to send or receive up to [`WAYLAND_FD_LIMIT`] file descriptors
/// via `SCM_RIGHTS` in a single `recvmsg` / `sendmsg` call.
///
/// Equals `rustix::cmsg_space!(ScmRights(WAYLAND_FD_LIMIT))`. Use this as the `S` parameter of
/// [`AnchovyStream`] when working with Wayland.
pub const WAYLAND_SCM_RIGHTS: usize = rustix::cmsg_space!(ScmRights(WAYLAND_FD_LIMIT));

/// A Unix socket stream with support for passing file descriptors via `SCM_RIGHTS`
/// ancillary messages.
///
/// Implements [`AsyncRead`] and [`AsyncWrite`], with vectored write support.
///
/// # Const generic `S`
///
/// `S` is the size in bytes of the stack-allocated ancillary data buffer used on each
/// [`recvmsg`] / [`sendmsg`] call. It must be at least
/// `rustix::cmsg_space!(ScmRights(N))` where `N` is the maximum number of file
/// descriptors expected in a single message. Passing a smaller value will cause
/// received file descriptors to be silently truncated by the kernel.
///
/// For D-Bus, pass [`DBUS_SCM_RIGHTS`] as `S`; for Wayland, pass [`WAYLAND_SCM_RIGHTS`].
///
/// # File descriptor queues
///
/// [`write_queue_mut`] holds [`OwnedFd`] values to send as `SCM_RIGHTS` ancillary data
/// with the next write. All queued descriptors go out together in a single message,
/// then the queue is cleared.
///
/// [`read_queue_mut`] receives file descriptors from each `recvmsg` call.
///
/// [`write_queue_mut`]: AnchovyStream::write_queue_mut
/// [`read_queue_mut`]: AnchovyStream::read_queue_mut
/// [rustix]: https://docs.rs/rustix
pub struct AnchovyStream<const S: usize> {
    stream: AsyncFd<UnixStream>,
    decode_fds: VecDeque<OwnedFd>,
    encode_fds: VecDeque<OwnedFd>,
}

/// Seals [`IntoUnixStream`] against external implementations.
mod sealed {
    pub trait Sealed {}

    impl Sealed for tokio::net::UnixStream {}
    impl Sealed for std::os::unix::net::UnixStream {}
}

/// Converts a Unix stream type into a [`std::os::unix::net::UnixStream`].
///
/// Implemented for both [`std::os::unix::net::UnixStream`] and
/// [`tokio::net::UnixStream`], allowing [`AnchovyStream::new`] to accept either.
/// Sealed to prevent external implementations.
pub trait IntoUnixStream: sealed::Sealed {
    /// Converts this stream into a [`std::os::unix::net::UnixStream`].
    fn into_unix_stream(self) -> io::Result<UnixStream>;
}

impl IntoUnixStream for UnixStream {
    fn into_unix_stream(self) -> io::Result<UnixStream> {
        Ok(self)
    }
}

impl IntoUnixStream for tokio::net::UnixStream {
    fn into_unix_stream(self) -> io::Result<UnixStream> {
        self.into_std()
    }
}

impl<const S: usize> AnchovyStream<S> {
    /// Creates a new `AnchovyStream` wrapping the given Unix stream.
    ///
    /// Accepts either a [`std::os::unix::net::UnixStream`] or a
    /// [`tokio::net::UnixStream`].
    pub fn new<T: IntoUnixStream>(stream: T) -> io::Result<Self> {
        AsyncFd::new(stream.into_unix_stream()?).map(|stream| Self {
            stream,
            decode_fds: VecDeque::new(),
            encode_fds: VecDeque::new(),
        })
    }

    /// Returns a reference to the queue of file descriptors received from the peer.
    ///
    /// Populated after each successful [`recvmsg`] call.
    pub fn read_queue(&self) -> &VecDeque<OwnedFd> {
        &self.decode_fds
    }

    /// Returns a mutable reference to the queue of file descriptors received from the peer.
    ///
    /// Drain this queue after reading to collect them.
    pub fn read_queue_mut(&mut self) -> &mut VecDeque<OwnedFd> {
        &mut self.decode_fds
    }

    /// Returns a reference to the queue of file descriptors to be sent to the peer.
    pub fn write_queue(&self) -> &VecDeque<OwnedFd> {
        &self.encode_fds
    }

    /// Returns a mutable reference to the queue of file descriptors to be sent to the peer.
    ///
    /// Push [`OwnedFd`] values here before writing data. All queued descriptors are
    /// sent together in the next [`sendmsg`] call and the queue is cleared afterwards.
    pub fn write_queue_mut(&mut self) -> &mut VecDeque<OwnedFd> {
        &mut self.encode_fds
    }

    fn poll_write_impl(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let stream = &mut self.stream;
        let encode_fds = &mut self.encode_fds;

        loop {
            let mut guard = ready!(stream.poll_write_ready(cx))?;

            let send_result = {
                let raw: Vec<BorrowedFd<'_>> = encode_fds.iter().map(|fd| fd.as_fd()).collect();

                let mut cmsg_space = [MaybeUninit::uninit(); S];
                let mut ancillary = SendAncillaryBuffer::new(&mut cmsg_space);

                if !raw.is_empty() {
                    ancillary.push(SendAncillaryMessage::ScmRights(&raw));
                }

                guard.try_io(|inner| {
                    sendmsg(
                        inner.get_ref(),
                        bufs,
                        &mut ancillary,
                        SendFlags::DONTWAIT | SendFlags::NOSIGNAL,
                    )
                    .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))
                })
            };

            match send_result {
                Ok(Ok(msg)) => {
                    encode_fds.clear();
                    return Poll::Ready(Ok(msg));
                }
                Ok(Err(err)) => {
                    return Poll::Ready(Err(err));
                }

                Err(_would_block) => continue,
            }
        }
    }
}

impl<const S: usize> AsyncRead for AnchovyStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        let stream = &mut this.stream;
        let decode_fds = &mut this.decode_fds;

        loop {
            let mut guard = ready!(stream.poll_read_ready(cx))?;

            let mut cmsg_space = [MaybeUninit::uninit(); S];
            let mut ancillary = RecvAncillaryBuffer::new(&mut cmsg_space);

            let unfilled = buf.initialize_unfilled();

            match guard.try_io(|inner| {
                recvmsg(
                    inner.get_ref(),
                    &mut [IoSliceMut::new(unfilled)],
                    &mut ancillary,
                    RecvFlags::DONTWAIT | RecvFlags::CMSG_CLOEXEC,
                )
                .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))
            }) {
                Ok(Ok(msg)) => {
                    for message in ancillary.drain() {
                        if let RecvAncillaryMessage::ScmRights(fds) = message {
                            for fd in fds {
                                decode_fds.push_back(fd);
                            }
                        }
                    }
                    buf.advance(msg.bytes);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(err)) => return Poll::Ready(Err(err)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl<const S: usize> AsyncWrite for AnchovyStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll_write_impl(cx, &[IoSlice::new(buf)])
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll_write_impl(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut()
            .stream
            .get_ref()
            .shutdown(std::net::Shutdown::Write)?;
        Poll::Ready(Ok(()))
    }
}
