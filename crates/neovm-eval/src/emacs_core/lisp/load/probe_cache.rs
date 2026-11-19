//! Directory listings that let a `load-path` search skip candidates that
//! cannot exist.
//!
//! GNU `openp` (src/lread.c) tries every suffix in every `load-path`
//! directory with one `open` each, and so did this port with one `stat` each:
//! a `require` near the end of a 190-entry Doom `load-path` cost ~760 failed
//! system calls.  A directory's entry set only changes when the directory's
//! own timestamps do, so one `stat` of the directory plus a listing read at
//! most once per change answers every miss in it.  A candidate the listing
//! does contain is still probed for real, so what a hit returns (and the
//! regular-file, symlink and `load-prefer-newer` rules) is unchanged.
//!
//! A listing is trusted only when it is a complete and exact answer:
//!
//! - the directory's change time is at least [`SETTLE`] old when it is read,
//!   so a later entry change cannot share its coarse timestamp;
//! - every entry name is ASCII, and names are compared ASCII-case-folded, so
//!   a case-insensitive or normalizing file system can only turn a miss into
//!   a real probe, never into a wrong answer;
//! - the directory path is absolute, so the process working directory is not
//!   part of the key.
//!
//! Anything else is answered by probing each candidate as before.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How old a directory's last change must be before its listing is trusted.
///
/// File systems stamp entry changes with a coarse clock (a kernel tick on
/// Linux, 2 s on FAT), so a change made right after a listing was read can
/// carry the same timestamp as the change before it.  Two seconds covers the
/// coarsest common granularity; a directory younger than that is probed
/// candidate by candidate until it settles.
pub(crate) const SETTLE: Duration = Duration::from_secs(2);

/// What a lookup may assume about the candidates of one directory.
pub(crate) enum CandidateFilter<'a> {
    /// The directory cannot be reached, so no candidate under it exists
    /// (a probe of `DIR/NAME` would fail with the same error).
    NoneExist,
    /// A trusted listing: only names in it can exist.
    Listed(&'a HashSet<Box<[u8]>>),
    /// No trustworthy listing; every candidate is probed.
    ProbeEach,
}

impl CandidateFilter<'_> {
    /// Whether a candidate called NAME in this directory has to be probed.
    pub(crate) fn may_exist(&self, name: &OsStr) -> bool {
        match self {
            Self::NoneExist => false,
            Self::Listed(names) => {
                let name = name.as_bytes();
                !name.is_ascii() || names.contains(fold(name).as_slice())
            }
            Self::ProbeEach => true,
        }
    }
}

/// Identity and last change of a directory, as one `stat` reports them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirStamp {
    dev: u64,
    ino: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl DirStamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }

    /// Whether the last change is at least [`SETTLE`] older than NOW.  A
    /// change time in the future (clock skew) is never settled.
    fn settled_at(&self, now: SystemTime) -> bool {
        let (secs, nanos) = self.ctime;
        let Ok(secs) = u64::try_from(secs) else {
            return true;
        };
        let nanos = u32::try_from(nanos).unwrap_or(0);
        let changed = SystemTime::UNIX_EPOCH + Duration::new(secs, nanos);
        now.duration_since(changed).is_ok_and(|age| age >= SETTLE)
    }
}

struct Listing {
    stamp: DirStamp,
    folded_names: HashSet<Box<[u8]>>,
}

/// Trusted listings of the directories `load` has searched, keyed by path.
///
/// Each evaluator thread owns one (see [`with_listings`]); entries hold only
/// file-system facts, never Lisp values, and are revalidated against the
/// directory's [`DirStamp`] on every lookup.
#[derive(Default)]
pub(crate) struct DirectoryListings {
    dirs: HashMap<PathBuf, Listing>,
}

impl DirectoryListings {
    /// The filter for candidates in DIR, revalidating or rereading its
    /// listing.  NOW decides whether a fresh listing has settled.
    pub(crate) fn filter(&mut self, dir: &Path, now: SystemTime) -> CandidateFilter<'_> {
        if !dir.is_absolute() {
            return CandidateFilter::ProbeEach;
        }
        let Ok(meta) = fs::metadata(dir) else {
            self.dirs.remove(dir);
            return CandidateFilter::NoneExist;
        };
        let stamp = DirStamp::of(&meta);
        let current = self
            .dirs
            .get(dir)
            .is_some_and(|listing| listing.stamp == stamp);
        if !current {
            match read_listing(dir, stamp, now) {
                Some(listing) => {
                    self.dirs.insert(dir.to_path_buf(), listing);
                }
                None => {
                    self.dirs.remove(dir);
                    return CandidateFilter::ProbeEach;
                }
            }
        }
        match self.dirs.get(dir) {
            Some(listing) => CandidateFilter::Listed(&listing.folded_names),
            None => CandidateFilter::ProbeEach,
        }
    }

    /// Number of directories with a trusted listing.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.dirs.len()
    }
}

/// Read DIR's entries if the result can be trusted (see the module docs).
fn read_listing(dir: &Path, stamp: DirStamp, now: SystemTime) -> Option<Listing> {
    if !stamp.settled_at(now) {
        return None;
    }
    let mut folded_names = HashSet::new();
    for entry in fs::read_dir(dir).ok()? {
        let name = entry.ok()?.file_name();
        let name = name.as_bytes();
        if !name.is_ascii() {
            return None;
        }
        folded_names.insert(fold(name).into_boxed_slice());
    }
    Some(Listing {
        stamp,
        folded_names,
    })
}

fn fold(name: &[u8]) -> Vec<u8> {
    name.to_ascii_lowercase()
}

thread_local! {
    static LOAD_PATH_LISTINGS: std::cell::RefCell<DirectoryListings> =
        std::cell::RefCell::new(DirectoryListings::default());
}

/// Run F with this thread's listings.  F must not re-enter `load`.
pub(crate) fn with_listings<R>(f: impl FnOnce(&mut DirectoryListings) -> R) -> R {
    LOAD_PATH_LISTINGS.with_borrow_mut(f)
}

#[cfg(test)]
#[path = "tests/probe_cache_test.rs"]
mod tests;
