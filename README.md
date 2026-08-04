# anchovy

Unix sockets can carry open file descriptors alongside data, but nothing in the
standard async I/O stack handles this. In async Rust you have to drop down to
`sendmsg`/`recvmsg` yourself. anchovy handles that: it wraps a `UnixStream` and
implements `AsyncRead`/`AsyncWrite` with fd passing via `SCM_RIGHTS` ancillary
messages.

## Usage

```toml
[dependencies]
anchovy = "0.4"
```

The const generic `S` is the maximum number of file descriptors a single message
can carry. Use `AnchovyStream<DBUS_FD_LIMIT>` for D-Bus or
`AnchovyStream<WAYLAND_FD_LIMIT>` for Wayland. For other protocols, set `S` to
the maximum number of file descriptors you expect per message.

### Sending file descriptors

Push `OwnedFd` values into the write queue before writing. All queued
descriptors go out together in a single `sendmsg` call, then the queue is
cleared.

```rust,no_run
use anchovy::{AnchovyStream, DBUS_FD_LIMIT};
use std::os::fd::OwnedFd;
use tokio::io::AsyncWriteExt;

async fn send_fd(stream: &mut AnchovyStream<DBUS_FD_LIMIT>, fd: OwnedFd) -> std::io::Result<()> {
    stream.write_queue_mut().push_back(fd);
    stream.write_all(b"payload").await
}
```

### Receiving file descriptors

Descriptors received with a message land in the read queue. Drain it after
each read.

```rust,no_run
use anchovy::{AnchovyStream, DBUS_FD_LIMIT};
use tokio::io::AsyncReadExt;

async fn recv_fds(stream: &mut AnchovyStream<DBUS_FD_LIMIT>) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64];
    stream.read(&mut buf).await?;
    for fd in stream.read_queue_mut().drain(..) {
        // handle fd
    }
    Ok(())
}
```

## Origin

[waynest](https://github.com/verdiwm/waynest) and
[abus](https://github.com/tailwags/abus) both needed this and ended up writing
the same wrapper independently. anchovy is that wrapper, extracted into a shared
crate before the two implementations diverged further.

## License

This project is licensed under the
[Apache-2.0 License](http://www.apache.org/licenses/LICENSE-2.0). For more
information, please see the [LICENSE](LICENSE) file.
