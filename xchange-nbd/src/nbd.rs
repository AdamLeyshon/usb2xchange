//! Just enough of the NBD kernel interface to hand it a socket.
//!
//! `nbd-client` negotiates an export over TCP and passes the socket on. With
//! the server in the same process that is ceremony, so we give one end of a
//! socketpair to `/dev/nbdN` and serve the other.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// `_IO(0xab, nr)` from `linux/nbd.h`.
const fn nbd_ioctl(nr: u64) -> u64 {
    (0xab << 8) | nr
}

pub const NBD_SET_SOCK: u64 = nbd_ioctl(0);
pub const NBD_SET_BLKSIZE: u64 = nbd_ioctl(1);
pub const NBD_DO_IT: u64 = nbd_ioctl(3);
pub const NBD_CLEAR_SOCK: u64 = nbd_ioctl(4);
pub const NBD_SET_SIZE_BLOCKS: u64 = nbd_ioctl(7);
pub const NBD_DISCONNECT: u64 = nbd_ioctl(8);
pub const NBD_SET_FLAGS: u64 = nbd_ioctl(10);

/// `BLKFLSBUF` from `linux/fs.h`, `_IO(0x12, 97)`.
///
/// Not an NBD call. NBD cannot say "what you cached came off a cartridge that
/// has left", so after a medium change this is the only thing stopping the old
/// contents being served from memory. `blockdev --flushbufs` issues it.
pub const BLKFLSBUF: u64 = 0x1261;

pub const NBD_FLAG_HAS_FLAGS: u64 = 1 << 0;
pub const NBD_FLAG_READ_ONLY: u64 = 1 << 1;
pub const NBD_FLAG_SEND_FLUSH: u64 = 1 << 2;

const REQUEST_MAGIC: u32 = 0x2560_9513;
const REPLY_MAGIC: u32 = 0x6744_6698;
const REQUEST_LEN: usize = 28;

pub const CMD_READ: u32 = 0;
pub const CMD_WRITE: u32 = 1;
pub const CMD_DISCONNECT: u32 = 2;
pub const CMD_FLUSH: u32 = 3;
pub const CMD_TRIM: u32 = 4;

/// Errors reported back to the kernel in a reply header.
pub const EPERM: u32 = 1;
pub const EIO: u32 = 5;
pub const EINVAL: u32 = 22;
pub const EOPNOTSUPP: u32 = 95;

/// One request from the kernel. Everything on the wire is big-endian.
#[derive(Debug)]
pub struct Request {
    pub command: u32,
    pub handle: [u8; 8],
    pub offset: u64,
    pub length: u32,
}

/// What came off the socket this time round.
#[derive(Debug)]
pub enum Incoming {
    Request(Request),
    /// The kernel closed its end.
    Closed,
    /// Timed out with nothing, or half a header, delivered. The partial is
    /// kept, so this is a tick rather than a lost request; it drives the media
    /// watch, which a server blocked on the socket could never run.
    Idle,
}

/// A read timeout rather than a real failure. Linux reports `SO_RCVTIMEO` as
/// `EAGAIN`, so `WouldBlock`; `TimedOut` covers other platforms.
fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

