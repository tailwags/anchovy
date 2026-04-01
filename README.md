# anchovy

Unix sockets can carry open file descriptors alongside data, but nothing in the
standard async I/O stack handles this. In async Rust you have to drop down to
`sendmsg`/`recvmsg` yourself. anchovy handles that: it wraps a `UnixStream` and
implements `AsyncRead`/`AsyncWrite` with fd passing via `SCM_RIGHTS` ancillary
messages.

## Usage

`AnchovyStream` exposes two public queues: `encode_fds` and `decode_fds`. To
send file descriptors, push `OwnedFd` values into `encode_fds` before writing.
Received descriptors land in `decode_fds` after a read.

## Origin

[waynest](https://github.com/verdiwm/waynest) and
[abus](https://github.com/tailwags/abus) both needed this and ended up writing
the same wrapper independently. anchovy is that wrapper, extracted into a shared
crate before the two implementations diverged further.

## License

This project is licensed under the
[Apache-2.0 License](http://www.apache.org/licenses/LICENSE-2.0). For more
information, please see the [LICENSE](LICENSE) file.
