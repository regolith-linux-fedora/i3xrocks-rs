//! Configuration loading: INI files, conf.d directories and `xresource:`
//! values.

use crate::lines::{line_read, FdLines};
use crate::log::{cat, error, fatal};
use crate::map::Map;
use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;
use std::ffi::OsStr;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use x11rb::resource_manager::Database;

const XRESOURCE: &[u8] = b"xresource:";

/// Lazily opened X resource database, one per configuration file.
enum XResources {
    Untried,
    Loaded(Option<Database>),
}

impl XResources {
    fn database(&mut self) -> Option<&Database> {
        if let XResources::Untried = self {
            let database = x11rb::rust_connection::RustConnection::connect(None)
                .ok()
                .and_then(|(conn, _)| x11rb::resource_manager::new_from_default(&conn).ok());
            *self = XResources::Loaded(database);
        }
        match self {
            XResources::Loaded(database) => database.as_ref(),
            XResources::Untried => None,
        }
    }
}

/// The state shared by all files of one load: the section being built and
/// the global properties.
pub struct Config<'a> {
    section: Option<Map>,
    global: Option<Map>,
    callback: &'a mut dyn FnMut(Map),
}

impl<'a> Config<'a> {
    pub fn new(callback: &'a mut dyn FnMut(Map)) -> Config<'a> {
        Config {
            section: None,
            global: None,
            callback,
        }
    }

    fn finalize(&mut self) {
        if let Some(section) = self.section.take() {
            (self.callback)(section);
        }
    }

    fn reset(&mut self) {
        let mut section = Map::new();
        if let Some(global) = &self.global {
            section.copy_from(global);
        }
        self.section = Some(section);
    }

    fn set(&mut self, key: &[u8], value: &[u8]) {
        match &mut self.section {
            Some(section) => section.set(key, value),
            None => self.global.get_or_insert_with(Map::new).set(key, value),
        }
    }

    fn property(&mut self, xres: &mut XResources, key: &[u8], value: &[u8]) -> Result<(), Errno> {
        let mut value = value;
        if let Some(rest) = value.strip_prefix(XRESOURCE) {
            let Some(database) = xres.database() else {
                fatal(b"Unable to access Xresources.");
                return Err(Errno::EINVAL);
            };
            let (name, default) = match rest.iter().position(|&c| c == b' ') {
                Some(pos) => (&rest[..pos], Some(&rest[pos + 1..])),
                None => (rest, None),
            };
            let found = std::str::from_utf8(name)
                .ok()
                .and_then(|name| database.get_bytes(name, ""))
                .map(|v| v.to_vec());
            if let Some(resource) = found {
                self.set(key, &resource);
                return Ok(());
            }
            match default {
                Some(default) if !default.is_empty() => value = default,
                _ => {
                    error(&cat(&[
                        b"Xresource \"",
                        name,
                        b"\" is not defined and has no default, ignoring ",
                        key,
                    ]));
                    return Ok(());
                }
            }
        }
        self.set(key, value);
        Ok(())
    }

    fn parse_line(&mut self, xres: &mut XResources, line: &[u8]) -> Result<(), Errno> {
        let first = line.first().copied().unwrap_or(0);
        if first == 0 || first == b'#' {
            return Ok(());
        }
        if first == b'[' {
            let Some(closing) = line.iter().position(|&c| c == b']') else {
                error(&cat(&[b"malformated section \"", line, b"\""]));
                return Err(Errno::EINVAL);
            };
            if closing + 1 != line.len() {
                error(&cat(&[b"trailing characters \"", &line[closing..], b"\""]));
                return Err(Errno::EINVAL);
            }
            self.finalize();
            self.reset();
            self.set(b"name", &line[1..closing]);
            return Ok(());
        }
        if first.is_ascii_alphabetic() || first == b'_' {
            let Some(equals) = line.iter().position(|&c| c == b'=') else {
                error(b"malformated property, should be a key=value pair");
                return Err(Errno::EINVAL);
            };
            return self.property(xres, &line[..equals], &line[equals + 1..]);
        }
        error(&cat(&[b"invalid INI syntax for line: \"", line, b"\""]));
        Err(Errno::EINVAL)
    }

    fn read(&mut self, lines: &mut FdLines<OwnedFd>) -> Result<(), Errno> {
        let mut xres = XResources::Untried;
        match line_read(lines, usize::MAX, |line, _| {
            self.parse_line(&mut xres, line)
        }) {
            Ok(()) | Err(Errno::EAGAIN) => {}
            Err(err) => return Err(err),
        }
        self.finalize();
        Ok(())
    }

    fn open_file(&mut self, path: &[u8], single: bool) -> Result<(), Errno> {
        let fd = open(
            OsStr::from_bytes(path),
            OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        let dname = dirname(path);
        if let Err(err) = nix::unistd::chdir(OsStr::from_bytes(&dname)) {
            error(&cat(&[b"failed to change directory to ", &dname]));
            return Err(err);
        }
        let mut lines = FdLines::new(fd);
        let result = self.read(&mut lines);
        if single {
            self.global = None;
        }
        result
    }

    /// Load a single configuration file.
    pub fn load_file(&mut self, path: &[u8]) -> Result<(), Errno> {
        self.open_file(path, true)
    }

    /// Load every entry of a directory in byte order, dotfiles included.
    pub fn load_dir(&mut self, path: &[u8], quiet: bool) -> Result<(), Errno> {
        let entries = match std::fs::read_dir(OsStr::from_bytes(path)) {
            Ok(entries) => entries,
            Err(err) => {
                if !quiet {
                    let errno = Errno::from_raw(err.raw_os_error().unwrap_or(0));
                    let line = cat(&[path, b": ", errno.desc().as_bytes(), b"\n"]);
                    let _ = std::io::stderr().write_all(&line);
                }
                return Err(Errno::EPERM);
            }
        };
        let mut names: Vec<Vec<u8>> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().as_bytes().to_vec())
            .collect();
        names.sort();
        for name in names {
            let full = cat(&[path, b"/", &name]);
            if let Err(err) = self.open_file(&full, false) {
                error(&cat(&[b"failed to load config file ", &name]));
                return Err(err);
            }
        }
        self.global = None;
        Ok(())
    }
}

/// `dirname(3)` as the GNU C library implements it.
pub fn dirname(path: &[u8]) -> Vec<u8> {
    let Some(mut last) = path.iter().rposition(|&c| c == b'/') else {
        return b".".to_vec();
    };
    if last != 0 && last + 1 == path.len() {
        let mut run = last;
        while run != 0 && path[run - 1] == b'/' {
            run -= 1;
        }
        if run != 0 {
            match path[..run].iter().rposition(|&c| c == b'/') {
                Some(pos) => last = pos,
                None => return b".".to_vec(),
            }
        }
    }
    let mut run = last;
    while run != 0 && path[run - 1] == b'/' {
        run -= 1;
    }
    let end = if run == 0 {
        if last == 1 {
            2
        } else {
            1
        }
    } else {
        run
    };
    path[..end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::dirname;

    #[test]
    fn dirname_like_glibc() {
        assert_eq!(dirname(b"/a/b/c.conf"), b"/a/b");
        assert_eq!(dirname(b"/a/b//c"), b"/a/b");
        assert_eq!(dirname(b"/c"), b"/");
        assert_eq!(dirname(b"c"), b".");
        assert_eq!(dirname(b"/a/b/"), b"/a");
        assert_eq!(dirname(b"//c"), b"//");
        assert_eq!(dirname(b"a/"), b".");
    }
}
