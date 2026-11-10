//! syslog-style prefixed messages on stderr.

use std::io::Write;

fn emit(prefix: &[u8], msg: &[u8]) {
    let mut line = Vec::with_capacity(prefix.len() + msg.len() + 1);
    line.extend_from_slice(prefix);
    line.extend_from_slice(msg);
    line.push(b'\n');
    let _ = std::io::stderr().write_all(&line);
}

/// A critical message (`<2>`), always printed.
pub fn fatal(msg: &[u8]) {
    emit(b"<2>", msg);
}

/// An error message (`<3>`), always printed.
pub fn error(msg: &[u8]) {
    emit(b"<3>", msg);
}

/// Concatenate byte fragments into one message.
pub fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}
