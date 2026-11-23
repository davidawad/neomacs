//! Wait passes do no per-child work for children whose status did not change:
//! GNU walks children only after `SIGCHLD` (src/process.c:7691).  Exits, kills
//! and stops are still recorded and notified inside the wait that finds them.

use neovm_eval::emacs_core::eval::Context;
use neovm_eval::emacs_core::eval::data::process_state::{
    ChildHandle, ChildOwnership, ReapableChild,
};
use neovm_eval::emacs_core::print::format_eval_result;
use neovm_system::emacs_core::process::*;
use neovm_value::heap_types::{LispString, ProcessId};
use neovm_value::value::Value;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const IDLE_CHILDREN: usize = 50;
const ROUND_TRIPS: usize = 100;

/// A running child that never changes state and counts how often it is asked.
#[derive(Debug)]
struct IdleChild(Arc<AtomicUsize>);

impl portable_pty::ChildKiller for IdleChild {
    fn kill(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(IdleChild(Arc::clone(&self.0)))
    }
}

impl portable_pty::Child for IdleChild {
    fn try_wait(&mut self) -> io::Result<Option<portable_pty::ExitStatus>> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(None)
    }

    fn wait(&mut self) -> io::Result<portable_pty::ExitStatus> {
        panic!("a wait pass must never block on a child")
    }

    fn process_id(&self) -> Option<u32> {
        None
    }
}

fn eval(ev: &mut Context, src: &str) -> String {
    format_eval_result(&ev.eval_str(src))
}

/// Evaluate FORM in a bare context beside fifty live process records whose
/// child status probes are counted; return its result and that count.  The
/// records carry the pid of a real `sleep` child, reaped and marked reaped
/// before teardown.  With no subr.el, `idle-live-p` spells `process-live-p`,
/// `idle-wait` is the usual wait loop, `idle-record` collects sentinel events.
fn eval_among_idle_children(form: &str) -> (String, usize) {
    crate::test_utils::init_test_tracing();
    let mut ev = crate::emacs_core::eval_hook_installers::new_context();
    let mut sleeper = std::process::Command::new("/bin/sh")
        .args(["-c", "exec sleep 300"])
        .spawn()
        .expect("spawn the pid the idle records carry");
    let probes = Arc::new(AtomicUsize::new(0));
    let ids: Vec<ProcessId> = (0..IDLE_CHILDREN)
        .map(|n| {
            let processes = ev.processes_mut();
            let id = processes.create_process_lisp(
                LispString::from_utf8(&format!("idle-{n}")),
                Value::NIL,
                LispString::from_utf8("idle"),
                Vec::new(),
                ProcessCodingSystems::gnu_make_process_initial(),
            );
            processes.get_mut(id).expect("idle record").live_io.child =
                ChildOwnership::Unreaped(ReapableChild {
                    pid: sleeper.id(),
                    handle: ChildHandle::Pty(Box::new(IdleChild(Arc::clone(&probes)))),
                });
            id
        })
        .collect();
    let prelude = eval(
        &mut ev,
        r#"(progn
             (defvar idle-events nil)
             (fset 'idle-live-p (function (lambda (p)
               (memq (process-status p) '(run open listen connect stop)))))
             (fset 'idle-record (function (lambda (_p e) (setq idle-events (cons e idle-events)))))
             (fset 'idle-wait (function (lambda (p)
               (let ((n 0))
                 (while (and (idle-live-p p) (< n 200)) (accept-process-output p 0.1) (setq n (1+ n)))))))
             (fset 'idle-spin (function (lambda (s)
               (let ((end (+ (float-time) s))) (while (< (float-time) end)))))))"#,
    );
    assert!(prelude.starts_with("OK "), "the helper forms: {prelude}");
    let before = probes.load(Ordering::Relaxed);
    let result = eval(&mut ev, &form.replace("SH", "/bin/sh"));
    let probed = probes.load(Ordering::Relaxed) - before;
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    for id in ids {
        if let Some(proc) = ev.processes_mut().get_mut(id) {
            proc.live_io.child = ChildOwnership::Reaped;
        }
    }
    (result, probed)
}

