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

use rustix::{
    io::retry_on_intr,
    net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
    },
};
use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

/// Maximum number of file descriptors that can be passed in a single D-Bus message,
/// as defined by the [D-Bus specification].
///
/// Use this as the `S` parameter of [`AnchovyStream`] when working with D-Bus.
///
/// [D-Bus specification]: https://dbus.freedesktop.org/doc/dbus-specification.html
pub const DBUS_FD_LIMIT: usize = 253;

/// Maximum number of file descriptors that can be passed in a single Wayland message,
/// as defined by the Wayland reference implementation (`WAYLAND_MAX_FDS_OUT`).
///
/// Use this as the `S` parameter of [`AnchovyStream`] when working with Wayland.
pub const WAYLAND_FD_LIMIT: usize = 28;

/// A Unix socket stream with support for passing file descriptors via `SCM_RIGHTS`
/// ancillary messages.
///
/// Implements [`AsyncRead`] and [`AsyncWrite`], with vectored write support.
///
/// # Const generic `S`
///
/// `S` is the maximum number of file descriptors that can be carried by a single
/// message. The ancillary data buffer needed to hold that many descriptors is sized
/// internally and allocated once at construction.
/// At least `S` descriptors per message are guaranteed to fit; if a
/// message carries more than fit in the buffer, sends fail with
/// [`InvalidInput`](io::ErrorKind::InvalidInput) and reads fail with
/// [`InvalidData`](io::ErrorKind::InvalidData) (the kernel has already closed the
/// descriptors that did not fit, so the stream is desynchronized).
///
/// For D-Bus, pass [`DBUS_FD_LIMIT`] as `S`; for Wayland, pass [`WAYLAND_FD_LIMIT`].
///
/// # File descriptor queues
///
/// [`write_queue_mut`] holds [`OwnedFd`] values to send as `SCM_RIGHTS` ancillary data
/// with the next write. All queued descriptors go out together in a single message,
/// then the queue is cleared.
///
/// [`read_queue_mut`] receives file descriptors from each `recvmsg` call.
///
/// The read queue is bounded to prevent a peer from exhausting the process file
/// descriptor table: a read whose descriptors would push the queue past the limit
/// ([`DEFAULT_READ_QUEUE_LIMIT`] unless set via [`with_limits`] or
/// [`set_read_queue_limit`]) fails with [`InvalidData`](io::ErrorKind::InvalidData)
/// and that message's descriptors are closed.
///
/// [`write_queue_mut`]: AnchovyStream::write_queue_mut
/// [`read_queue_mut`]: AnchovyStream::read_queue_mut
/// [`DEFAULT_READ_QUEUE_LIMIT`]: AnchovyStream::DEFAULT_READ_QUEUE_LIMIT
/// [`with_limits`]: AnchovyStream::with_limits
/// [`set_read_queue_limit`]: AnchovyStream::set_read_queue_limit
pub struct AnchovyStream<const S: usize> {
    stream: AsyncFd<UnixStream>,
    decode_fds: VecDeque<OwnedFd>,
    encode_fds: VecDeque<OwnedFd>,
    cmsg_buffer: Box<[MaybeUninit<u8>]>,
    read_queue_limit: usize,
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
        self.set_nonblocking(true)?;

        Ok(self)
    }
}

impl IntoUnixStream for tokio::net::UnixStream {
    fn into_unix_stream(self) -> io::Result<UnixStream> {
        self.into_std()
    }
}

impl<const S: usize> AnchovyStream<S> {
    /// Ancillary buffer size (bytes) required to send or receive up to `S` file
    /// descriptors via `SCM_RIGHTS` in a single `recvmsg` / `sendmsg` call.
    const SCM_RIGHTS_SPACE: usize = rustix::cmsg_space!(ScmRights(S));

    /// Default limit on file descriptors retained in the read queue: four
    /// messages' worth.
    pub const DEFAULT_READ_QUEUE_LIMIT: usize = 4 * S;

    /// Creates a new `AnchovyStream` wrapping the given Unix stream, with the
    /// read queue limited to [`DEFAULT_READ_QUEUE_LIMIT`](Self::DEFAULT_READ_QUEUE_LIMIT).
    ///
    /// Accepts either a [`std::os::unix::net::UnixStream`] or a
    /// [`tokio::net::UnixStream`].
    pub fn new<T: IntoUnixStream>(stream: T) -> io::Result<Self> {
        Self::with_limits(stream, Self::DEFAULT_READ_QUEUE_LIMIT)
    }

