;;; threads-json-parse.el --- threaded JSON parser benchmark -*- lexical-binding: t; -*-

(require 'json)

(defun neomacs-perf-threads-json--required-env (name)
  (or (getenv name) (error "required performance environment %s is absent" name)))

(defvar neomacs-perf-threads-json--gate-process nil)
(defvar neomacs-perf-threads-json--gate-response "")

(defun neomacs-perf-threads-json--gate-filter (_process output)
  (setq neomacs-perf-threads-json--gate-response
        (concat neomacs-perf-threads-json--gate-response output)))

(defun neomacs-perf-threads-json--sampling-command (command)
  (let* ((port-text (getenv "NEOMACS_PERF_GATE_PORT"))
         (port (and port-text (string-to-number port-text))))
    (when (and port-text (not (> port 0)))
      (error "invalid performance sampling gate port %S" port-text))
    (when (and port-text
               (not (process-live-p neomacs-perf-threads-json--gate-process)))
      (setq neomacs-perf-threads-json--gate-process
            (make-network-process
             :name "neomacs-perf-threads-json-gate" :family 'ipv4
             :host "127.0.0.1" :service port :coding 'binary :noquery t
             :filter #'neomacs-perf-threads-json--gate-filter)))
    (when port-text
      (setq neomacs-perf-threads-json--gate-response "")
      (process-send-string neomacs-perf-threads-json--gate-process (concat command "\n"))
      (let ((deadline (+ (float-time) 30.0)))
        (while (and (not (string-suffix-p "\n" neomacs-perf-threads-json--gate-response))
                    (< (float-time) deadline))
          (unless (process-live-p neomacs-perf-threads-json--gate-process)
            (error "performance sampling gate disconnected"))
          (accept-process-output neomacs-perf-threads-json--gate-process 0.05))
        (unless (equal neomacs-perf-threads-json--gate-response "ack\n")
          (error "performance sampling gate rejected %s: %S"
                 command neomacs-perf-threads-json--gate-response))))))

(defun neomacs-perf-threads-json--close-gate ()
  (when (processp neomacs-perf-threads-json--gate-process)
    (delete-process neomacs-perf-threads-json--gate-process)
    (setq neomacs-perf-threads-json--gate-process nil)))

(defun neomacs-perf-threads-json--with-gate (thunk)
  (let ((enabled nil))
    (unwind-protect
        (progn
          (neomacs-perf-threads-json--sampling-command "enable")
          (setq enabled t)
          (funcall thunk))
      (when enabled
        (neomacs-perf-threads-json--sampling-command "disable")))))

(defun neomacs-perf-threads-json--run-thread-batch (fn argument count)
  (let ((threads nil))
    (dotimes (_ count)
      (push (make-thread (lambda () (funcall fn argument))) threads))
    (mapcar #'thread-join (nreverse threads))))

(defun neomacs-perf-threads-json--run-isolate-batch (fn argument count)
  (let ((handles nil))
    (dotimes (_ count)
      (push (neomacs-worker-run fn argument) handles))
    (mapcar #'neomacs-worker-wait (nreverse handles))))

(defun neomacs-perf-threads-json--same-json-p (left right)
  "Compare JSON values recursively, including hash-table objects by key set."
  (cond
   ((and (hash-table-p left) (hash-table-p right))
    (and (= (hash-table-count left) (hash-table-count right))
         (let ((same t))
           (maphash (lambda (key value)
                      (if (eq (gethash key right :neomacs-missing) :neomacs-missing)
                          (setq same nil)
                        (unless (neomacs-perf-threads-json--same-json-p
                                 value (gethash key right))
                          (setq same nil))))
                    left)
           same)))
   ((and (consp left) (consp right))
    (and (neomacs-perf-threads-json--same-json-p (car left) (car right))
         (neomacs-perf-threads-json--same-json-p (cdr left) (cdr right))))
   ((and (vectorp left) (vectorp right) (not (stringp left)) (not (stringp right)))
    (and (= (length left) (length right))
         (let ((same t))
           (dotimes (index (length left))
             (unless (neomacs-perf-threads-json--same-json-p
                      (aref left index) (aref right index))
               (setq same nil)))
           same)))
   (t (equal left right))))

(defun neomacs-perf-threads-json--check-results (label values expected)
  (dolist (value values)
    (unless (neomacs-perf-threads-json--same-json-p value expected)
      (error "%s parsed JSON differs structurally from serial result" label))))

(defun neomacs-perf-threads-json--elapsed-us (thunk)
  (let ((started (float-time)))
    (funcall thunk)
    (round (* 1000000 (- (float-time) started)))))

(defun neomacs-perf-threads-json--document ()
  (concat
   "{\"records\":["
   (mapconcat
    (lambda (i)
      (format
       "{\"id\":%d,\"label\":\"record-%04d-sample\",\"numbers\":[1,3,5,7,11,13,17,19],\"strings\":[\"alpha\",\"beta\",\"gamma\",\"delta\"],\"nested\":{\"active\":true,\"ratio\":0.625,\"path\":[\"root\",\"items\",\"leaf\"]}}"
       i i))
    (number-sequence 1 1000)
    ",")
   "]}"))

(defun neomacs-perf-threads-json--work (arguments)
  (let ((source (nth 0 arguments))
        (calls (nth 1 arguments))
        (object-type (nth 2 arguments))
        (results nil))
    (dotimes (_ calls)
      (push (json-parse-string source :object-type object-type :array-type 'array)
            results))
    (nreverse results)))

(defun neomacs-perf-threads-json--threading-mode ()
  (or (getenv "NEOVM_THREADS") "sim"))

(defun neomacs-perf-threads-json--write-result
    (path status threads iterations hash-times alist-times equal-ok error-message)
  (let ((hash-serial (nth 0 hash-times))
        (hash-threads (nth 1 hash-times))
        (hash-isolates (nth 2 hash-times))
        (alist-serial (nth 0 alist-times))
        (alist-threads (nth 1 alist-times))
        (alist-isolates (nth 2 alist-times)))
    (with-temp-file path
      (insert
       (json-serialize
        `((schema_version . 1)
          (scenario . ,(neomacs-perf-threads-json--required-env
                        "NEOMACS_PERF_SCENARIO_ID"))
          (status . ,status)
          (threads . ,threads)
          (iterations . ,iterations)
          (serial_us . ,hash-serial)
          (threads_us . ,hash-threads)
          (isolates_us . ,hash-isolates)
          (threads_speedup . ,(and hash-serial hash-threads
                                   (/ (* 1.0 hash-serial) hash-threads)))
          (isolates_speedup . ,(and hash-serial hash-isolates
                                    (/ (* 1.0 hash-serial) hash-isolates)))
          (alist_serial_us . ,alist-serial)
          (alist_threads_us . ,alist-threads)
          (alist_isolates_us . ,alist-isolates)
          (alist_threads_speedup . ,(and alist-serial alist-threads
                                         (/ (* 1.0 alist-serial) alist-threads)))
          (alist_isolates_speedup . ,(and alist-serial alist-isolates
                                          (/ (* 1.0 alist-serial) alist-isolates)))
          (equal_ok . ,(if equal-ok t :false))
          (threading_mode . ,(neomacs-perf-threads-json--threading-mode))
          (error . ,error-message))
        :null-object nil :false-object :false)))))

(defun neomacs-perf-threads-json--measure-mode (source iterations threads object-type)
  (let* ((argument (list source iterations object-type))
         (fn #'neomacs-perf-threads-json--work)
         (expected (funcall fn argument))
         (serial-us 0) (threads-us 0) (isolates-us nil)
         (thread-results nil) (isolate-results nil))
    (neomacs-perf-threads-json--with-gate
     (lambda ()
       (setq serial-us
             (neomacs-perf-threads-json--elapsed-us
              (lambda () (dotimes (_ threads) (funcall fn argument)))))
       (setq threads-us
             (neomacs-perf-threads-json--elapsed-us
              (lambda ()
                (setq thread-results
                      (neomacs-perf-threads-json--run-thread-batch fn argument threads)))))
       (when (featurep 'neomacs-worker-isolates)
         (setq isolates-us
               (neomacs-perf-threads-json--elapsed-us
                (lambda ()
                  (setq isolate-results
                        (neomacs-perf-threads-json--run-isolate-batch fn argument threads))))))))
    (neomacs-perf-threads-json--check-results "Lisp-thread" thread-results expected)
    (when isolates-us
      (neomacs-perf-threads-json--check-results "worker-isolate" isolate-results expected))
    (list serial-us threads-us isolates-us)))

(defun neomacs-perf-threads-json--run ()
  (let* ((scenario (neomacs-perf-threads-json--required-env "NEOMACS_PERF_SCENARIO_ID"))
         (result-path (neomacs-perf-threads-json--required-env "NEOMACS_PERF_RESULT"))
         (iterations (string-to-number (or (getenv "NEOMACS_PERF_ITERATIONS") "20")))
         (threads (num-processors))
         (hash-times nil) (alist-times nil) (equal-ok nil)
         (status "error") (error-message nil))
    (condition-case err
        (progn
          (unless (equal scenario "threads-json-parse")
            (error "unknown JSON thread scenario %s" scenario))
          (unless (> iterations 0) (error "iteration count must be positive"))
          (when (featurep 'neomacs-worker-isolates)
            (unless (and (fboundp 'neomacs-worker-run) (fboundp 'neomacs-worker-wait))
              (error "worker-isolate feature lacks its Lisp entry points")))
          (let ((source (neomacs-perf-threads-json--document)))
            (setq hash-times
                  (neomacs-perf-threads-json--measure-mode
                   source iterations threads 'hash-table)
                  alist-times
                  (neomacs-perf-threads-json--measure-mode
                   source iterations threads 'alist)
                  equal-ok t
                  status "ok")))
      (error
       (setq error-message (error-message-string err))
       (message "thread JSON scenario invalid: %s" error-message)))
    (neomacs-perf-threads-json--write-result
     result-path status threads iterations hash-times alist-times equal-ok error-message)
    (neomacs-perf-threads-json--close-gate)
    (write-region "done\n" nil (neomacs-perf-threads-json--required-env "SENTINEL") nil 'silent)
    (kill-emacs 0)))

(if noninteractive
    (neomacs-perf-threads-json--run)
  (run-at-time 0 nil #'neomacs-perf-threads-json--run))

;;; threads-json-parse.el ends here