#[test]
fn wait_round_trips_do_not_probe_idle_children() {
    let (result, probed) = eval_among_idle_children(&format!(
        r#"(let* ((echoes 0) (i 0)
                  (p (make-process :name "echo" :command '("SH" "-c" "exec cat")
                                   :connection-type 'pipe :noquery t
                                   :filter (function (lambda (_p _s) (setq echoes (1+ echoes)))))))
             (while (< i {ROUND_TRIPS})
               (let ((want (1+ echoes)))
                 (process-send-string p "x\n")
                 (while (< echoes want) (accept-process-output p 1)))
               (setq i (1+ i)))
             (delete-process p)
             echoes)"#
    ));
    assert_eq!(result, format!("OK {ROUND_TRIPS}"));
    // Exactly zero: no child of this test process has a report pending during
    // the round trips (the sleeper runs, the echo child is reaped only after
    // the loop, nextest runs each test in its own process).  Any pending
    // report, even a stranger's, opens the gate for every idle record.
    assert_eq!(
        probed, 0,
        "{ROUND_TRIPS} round trips beside {IDLE_CHILDREN} idle children probed them {probed} times"
    );
}
#[test]
fn wait_records_and_notifies_an_exit_among_idle_children() {
    let (result, _) = eval_among_idle_children(
        r#"(let ((p (make-process :name "exit3" :command '("SH" "-c" "exit 3")
                                  :connection-type 'pipe :noquery t :sentinel 'idle-record)))
             (idle-wait p)
             (list (idle-live-p p) idle-events (process-status p) (process-exit-status p)))"#,
    );
    assert_eq!(
        result,
        "OK (nil (\"exited abnormally with code 3\n\") exit 3)"
    );
}

#[test]
fn wait_records_and_notifies_a_signalled_child_among_idle_children() {
    let (result, _) = eval_among_idle_children(
        r#"(let ((p (make-process :name "killed" :command '("SH" "-c" "exec cat")
                                  :connection-type 'pipe :noquery t :sentinel 'idle-record)))
             (accept-process-output p 0.05)
             (signal-process p 'KILL)
             (idle-wait p)
             (list (idle-live-p p) idle-events (process-status p) (process-exit-status p)))"#,
    );
    assert_eq!(result, "OK (nil (\"killed\n\") signal 9)");
}

/// A stop has no exit event behind it, only the kernel's `WUNTRACED` report.
#[test]
fn wait_records_and_notifies_a_stop_among_idle_children() {
    let (result, _) = eval_among_idle_children(
        r#"(let ((p (make-process :name "stopped" :command '("SH" "-c" "exec cat")
                                  :connection-type 'pipe :noquery t :sentinel 'idle-record))
                 (n 0) (seen nil))
             (accept-process-output p 0.05)
             (signal-process p 'STOP)
             (while (and (eq (process-status p) 'run) (< n 200))
               (accept-process-output p 0.1) (setq n (1+ n)))
             (setq seen (list (process-status p) idle-events))
             (continue-process p)
             (setq seen (append seen (list (process-status p))))
             (signal-process p 'KILL)
             (idle-wait p)
             seen)"#,
    );
    assert_eq!(result, "OK (stop (\"stopped (signal)\n\") run)");
}

/// The idiom the after-output probe exists for: a child that writes and exits
/// at once, waited on until it is no longer live.  Its sentinel must have run
/// by then, every time.
#[test]
fn wait_publishes_an_exit_right_after_output_among_idle_children() {
    let (result, _) = eval_among_idle_children(
        r#"(let ((i 0) (lost 0))
             (while (< i 60)
               (setq idle-events nil)
               (idle-wait (make-process :name "printexit" :command '("SH" "-c" "printf x; exit 0")
                                        :connection-type 'pipe :noquery t :sentinel 'idle-record
                                        :filter (function (lambda (_p _s) nil))))
               (if idle-events nil (setq lost (1+ lost)))
               (setq i (1+ i)))
             lost)"#,
    );
    assert_eq!(
        result, "OK 0",
        "sentinels lost out of 60 short-lived children"
    );
}

