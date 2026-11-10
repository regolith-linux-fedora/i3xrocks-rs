//! Line reading with the semantics of a fixed `BUFSIZ` line buffer:
//! a line is returned without its newline and cut at an embedded NUL,
//! a line that does not fit fails with `ENOSPC`, and running out of
//! data in the middle of a line drops that partial line (`EAGAIN`).

use crate::json::BUFSIZ;
use nix::errno::Errno;
use std::os::fd::{AsFd, OwnedFd};

pub trait LineSource {
    fn next_line(&mut self) -> Result<Vec<u8>, Errno>;
}

/// Take the next complete line from `buf`, if there is one.
fn take_line(buf: &mut Vec<u8>) -> Option<Result<Vec<u8>, Errno>> {
    let window = buf.len().min(BUFSIZ);
    if let Some(pos) = buf[..window].iter().position(|&c| c == b'\n') {
        let mut line: Vec<u8> = buf.drain(..=pos).collect();
        line.pop();
        if let Some(nul) = line.iter().position(|&c| c == 0) {
            line.truncate(nul);
        }
        return Some(Ok(line));
    }
    if buf.len() >= BUFSIZ {
        buf.drain(..BUFSIZ);
        return Some(Err(Errno::ENOSPC));
    }
    None
}

/// Lines of a file or of a blocking pipe, read only as far as needed.
pub struct FdLines<F: AsFd> {
    fd: F,
    buf: Vec<u8>,
}

impl<F: AsFd> FdLines<F> {
    pub fn new(fd: F) -> FdLines<F> {
        FdLines {
            fd,
            buf: Vec::new(),
        }
    }
}

impl<F: AsFd> LineSource for FdLines<F> {
    fn next_line(&mut self) -> Result<Vec<u8>, Errno> {
        loop {
            if let Some(result) = take_line(&mut self.buf) {
                return result;
            }
            let mut chunk = [0u8; 4096];
            match nix::unistd::read(self.fd.as_fd(), &mut chunk) {
                Ok(0) | Err(Errno::EAGAIN) => {
                    self.buf.clear();
                    return Err(Errno::EAGAIN);
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(Errno::EINTR) => {}
                Err(err) => return Err(err),
            }
        }
    }
}

/// Lines of a non-blocking pipe, read one byte at a time so that whatever
/// follows the line stays in the pipe.
pub struct ByteLines<'a> {
    pub fd: &'a OwnedFd,
}

impl LineSource for ByteLines<'_> {
    fn next_line(&mut self) -> Result<Vec<u8>, Errno> {
        let mut line = Vec::new();
        loop {
            if line.len() == BUFSIZ {
                return Err(Errno::ENOSPC);
            }
            let mut byte = [0u8; 1];
            match nix::unistd::read(self.fd, &mut byte) {
                Ok(0) | Err(Errno::EAGAIN) => return Err(Errno::EAGAIN),
                Ok(_) => {
                    if byte[0] == b'\n' {
                        if let Some(nul) = line.iter().position(|&c| c == 0) {
                            line.truncate(nul);
                        }
                        return Ok(line);
                    }
                    line.push(byte[0]);
                }
                Err(Errno::EINTR) => {}
                Err(err) => return Err(err),
            }
        }
    }
}

/// Lines already pulled into a buffer (the bar's stdin): a partial line at
/// the end of the available data is consumed and dropped.
pub struct BufLines<'a> {
    pub buf: &'a mut Vec<u8>,
}

impl LineSource for BufLines<'_> {
    fn next_line(&mut self) -> Result<Vec<u8>, Errno> {
        match take_line(self.buf) {
            Some(result) => result,
            None => {
                self.buf.clear();
                Err(Errno::EAGAIN)
            }
        }
    }
}

/// Read up to `count` lines, handing each to `cb` with its index.
pub fn line_read<S, F>(source: &mut S, count: usize, mut cb: F) -> Result<(), Errno>
where
    S: LineSource + ?Sized,
    F: FnMut(&[u8], usize) -> Result<(), Errno>,
{
    let mut num = 0;
    while num < count {
        let line = source.next_line()?;
        cb(&line, num)?;
        num += 1;
    }
    Ok(())
}
