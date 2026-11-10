//! i3xrocks: run block commands and print the i3bar protocol on stdout,
//! reading click events on stdin.

#![forbid(unsafe_code)]

mod bar;
mod config;
mod json;
mod lines;
mod log;
mod map;
mod sys;

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;

const PACKAGE_STRING: &str = "i3xrocks 1.3.3";

#[derive(Default)]
struct Options {
    path: Option<Vec<u8>>,
    conf_dir: Option<Vec<u8>>,
    user_conf_dir: Option<Vec<u8>>,
    output: Option<Vec<u8>>,
    log_level: u32,
}

enum Parsed {
    Run(Options),
    Exit(i32),
}

fn stdout_write(data: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(data).and_then(|()| stdout.flush());
}

fn stderr_write(data: &[u8]) {
    let _ = std::io::stderr().write_all(data);
}

fn usage_error(argv0: &[u8], message: &str, option: u8) -> Parsed {
    stderr_write(&log::cat(&[
        argv0,
        b": ",
        message.as_bytes(),
        b" -- '",
        &[option],
        b"'\n",
    ]));
    log::error(&log::cat(&[b"Try '", argv0, b" -h' for more information."]));
    Parsed::Exit(1)
}

/// Parse options the way GNU getopt does with "c:o:d:u:vhV": options may
/// follow operands, clustered and attached forms are accepted, and "--"
/// ends option processing.
fn parse_args(args: &[OsString]) -> Parsed {
    let argv0 = args.first().map(|a| a.as_bytes()).unwrap_or(b"");
    let mut options = Options::default();
    let mut i = 1;
    'args: while i < args.len() {
        let arg = args[i].as_bytes();
        if arg == b"--" {
            break;
        }
        if arg.len() < 2 || arg[0] != b'-' {
            i += 1;
            continue;
        }
        let mut j = 1;
        while j < arg.len() {
            let c = arg[j];
            match c {
                b'c' | b'o' | b'd' | b'u' => {
                    let value = if j + 1 < arg.len() {
                        arg[j + 1..].to_vec()
                    } else if i + 1 < args.len() {
                        i += 1;
                        args[i].as_bytes().to_vec()
                    } else {
                        return usage_error(argv0, "option requires an argument", c);
                    };
                    match c {
                        b'c' => options.path = Some(value),
                        b'o' => options.output = Some(value),
                        b'd' => options.conf_dir = Some(value),
                        _ => options.user_conf_dir = Some(value),
                    }
                    i += 1;
                    continue 'args;
                }
                b'v' => options.log_level += 1,
                b'h' => {
                    stdout_write(&log::cat(&[
                        b"Usage: ",
                        argv0,
                        b" [-c <configfile>] [-d <conf.d dir>] [-u <conf.d dir>] [-o <output>] [-v] [-h] [-V]\n",
                    ]));
                    return Parsed::Exit(0);
                }
                b'V' => {
                    stdout_write(
                        format!("{} \u{a9} 2020 - 2021 Ken Gilmer\n", PACKAGE_STRING).as_bytes(),
                    );
                    return Parsed::Exit(0);
                }
                _ => return usage_error(argv0, "invalid option", c),
            }
            j += 1;
        }
        i += 1;
    }
    Parsed::Run(options)
}

fn main() {
    // Start the monotonic clock used for block timestamps.
    sys::gettime();
    let args: Vec<OsString> = std::env::args_os().collect();
    let options = match parse_args(&args) {
        Parsed::Run(options) => options,
        Parsed::Exit(code) => std::process::exit(code),
    };

    let mut term = nix::unistd::isatty(std::io::stdout()).unwrap_or(false);
    if let Some(output) = &options.output {
        term = output.as_slice() == b"term";
    }

    let mut bar = bar::Bar::new(term, options.log_level);
    bar.start();
    bar.load(
        options.path.as_deref(),
        options.conf_dir.as_deref(),
        options.user_conf_dir.as_deref(),
    );
    let result = bar.run();
    bar.stop();
    bar.flush();
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}
