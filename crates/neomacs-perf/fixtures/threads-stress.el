;;; threads-stress.el --- threaded allocation and GC stress -*- lexical-binding: t; -*-

(require 'json)

(defun neomacs-perf-threads-stress--required-env (name)
  (or (getenv name) (error "required performance environment %s is absent" name)))

(defvar neomacs-perf-threads-stress--gate-process nil)
(defvar neomacs-perf-threads-stress--gate-response "")
(defvar neomacs-perf-threads-stress--counter 0)
(defvar neomacs-perf-threads-stress--special 0)

(defun neomacs-perf-threads-stress--gate-filter (_process output)
  (setq neomacs-perf-threads-stress--gate-response
        (concat neomacs-perf-threads-stress--gate-response output)))

(defun neomacs-perf-threads-stress--sampling-command (command)
  (let* ((port-text (getenv "NEOMACS_PERF_GATE_PORT"))
         (port (and port-text (string-to-number port-text))))
    (when (and port-text (not (> port 0)))
      (error "invalid performance sampling gate port %S" port-text))
    (when (and port-text
               (not (process-live-p neomacs-perf-threads-stress--gate-process)))
      (setq neomacs-perf-threads-stress--gate-process
            (make-network-process
             :name "neomacs-perf-threads-stress-gate" :family 'ipv4
             :host "127.0.0.1" :service port :coding 'binary :noquery t
             :filter #'neomacs-perf-threads-stress--gate-filter)))
    (when port-text
      (setq neomacs-perf-threads-stress--gate-response "")
      (process-send-string neomacs-perf-threads-stress--gate-process (concat command "\n"))
      (let ((deadline (+ (float-time) 30.0)))
        (while (and (not (string-suffix-p "\n" neomacs-perf-threads-stress--gate-response))
                    (< (float-time) deadline))
          (unless (process-live-p neomacs-perf-threads-stress--gate-process)
            (error "performance sampling gate disconnected"))
          (accept-process-output neomacs-perf-threads-stress--gate-process 0.05))
        (unless (equal neomacs-perf-threads-stress--gate-response "ack\n")
          (error "performance sampling gate rejected %s: %S"
                 command neomacs-perf-threads-stress--gate-response))))))

(defun neomacs-perf-threads-stress--close-gate ()
  (when (processp neomacs-perf-threads-stress--gate-process)
    (delete-process neomacs-perf-threads-stress--gate-process)
    (setq neomacs-perf-threads-stress--gate-process nil)))

(defun neomacs-perf-threads-stress--with-gate (thunk)
  (let ((enabled nil))
    (unwind-protect
        (progn
          (neomacs-perf-threads-stress--sampling-command "enable")
          (setq enabled t)
          (funcall thunk))
      (when enabled
        (neomacs-perf-threads-stress--sampling-command "disable")))))

(defun neomacs-perf-threads-stress--one-iteration (mutex)
  (let* ((items (number-sequence 0 999))
         (list-sum (apply #'+ items))
         (neomacs-perf-threads-stress--special 71)
         (special-ok (= neomacs-perf-threads-stress--special 71))
         (table (make-hash-table :test 'eql))
         (string (concat "thread" (number-to-string list-sum))))
    (dotimes (index 100)
      (puthash index (+ index 1) table))
    (aset string 0 ?T)
    (unless (and (= list-sum 499500)
                 special-ok
                 (= (gethash 99 table) 100)
                 (equal (substring string 0 1) "T")
                 (equal (with-temp-buffer
                          (insert string)
                          (buffer-string))
                        string))
      (error "thread stress operation invariant failed"))
    ;; The shared counter update is intentionally the only workload lock.
    (mutex-lock mutex)
    (unwind-protect
        (setq neomacs-perf-threads-stress--counter
              (1+ neomacs-perf-threads-stress--counter))
      (mutex-unlock mutex))
    list-sum))

(defun neomacs-perf-threads-stress--worker (deadline mutex)
  (let ((count 0) (last-sum 0))
    (while (< (float-time) deadline)
      (setq last-sum (neomacs-perf-threads-stress--one-iteration mutex)
            count (1+ count))
      ;; Green runtimes need a yield between units so every worker reaches the deadline.
      (thread-yield))
    (vector count last-sum)))

(defun neomacs-perf-threads-stress--write-result
    (path status threads seconds iterations invariants gcs-done error-message)
  (with-temp-file path
    (insert
     (json-serialize
      `((schema_version . 1)
        (scenario . ,(neomacs-perf-threads-stress--required-env
                      "NEOMACS_PERF_SCENARIO_ID"))
        (status . ,status)
        (threads . ,threads)
        (seconds . ,seconds)
        (iterations_total . ,iterations)
        (invariants_ok . ,(if invariants t :false))
        (gcs_done_delta . ,gcs-done)
        (error . ,error-message))
      :null-object nil :false-object :false))))

(defun neomacs-perf-threads-stress--run ()
  (let* ((scenario (neomacs-perf-threads-stress--required-env
                    "NEOMACS_PERF_SCENARIO_ID"))
         (result-path (neomacs-perf-threads-stress--required-env "NEOMACS_PERF_RESULT"))
         (seconds (string-to-number (or (getenv "NEOMACS_PERF_ITERATIONS") "20")))
         (threads (num-processors))
         (mutex (make-mutex "neomacs-perf-thread-stress-counter"))
         (deadline nil) (results nil) (iterations-total 0)
         (gcs-before (and (boundp 'gcs-done) gcs-done))
         (gcs-delta nil) (invariants nil) (status "error") (error-message nil)
         (gc-cons-threshold (if-let ((value (getenv "NEOVM_GC_THRESHOLD")))
                                (string-to-number value)
                              gc-cons-threshold)))
    (condition-case err
        (progn
          (unless (equal scenario "threads-stress")
            (error "unknown thread stress scenario %s" scenario))
          (unless (and (> seconds 0) (> threads 0))
            (error "stress duration and thread count must be positive"))
          (setq neomacs-perf-threads-stress--counter 0
                deadline (+ (float-time) seconds))
          (neomacs-perf-threads-stress--with-gate
           (lambda ()
             (let ((workers nil))
               (dotimes (_ threads)
                 (push (make-thread
                        (lambda ()
                          (neomacs-perf-threads-stress--worker deadline mutex)))
                       workers))
               (setq results (mapcar #'thread-join (nreverse workers))))))
          (setq iterations-total
                (apply #'+ (mapcar (lambda (result) (aref result 0)) results)))
          (setq invariants
                (and (= iterations-total neomacs-perf-threads-stress--counter)
                     (cl-every (lambda (result) (= (aref result 1) 499500)) results)))
          (unless invariants
            (error "thread stress invariant failed: iterations=%d counter=%d last-sums=%S"
                   iterations-total neomacs-perf-threads-stress--counter
                   (mapcar (lambda (result) (aref result 1)) results)))
          (setq gcs-delta (and gcs-before (boundp 'gcs-done) (- gcs-done gcs-before))
                status "ok"))
      (error
       (setq error-message (error-message-string err))
       (message "thread stress scenario invalid: %s" error-message)))
    (neomacs-perf-threads-stress--write-result
     result-path status threads seconds iterations-total invariants gcs-delta error-message)
    (neomacs-perf-threads-stress--close-gate)
    (write-region "done\n" nil (neomacs-perf-threads-stress--required-env "SENTINEL") nil 'silent)
    (kill-emacs 0)))

(require 'cl-lib)
(if noninteractive
    (neomacs-perf-threads-stress--run)
  (run-at-time 0 nil #'neomacs-perf-threads-stress--run))

;;; threads-stress.el ends here
