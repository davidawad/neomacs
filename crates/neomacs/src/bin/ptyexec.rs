//! `neomacs-ptyexec`: the pty child's trampoline on macOS.
//!
//! XNU grants a controlling terminal only through `ioctl(TIOCSCTTY)`; unlike
//! Linux, opening the slave of a pty as a session leader does not.  So the
//! editor cannot express "start this child on a pty" as `posix_spawn` file
//! actions alone, and forking the editor for it costs time proportional to the
//! editor's address space.  Instead the editor `posix_spawn`s this small
//! program in a new session (`POSIX_SPAWN_SETSID`), and this program makes the
//! slave its controlling terminal and `exec`s the real child.
//!
//! Usage (built by `neovm-system`'s `callproc/spawn.rs`, `mod posix`):
//!
//! ```text
//! neomacs-ptyexec SLAVE STATUS-FD STDERR-FD|- -- PROGRAM ARGV0 [ARG...]
//! ```
//!
//! * `SLAVE`: path of the pty slave.  It becomes the controlling terminal and
//!   fds 0 and 1, and fd 2 too unless `STDERR-FD` names a descriptor.
//! * `STATUS-FD`: a pipe's write end.  It is close-on-exec, so a successful
//!   `exec` closes it and the editor reads end-of-file; a failure writes the
//!   `errno` as four native-endian bytes first and exits with status 127.
//! * `STDERR-FD`: a descriptor that becomes fd 2 (a `:stderr` pipe), or `-`.
//! * `PROGRAM`: what `execvp` runs (a path, or a name searched on `PATH`);
//!   `ARGV0 [ARG...]` is the child's argument vector.
//!
//! Every other descriptor is closed on `exec`: on macOS the editor spawns with
//! `POSIX_SPAWN_CLOEXEC_DEFAULT`; elsewhere (the test seam) this program
//! closes them itself.

use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStrExt;
use std::process::ExitCode;

/// What the editor was told when this program could not start the child.
const USAGE: &str = "usage: neomacs-ptyexec SLAVE STATUS-FD STDERR-FD|- -- PROGRAM ARGV0 [ARG...]";

/// Exit status of a trampoline that failed before the program ran (`env(1)`).
const EXIT_FAILED: u8 = 127;

/// `TIOCSCTTY` is a macro the `libc` crate exports only where it is stable.
#[cfg(target_os = "macos")]
const TIOCSCTTY: libc::c_ulong = 0x2000_7461;
#[cfg(not(target_os = "macos"))]
const TIOCSCTTY: libc::c_ulong = libc::TIOCSCTTY as libc::c_ulong;

struct Request {
    slave: CString,
    status_fd: libc::c_int,
    stderr_fd: Option<libc::c_int>,
    program: CString,
    argv: Vec<CString>,
}

fn parse(args: &[OsString]) -> Result<Request, String> {
    let [
        _,
        slave,
        status_fd,
        stderr_fd,
        separator,
        program,
        _argv0,
        ..,
    ] = args
    else {
        return Err(USAGE.to_string());
    };
    if separator != "--" {
        return Err(USAGE.to_string());
    }
    let number = |value: &OsString, what: &str| {
        value
            .to_str()
            .and_then(|text| text.parse::<libc::c_int>().ok())
            .filter(|fd| *fd > libc::STDERR_FILENO)
            .ok_or_else(|| format!("{what} must be a descriptor above 2"))
    };
    let c_string = |value: &OsString| {
        CString::new(value.as_bytes()).map_err(|_| "argument contains a NUL byte".to_string())
    };
    Ok(Request {
        slave: c_string(slave)?,
        status_fd: number(status_fd, "STATUS-FD")?,
        stderr_fd: if stderr_fd == "-" {
            None
        } else {
            Some(number(stderr_fd, "STDERR-FD")?)
        },
        program: c_string(program)?,
        argv: args[6..].iter().map(c_string).collect::<Result<_, _>>()?,
    })
}

