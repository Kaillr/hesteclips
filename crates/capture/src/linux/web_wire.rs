//! What the app and its web page host (`super::web_host`) say to each
//! other: messages over a Unix socket pair (`SOCK_SEQPACKET`, so each comes
//! whole), a frame's file descriptor sent along with its message.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use anyhow::{Result, anyhow, bail};

/// From the app to the host.
#[derive(Debug, PartialEq)]
pub(crate) enum ToHost {
    /// Show `url` as page `page`: `width`×`height` big, `css` added to it,
    /// up to `fps` frames a second, its frames on the graphics card
    /// (`card`) or in memory.
    Open { page: u64, url: String, width: u32, height: u32, css: String, fps: u32, card: bool },
    /// Up to `fps` frames a second from now on.
    Rate { page: u64, fps: u32 },
    Close { page: u64 },
    /// The page's frame sent last is copied: its buffer can be drawn in again.
    Done { page: u64 },
}

/// From the host to the app.
#[derive(Debug, PartialEq)]
pub(crate) enum ToApp {
    /// The engine is up: pages can be opened.
    Ready,
    /// It couldn't start, and why.
    Failed { why: String },
    /// A frame on the graphics card: one plane of a DMA-BUF (sent with
    /// this), `width`×`height` (of which `visible` shows the page: x, y, w,
    /// h), colours premultiplied.
    Card { page: u64, width: u32, height: u32, visible: [u32; 4], fourcc: u32, modifier: u64, offset: u64, stride: u32 },
    /// A frame in memory: B, G, R, A rows, packed, colours premultiplied,
    /// at the start of the memory file sent with this.
    Memory { page: u64, width: u32, height: u32 },
    /// The page loaded, or couldn't (why).
    Loaded { page: u64, error: Option<String> },
}

/// The longest message: an address and a style sheet fit with room to spare.
pub(crate) const MAX_MESSAGE: usize = 192 * 1024;

#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn str(self, s: &str) -> Self {
        let mut me = self.u32(s.len() as u32);
        me.0.extend_from_slice(s.as_bytes());
        me
    }
}

struct In<'a>(&'a [u8]);

