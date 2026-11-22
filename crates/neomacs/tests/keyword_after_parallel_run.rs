use std::{path::PathBuf, process::Command};

struct RestoreDump(Option<(PathBuf, PathBuf)>);

impl Drop for RestoreDump {
    fn drop(&mut self) {
        if let Some((saved, original)) = self.0.take() {
            let _ = std::fs::rename(saved, original);
        }
    }
}

/// Run the release-path reproduction reported for fresh keyword symbols.
/// Keep the workers and the two later interning paths in one batch eval so
/// fresh symbols are created only after parallel execution has started.
#[test]
fn fresh_keywords_after_parallel_run_are_self_evaluating() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_neomacs"));
    // This is the no-pdump repro. Cargo can leave an image from the previous
    // link beside the binary, so move that generated artifact aside and put it
    // back even if the assertion below fails.
    let final_dump = binary.parent().unwrap().join("neomacs.pdump");
    let held_dump = binary.parent().unwrap().join(format!(
        ".neomacs.pdump.keyword-repro-{}",
        std::process::id()
    ));
    let restore = if final_dump.exists() {
        std::fs::rename(&final_dump, &held_dump).expect("move stale adjacent pdump aside");
        RestoreDump(Some((held_dump, final_dump)))
    } else {
        RestoreDump(None)
    };
    let output = Command::new(binary)
        .env("NEOVM_THREADS", "parallel")
        .args([
            "-Q",
            "--batch",
            "--eval",
            "(progn (mapc (function thread-join) (mapcar (lambda (_) (make-thread (lambda () (let ((s 0)) (dotimes (i 100000) (setq s (+ s i))) s)))) (number-sequence 1 8))) (message \"R %S\" (list (condition-case e (eval (car (read-from-string \":zzkw1\"))) (error e)) (condition-case e (eval (intern \":zzkw2\")) (error e)))))",
        ])
        .output()
        .expect("run release keyword reproduction");
    drop(restore);

    assert!(
        output.status.success(),
        "neomacs exited {:?}:\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        report.contains("R (:zzkw1 :zzkw2)"),
        "fresh keywords were not self-evaluating:\n{report}"
    );
}
