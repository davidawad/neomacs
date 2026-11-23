//! Transparent byte-compile cache for `.el` files that `load` reads as source.
//!
//! Opt-in through `load-source-compile-cache`: `nil` (the default) leaves `load`
//! exactly as GNU has it; a directory name turns the cache on.  A cached load
//! reads `<dir>/<aa>/<key>.elc` through the compiled-file arm of the loader, but
//! `found`, `load-file-name`, `load-true-file-name` and the `load-history` entry
//! all stay the SOURCE path, so files that find siblings or walk ancestor
//! directories from `load-file-name` behave as they do when loaded as source.
//!
//! A load that misses reads the source as usual and then queues the file for a
//! compile in a child editor (`neomacs -Q --batch`), at most
//! `load-source-compile-cache-jobs` at a time.  The child writes the `.elc`
//! under a temporary name, then `<key>.meta`, then renames the `.elc` into place,
//! so a visible `.elc` is always complete.  A compile that errors, or produces no
//! output (`no-byte-compile`), leaves `<key>.failed` and is never retried for that
//! key.  The key covers the path, size, mtime and the build id of this binary.

use super::{
    LexicalBindingCookie, lexical_binding_cookie_for_source, load_path_buf, runtime_project_root,
};
use neovm_value::value::{Value, list_to_vec};
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

const CACHE_VAR: &str = "load-source-compile-cache";
const EXCLUDE_VAR: &str = "load-source-compile-cache-exclude";
const JOBS_VAR: &str = "load-source-compile-cache-jobs";
const DEFAULT_JOBS: usize = 2;
/// The cookie must sit on the first line (or the second after `#!`).
const COOKIE_PREFIX_BYTES: usize = 1024;