impl Request {
    /// Read the next request, resuming across timeouts. `partial` holds what
    /// arrived before one, and belongs to the caller because `read_exact`
    /// discards what it had: polling mid-header would eat half a request.
    pub fn read_from(stream: &mut impl Read, partial: &mut Vec<u8>) -> io::Result<Incoming> {
        while partial.len() < REQUEST_LEN {
            let mut chunk = [0u8; REQUEST_LEN];
            let wanted = REQUEST_LEN - partial.len();

            match stream.read(&mut chunk[..wanted]) {
                Ok(0) if partial.is_empty() => return Ok(Incoming::Closed),
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("socket closed {} bytes into a request header", partial.len()),
                    ))
                }
                Ok(read) => partial.extend_from_slice(&chunk[..read]),
                Err(error) if is_timeout(&error) => return Ok(Incoming::Idle),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }

        let header: [u8; REQUEST_LEN] = partial[..REQUEST_LEN].try_into().unwrap();
        partial.clear();

        let magic = u32::from_be_bytes(header[0..4].try_into().unwrap());
        if magic != REQUEST_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad request magic {magic:#010x}"),
            ));
        }

        // Command in the low half; the top carries FUA and friends.
        let raw_type = u32::from_be_bytes(header[4..8].try_into().unwrap());

        Ok(Incoming::Request(Self {
            command: raw_type & 0xffff,
            handle: header[8..16].try_into().unwrap(),
            offset: u64::from_be_bytes(header[16..24].try_into().unwrap()),
            length: u32::from_be_bytes(header[24..28].try_into().unwrap()),
        }))
    }
}

/// Read a write payload the header has already promised. A timeout here means
/// "not yet", never "no more", so it retries.
pub fn read_payload(stream: &mut impl Read, buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;

    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("socket closed {filled} bytes into a {} byte payload", buf.len()),
                ))
            }
            Ok(read) => filled += read,
            Err(error) if is_timeout(&error) => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

/// Send a reply header, optionally followed by data for a read.
pub fn reply(
    stream: &mut impl Write,
    handle: [u8; 8],
    error: u32,
    data: Option<&[u8]>,
) -> io::Result<()> {
    let mut header = [0u8; 16];
    header[0..4].copy_from_slice(&REPLY_MAGIC.to_be_bytes());
    header[4..8].copy_from_slice(&error.to_be_bytes());
    header[8..16].copy_from_slice(&handle);

    stream.write_all(&header)?;
    if let Some(data) = data {
        stream.write_all(data)?;
    }
    stream.flush()
}

/// A connected pair: one end for the kernel, one for us.
pub struct SocketPair {
    pub kernel: OwnedFd,
    pub server: UnixStream,
}

pub fn socket_pair() -> io::Result<SocketPair> {
    let (server, kernel) = UnixStream::pair()?;
    Ok(SocketPair {
        kernel: kernel.into(),
        server,
    })
}

/// Wrapper so ioctl failures carry the operation name.
pub fn ioctl(fd: RawFd, request: u64, arg: u64, what: &'static str) -> io::Result<()> {
    // SAFETY: `fd` is an open /dev/nbdN, and every request here takes an int.
    let result = unsafe { libc::ioctl(fd, request as libc::Ioctl, arg) };

    if result < 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("{what} failed: {error}"),
        ));
    }
    Ok(())
}