/// Tell the editor why the child did not start, then exit.
fn fail(status_fd: libc::c_int, errno: i32) -> ExitCode {
    let bytes = errno.to_ne_bytes();
    // SAFETY: `bytes` is a valid four-byte buffer.  A failed write leaves the
    // editor to report the exit status alone.
    unsafe { libc::write(status_fd, bytes.as_ptr().cast(), bytes.len()) };
    ExitCode::from(EXIT_FAILED)
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// Make the slave the controlling terminal and fds 0-2.  The caller is a
/// session leader without a controlling terminal (`POSIX_SPAWN_SETSID`).
fn attach_slave(request: &Request) -> Result<(), i32> {
    // SAFETY: `slave` is a NUL-terminated path.  `O_NOCTTY` keeps this open
    // from claiming the terminal where the platform would; `TIOCSCTTY` does it
    // explicitly, as XNU requires.
    let slave = unsafe { libc::open(request.slave.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave < 0 {
        return Err(last_errno());
    }
    // SAFETY: plain FFI on the descriptor opened above.
    if unsafe { libc::ioctl(slave, TIOCSCTTY, 0) } == -1 {
        return Err(last_errno());
    }
    let stderr = request.stderr_fd.unwrap_or(slave);
    for (source, target) in [
        (slave, libc::STDIN_FILENO),
        (slave, libc::STDOUT_FILENO),
        (stderr, libc::STDERR_FILENO),
    ] {
        // SAFETY: plain FFI on descriptors this process holds.
        if unsafe { libc::dup2(source, target) } == -1 {
            return Err(last_errno());
        }
    }
    for extra in [Some(slave), request.stderr_fd].into_iter().flatten() {
        if extra > libc::STDERR_FILENO {
            // SAFETY: a descriptor this process opened or was handed for the
            // purpose; nothing uses it afterwards.
            unsafe { libc::close(extra) };
        }
    }
    Ok(())
}

/// Close every descriptor above 2 except the status pipe.  The editor asks
/// macOS to do this at `exec` (`POSIX_SPAWN_CLOEXEC_DEFAULT`); other systems
/// reach this program only from tests.
#[cfg(target_os = "linux")]
fn close_inherited(status_fd: libc::c_int) {
    let above = (status_fd + 1).cast_unsigned();
    // SAFETY: `close_range` takes integers only; a failure (kernel before 5.9)
    // leaves the descriptors open, which a test tolerates.
    unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3 as libc::c_uint,
            status_fd.cast_unsigned() - 1,
            0,
        );
        libc::syscall(libc::SYS_close_range, above, libc::c_uint::MAX, 0);
    }
}

#[cfg(not(target_os = "linux"))]
fn close_inherited(_status_fd: libc::c_int) {}

fn main() -> ExitCode {
    let request = match parse(&std::env::args_os().collect::<Vec<_>>()) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("neomacs-ptyexec: {message}");
            return ExitCode::from(EXIT_FAILED);
        }
    };
    if let Err(errno) = attach_slave(&request) {
        return fail(request.status_fd, errno);
    }
    close_inherited(request.status_fd);
    // SAFETY: the status descriptor is open; `F_SETFD` takes an integer flag.
    if unsafe { libc::fcntl(request.status_fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return fail(request.status_fd, last_errno());
    }
    let mut argv: Vec<*const libc::c_char> = request.argv.iter().map(|arg| arg.as_ptr()).collect();
    argv.push(std::ptr::null());
    // SAFETY: `program` and every `argv` entry are NUL-terminated and outlive
    // the call; `argv` ends in a null pointer.  `execvp` only returns on error.
    unsafe { libc::execvp(request.program.as_ptr(), argv.as_ptr()) };
    fail(request.status_fd, last_errno())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(|part| OsString::from(*part)).collect()
    }

    #[test]
    fn parses_a_request_with_and_without_a_stderr_descriptor() {
        let request = parse(&args(&[
            "x",
            "/dev/ttys001",
            "4",
            "3",
            "--",
            "/bin/ls",
            "ls",
            "-l",
        ]))
        .unwrap();
        assert_eq!(request.status_fd, 4);
        assert_eq!(request.stderr_fd, Some(3));
        assert_eq!(request.program.to_bytes(), b"/bin/ls");
        assert_eq!(request.argv.len(), 2);
        let request = parse(&args(&["x", "/dev/ttys001", "4", "-", "--", "ls", "ls"])).unwrap();
        assert_eq!(request.stderr_fd, None);
    }

    #[test]
    fn rejects_a_request_that_could_clobber_the_standard_descriptors() {
        assert!(parse(&args(&["x", "/dev/ttys001", "1", "-", "--", "ls", "ls"])).is_err());
        assert!(parse(&args(&["x", "/dev/ttys001", "4", "-", "ls", "ls"])).is_err());
        assert!(parse(&args(&["x", "/dev/ttys001", "4", "-", "--", "ls"])).is_err());
    }
}