/// The child's whole program.  Everything it needs arrives in `NEOMACS_SC_*`
/// environment variables, so no quoting of paths into Lisp is needed.
const CHILD_PROGRAM: &str = r#"(let ((src (getenv "NEOMACS_SC_SRC"))
      (tmp (getenv "NEOMACS_SC_TMP"))
      (dst (getenv "NEOMACS_SC_DST"))
      (meta (getenv "NEOMACS_SC_META"))
      (failed (getenv "NEOMACS_SC_FAILED")))
  (require 'bytecomp)
  (setq load-path (delete-dups (append (split-string (or (getenv "NEOMACS_SC_LOAD_PATH") "")
                                                     path-separator t)
                                       load-path)))
  (let ((ok (condition-case nil
                ;; Inline docstrings: a lazy `(#$ . OFFSET)' doc would be fetched
                ;; from `load-file-name', which is the SOURCE for a cached load.
                (let ((byte-compile-dest-file-function (lambda (_) tmp))
                      (byte-compile-dynamic-docstrings nil))
                  (byte-compile-file src))
              (error nil))))
    (if (and ok (file-exists-p tmp))
        (progn (let ((coding-system-for-write 'utf-8-unix))
                 (write-region meta nil (concat (file-name-sans-extension dst) ".meta") nil 'silent))
               (rename-file tmp dst t))
      (ignore-errors (delete-file tmp))
      (write-region "failed\n" nil failed nil 'silent))))"#;

/// The settings read from the variables at one load.
pub(super) struct CacheConfig {
    dir: PathBuf,
    exclude: Vec<PathBuf>,
    jobs: usize,
}

/// One source file's cache identity.
struct Entry {
    source: PathBuf,
    base: PathBuf,
    meta: String,
}

impl Entry {
    fn elc(&self) -> PathBuf {
        self.base.with_extension("elc")
    }
    fn meta_path(&self) -> PathBuf {
        self.base.with_extension("meta")
    }
    fn failed(&self) -> PathBuf {
        self.base.with_extension("failed")
    }
}

struct Job {
    entry: Entry,
    load_path: String,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Job>,
    running: Vec<(Child, Entry)>,
    /// Entries queued or running, so one file is compiled once per session.
    known: HashSet<PathBuf>,
}

fn queue() -> &'static Mutex<Queue> {
    static QUEUE: OnceLock<Mutex<Queue>> = OnceLock::new();
    QUEUE.get_or_init(Mutex::default)
}

/// Identifies this binary: a new build invalidates every entry, because the
/// compiler and the bytecode format come with it.
fn build_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let mut hasher = Sha256::new();
        hasher.update(env!("CARGO_PKG_VERSION"));
        if let Ok(exe) = std::env::current_exe()
            && let Ok(meta) = fs::metadata(&exe)
        {
            hasher.update(exe.to_string_lossy().as_bytes());
            hasher.update(meta.len().to_le_bytes());
            hasher.update(mtime_nanos(&meta).to_le_bytes());
        }
        hex(&hasher.finalize()[..8])
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn mtime_nanos(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

fn path_of(value: &Value) -> Option<PathBuf> {
    value.as_lisp_string().map(load_path_buf)
}

/// `Some` when the cache is on.
pub(super) fn config(eval: &crate::emacs_core::eval::Context) -> Option<CacheConfig> {
    let dir = path_of(&eval.visible_variable_value_or_nil(CACHE_VAR))?;
    let mut exclude = vec![dir.clone(), runtime_project_root().join("lisp")];
    if let Some(items) = list_to_vec(&eval.visible_variable_value_or_nil(EXCLUDE_VAR)) {
        exclude.extend(items.iter().filter_map(path_of));
    }
    let jobs = eval
        .visible_variable_value_or_nil(JOBS_VAR)
        .as_int()
        .and_then(|n| usize::try_from(n).ok())
        .map_or(DEFAULT_JOBS, |n| n.max(1));
    Some(CacheConfig { dir, exclude, jobs })
}

/// The cache identity of `source`, or `None` for a file the cache never
/// touches (excluded directory, unreadable, not a `.el`).
fn entry_for(config: &CacheConfig, source: &Path) -> Option<Entry> {
    if source.extension()? != "el" {
        return None;
    }
    let truename = fs::canonicalize(source).ok()?;
    if config
        .exclude
        .iter()
        .any(|prefix| truename.starts_with(prefix))
    {
        return None;
    }
    let meta = fs::metadata(&truename).ok()?;
    let size = meta.len();
    let mtime = mtime_nanos(&meta);
    let build = build_id();
    let mut hasher = Sha256::new();
    hasher.update(truename.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(size.to_le_bytes());
    hasher.update(mtime.to_le_bytes());
    hasher.update(build);
    let key = hex(&hasher.finalize()[..16]);
    let base = config.dir.join(&key[..2]).join(&key);
    let meta = format!(
        "source\t{}\nsize\t{size}\nmtime\t{mtime}\nbuild\t{build}\n",
        truename.display()
    );
    Some(Entry {
        source: truename,
        base,
        meta,
    })
}

/// The cached `.elc` for `source`, when one is valid.  Also gives queued
/// compiles their turn to start or finish.
pub(super) fn probe(eval: &crate::emacs_core::eval::Context, source: &Path) -> Option<PathBuf> {
    let config = config(eval)?;
    pump(&config);
    let entry = entry_for(&config, source)?;
    let elc = entry.elc();
    (fs::read_to_string(entry.meta_path()).ok()? == entry.meta && elc.is_file()).then_some(elc)
}

/// Called after `source` loaded as source without error: queue its compile.
pub(super) fn after_source_load(eval: &crate::emacs_core::eval::Context, source: &Path) {
    enqueue(eval, source);
}

/// Queue a compile of `source` unless it is cached, known to fail, excluded,
/// not lexical-binding, or already queued.  True when it was queued.
pub(crate) fn enqueue(eval: &crate::emacs_core::eval::Context, source: &Path) -> bool {
    let Some(config) = config(eval) else {
        return false;
    };
    let Some(entry) = entry_for(&config, source) else {
        return false;
    };
    if entry.failed().exists() || entry.elc().exists() || !has_lexical_cookie(&entry.source) {
        return false;
    }
    let load_path = load_path_string(eval);
    {
        let mut queue = queue().lock().unwrap_or_else(|e| e.into_inner());
        if !queue.known.insert(entry.base.clone()) {
            return false;
        }
        queue.waiting.push_back(Job { entry, load_path });
    }
    pump(&config);
    true
}

fn has_lexical_cookie(source: &Path) -> bool {
    let mut prefix = Vec::with_capacity(COOKIE_PREFIX_BYTES);
    let read = fs::File::open(source)
        .and_then(|f| f.take(COOKIE_PREFIX_BYTES as u64).read_to_end(&mut prefix));
    read.is_ok()
        && lexical_binding_cookie_for_source(&String::from_utf8_lossy(&prefix))
            == LexicalBindingCookie::Lexical
}

fn load_path_string(eval: &crate::emacs_core::eval::Context) -> String {
    let items = list_to_vec(&eval.visible_variable_value_or_nil("load-path")).unwrap_or_default();
    let dirs: Vec<PathBuf> = items.iter().filter_map(path_of).collect();
    std::env::join_paths(dirs)
        .map(|joined| joined.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Reap finished children and start waiting jobs up to the job limit.
/// Returns how many children are still running or waiting.
fn pump(config: &CacheConfig) -> usize {
    let mut queue = queue().lock().unwrap_or_else(|e| e.into_inner());
    queue
        .running
        .retain_mut(|(child, entry)| match child.try_wait() {
            Ok(None) => true,
            _ => {
                // A child that died without a verdict must not be retried.
                if !entry.elc().exists() && !entry.failed().exists() {
                    let _ = fs::write(entry.failed(), "no output\n");
                }
                false
            }
        });
    while queue.running.len() < config.jobs {
        let Some(job) = queue.waiting.pop_front() else {
            break;
        };
        match spawn(&job) {
            Ok(child) => queue.running.push((child, job.entry)),
            Err(error) => {
                tracing::warn!(
                    "source cache: cannot start compile of {}: {error}",
                    job.entry.source.display()
                );
                let _ = fs::write(job.entry.failed(), "spawn failed\n");
            }
        }
    }
    queue.running.len() + queue.waiting.len()
}

fn spawn(job: &Job) -> std::io::Result<Child> {
    let entry = &job.entry;
    fs::create_dir_all(entry.base.parent().unwrap_or(Path::new(".")))?;
    let mut tmp = OsString::from(entry.elc());
    tmp.push(format!(".tmp{}", std::process::id()));
    Command::new(std::env::current_exe()?)
        .args(["-Q", "--batch", "--eval", CHILD_PROGRAM])
        .env("NEOMACS_SC_SRC", &entry.source)
        .env("NEOMACS_SC_TMP", &tmp)
        .env("NEOMACS_SC_DST", entry.elc())
        .env("NEOMACS_SC_META", &entry.meta)
        .env("NEOMACS_SC_FAILED", entry.failed())
        .env("NEOMACS_SC_LOAD_PATH", &job.load_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// `(neomacs-source-cache-flush &optional TIMEOUT-MS)`: start every waiting
/// compile and wait for the children, up to TIMEOUT-MS (default 60000).
/// Returns the number of compiles still unfinished.
pub(crate) fn flush(eval: &crate::emacs_core::eval::Context, timeout: Duration) -> usize {
    let Some(config) = config(eval) else { return 0 };
    let deadline = Instant::now() + timeout;
    loop {
        let pending = pump(&config);
        if pending == 0 || Instant::now() >= deadline {
            return pending;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `(neomacs-source-cache-clear)`: drop queued and running compiles, forget
/// what was queued, and delete the cache directory.
pub(crate) fn clear(eval: &crate::emacs_core::eval::Context) -> bool {
    let Some(config) = config(eval) else {
        return false;
    };
    {
        let mut queue = queue().lock().unwrap_or_else(|e| e.into_inner());
        queue
            .waiting
            .retain(|job| !job.entry.base.starts_with(&config.dir));
        queue.running.retain_mut(|(child, entry)| {
            let ours = entry.base.starts_with(&config.dir);
            if ours {
                let _ = child.kill();
                let _ = child.wait();
            }
            !ours
        });
        queue.known.retain(|base| !base.starts_with(&config.dir));
    }
    fs::remove_dir_all(&config.dir).is_ok()
}

/// Test hook: whether this session queued the compile named by `elc`.
#[cfg(test)]
pub(super) fn queued_for_test(elc: &Path) -> bool {
    let base = elc.with_extension("");
    queue()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .known
        .contains(&base)
}

/// Test hook: the cache identity `load` would use for `source`.
#[cfg(test)]
pub(super) fn entry_paths_for_test(
    config: &CacheConfig,
    source: &Path,
) -> Option<(PathBuf, PathBuf, PathBuf, String)> {
    let entry = entry_for(config, source)?;
    Some((entry.elc(), entry.meta_path(), entry.failed(), entry.meta))
}

#[cfg(test)]
#[path = "tests/source_cache_test.rs"]
mod tests;
