;;; threads-numeric-loop.el --- Lisp thread numeric benchmark -*- lexical-binding: t; -*-

(require 'json)

(defun neomacs-perf-threads--required-env (name)
  (or (getenv name) (error "required performance environment %s is absent" name)))

(defvar neomacs-perf-threads--gate-process nil)
(defvar neomacs-perf-threads--gate-response "")

(defun neomacs-perf-threads--gate-filter (_process output)
  (setq neomacs-perf-threads--gate-response
        (concat neomacs-perf-threads--gate-response output)))

(defun neomacs-perf-threads--sampling-command (command)
  (let* ((port-text (getenv "NEOMACS_PERF_GATE_PORT"))
         (port (and port-text (string-to-number port-text))))
    (when (and port-text (not (> port 0)))
      (error "invalid performance sampling gate port %S" port-text))
    (when (and port-text (not (process-live-p neomacs-perf-threads--gate-process)))
      (setq neomacs-perf-threads--gate-process
            (make-network-process
             :name "neomacs-perf-threads-gate" :family 'ipv4
             :host "127.0.0.1" :service port :coding 'binary :noquery t
             :filter #'neomacs-perf-threads--gate-filter)))
    (when port-text
      (setq neomacs-perf-threads--gate-response "")
      (process-send-string neomacs-perf-threads--gate-process (concat command "\n"))
      (let ((deadline (+ (float-time) 30.0)))
        (while (and (not (string-suffix-p "\n" neomacs-perf-threads--gate-response))
                    (< (float-time) deadline))
          (unless (process-live-p neomacs-perf-threads--gate-process)
            (error "performance sampling gate disconnected"))
          (accept-process-output neomacs-perf-threads--gate-process 0.05))
        (unless (equal neomacs-perf-threads--gate-response "ack\n")
          (error "performance sampling gate rejected %s: %S"
                 command neomacs-perf-threads--gate-response))))))

(defun neomacs-perf-threads--close-gate ()
  (when (processp neomacs-perf-threads--gate-process)
    (delete-process neomacs-perf-threads--gate-process)
    (setq neomacs-perf-threads--gate-process nil)))

(defun neomacs-perf-threads--with-gate (thunk)
  (let ((enabled nil))
    (unwind-protect
        (progn
          (neomacs-perf-threads--sampling-command "enable")
          (setq enabled t)
          (funcall thunk))
      (when enabled (neomacs-perf-threads--sampling-command "disable")))))

(defun neomacs-perf-threads--run-thread-batch (fn argument count)
  (let ((threads nil))
    (dotimes (_ count)
      (push (make-thread (lambda () (funcall fn argument))) threads))
    (mapcar #'thread-join (nreverse threads))))

(defun neomacs-perf-threads--run-isolate-batch (fn argument count)
  (let ((handles nil))
    (dotimes (_ count)
      (push (neomacs-worker-run fn argument) handles))
    (mapcar #'neomacs-worker-wait (nreverse handles))))

(defun neomacs-perf-threads--check-results (label values expected)
  (dolist (value values)
    (unless (equal value expected)
      (error "%s result differs from serial result" label))))

(defun neomacs-perf-threads--elapsed-us (thunk)
  (let ((started (float-time)))
    (funcall thunk)
    (round (* 1000000 (- (float-time) started)))))

(defun neomacs-perf-threads--numeric-loop (n)
  (let ((s 0))
    (dotimes (i n)
      (setq s (logand (+ s (* i i)) #xffffff)))
    s))

(defun neomacs-perf-threads--threading-mode ()
  (or (getenv "NEOVM_THREADS") "sim"))

(defun neomacs-perf-threads--shared-heap-stats ()
  (when (fboundp 'neomacs-shared-heap-stats)
    (let ((plist (neomacs-shared-heap-stats)) (fields nil))
      (while plist
        (let ((key (pop plist)) (value (pop plist)))
          (push (cons (substring (symbol-name key) 1)
                      (if (symbolp value) (symbol-name value) value))
                fields)))
      (nreverse fields))))

(defun neomacs-perf-threads--write-result
    (path status threads iterations serial threads-us isolates speedup isolate-speedup error-message)
  (with-temp-file path
    (insert
     (json-serialize
      `((schema_version . 1)
        (scenario . ,(neomacs-perf-threads--required-env "NEOMACS_PERF_SCENARIO_ID"))
        (status . ,status)
        (threads . ,threads)
        (iterations . ,iterations)
        (serial_us . ,serial)
        (threads_us . ,threads-us)
        (isolates_us . ,isolates)
        (threads_speedup . ,speedup)
        (isolates_speedup . ,isolate-speedup)
        (equal_ok . ,(if (equal status "ok") t :false))
        (threading_mode . ,(neomacs-perf-threads--threading-mode))
        (shared_heap_stats . ,(neomacs-perf-threads--shared-heap-stats))
        (error . ,error-message))
      :null-object nil :false-object :false))))

(defun neomacs-perf-threads--run ()
  (let* ((scenario (neomacs-perf-threads--required-env "NEOMACS_PERF_SCENARIO_ID"))
         (result-path (neomacs-perf-threads--required-env "NEOMACS_PERF_RESULT"))
         (n (string-to-number (or (getenv "NEOMACS_PERF_ITERATIONS") "3000000")))
         (threads (num-processors))
         (fn (symbol-function 'neomacs-perf-threads--numeric-loop))
         (expected nil) (serial-us 0) (threads-us 0) (isolates-us nil)
         (thread-results nil) (isolate-results nil)
         (threads-speedup nil) (isolates-speedup nil)
         (status "error") (error-message nil))
    (condition-case err
        (progn
          (unless (equal scenario "threads-numeric-loop")
            (error "unknown numeric thread scenario %s" scenario))
          (unless (> n 0) (error "iteration count must be positive"))
          (setq expected (funcall fn n))
          (when (featurep 'neomacs-worker-isolates)
            (unless (and (fboundp 'neomacs-worker-run) (fboundp 'neomacs-worker-wait))
              (error "worker-isolate feature lacks its Lisp entry points")))
          (neomacs-perf-threads--with-gate
           (lambda ()
             (setq serial-us
                   (neomacs-perf-threads--elapsed-us
                    (lambda () (dotimes (_ threads) (funcall fn n)))))
             (setq threads-us
                   (neomacs-perf-threads--elapsed-us
                    (lambda ()
                      (setq thread-results
                            (neomacs-perf-threads--run-thread-batch fn n threads)))))
             (when (featurep 'neomacs-worker-isolates)
               (setq isolates-us
                     (neomacs-perf-threads--elapsed-us
                      (lambda ()
                        (setq isolate-results
                              (neomacs-perf-threads--run-isolate-batch fn n threads))))))))
          (neomacs-perf-threads--check-results "Lisp-thread" thread-results expected)
          (when isolates-us
            (neomacs-perf-threads--check-results "worker-isolate" isolate-results expected))
          (setq threads-speedup (/ (* 1.0 serial-us) threads-us)
                isolates-speedup (and isolates-us (/ (* 1.0 serial-us) isolates-us))
                status "ok"))
      (error
       (setq error-message (error-message-string err))
       (message "thread numeric scenario invalid: %s" error-message)))
    (neomacs-perf-threads--write-result result-path status threads n serial-us threads-us
                                        isolates-us threads-speedup isolates-speedup error-message)
    (neomacs-perf-threads--close-gate)
    (write-region "done\n" nil (neomacs-perf-threads--required-env "SENTINEL") nil 'silent)
    (kill-emacs 0)))

(if noninteractive
    (neomacs-perf-threads--run)
  (run-at-time 0 nil #'neomacs-perf-threads--run))

;;; threads-numeric-loop.el ends here
