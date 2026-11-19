#![cfg(unix)]
//! Live GNU authority for the width of the mode line's position percentages.
//!
//! GNU's `decode_mode_spec` prints `%p`, `%o` and the number in `%P` with
//! `sprintf ("%2d%%")` (and `"Top%2d%%"`), so a one-digit percentage takes
//! two columns, while `%q` uses `"%d%%"` and `"%d-%d%%"`. A percentage only
//! appears after a redisplay has recorded where the window ends, which is why
//! this needs a terminal and not `--batch`.

use std::path::PathBuf;
use std::time::Duration;

use neomacs_tui_tests::{TuiLaunch, TuiProcessOutcome, TuiSession, TuiTerminalConfig};

const PROBE_EL: &str = r##";;; -*- lexical-binding: t; -*-
(let ((out (getenv "NEOMACS_PCT_PAD_OUT"))
      (ready (getenv "NEOMACS_PCT_PAD_READY"))
      (release (getenv "NEOMACS_PCT_PAD_RELEASE"))
      (spec "%p|%P|%o|%q")
      (values nil))
  (when noninteractive
    (error "The percent-padding probe needs a terminal frame"))
  (switch-to-buffer (get-buffer-create "pct-pad"))
  (fundamental-mode)
  (setq header-line-format nil tab-line-format nil truncate-lines t)
  (erase-buffer)
  (dotimes (i 2000) (insert (format "line-%04d\n" i)))
  (setq mode-line-format (list "PCT[" spec "]"))
  ;; One-digit, two-digit and end-of-buffer window starts.
  (dolist (line '(0 3 23 30 101 500 1001 1985))
    (goto-char (point-min))
    (forward-line line)
    (set-window-start nil (point))
    (redisplay t)
    (push (list line 'after-redisplay (window-start) (format-mode-line spec)) values)
    ;; A start set after the last redisplay, read before the next one: the
    ;; tty scroll replay's case.
    (forward-line 40)
    (set-window-start nil (point))
    (push (list line 'stale-end (window-start) (format-mode-line spec)) values))
  (goto-char (point-min))
  (forward-line 30)
  (set-window-start nil (point))
  (redisplay t)
  (with-temp-file out (prin1 (nreverse values) (current-buffer)))
  (let ((inhibit-redisplay t))
    (with-temp-file ready (insert "ready\n"))
    (while (not (file-exists-p release))
      (accept-process-output nil 0.01))))
(kill-emacs 0)
"##;

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    values: String,
    mode_line: String,
}

fn run_probe(name: &str, program: PathBuf, gnu: bool) -> Observation {
    let home = neomacs_tui_tests::TuiTempDirectory::new("neomacs-percent-padding-");
    let script = home.path().join("percent-padding.el");
    let out = home.path().join("percent-padding.out");
    let ready = home.path().join("percent-padding.ready");
    let release = home.path().join("percent-padding.release");
    std::fs::write(&script, PROBE_EL).expect("write percent-padding probe");
    let mut launch = TuiLaunch::new(program.as_os_str()).arg("-nw").arg("-Q");
    if gnu {
        launch = launch
            .arg("-no-comp-spawn")
            .arg("--eval=(progn(set'native-comp-jit-compilation())(set'native-comp-async-report-warnings-errors'silent))");
    }
    launch = launch
        .arg("-l")
        .arg(&script)
        .env("HOME", home.path())
        .env("TMPDIR", home.path())
        .env("LC_ALL", "C.UTF-8")
        .env("NEOMACS_PCT_PAD_OUT", &out)
        .env("NEOMACS_PCT_PAD_READY", &ready)
        .env("NEOMACS_PCT_PAD_RELEASE", &release);
    let mut session = TuiSession::spawn_launch_on_terminal(
        launch,
        name,
        TuiTerminalConfig::new("xterm-256color", 40, 120),
    );
    session.read_until(Duration::from_secs(60), |_| ready.exists());
    assert!(
        ready.exists(),
        "{name} percent-padding probe did not reach READY; grid:\n{}",
        session.text_grid().join("\n")
    );
    session.read(Duration::from_millis(100));
    let values = std::fs::read_to_string(&out).expect("percent-padding OUT before READY");
    let mode_line = session
        .text_grid()
        .get(38)
        .expect("40-row terminal")
        .clone();
    std::fs::write(&release, "release\n").expect("release owned editor");
    assert_eq!(
        session.run_to_completion(Duration::from_secs(15)),
        TuiProcessOutcome::Exited,
        "{name} percent-padding probe did not exit"
    );
    Observation { values, mode_line }
}

#[test]
fn position_percentages_are_padded_like_gnu() {
    let gnu = run_probe("GNU", neomacs_tui_tests::gnu_oracle_program(), true);
    // Guard the probe itself: the final window start (line 30 of 2000) shows
    // a one-digit `%p`, so GNU's row must hold a padded percentage, or the
    // comparison below would not exercise the padding at all.
    assert!(
        gnu.mode_line.contains("PCT[ "),
        "GNU did not display the probe's mode line: {:?}",
        gnu.mode_line
    );
    let program = neomacs_tui_tests::neomacs_binary();
    neomacs_tui_tests::assert_neomacs_dumped_runtime(&program);
    let neo = run_probe("Neomacs", program, false);
    assert_eq!(
        neo, gnu,
        "mode-line position percentages differ; the expected values and row come from live GNU"
    );
}