/// Convenience for passing a descriptor to an ioctl expecting one.
pub fn as_arg(fd: &impl AsRawFd) -> u64 {
    fd.as_raw_fd() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hands back scripted results, so a request can arrive in pieces.
    struct Scripted(Vec<io::Result<Vec<u8>>>);

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.remove(0) {
                Ok(bytes) => {
                    let take = bytes.len().min(buf.len());
                    buf[..take].copy_from_slice(&bytes[..take]);
                    Ok(take)
                }
                Err(error) => Err(error),
            }
        }
    }

    fn timeout() -> io::Result<Vec<u8>> {
        Err(io::Error::from(io::ErrorKind::WouldBlock))
    }

    /// A READ of 4096 bytes at offset 8192, handle 1.
    fn header() -> Vec<u8> {
        let mut header = Vec::new();
        header.extend_from_slice(&REQUEST_MAGIC.to_be_bytes());
        header.extend_from_slice(&CMD_READ.to_be_bytes());
        header.extend_from_slice(&1u64.to_be_bytes());
        header.extend_from_slice(&8192u64.to_be_bytes());
        header.extend_from_slice(&4096u32.to_be_bytes());
        assert_eq!(header.len(), REQUEST_LEN);
        header
    }

    fn expect_request(incoming: Incoming) -> Request {
        match incoming {
            Incoming::Request(request) => request,
            Incoming::Closed => panic!("expected a request, got a closed socket"),
            Incoming::Idle => panic!("expected a request, got an idle tick"),
        }
    }

    #[test]
    fn a_whole_header_in_one_read() {
        let mut stream = Scripted(vec![Ok(header())]);
        let mut partial = Vec::new();

        let request = expect_request(Request::read_from(&mut stream, &mut partial).unwrap());
        assert_eq!(request.command, CMD_READ);
        assert_eq!(request.offset, 8192);
        assert_eq!(request.length, 4096);
        assert!(partial.is_empty(), "a consumed header leaves nothing behind");
    }

    #[test]
    fn a_timeout_part_way_through_does_not_lose_the_first_half() {
        // Why the buffer lives outside: read_exact discards what it had, so a
        // media poll between two halves of a header would eat one.
        let whole = header();
        let (front, back) = whole.split_at(9);

        let mut stream = Scripted(vec![
            Ok(front.to_vec()),
            timeout(),
            Ok(back.to_vec()),
        ]);
        let mut partial = Vec::new();

        assert!(matches!(
            Request::read_from(&mut stream, &mut partial).unwrap(),
            Incoming::Idle
        ));
        assert_eq!(partial.len(), 9, "the delivered half is held for next time");

        let request = expect_request(Request::read_from(&mut stream, &mut partial).unwrap());
        assert_eq!(request.offset, 8192, "reassembled across the timeout");
        assert_eq!(request.length, 4096);
        assert!(partial.is_empty());
    }

    #[test]
    fn a_header_dribbled_in_one_byte_at_a_time() {
        let mut script: Vec<io::Result<Vec<u8>>> = Vec::new();
        for byte in header() {
            script.push(timeout());
            script.push(Ok(vec![byte]));
        }

        let mut stream = Scripted(script);
        let mut partial = Vec::new();

        loop {
            match Request::read_from(&mut stream, &mut partial).unwrap() {
                Incoming::Idle => continue,
                other => {
                    let request = expect_request(other);
                    assert_eq!(request.offset, 8192);
                    assert_eq!(request.length, 4096);
                    return;
                }
            }
        }
    }

    #[test]
    fn a_quiet_socket_is_idle_not_closed() {
        let mut stream = Scripted(vec![timeout()]);
        let mut partial = Vec::new();

        assert!(matches!(
            Request::read_from(&mut stream, &mut partial).unwrap(),
            Incoming::Idle
        ));
        assert!(partial.is_empty());
    }

    #[test]
    fn a_closed_socket_is_closed_not_idle() {
        // Reading a detach as idle would leave the server polling a device
        // the kernel has finished with.
        let mut stream = Scripted(vec![Ok(Vec::new())]);
        let mut partial = Vec::new();

        assert!(matches!(
            Request::read_from(&mut stream, &mut partial).unwrap(),
            Incoming::Closed
        ));
    }

    #[test]
    fn closing_mid_header_is_an_error() {
        let mut stream = Scripted(vec![Ok(header()[..4].to_vec()), Ok(Vec::new())]);
        let mut partial = Vec::new();

        let error = Request::read_from(&mut stream, &mut partial).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut wrong = header();
        wrong[0] = 0;

        let mut stream = Scripted(vec![Ok(wrong)]);
        let mut partial = Vec::new();

        let error = Request::read_from(&mut stream, &mut partial).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_payload_waits_through_timeouts_rather_than_giving_up() {
        // The header promised these bytes; treating a timeout as the end
        // would truncate a write.
        let mut stream = Scripted(vec![
            Ok(vec![1, 2, 3]),
            timeout(),
            timeout(),
            Ok(vec![4, 5]),
        ]);

        let mut buf = [0u8; 5];
        read_payload(&mut stream, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_truncated_payload_is_an_error() {
        let mut stream = Scripted(vec![Ok(vec![1, 2]), Ok(Vec::new())]);

        let mut buf = [0u8; 5];
        let error = read_payload(&mut stream, &mut buf).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}
