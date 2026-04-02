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
    net::{
        RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
        SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
    },
};
use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

const MAX_FDS_PER_MSG: usize = 253;

pub struct AnchovyStream {
    stream: AsyncFd<UnixStream>,
    decode_fds: VecDeque<OwnedFd>,
    encode_fds: VecDeque<OwnedFd>,
}

impl AnchovyStream {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        AsyncFd::new(stream).map(|stream| Self {
            stream,

            decode_fds: VecDeque::new(),
            encode_fds: VecDeque::new(),
        })
    }
    pub fn read_queue(&self) -> &VecDeque<OwnedFd> {
        &self.decode_fds
    }

    pub fn read_queue_mut(&mut self) -> &mut VecDeque<OwnedFd> {
        &mut self.decode_fds
    }

    pub fn write_queue(&self) -> &VecDeque<OwnedFd> {
        &self.encode_fds
    }

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

                let mut cmsg_space =
                    [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_PER_MSG))];
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

impl AsyncRead for AnchovyStream {
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

            let mut cmsg_space =
                [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS_PER_MSG))];
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

impl AsyncWrite for AnchovyStream {
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