/// An end-of-file read runs Lisp too: its zero-byte last block goes through
/// `:post-read-conversion` (evaluated in a `with-temp-buffer`, defined here).
/// A child killed while that hook runs is published by the probe after the
/// read, inside the zero-timeout wait that read it.
#[test]
fn wait_publishes_an_exit_during_an_end_of_file_decode_among_idle_children() {
    let (result, _) = eval_among_idle_children(
        r#"(progn
             (defvar idle-eof-proc nil)
             (fset 'with-temp-buffer (cons 'macro (function (lambda (&rest body)
               (cons 'save-current-buffer
                     (cons '(set-buffer (get-buffer-create " *eof*")) (cons '(erase-buffer) body)))))))
             (fset 'idle-eof-hook (function (lambda (len)
               (if idle-eof-proc
                   (progn (signal-process idle-eof-proc 'KILL) (setq idle-eof-proc nil) (idle-spin 0.3)))
               len)))
             (define-coding-system-internal 'idle-eof-utf-8 ?U 'utf-8 '(unicode) t
               nil nil 'idle-eof-hook nil nil nil nil 'unix nil)
             (let ((p (make-process :name "eof" :command '("SH" "-c" "exec >&- 2>&-; exec sleep 30")
                                    :connection-type 'pipe :noquery t :coding 'idle-eof-utf-8
                                    :filter (function (lambda (_p _s) nil)) :sentinel 'idle-record)))
               (setq idle-eof-proc p)
               (idle-spin 0.3)
               (accept-process-output p 0)
               (prog1 (list idle-events idle-eof-proc) (delete-process p))))"#,
    );
    assert_eq!(result, "OK ((\"killed\n\") nil)");
}

/// Read syscalls this process has made so far (`syscr` in `/proc/self/io`).
#[cfg(target_os = "linux")]
fn read_syscalls() -> u64 {
    let io = std::fs::read_to_string("/proc/self/io").expect("read /proc/self/io");
    io.lines()
        .find_map(|line| line.strip_prefix("syscr: "))
        .and_then(|count| count.parse().ok())
        .expect("a syscr line")
}

/// GNU reads only the descriptors `pselect` reported (src/process.c:5993), so
/// an idle child's pipe is not read.  Fifty real idle `cat` children, then
/// the round trips of `wait_round_trips_do_not_probe_idle_children`.  What
/// the process still reads is the echo and the poller's own wake-up counter,
/// which the poller drains on every wait: a few reads per round trip, however
/// many children are idle.  One read of each idle pipe per round trip alone
/// would be fifty.
#[cfg(target_os = "linux")]
#[test]
fn wait_round_trips_do_not_read_idle_children() {
    crate::test_utils::init_test_tracing();
    let mut ev = crate::emacs_core::eval_hook_installers::new_context();
    let setup = eval(
        &mut ev,
        &format!(
            r#"(progn
                 (defvar idle-cats nil)
                 (defvar echoes 0)
                 (let ((i 0))
                   (while (< i {IDLE_CHILDREN})
                     (setq idle-cats (cons (make-process :name "idle" :command '("/bin/sh" "-c" "exec cat")
                                                         :connection-type 'pipe :noquery t)
                                           idle-cats))
                     (setq i (1+ i))))
                 (defvar echo (make-process :name "echo" :command '("/bin/sh" "-c" "exec cat")
                                            :connection-type 'pipe :noquery t
                                            :filter (function (lambda (_p _s) (setq echoes (1+ echoes))))))
                 (process-send-string echo "x\n")
                 (while (< echoes 1) (accept-process-output echo 1))
                 (length idle-cats))"#
        ),
    );
    assert_eq!(setup, format!("OK {IDLE_CHILDREN}"));
    let before = read_syscalls();
    let result = eval(
        &mut ev,
        &format!(
            r#"(let ((i 0))
                 (while (< i {ROUND_TRIPS})
                   (let ((want (1+ echoes)))
                     (process-send-string echo "x\n")
                     (while (< echoes want) (accept-process-output echo 1)))
                   (setq i (1+ i)))
                 (1- echoes))"#
        ),
    );
    let reads = read_syscalls() - before;
    let _ = eval(&mut ev, "(mapc 'delete-process (cons echo idle-cats))");
    assert_eq!(result, format!("OK {ROUND_TRIPS}"));
    assert!(
        reads < 10 * ROUND_TRIPS as u64,
        "{ROUND_TRIPS} round trips beside {IDLE_CHILDREN} idle children made {reads} reads"
    );
}
