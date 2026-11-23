//! `neomacs-ptyexec`, the trampoline that starts a pty child on macOS.
//!
//! The editor `posix_spawn`s it in a new session; these tests start it the
//! same way (`setsid`, then the descriptors the editor's file actions set up)
//! and check what the child it `exec`s observes.  `/proc` stands in for `ps`,
//! so the child-state test runs on Linux only.

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

const STATUS_FD: RawFd = 4;
const STDERR_FD: RawFd = 3;
/// Where the leaked descriptor lands in the child.
const LEAK_FD: RawFd = 7;

/// A pty pair and the helper's start-up error pipe.
struct Harness {
    master: File,
    slave_path: String,
    status_read: File,
    status_write: Option<OwnedFd>,
    /// A descriptor the editor failed to mark close-on-exec.
    leak: Option<OwnedFd>,
}

fn pipe() -> (File, OwnedFd) {
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).expect("pipe");
    (File::from(read), write)
}

impl Harness {
    fn new() -> Self {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("openpt");
        grantpt(&master).expect("grantpt");
        unlockpt(&master).expect("unlockpt");
        let slave_path = ptsname(&master, Vec::new())
            .expect("ptsname")
            .into_string()
            .expect("utf-8 slave name");
        let (status_read, status_write) = pipe();
        Self {
            master: File::from(master),
            slave_path,
            status_read,
            status_write: Some(status_write),
            leak: None,
        }
    }