impl In<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.0.len() < n {
            bail!("a message cut short");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn str(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
}

impl ToHost {
    pub(crate) fn encode(&self) -> Vec<u8> {
        match self {
            ToHost::Open { page, url, width, height, css, fps, card } => Out::default().u8(1).u64(*page).str(url).u32(*width).u32(*height).str(css).u32(*fps).u8(u8::from(*card)),
            ToHost::Rate { page, fps } => Out::default().u8(2).u64(*page).u32(*fps),
            ToHost::Close { page } => Out::default().u8(3).u64(*page),
            ToHost::Done { page } => Out::default().u8(4).u64(*page),
        }
        .0
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        let mut i = In(b);
        Ok(match i.u8()? {
            1 => ToHost::Open { page: i.u64()?, url: i.str()?, width: i.u32()?, height: i.u32()?, css: i.str()?, fps: i.u32()?, card: i.u8()? != 0 },
            2 => ToHost::Rate { page: i.u64()?, fps: i.u32()? },
            3 => ToHost::Close { page: i.u64()? },
            4 => ToHost::Done { page: i.u64()? },
            k => bail!("a message of an unknown kind ({k})"),
        })
    }
}

impl ToApp {
    pub(crate) fn encode(&self) -> Vec<u8> {
        match self {
            ToApp::Ready => Out::default().u8(1),
            ToApp::Failed { why } => Out::default().u8(2).str(why),
            ToApp::Card { page, width, height, visible, fourcc, modifier, offset, stride } => {
                let o = Out::default().u8(3).u64(*page).u32(*width).u32(*height);
                visible.iter().fold(o, |o, v| o.u32(*v)).u32(*fourcc).u64(*modifier).u64(*offset).u32(*stride)
            }
            ToApp::Memory { page, width, height } => Out::default().u8(4).u64(*page).u32(*width).u32(*height),
            ToApp::Loaded { page, error } => {
                let o = Out::default().u8(5).u64(*page);
                match error {
                    Some(e) => o.u8(1).str(e),
                    None => o.u8(0),
                }
            }
        }
        .0
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        let mut i = In(b);
        Ok(match i.u8()? {
            1 => ToApp::Ready,
            2 => ToApp::Failed { why: i.str()? },
            3 => ToApp::Card {
                page: i.u64()?,
                width: i.u32()?,
                height: i.u32()?,
                visible: [i.u32()?, i.u32()?, i.u32()?, i.u32()?],
                fourcc: i.u32()?,
                modifier: i.u64()?,
                offset: i.u64()?,
                stride: i.u32()?,
            },
            4 => ToApp::Memory { page: i.u64()?, width: i.u32()?, height: i.u32()? },
            5 => ToApp::Loaded { page: i.u64()?, error: if i.u8()? != 0 { Some(i.str()?) } else { None } },
            k => bail!("a message of an unknown kind ({k})"),
        })
    }
}

/// One end of the socket pair.
pub(crate) struct Wire(OwnedFd);

impl Wire {
    /// A connected pair: one end for each side. Close-on-exec both; the
    /// host's end is handed over as fd 3 by the app (`super::web`).
    pub(crate) fn pair() -> io::Result<(Wire, Wire)> {
        let mut fds = [0; 2];
        // SAFETY: an array of two for the new sockets.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: two new sockets of ours.
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for s in [&a, &b] {
            // Room for the longest message (the kernel caps it at its own most).
            let size: libc::c_int = (MAX_MESSAGE * 2) as libc::c_int;
            for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
                // SAFETY: a socket of ours, an int option.
                unsafe { libc::setsockopt(s.as_raw_fd(), libc::SOL_SOCKET, opt, (&raw const size).cast(), size_of::<libc::c_int>() as u32) };
            }
        }
        Ok((Wire(a), Wire(b)))
    }

    pub(crate) fn from_fd(fd: OwnedFd) -> Self {
        Wire(fd)
    }

