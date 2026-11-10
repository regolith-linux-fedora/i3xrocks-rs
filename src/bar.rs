//! The status line: blocks, the i3bar and terminal outputs, and the event
//! loop that schedules block commands and reads clicks.

use crate::config::Config;
use crate::json::{self, BUFSIZ};
use crate::lines::{line_read, BufLines, ByteLines, FdLines, LineSource};
use crate::log::{cat, error, fatal};
use crate::map::Map;
use crate::sys;
use nix::errno::Errno;
use nix::fcntl::{open, OFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::signal::{sigprocmask, SigSet, SigmaskHow, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::stat::Mode;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

pub const LOG_FATAL: u32 = 0;
pub const LOG_ERROR: u32 = 1;

const INTERVAL_ONCE: i32 = -1;
const INTERVAL_REPEAT: i32 = -2;
const INTERVAL_PERSIST: i32 = -3;

const EXIT_URGENT: i32 = 33;
const EXIT_ERR_INTERNAL: i32 = 66;

/// The i3bar keys, in the order raw output lines map to them, and whether
/// each one is a string.
const I3BAR_KEYS: [(&[u8], bool); 18] = [
    (b"", false),
    (b"full_text", true),
    (b"short_text", true),
    (b"color", true),
    (b"background", true),
    (b"border", true),
    (b"min_width", false),
    (b"align", true),
    (b"name", true),
    (b"instance", true),
    (b"urgent", false),
    (b"separator", false),
    (b"separator_block_width", false),
    (b"markup", true),
    (b"border_top", false),
    (b"border_bottom", false),
    (b"border_left", false),
    (b"border_right", false),
];

fn i3bar_indexof(key: &[u8]) -> usize {
    I3BAR_KEYS.iter().position(|(k, _)| *k == key).unwrap_or(0)
}

/// `atoi(3)`: leading whitespace, an optional sign, decimal digits.
fn atoi(s: &[u8]) -> i32 {
    let mut i = 0;
    while i < s.len() && json::is_space(s[i]) {
        i += 1;
    }
    let negative = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut value: i64 = 0;
    let mut overflow = false;
    while i < s.len() && s[i].is_ascii_digit() {
        let digit = i64::from(s[i] - b'0');
        match value.checked_mul(10).and_then(|v| v.checked_add(digit)) {
            Some(v) => value = v,
            None => overflow = true,
        }
        i += 1;
    }
    let value = if overflow {
        if negative {
            i64::MIN
        } else {
            i64::MAX
        }
    } else if negative {
        -value
    } else {
        value
    };
    value as i32
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}

fn trunc_bufsiz(mut msg: Vec<u8>) -> Vec<u8> {
    msg.truncate(BUFSIZ - 1);
    msg
}

struct Block {
    config: Map,
    env: Map,
    tainted: bool,
    /// Pretty name for messages ("name" or "name:instance").
    name: Option<Vec<u8>>,
    command: Option<Vec<u8>>,
    interval: i32,
    signal: i32,
    json: bool,
    timestamp: u64,
    code: i32,
    pid: i32,
    /// Read end of the command's stdout.
    out: Option<OwnedFd>,
    /// Write end of a persistent command's stdin.
    input: Option<OwnedFd>,
    /// The descriptor number the stdout read end is known by: 0 before the
    /// first spawn, -1 once closed.
    out_fd: i32,
    /// Spawn sequence number.
    serial: u64,
    /// A persistent command's stdout reached end of file.
    hup: bool,
}

impl Block {
    fn new(config: Option<Map>) -> Block {
        Block {
            config: config.unwrap_or_default(),
            env: Map::new(),
            tainted: false,
            name: None,
            command: None,
            interval: 0,
            signal: 0,
            json: false,
            timestamp: 0,
            code: 0,
            pid: 0,
            out: None,
            input: None,
            out_fd: 0,
            serial: 0,
            hup: false,
        }
    }

    fn label(&self) -> &[u8] {
        self.name.as_deref().unwrap_or(b"(null)")
    }

    fn reset(&mut self) {
        self.env.clear();
        let config = self.config.clone();
        self.env.copy_from(&config);
    }

    fn is_spawned(&self) -> bool {
        self.pid > 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Source {
    /// A signal read from the signal descriptor.
    Signal(i32),
    /// New data (or end of file) on stdin: a click signal.
    Stdin,
    /// Output (or end of file) from a persistent command.
    Persist(usize, u64),
    /// The interval timer expired.
    Alarm,
}

#[derive(Clone, Copy, Debug)]
struct Event {
    signo: i32,
    fd: i32,
    source: Source,
}

struct Timer {
    period: Duration,
    next: Instant,
}

pub struct Bar {
    blocks: Vec<Block>,
    term: bool,
    log_level: u32,
    out: Vec<u8>,
    environ: Vec<(Vec<u8>, Vec<u8>)>,
    stdin_buf: Vec<u8>,
    stdin_eof: bool,
    sigset: SigSet,
    sigfd: Option<SignalFd>,
    timer: Option<Timer>,
    pending: Vec<Event>,
    serial: u64,
}

impl Bar {
    pub fn new(term: bool, log_level: u32) -> Bar {
        use std::os::unix::ffi::OsStrExt;
        let environ = std::env::vars_os()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();
        Bar {
            blocks: vec![Block::new(None)],
            term,
            log_level,
            out: Vec::new(),
            environ,
            stdin_buf: Vec::new(),
            stdin_eof: false,
            sigset: SigSet::empty(),
            sigfd: None,
            timer: None,
            pending: Vec::new(),
            serial: 0,
        }
    }

    // ------------------------------------------------------------------
    // Output
    // ------------------------------------------------------------------

    pub fn flush(&mut self) {
        if self.out.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.out);
        let mut stdout = std::io::stdout().lock();
        let result = stdout.write_all(&data).and_then(|()| stdout.flush());
        if let Err(err) = result {
            if err.raw_os_error() == Some(libc::EPIPE) {
                let _ = signal_hook::low_level::emulate_default_handler(libc::SIGPIPE);
            }
        }
    }

    pub fn start(&mut self) {
        if self.term {
            // Saved, hidden cursor; stays buffered until the first update.
            self.out.extend_from_slice(b"\x1b[s\x1b[?25l\x1b[u\x1b[K");
        } else {
            self.out
                .extend_from_slice(b"{\"version\":1,\"click_events\":true}\n[[]\n");
            self.flush();
        }
    }

    pub fn stop(&mut self) {
        if self.term {
            self.out.extend_from_slice(b"\x1b[?25h");
        } else {
            self.out.extend_from_slice(b"]\n");
            self.flush();
        }
    }

    fn print_term(&mut self) {
        let mut line = b"\x1b[u\x1b[K".to_vec();
        for block in &self.blocks {
            if let Some(full_text) = block.env.get(b"full_text") {
                line.extend_from_slice(full_text);
                line.push(b' ');
            }
        }
        self.out.extend_from_slice(&line);
        self.flush();
    }

    fn print_pair(
        line: &mut Vec<u8>,
        count: &mut usize,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), Errno> {
        let index = i3bar_indexof(key);
        if index == 0 {
            return Ok(());
        }
        let string = I3BAR_KEYS[index].1;
        let escape = if string {
            !json::is_string(value)
        } else {
            !json::is_valid(value)
        };
        let escaped;
        let value = if escape {
            escaped = json::escape(value)?;
            &escaped[..]
        } else {
            value
        };
        if *count > 0 {
            line.push(b',');
        }
        *count += 1;
        line.push(b'"');
        line.extend_from_slice(key);
        line.extend_from_slice(b"\":");
        line.extend_from_slice(value);
        Ok(())
    }

    fn i3bar_print(&mut self) -> Result<(), Errno> {
        if self.term {
            self.print_term();
            return Ok(());
        }
        let mut line = b",[".to_vec();
        let mut blocks = 0;
        let mut result = Ok(());
        for block in &self.blocks {
            if block.env.get(b"full_text").is_none() {
                continue;
            }
            if blocks > 0 {
                line.push(b',');
            }
            blocks += 1;
            line.push(b'{');
            let mut pairs = 0;
            for (key, value) in block.env.iter() {
                result = Bar::print_pair(&mut line, &mut pairs, key, value);
                if result.is_err() {
                    break;
                }
            }
            line.push(b'}');
            if result.is_err() {
                break;
            }
        }
        line.extend_from_slice(b"]\n");
        self.out.extend_from_slice(&line);
        self.flush();
        result
    }

    fn print(&mut self) {
        if self.i3bar_print().is_err() {
            fatal(b"failed to print bar!");
        }
    }

    /// Show a message in a block (errors only, and only on i3bar).
    fn i3bar_printf(&mut self, idx: usize, lvl: u32, msg: &[u8]) -> Result<(), Errno> {
        if self.term || lvl > LOG_ERROR {
            return Ok(());
        }
        let block = &mut self.blocks[idx];
        block.tainted = true;
        block.env.set(b"full_text", msg);
        block.env.set(b"urgent", b"true");
        self.i3bar_print()
    }

    fn block_printf(&mut self, idx: usize, lvl: u32, msg: Vec<u8>) {
        if lvl > self.log_level {
            return;
        }
        let msg = trunc_bufsiz(msg);
        if self.i3bar_printf(idx, lvl, &msg).is_err() {
            let name = self.blocks[idx].label().to_vec();
            fatal(&cat(&[
                b"failed to format message for block ",
                &name,
                b": ",
                &msg,
            ]));
        }
    }

    fn block_error(&mut self, idx: usize, msg: &[u8]) {
        let name = self.blocks[idx].label().to_vec();
        error(&cat(&[b"[", &name, b"] ", msg]));
        self.block_printf(idx, LOG_ERROR, cat(&[b"Error: ", msg]));
    }

    fn tty_prefix(&self) -> &'static [u8] {
        if self.term {
            b"TTY "
        } else {
            b""
        }
    }

    fn bar_error(&mut self, msg: &[u8]) {
        error(msg);
        let text = cat(&[self.tty_prefix(), b"Error: ", msg]);
        self.block_printf(0, LOG_ERROR, text);
    }

    fn bar_fatal(&mut self, msg: &[u8]) {
        fatal(msg);
        let text = cat(&[
            self.tty_prefix(),
            b"Oops! ",
            msg,
            b". Increase log level for details.",
        ]);
        self.block_printf(0, LOG_FATAL, text);
    }

    // ------------------------------------------------------------------
    // Configuration
    // ------------------------------------------------------------------

    pub fn load(&mut self, path: Option<&[u8]>, conf_dir: Option<&[u8]>, user_dir: Option<&[u8]>) {
        let mut configs: Vec<Map> = Vec::new();
        let mut push = |map: Map| configs.push(map);
        let result = if let Some(path) = path {
            Config::new(&mut push).load_file(path)
        } else if conf_dir.is_some() || user_dir.is_some() {
            let mut err = Ok(());
            if let Some(user) = user_dir {
                err = Config::new(&mut push).load_dir(user, true);
            }
            if let Some(dir) = conf_dir {
                if user_dir.is_none() || err.is_err() {
                    err = Config::new(&mut push).load_dir(dir, false);
                }
            }
            err
        } else {
            Err(Errno::EINVAL)
        };
        for config in configs {
            self.blocks.push(Block::new(Some(config)));
        }
        if result.is_err() {
            self.bar_fatal(b"Failed to load configuration.");
        }
    }

    // ------------------------------------------------------------------
    // Blocks
    // ------------------------------------------------------------------

    fn block_setup(&mut self, idx: usize) {
        let block = &mut self.blocks[idx];
        let name = match block.config.get(b"name") {
            Some(name) => name.to_vec(),
            None => {
                block.config.set(b"name", b"foo");
                b"foo".to_vec()
            }
        };
        let pretty = match block.config.get(b"instance") {
            Some(instance) => cat(&[&name, b":", instance]),
            None => name,
        };
        block.name = Some(trunc_bufsiz(pretty));

        block.command = block
            .config
            .get(b"command")
            .filter(|c| !c.is_empty())
            .map(|c| c.to_vec());
        block.interval = match block.config.get(b"interval") {
            None => 0,
            Some(b"once") => INTERVAL_ONCE,
            Some(b"repeat") => INTERVAL_REPEAT,
            Some(b"persist") => INTERVAL_PERSIST,
            Some(value) => atoi(value),
        };
        block.json = block.config.get(b"format") == Some(&b"json"[..]);
        block.signal = block.config.get(b"signal").map(atoi).unwrap_or(0);
        block.reset();
    }

    /// The environment of a block command: the bar's own, plus every block
    /// variable and the legacy names. `None` when a name cannot be set.
    fn child_env(&self, idx: usize) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        fn setenv(env: &mut Vec<(Vec<u8>, Vec<u8>)>, name: &[u8], value: &[u8]) {
            match env.iter_mut().find(|(k, _)| k.as_slice() == name) {
                Some(entry) => entry.1 = value.to_vec(),
                None => env.push((name.to_vec(), value.to_vec())),
            }
        }
        let mut env = self.environ.clone();
        for (name, value) in self.blocks[idx].env.iter() {
            if name.is_empty() || name.contains(&b'=') {
                return None;
            }
            setenv(&mut env, name, value);
            let legacy: &[u8] = match name {
                b"name" => b"BLOCK_NAME",
                b"instance" => b"BLOCK_INSTANCE",
                b"interval" => b"BLOCK_INTERVAL",
                b"button" => b"BLOCK_BUTTON",
                b"x" => b"BLOCK_X",
                b"y" => b"BLOCK_Y",
                _ => continue,
            };
            setenv(&mut env, legacy, value);
        }
        Some(env)
    }

    fn block_spawn(&mut self, idx: usize) -> Result<(), Errno> {
        let Some(command) = self.blocks[idx].command.clone() else {
            return Ok(());
        };
        if self.blocks[idx].is_spawned() {
            return Ok(());
        }
        let persist = self.blocks[idx].interval == INTERVAL_PERSIST;
        let (out_r, out_w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
        let (in_r, in_w) = if persist {
            let (r, w) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
            (r, Some(w))
        } else {
            let null = open(
                "/dev/null",
                OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
                Mode::empty(),
            )?;
            (null, None)
        };
        // A variable the C library refuses to set makes the child fail
        // before it runs the command.
        let (command, env) = match self.child_env(idx) {
            Some(env) => (command, env),
            None => (
                format!("exit {}", EXIT_ERR_INTERNAL).into_bytes(),
                self.environ.clone(),
            ),
        };
        let pid = sys::spawn_sh(&command, &env, &in_r, &out_w)?;
        drop(in_r);
        drop(out_w);
        if persist {
            sys::set_nonblock(out_r.as_fd(), true)?;
        }
        self.serial += 1;
        let block = &mut self.blocks[idx];
        block.pid = pid.as_raw();
        block.out_fd = out_r.as_raw_fd();
        block.out = Some(out_r);
        block.input = in_w;
        block.serial = self.serial;
        block.hup = false;
        Ok(())
    }

    fn block_touch(&mut self, idx: usize) {
        let now = sys::gettime();
        let block = &mut self.blocks[idx];
        if block.timestamp != now {
            block.timestamp = now;
        }
    }

    fn block_close(&mut self, idx: usize) {
        let block = &mut self.blocks[idx];
        if block.interval == INTERVAL_PERSIST {
            block.input = None;
        }
        block.out = None;
        block.out_fd = -1;
        block.hup = false;
    }

    /// Read the block's output into its variables.
    fn block_stdout(&mut self, idx: usize, source: &mut dyn LineSource) -> Result<(), Errno> {
        let block = &mut self.blocks[idx];
        let count = if block.interval == INTERVAL_PERSIST {
            1
        } else {
            usize::MAX
        };
        let json = block.json;
        let env = &mut block.env;
        let result = line_read(source, count, |line, num| {
            if json {
                json::parse_line(line, env)
            } else {
                let index = num + 1;
                if index < I3BAR_KEYS.len() {
                    env.set(I3BAR_KEYS[index].0, line);
                }
                Ok(())
            }
        });
        match result {
            Ok(()) | Err(Errno::EAGAIN) => {}
            Err(err) => return Err(err),
        }
        if let (Some(label), Some(full_text)) =
            (block.env.get(b"label"), block.env.get(b"full_text"))
        {
            let text = trunc_bufsiz(cat(&[label, full_text]));
            block.env.set(b"full_text", &text);
        }
        Ok(())
    }

    /// Reset the block's variables and read its output (from its stdout, or
    /// from the bar's stdin for a block known by descriptor 0).
    fn block_update(&mut self, idx: usize) -> Result<(), Errno> {
        self.blocks[idx].reset();
        let result = match self.blocks[idx].out.take() {
            Some(fd) => {
                let result = if self.blocks[idx].interval == INTERVAL_PERSIST {
                    self.block_stdout(idx, &mut ByteLines { fd: &fd })
                } else {
                    self.block_stdout(idx, &mut FdLines::new(&fd))
                };
                self.blocks[idx].out = Some(fd);
                result
            }
            None if self.blocks[idx].out_fd == 0 => {
                self.pull_stdin();
                let mut buf = std::mem::take(&mut self.stdin_buf);
                let result = self.block_stdout(idx, &mut BufLines { buf: &mut buf });
                self.stdin_buf = buf;
                result
            }
            None => Ok(()),
        };
        result?;
        let block = &mut self.blocks[idx];
        if block.code == EXIT_URGENT {
            block.env.set(b"urgent", b"true");
        }
        Ok(())
    }

    /// Report how the command ended (its pid was just reaped).
    fn block_reap(&mut self, idx: usize, code: i32) {
        self.blocks[idx].code = code;
        self.blocks[idx].pid = 0;
        let command = self.blocks[idx].command.clone().unwrap_or_default();
        match code {
            EXIT_ERR_INTERNAL => self.block_error(idx, b"Internal error"),
            0 | EXIT_URGENT => {}
            126 => self.block_error(idx, &cat(&[b"Command '", &command, b"' not executable"])),
            127 => self.block_error(
                idx,
                &cat(&[b"Command '", &command, b"' not found or missing dependency"]),
            ),
            _ => self.block_error(
                idx,
                &cat(&[
                    b"Command '",
                    &command,
                    b"' exited unexpectedly with code ",
                    code.to_string().as_bytes(),
                ]),
            ),
        }
    }

    fn write_input(&mut self, idx: usize, data: &[u8]) {
        if let Some(fd) = &self.blocks[idx].input {
            let mut rest = data;
            while !rest.is_empty() {
                match nix::unistd::write(fd, rest) {
                    Ok(n) => rest = &rest[n..],
                    Err(Errno::EINTR) => {}
                    Err(Errno::EPIPE) => {
                        let _ = signal_hook::low_level::emulate_default_handler(libc::SIGPIPE);
                        return;
                    }
                    Err(_) => return,
                }
            }
        }
    }

    /// Send click data to a persistent command's stdin.
    fn block_send(&mut self, idx: usize) -> Result<(), Errno> {
        let Some(button) = self.blocks[idx].env.get(b"button").map(|b| b.to_vec()) else {
            self.block_error(idx, b"no click data to send");
            return Err(Errno::EINVAL);
        };
        if !self.blocks[idx].is_spawned() {
            self.block_error(idx, b"persistent block not spawned");
            return Ok(());
        }
        if !self.blocks[idx].json {
            self.write_input(idx, &cat(&[&button, b"\n"]));
            return Ok(());
        }
        self.write_input(idx, b"{\"\":\"\"");
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = self.blocks[idx]
            .env
            .iter()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();
        let mut result = Ok(());
        for (key, value) in pairs {
            let value = if json::is_valid(&value) {
                value
            } else {
                match json::escape(&value) {
                    Ok(escaped) => escaped,
                    Err(err) => {
                        result = Err(err);
                        break;
                    }
                }
            };
            self.write_input(idx, &cat(&[b",\"", &key, b"\":", &value]));
        }
        self.write_input(idx, b"}\n");
        result
    }

    fn block_click(&mut self, idx: usize) -> Result<(), Errno> {
        if self.blocks[idx].interval == INTERVAL_PERSIST {
            self.block_send(idx)
        } else {
            self.block_spawn(idx)
        }
    }

    // ------------------------------------------------------------------
    // Clicks
    // ------------------------------------------------------------------

    /// Move whatever stdin has available into the buffer. Returns whether
    /// anything new (data or end of file) was seen.
    fn pull_stdin(&mut self) -> bool {
        if self.stdin_eof {
            return false;
        }
        let stdin = std::io::stdin();
        let mut seen = false;
        loop {
            let mut chunk = [0u8; 4096];
            match nix::unistd::read(stdin.as_fd(), &mut chunk) {
                Ok(0) => {
                    self.stdin_eof = true;
                    return true;
                }
                Ok(n) => {
                    self.stdin_buf.extend_from_slice(&chunk[..n]);
                    seen = true;
                }
                Err(Errno::EINTR) => {}
                Err(_) => return seen,
            }
        }
    }

    fn find_block(&self, click: &Map) -> Option<usize> {
        let name = click.get(b"name").unwrap_or(b"");
        let instance = click.get(b"instance").unwrap_or(b"");
        self.blocks.iter().position(|block| {
            block.env.get(b"name").unwrap_or(b"") == name
                && block.env.get(b"instance").unwrap_or(b"") == instance
        })
    }

    fn i3bar_click(&mut self) -> Result<(), Errno> {
        self.pull_stdin();
        loop {
            let mut click = Map::new();
            let line = {
                let mut lines = BufLines {
                    buf: &mut self.stdin_buf,
                };
                match lines.next_line() {
                    Ok(line) => line,
                    Err(Errno::EAGAIN) => return Ok(()),
                    Err(err) => return Err(err),
                }
            };
            json::parse_line(&line, &mut click)?;
            if let Some(idx) = self.find_block(&click) {
                if self.blocks[idx].tainted {
                    self.blocks[idx].reset();
                    self.blocks[idx].tainted = false;
                    self.i3bar_print()?;
                } else {
                    self.blocks[idx].env.copy_from(&click);
                    self.block_click(idx)?;
                }
            }
        }
    }

    fn bar_read(&mut self) {
        if self.i3bar_click().is_err() {
            self.bar_error(b"failed to read bar");
        }
    }

    // ------------------------------------------------------------------
    // Event handlers
    // ------------------------------------------------------------------

    fn poll_timed(&mut self) {
        for idx in 0..self.blocks.len() {
            if self.blocks[idx].interval != 0 {
                let _ = self.block_spawn(idx);
                self.block_touch(idx);
            }
        }
    }

    fn poll_expired(&mut self) {
        for idx in 0..self.blocks.len() {
            let block = &self.blocks[idx];
            if block.interval > 0 {
                let next_update = block.timestamp.wrapping_add(block.interval as u64);
                let now = sys::gettime();
                if (next_update.wrapping_sub(now) as i64) <= 0 {
                    let _ = self.block_spawn(idx);
                    self.block_touch(idx);
                }
            }
        }
    }

    fn poll_signaled(&mut self, sig: i32) {
        for idx in 0..self.blocks.len() {
            if self.blocks[idx].signal == sig {
                let _ = self.block_spawn(idx);
                self.block_touch(idx);
            }
        }
    }

    /// Reap one exited block command, oldest spawn first.
    fn reap_one(&mut self) -> Option<(usize, i32)> {
        let mut spawned: Vec<usize> = (0..self.blocks.len())
            .filter(|&idx| self.blocks[idx].is_spawned())
            .collect();
        spawned.sort_by_key(|&idx| self.blocks[idx].serial);
        for idx in spawned {
            let pid = Pid::from_raw(self.blocks[idx].pid);
            match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => return Some((idx, code & 0xff)),
                Ok(WaitStatus::Signaled(..)) => return Some((idx, 0)),
                Ok(_) => {}
                // A child killed by a signal unknown to nix is reaped anyway.
                Err(Errno::EINVAL) => return Some((idx, 0)),
                Err(_) => {}
            }
        }
        None
    }

    fn poll_exited(&mut self) {
        while let Some((idx, code)) = self.reap_one() {
            self.block_reap(idx, code);
            if self.blocks[idx].interval != INTERVAL_PERSIST {
                let _ = self.block_update(idx);
            }
            self.block_close(idx);
            if self.blocks[idx].interval == INTERVAL_REPEAT {
                let _ = self.block_spawn(idx);
                self.block_touch(idx);
            }
        }
    }

    fn poll_readable(&mut self, event: Event) {
        let idx = match event.source {
            Source::Persist(idx, serial) => {
                let block = &self.blocks[idx];
                (block.out.is_some() && block.serial == serial).then_some(idx)
            }
            _ => self
                .blocks
                .iter()
                .position(|block| block.out_fd == event.fd),
        };
        if let Some(idx) = idx {
            let _ = self.block_update(idx);
        }
    }

    // ------------------------------------------------------------------
    // Event loop
    // ------------------------------------------------------------------

    fn setup(&mut self) -> Result<(), Errno> {
        let mut sleeptime: u64 = 0;
        for idx in 0..self.blocks.len() {
            self.block_setup(idx);
            let interval = self.blocks[idx].interval;
            if interval > 0 {
                sleeptime = if sleeptime > 0 {
                    gcd(sleeptime, interval as u64)
                } else {
                    interval as u64
                };
            }
        }
        self.sigset = sys::bar_sigset();
        sigprocmask(SigmaskHow::SIG_SETMASK, Some(&self.sigset), None)?;
        self.sigfd = Some(SignalFd::with_flags(
            &self.sigset,
            SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC,
        )?);
        if sleeptime > 0 {
            let period = Duration::from_secs(sleeptime);
            self.timer = Some(Timer {
                period,
                next: Instant::now() + period,
            });
        }
        let stdin = std::io::stdin();
        sys::set_cloexec(stdin.as_fd())?;
        sys::set_nonblock(stdin.as_fd(), true)?;
        Ok(())
    }

    fn push(&mut self, event: Event) {
        let duplicate = self.pending.iter().any(|queued| {
            if event.signo < sys::sigrtmin() {
                // Standard signals do not queue.
                queued.signo == event.signo
            } else {
                matches!(event.source, Source::Persist(..)) && queued.source == event.source
            }
        });
        if !duplicate {
            self.pending.push(event);
        }
    }

    fn pop(&mut self) -> Option<Event> {
        let (pos, _) = self
            .pending
            .iter()
            .enumerate()
            .min_by_key(|(pos, event)| (event.signo, *pos))?;
        Some(self.pending.remove(pos))
    }

    /// Wait up to `timeout` for signals, input and output, queueing them as
    /// the signals they stand for.
    fn gather(&mut self, timeout: PollTimeout, watch_stdin: bool) {
        let rtmin = sys::sigrtmin();
        let mut persist: Vec<usize> = Vec::new();
        let (sig_ready, stdin_ready, ready) = {
            let stdin = std::io::stdin();
            let mut fds = Vec::new();
            if let Some(sigfd) = &self.sigfd {
                fds.push(PollFd::new(sigfd.as_fd(), PollFlags::POLLIN));
            }
            let with_stdin = watch_stdin && !self.stdin_eof;
            if with_stdin {
                fds.push(PollFd::new(stdin.as_fd(), PollFlags::POLLIN));
            }
            for (idx, block) in self.blocks.iter().enumerate() {
                if let Some(fd) = &block.out {
                    if block.interval == INTERVAL_PERSIST && !block.hup {
                        fds.push(PollFd::new(fd.as_fd(), PollFlags::POLLIN));
                        persist.push(idx);
                    }
                }
            }
            if poll(&mut fds, timeout).is_err() {
                return;
            }
            let revents: Vec<PollFlags> = fds
                .iter()
                .map(|fd| fd.revents().unwrap_or(PollFlags::empty()))
                .collect();
            let mut it = revents.into_iter();
            let sig_ready = it.next().map(|r| !r.is_empty()).unwrap_or(false);
            let stdin_ready = if with_stdin {
                it.next().map(|r| !r.is_empty()).unwrap_or(false)
            } else {
                false
            };
            (sig_ready, stdin_ready, it.collect::<Vec<_>>())
        };
        if sig_ready {
            while let Some(Ok(Some(info))) = self.sigfd.as_ref().map(|fd| fd.read_signal()) {
                self.push(Event {
                    signo: info.ssi_signo as i32,
                    fd: info.ssi_fd,
                    source: Source::Signal(info.ssi_signo as i32),
                });
            }
        }
        if stdin_ready && self.pull_stdin() {
            self.push(Event {
                signo: libc::SIGIO,
                fd: 0,
                source: Source::Stdin,
            });
        }
        for (idx, revents) in persist.into_iter().zip(ready) {
            if revents.is_empty() {
                continue;
            }
            let block = &mut self.blocks[idx];
            if !revents.contains(PollFlags::POLLIN) {
                block.hup = true;
            }
            let event = Event {
                signo: rtmin,
                fd: block.out_fd,
                source: Source::Persist(idx, block.serial),
            };
            self.push(event);
        }
        if let Some(timer) = &mut self.timer {
            let now = Instant::now();
            if now >= timer.next {
                while timer.next <= now {
                    timer.next += timer.period;
                }
                self.push(Event {
                    signo: libc::SIGALRM,
                    fd: 0,
                    source: Source::Alarm,
                });
            }
        }
    }

    fn timeout(&self) -> PollTimeout {
        match &self.timer {
            None => PollTimeout::NONE,
            Some(timer) => {
                let left = timer.next.saturating_duration_since(Instant::now());
                let millis = left.as_millis() + u128::from(left.subsec_nanos() % 1_000_000 != 0);
                PollTimeout::try_from(millis.min(i32::MAX as u128)).unwrap_or(PollTimeout::MAX)
            }
        }
    }

    fn next_event(&mut self) -> Event {
        loop {
            self.gather(PollTimeout::ZERO, true);
            if let Some(event) = self.pop() {
                return event;
            }
            let timeout = self.timeout();
            self.gather(timeout, true);
            if let Some(event) = self.pop() {
                return event;
            }
        }
    }

    fn teardown(&mut self) {
        // Event I/O stops for persistent commands; a command that already
        // exited has no descriptor left to reconfigure.
        for idx in 0..self.blocks.len() {
            if self.blocks[idx].interval == INTERVAL_PERSIST && self.blocks[idx].out_fd < 0 {
                self.block_error(idx, b"failed to disable event I/O");
            }
        }
        // Event I/O stops for stdin too: it goes back to blocking mode.
        let _ = sys::set_nonblock(std::io::stdin().as_fd(), false);
        // Whatever is pending now is delivered once the signals are
        // unblocked, with its default action.
        self.gather(PollTimeout::ZERO, false);
        let mut unblock = self.sigset;
        unblock.remove(Signal::SIGCHLD);
        let _ = sigprocmask(SigmaskHow::SIG_UNBLOCK, Some(&unblock), None);
        let mut pending: Vec<i32> = self
            .pending
            .iter()
            .map(|event| event.signo)
            .filter(|&signo| signo != libc::SIGCHLD)
            .collect();
        pending.sort_unstable();
        for signo in pending {
            let _ = signal_hook::low_level::raise(signo);
        }
        self.pending.clear();
        // Wait for every child; the interval timer keeps running.
        loop {
            match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => {}
                Err(Errno::ECHILD) => break,
                Ok(_) | Err(Errno::EINVAL) => continue,
                Err(_) => break,
            }
            let timeout = self.timeout();
            if let Some(sigfd) = &self.sigfd {
                let mut fds = [PollFd::new(sigfd.as_fd(), PollFlags::POLLIN)];
                let _ = poll(&mut fds, timeout);
                while let Ok(Some(_)) = sigfd.read_signal() {}
            }
            if let Some(timer) = &self.timer {
                if Instant::now() >= timer.next {
                    let _ = nix::sys::signal::raise(Signal::SIGALRM);
                }
            }
        }
    }

    /// Run the bar until SIGTERM or SIGINT.
    pub fn run(&mut self) -> Result<(), Errno> {
        self.setup()?;
        // Initial display (static blocks), then the first commands.
        self.print();
        self.poll_timed();
        let rtmin = sys::sigrtmin();
        let rtmax = sys::sigrtmax();
        loop {
            let event = self.next_event();
            let sig = event.signo;
            if sig == libc::SIGTERM || sig == libc::SIGINT {
                break;
            }
            if sig == libc::SIGALRM {
                self.poll_expired();
            } else if sig == libc::SIGCHLD {
                self.poll_exited();
                self.print();
            } else if sig == libc::SIGIO {
                self.bar_read();
            } else if sig == rtmin {
                self.poll_readable(event);
                self.print();
            } else if sig > rtmin && sig <= rtmax {
                self.poll_signaled(sig - rtmin);
            } else if sig == libc::SIGUSR1 || sig == libc::SIGUSR2 {
                error(b"SIGUSR{1,2} are deprecated, ignoring.");
            }
        }
        self.teardown();
        Ok(())
    }
}