    /// Creates a new `AnchovyStream` with an explicit read queue limit.
    ///
    /// `read_queue_limit` bounds how many received file descriptors may sit
    /// undrained in the read queue; a read that would exceed it fails with
    /// [`InvalidData`](io::ErrorKind::InvalidData). Pass [`usize::MAX`] for an
    /// effectively unbounded queue.
    pub fn with_limits<T: IntoUnixStream>(stream: T, read_queue_limit: usize) -> io::Result<Self> {
        AsyncFd::new(stream.into_unix_stream()?).map(|stream| Self {
            stream,
            decode_fds: VecDeque::new(),
            encode_fds: VecDeque::new(),
            cmsg_buffer: Box::new_uninit_slice(Self::SCM_RIGHTS_SPACE),
            read_queue_limit,
        })
    }

    /// Sets the limit on file descriptors retained in the read queue.
    ///
    /// Takes effect on subsequent reads; descriptors already queued are unaffected,
    /// but a limit below the current queue length makes the next fd-carrying read
    /// fail.
    pub fn set_read_queue_limit(&mut self, limit: usize) {
        self.read_queue_limit = limit;
    }

    /// Returns the current limit on file descriptors retained in the read queue.
    pub fn read_queue_limit(&self) -> usize {
        self.read_queue_limit
    }

    /// Returns a reference to the queue of file descriptors received from the peer.
    ///
    /// Populated after each successful `recvmsg` call.
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
    /// sent together in the next `sendmsg` call and the queue is cleared afterwards.
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
        let cmsg_buffer = &mut self.cmsg_buffer;

        // Ancillary data is only transmitted alongside at least one byte of payload,
        // so an empty write would silently drop the queued fds.
        if bufs.iter().all(|buf| buf.is_empty()) {
            return Poll::Ready(Ok(0));
        }

        loop {
            let mut guard = ready!(stream.poll_write_ready(cx))?;

            let send_result = {
                let raw: Vec<BorrowedFd<'_>> = encode_fds.iter().map(|fd| fd.as_fd()).collect();

                let mut ancillary = SendAncillaryBuffer::new(cmsg_buffer);

                if !raw.is_empty() && !ancillary.push(SendAncillaryMessage::ScmRights(&raw)) {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "more file descriptors queued than the stream's `S` limit",
                    )));
                }

                guard.try_io(|inner| {
                    retry_on_intr(|| {
                    sendmsg(
                        inner.get_ref(),
                        bufs,
                        &mut ancillary,
                        SendFlags::DONTWAIT | SendFlags::NOSIGNAL,
                    )
                    })
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
        let cmsg_buffer = &mut this.cmsg_buffer;
        let read_queue_limit = this.read_queue_limit;

        loop {
            let mut guard = ready!(stream.poll_read_ready(cx))?;

            let mut ancillary = RecvAncillaryBuffer::new(cmsg_buffer);

            let unfilled = buf.initialize_unfilled();

            match guard.try_io(|inner| {
                retry_on_intr(|| {
                recvmsg(
                    inner.get_ref(),
                    &mut [IoSliceMut::new(unfilled)],
                    &mut ancillary,
                    RecvFlags::DONTWAIT | RecvFlags::CMSG_CLOEXEC,
                )
                })
                .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))
            }) {
                Ok(Ok(msg)) => {
                    if msg.flags.contains(ReturnFlags::CTRUNC) {
                        // The kernel truncated the ancillary data: descriptors that
                        // did not fit have already been closed and are unrecoverable,
                        // leaving the stream desynchronized. Descriptors that did fit
                        // are closed when `ancillary` is dropped.
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ancillary data truncated: the peer sent more file descriptors than the stream's `S` limit and some were lost",
                        )));
                    }

                    // Push straight into the queue and roll back on overflow: we
                    // hold `&mut self`, so the transient overshoot is unobservable
                    // and no per-read staging allocation is needed.
                    let retained = decode_fds.len();

                    for message in ancillary.drain() {
                        if let RecvAncillaryMessage::ScmRights(fds) = message {
                            for fd in fds {
                                decode_fds.push_back(fd);
                            }
                        }
                    }

                    if decode_fds.len() > read_queue_limit {
                        // Retaining these descriptors would let an undrained queue
                        // grow without bound (fd table exhaustion). Truncating
                        // closes this message's descriptors; delivering the bytes
                        // anyway would desynchronize fd-carrying protocols, so the
                        // read fails.
                        decode_fds.truncate(retained);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "peer exceeded the stream's fd queue limit",
                        )));
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

impl<const S: usize> AsFd for AnchovyStream<S> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }
}
