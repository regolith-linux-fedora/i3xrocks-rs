# i3xrocks-rs

A Rust port of [i3xrocks](https://github.com/regolith-linux/i3xrocks), the
status command of the [Regolith](https://regolith-desktop.com/) X11 session's
i3bar. i3xrocks runs the block commands ("blocklets") from a configuration
file or conf.d directories, prints the i3bar JSON protocol on stdout and
reads click events on stdin. It is a tracking fork of
[i3blocks](https://github.com/vivien/i3blocks) that can read property values
from the X resource database (`color=xresource:i3xrocks.value.color`) and
load conf.d directories (`-d`, `-u`).

## Status

**Experimental port; not a drop-in replacement yet.**

- **A specific moment in time.** The port reproduces i3xrocks as of
  [regolith-linux/i3xrocks@4c30d06](https://github.com/regolith-linux/i3xrocks/commit/4c30d060b77ddef3f01152dc27457db341f16a43)
  (October 2026; program version 1.3.3, Debian package 1.3.6-2 plus #5,
  "do not crash on an undefined xresource without a default"). The C
  program is rewritten in Rust; the command line, the configuration format,
  the i3bar output and the installed files are unchanged.
- **Upstream fixes since then.** None in the code. After `4c30d06`,
  regolith-linux/i3xrocks `master` received only #7 (`cf7529c`, the 1.3.7
  release), which adds the `debian/changelog` entry for #5; it is part of
  this repository's history, so `main` matches the current upstream
  `master` in behaviour. One pull request is open: #6, "read the user and
  package conf.d directories together" (user files and sections layered over
  the package ones, commands run in the directory of the file that set them,
  and SIGHUP reloads the configuration). The maintainer approved it but holds
  it for the release after Regolith 3.5, because it changes how existing
  customised setups behave. It is not on `main`; it will be ported when it
  is merged upstream (a port has been drafted). This list was made on
  2026-11-09 and may not be complete: check upstream before relying on it.
- **Very deep testing is needed** before anyone considers this port as a
  replacement for the C i3xrocks.
- **No deep code reviews yet.** The port has not yet been reviewed in depth
  by the major AI coding agents and code reviewers. Those reviews are
  planned before any replacement decision.
- **Upstreaming is undecided.** This repository is not a pull request to
  regolith-linux/i3xrocks; whether, how and when any of it goes upstream
  will be decided later.

## What it ports

The whole program, as one Cargo package (`x11rb` with `resource_manager`,
`nix`, `signal-hook` and `libc`), with no `unsafe` code:

- the command line (`-c`, `-d`, `-u`, `-o`, `-v`, `-h`, `-V`, with GNU
  getopt's clustered and attached forms and `--`), and the default output
  mode, which depends on whether stdout is a terminal;
- configuration loading: the INI syntax, global properties across files,
  conf.d directories read in byte order (dotfiles included), `-u` falling
  back to `-d`, and the error handling;
- `xresource:` values, with defaults, looked up through x11rb's resource
  manager, which builds the database as libxcb-xrm does: the
  `RESOURCE_MANAGER` property (or `~/.Xresources`, then `~/.Xdefaults`),
  then `XENVIRONMENT` (or `~/.Xdefaults-<hostname>`), with `#include`;
- the i3bar stream (header, key order, value printing and escaping, the
  trailer) and terminal mode;
- block output in both forms (plain lines and JSON), exit code 33, and the
  error texts;
- scheduling: intervals, `once`, `repeat`, `persist`, signal bindings
  (`SIGRTMIN+n`), one run at a time per block;
- clicks, including the environment and stdin a block receives;
- the block process (argv, environment, stdin, working directory), signals
  and shutdown.

The binary no longer links libxcb or libxcb-xrm: it talks to the X server
through x11rb's pure-Rust connection.

During development, the port was compared with the C program by a black-box
differential suite of about 350 scenarios covering all of the above. That
suite is not part of this repository. These areas were not compared and may
differ: lines over 4095 bytes, more than 64 KiB of output from a
non-persistent command, invalid UTF-8, `\u` surrogates and `\u0000` in
click records, a closed stdin or stdout, file descriptors beyond 0-2 in
block commands, and the debug output of `-vv` and `-vvv`.

## History

The branch is upstream's history up to `cf7529c` (current `master`: `4c30d06`
plus the 1.3.7 changelog entry), followed by:

1. `feat: port i3xrocks from C to Rust`: the Cargo package, the Debian build
   with cargo, and the removal of the C sources, the autotools inputs and
   the Regolith package-publishing workflows (`.github/`).
2. `test: run the crate tests and the xresource regression test in the
   package build`: `dh_auto_test` runs `cargo test` and upstream's
   `tests/xresource-missing.sh` against the Rust binary, as `make check` did.
3. This README and the package version (`1.3.7+rs1`).

## Build

On Ubuntu 26.04, install the build dependencies from `debian/control`
(for example `sudo apt build-dep ./` from the checkout), then:

```sh
cargo build --release --locked      # fetches the locked crates
target/release/i3xrocks -V
```

Offline builds: vendor the crates once, and cargo (and therefore the Debian
build) uses them:

```sh
mkdir -p .cargo
cargo vendor --locked vendor > .cargo/config.toml
```

## Test

```sh
RUSTFLAGS="-D warnings" cargo build --release --locked
cargo test --locked
cargo fmt --check
I3XROCKS=target/release/i3xrocks sh tests/xresource-missing.sh   # needs Xvfb and xrdb
```

To try it by hand with Regolith's blocklets installed:

```sh
target/release/i3xrocks -u ~/.config/regolith3/i3xrocks/conf.d -d /usr/share/i3xrocks/conf.d
```

then type `[` and click records such as `{"name":"time","button":1}`, one per
line.

## Package

```sh
dpkg-buildpackage -b -us -uc
```

builds `i3xrocks_<version>_<arch>.deb` with the same files as the C package:
`/usr/bin/i3xrocks`, `/etc/i3xrocks.conf` (a conffile) and the man page. The
package build runs the tests above unless `nocheck` is set; cargo needs
network access during the build unless the crates were vendored as above.

The Debian version is upstream's Debian version plus `+rs1`. The program
itself still reports version 1.3.3 (`i3xrocks -V`), as the C program does.

## Usage

Unchanged from the C program; see the man page (`docs/i3xrocks.1`) and
[docs/README.adoc](docs/README.adoc). Regolith starts the bar as:

```
i3xrocks -u ~/.config/regolith3/i3xrocks/conf.d -d /usr/share/i3xrocks/conf.d
```

## License

GNU General Public License v3.0 or later, as the original program; see
[COPYING](COPYING). i3blocks was written by Vivien Didelot and the i3blocks
contributors; i3xrocks is maintained by the Regolith contributors.