    /// Start the trampoline as the editor does: in a new session, with the
    /// status pipe on fd 4 and `stderr` (when given) on fd 3.
    fn spawn(&self, stderr: Option<&OwnedFd>, program: &str, argv: &[&str]) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_neomacs-ptyexec"));
        command
            .arg(&self.slave_path)
            .arg(STATUS_FD.to_string())
            .arg(stderr.map_or("-".to_string(), |_| STDERR_FD.to_string()))
            .arg("--")
            .arg(program)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = self.status_write.as_ref().expect("status pipe").as_raw_fd();
        let stderr = stderr.map(AsRawFd::as_raw_fd);
        let leak = self.leak.as_ref().map(AsRawFd::as_raw_fd);
        // SAFETY: the closure runs between fork and exec and calls only
        // async-signal-safe functions (`setsid`, `dup2`).
        unsafe {
            command.pre_exec(move || {
                let mut placements = vec![(status, STATUS_FD)];
                placements.extend(stderr.map(|fd| (fd, STDERR_FD)));
                placements.extend(leak.map(|fd| (fd, LEAK_FD)));
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                for (source, target) in placements {
                    if libc::dup2(source, target) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        command.spawn().expect("spawn neomacs-ptyexec")
    }

    /// What the child wrote to the start-up error pipe.  Our own write end
    /// closes first, for the pipe to reach end of file.
    fn report(&mut self) -> Vec<u8> {
        self.status_write = None;
        let mut report = Vec::new();
        self.status_read
            .read_to_end(&mut report)
            .expect("read report");
        report
    }

    /// Everything the child wrote to the pty, until the slave closes.
    fn output(&mut self) -> String {
        let mut text = Vec::new();
        let mut chunk = [0u8; 4096];
        // Reading the master fails with EIO once every slave descriptor is
        // closed; that is this loop's end of file.
        while let Ok(count) = self.master.read(&mut chunk) {
            if count == 0 {
                break;
            }
            text.extend_from_slice(&chunk[..count]);
        }
        String::from_utf8_lossy(&text).replace('\r', "")
    }
}

/// Wait for `child`, killing it after a deadline so a bug fails the test
/// instead of hanging it.
fn wait(mut child: Child) -> ExitStatus {
    let pid = child.id() as libc::pid_t;
    // The watchdog is left to sleep out its deadline; the test process exits.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(30));
        // SAFETY: a signal to a pid that this test spawned.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    });
    child.wait().expect("wait")
}

#[test]
fn a_successful_exec_closes_the_status_pipe_without_a_report() {
    let mut harness = Harness::new();
    let child = harness.spawn(None, "/bin/sh", &["sh", "-c", "echo ran"]);
    assert!(harness.report().is_empty());
    assert_eq!(harness.output().trim(), "ran");
    assert!(wait(child).success());
}

#[test]
fn a_failed_exec_reports_errno_on_the_status_pipe_and_exits_127() {
    let mut harness = Harness::new();
    let child = harness.spawn(None, "/nonexistent/neomacs-ptyexec-test", &["x"]);
    let report = harness.report();
    let errno = i32::from_ne_bytes(report.try_into().expect("four bytes of errno"));
    assert_eq!(errno, libc::ENOENT);
    assert_eq!(wait(child).code(), Some(127));
}

#[test]
fn an_unopenable_slave_is_reported_too() {
    let mut harness = Harness::new();
    harness.slave_path = "/nonexistent/pts/9".to_string();
    let child = harness.spawn(None, "/bin/true", &["true"]);
    let errno = i32::from_ne_bytes(harness.report().try_into().expect("four bytes of errno"));
    assert_eq!(errno, libc::ENOENT);
    assert_eq!(wait(child).code(), Some(127));
}

#[test]
fn a_stderr_descriptor_becomes_fd_2_and_the_pty_keeps_fds_0_and_1() {
    let mut harness = Harness::new();
    let (mut errors, errors_write) = pipe();
    let child = harness.spawn(
        Some(&errors_write),
        "/bin/sh",
        &["sh", "-c", "echo to-pty; echo to-pipe >&2"],
    );
    drop(errors_write);
    assert!(harness.report().is_empty());
    assert_eq!(harness.output().trim(), "to-pty");
    let mut piped = String::new();
    errors.read_to_string(&mut piped).unwrap();
    assert_eq!(piped.trim(), "to-pipe");
    assert!(wait(child).success());
}

/// The child reads its own `/proc/<pid>/stat`: session id, controlling tty,
/// and the descriptors it holds.
#[test]
#[cfg(target_os = "linux")]
fn the_child_leads_a_session_controlled_by_the_slave_and_holds_no_other_descriptor() {
    let mut harness = Harness::new();
    harness.leak = Some(pipe().1);
    // `/proc/self` is the shell: the redirection is its own.
    let script = "read -r pid comm state ppid pgrp session tty rest < /proc/self/stat; \
                  echo \"$pid $session $tty\"; \
                  for fd in /proc/self/fd/*; do echo \"fd ${fd##*/}\"; done";
    let child = harness.spawn(None, "/bin/sh", &["sh", "-c", script]);
    assert!(harness.report().is_empty());
    let output = harness.output();
    assert!(wait(child).success());
    let mut lines = output.lines();
    let [pid, session, tty] = lines
        .next()
        .expect("the stat line")
        .split(' ')
        .map(|field| {
            field
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("a number in {output:?}"))
        })
        .collect::<Vec<_>>()[..]
    else {
        panic!("stat line of {output:?}");
    };
    assert_eq!(session, pid, "the child leads its own session");
    let rdev = std::fs::metadata(&harness.slave_path).unwrap().rdev();
    let (major, minor) = (libc::major(rdev) as u64, libc::minor(rdev) as u64);
    assert_eq!(
        tty,
        (major << 8) | (minor & 0xff) | ((minor & !0xff) << 12),
        "the slave is the controlling terminal"
    );
    // The shell's own `/proc/self/fd` scan holds one more descriptor (fd 3)
    // while it lists; the status pipe (4) and the leaked one (7) must be gone.
    let mut fds: Vec<&str> = lines.collect();
    fds.sort_unstable();
    assert_eq!(
        fds,
        ["fd 0", "fd 1", "fd 2", "fd 3"],
        "nothing else is open"
    );
}

/// The editor's own start through the trampoline (`NEOMACS_PTYEXEC` selects
/// it where the platform needs none, so Linux runs macOS's engine arm): the
/// child is a session leader with a controlling terminal, and a program that
/// cannot be run is reported as a failed exec.
#[test]
#[cfg(target_os = "linux")]
fn the_editor_starts_a_pty_child_through_the_trampoline() {
    let lisp = r#"
(let ((report nil))
  (dolist (command (list
                    (list "/bin/sh" "-c"
                          (concat "read -r pid comm state ppid pgrp session tty rest < /proc/self/stat; "
                                  "[ \"$pid\" = \"$session\" ] && echo LEADER; "
                                  "[ -t 0 ] && [ -t 1 ] && [ -t 2 ] && echo TTY; "
                                  ": < /dev/tty && echo CTTY"))
                    (list "/nonexistent/neomacs-ptyexec-test")))
    (let* ((out "")
           (p (make-process :name "t" :command command :connection-type 'pty
                            :noquery t
                            :filter (lambda (_p s) (setq out (concat out s))))))
      (while (process-live-p p) (accept-process-output p 0.1))
      (accept-process-output p 0.1)
      (push (list (process-exit-status p) (replace-regexp-in-string "\r" "" out)) report)))
  (princ (format "%S" (nreverse report))))
"#;
    let output = Command::new(env!("CARGO_BIN_EXE_neomacs"))
        .env("NEOMACS_PTYEXEC", env!("CARGO_BIN_EXE_neomacs-ptyexec"))
        .args(["-Q", "--batch", "--eval", lisp])
        .output()
        .expect("run the editor");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("(0 \"LEADER\nTTY\nCTTY\n\")"),
        "stdout {stdout:?} stderr {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("neomacs-ptyexec-test: No such file or directory"),
        "{stdout:?}"
    );
}
