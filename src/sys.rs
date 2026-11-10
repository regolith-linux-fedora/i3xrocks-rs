//! Process, signal and descriptor helpers on top of the safe `nix` API.

use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::spawn::{posix_spawn, PosixSpawnAttr, PosixSpawnFileActions, PosixSpawnFlags};
use nix::sys::signal::{SigSet, Signal};
use nix::unistd::Pid;
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::OnceLock;
use std::time::Instant;

pub fn sigrtmin() -> i32 {
    libc::SIGRTMIN()
}

pub fn sigrtmax() -> i32 {
    libc::SIGRTMAX()
}

/// The signals the bar waits for: control, timer, children, the
/// deprecated user signals, clicks, and every real-time signal.
pub fn bar_sigset() -> SigSet {
    let mut set = SigSet::all();
    for signal in Signal::iterator() {
        let wanted = matches!(
            signal,
            Signal::SIGTERM
                | Signal::SIGINT
                | Signal::SIGALRM
                | Signal::SIGCHLD
                | Signal::SIGUSR1
                | Signal::SIGUSR2
                | Signal::SIGIO
        );
        if !wanted {
            set.remove(signal);
        }
    }
    set
}

/// Whole seconds of a monotonic clock.
pub fn gettime() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    // Offset so that the clock never reads as the zero of a fresh block.
    1_000_000 + start.elapsed().as_secs()
}

pub fn set_cloexec(fd: BorrowedFd<'_>) -> Result<(), Errno> {
    let flags = FdFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFD)?);
    fcntl(fd, FcntlArg::F_SETFD(flags | FdFlag::FD_CLOEXEC))?;
    Ok(())
}

pub fn set_nonblock(fd: BorrowedFd<'_>, on: bool) -> Result<(), Errno> {
    let mut flags = OFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFL)?);
    flags.set(OFlag::O_NONBLOCK, on);
    fcntl(fd, FcntlArg::F_SETFL(flags))?;
    Ok(())
}

fn cstring(bytes: Vec<u8>) -> CString {
    CString::new(bytes).unwrap_or_default()
}

/// Run `/bin/sh -c command` with the given environment, stdin and stdout.
/// The child starts with no blocked signals and default SIGPIPE handling.
pub fn spawn_sh(
    command: &[u8],
    env: &[(Vec<u8>, Vec<u8>)],
    stdin: &OwnedFd,
    stdout: &OwnedFd,
) -> Result<Pid, Errno> {
    let mut actions = PosixSpawnFileActions::init()?;
    actions.add_dup2(stdin.as_fd().as_raw_fd(), 0)?;
    actions.add_dup2(stdout.as_fd().as_raw_fd(), 1)?;
    let mut attr = PosixSpawnAttr::init()?;
    attr.set_flags(
        PosixSpawnFlags::POSIX_SPAWN_SETSIGMASK | PosixSpawnFlags::POSIX_SPAWN_SETSIGDEF,
    )?;
    attr.set_sigmask(&SigSet::empty())?;
    let mut defaults = SigSet::empty();
    defaults.add(Signal::SIGPIPE);
    attr.set_sigdefault(&defaults)?;
    let args = [
        cstring(b"/bin/sh".to_vec()),
        cstring(b"-c".to_vec()),
        cstring(command.to_vec()),
    ];
    let envp: Vec<CString> = env
        .iter()
        .map(|(k, v)| {
            let mut entry = k.clone();
            entry.push(b'=');
            entry.extend_from_slice(v);
            cstring(entry)
        })
        .collect();
    posix_spawn("/bin/sh", &actions, &attr, &args, &envp)
}