    /// A send that can't go within `d` fails (the other side's stuck).
    pub(crate) fn send_timeout(&self, d: std::time::Duration) {
        let tv = libc::timeval { tv_sec: d.as_secs() as libc::time_t, tv_usec: libc::suseconds_t::from(d.subsec_micros()) };
        // SAFETY: a socket of ours, a timeval option.
        unsafe { libc::setsockopt(self.0.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDTIMEO, (&raw const tv).cast(), size_of::<libc::timeval>() as u32) };
    }

    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.0.as_fd()
    }

    /// Send one message, with `fd` if any (the receiver gets its own copy).
    pub(crate) fn send(&self, msg: &[u8], fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
        let mut iov = libc::iovec { iov_base: msg.as_ptr() as *mut libc::c_void, iov_len: msg.len() };
        // SAFETY: zeroed is a valid empty msghdr.
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &raw mut iov;
        hdr.msg_iovlen = 1;
        // Room for one fd's control message (aligned as cmsg wants).
        let mut control = [0u64; 4];
        if let Some(fd) = fd {
            // SAFETY: CMSG_SPACE of one int fits in `control` (32 bytes).
            let space = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as usize;
            hdr.msg_control = control.as_mut_ptr().cast();
            hdr.msg_controllen = space;
            // SAFETY: the header's control buffer is ours and big enough.
            unsafe {
                let c = libc::CMSG_FIRSTHDR(&raw const hdr);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as usize;
                std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>(), fd.as_raw_fd());
            }
        }
        loop {
            // SAFETY: a socket of ours, a header pointing at live buffers.
            let n = unsafe { libc::sendmsg(self.0.as_raw_fd(), &raw const hdr, libc::MSG_NOSIGNAL) };
            if n >= 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }

    /// The next message, and the fd sent with it, if any; none once the
    /// other end has closed.
    pub(crate) fn recv(&self, buf: &mut [u8]) -> Result<Option<(usize, Option<OwnedFd>)>> {
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        let mut control = [0u64; 8];
        // SAFETY: zeroed is a valid empty msghdr.
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &raw mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = control.as_mut_ptr().cast();
        hdr.msg_controllen = size_of_val(&control);
        let n = loop {
            // SAFETY: a socket of ours, a header pointing at live buffers.
            let n = unsafe { libc::recvmsg(self.0.as_raw_fd(), &raw mut hdr, libc::MSG_CMSG_CLOEXEC) };
            if n >= 0 {
                break n as usize;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(anyhow!("can't read from the other side: {e}"));
            }
        };
        let mut fd = None;
        // SAFETY: walking the control messages the kernel filled in.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&raw const hdr);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let count = ((*c).cmsg_len - libc::CMSG_LEN(0) as usize) / size_of::<libc::c_int>();
                    for k in 0..count {
                        let raw = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>().add(k));
                        // The first is the frame's; any more (never sent) are closed.
                        let owned = OwnedFd::from_raw_fd(raw);
                        if fd.is_none() {
                            fd = Some(owned);
                        }
                    }
                }
                c = libc::CMSG_NXTHDR(&raw const hdr, c);
            }
        }
        if n == 0 && fd.is_none() {
            return Ok(None);
        }
        if hdr.msg_flags & libc::MSG_TRUNC != 0 {
            bail!("a message too long ({n} bytes)");
        }
        Ok(Some((n, fd)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every message reads back as it was written; a cut one is refused.
    #[test]
    fn messages_read_back_as_written() {
        let to_host = [
            ToHost::Open { page: 7, url: "https://example.com/ä".into(), width: 800, height: 600, css: "body { color: red; }".into(), fps: 60, card: true },
            ToHost::Rate { page: 7, fps: 120 },
            ToHost::Close { page: u64::MAX },
            ToHost::Done { page: 1 },
        ];
        for m in to_host {
            assert_eq!(ToHost::decode(&m.encode()).unwrap(), m);
        }
        let to_app = [
            ToApp::Ready,
            ToApp::Failed { why: "libcups.so.2: cannot open".into() },
            ToApp::Card { page: 3, width: 800, height: 608, visible: [0, 0, 800, 600], fourcc: 0x3432_5241, modifier: 0x0300_0000_0060_6014, offset: 0, stride: 3200 },
            ToApp::Memory { page: 3, width: 16, height: 9 },
            ToApp::Loaded { page: 3, error: None },
            ToApp::Loaded { page: 3, error: Some("net::ERR_NAME_NOT_RESOLVED".into()) },
        ];
        for m in to_app {
            let b = m.encode();
            assert_eq!(ToApp::decode(&b).unwrap(), m);
            if b.len() > 1 {
                assert!(ToApp::decode(&b[..b.len() - 1]).is_err());
            }
        }
    }

    /// A message and its fd arrive together; closing one end ends the other's.
    #[test]
    fn a_frame_arrives_with_its_fd() {
        let (a, b) = Wire::pair().unwrap();
        let file = std::fs::File::open("/proc/self/exe").unwrap();
        use std::os::fd::AsFd;
        a.send(&ToApp::Memory { page: 1, width: 2, height: 2 }.encode(), Some(file.as_fd())).unwrap();
        a.send(&ToApp::Ready.encode(), None).unwrap();
        let mut buf = vec![0; MAX_MESSAGE];
        let (n, fd) = b.recv(&mut buf).unwrap().unwrap();
        assert_eq!(ToApp::decode(&buf[..n]).unwrap(), ToApp::Memory { page: 1, width: 2, height: 2 });
        assert!(fd.is_some());
        let (n, fd) = b.recv(&mut buf).unwrap().unwrap();
        assert_eq!((ToApp::decode(&buf[..n]).unwrap(), fd.is_none()), (ToApp::Ready, true));
        drop(a);
        assert!(b.recv(&mut buf).unwrap().is_none());
    }
}
