use super::*;
use std::time::{Duration, SystemTime};

/// A clock reading far enough ahead that every fixture directory has settled.
fn settled_now() -> SystemTime {
    SystemTime::now() + Duration::from_secs(3600)
}

fn fixture_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "neovm-probe-cache-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create fixture directory");
    dir
}

fn touch(path: &Path) {
    fs::write(path, b";; fixture\n").expect("write fixture file");
}

/// Give DIR a modification time no earlier listing can have recorded.
fn restamp(dir: &Path, secs_after_epoch: u64) {
    fs::File::open(dir)
        .and_then(|handle| {
            handle.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs_after_epoch))
        })
        .expect("set directory mtime");
}

#[test]
fn missing_directory_rules_out_every_candidate() {
    let dir = fixture_dir("missing").join("absent");
    let mut listings = DirectoryListings::default();
    let filter = listings.filter(&dir, settled_now());
    assert!(!filter.may_exist(OsStr::new("foo.el")));
}

#[test]
fn settled_listing_answers_misses_and_folds_ascii_case() {
    let dir = fixture_dir("settled");
    touch(&dir.join("foo.el"));
    let mut listings = DirectoryListings::default();
    let filter = listings.filter(&dir, settled_now());
    assert!(filter.may_exist(OsStr::new("foo.el")));
    assert!(!filter.may_exist(OsStr::new("foo.elc")));
    assert!(!filter.may_exist(OsStr::new("bar.el")));
    // A case-insensitive file system could still open FOO.EL; only a real
    // probe can say, so the listing must not rule it out.
    assert!(filter.may_exist(OsStr::new("FOO.EL")));
    // Non-ASCII names are never ruled out (normalizing file systems).
    assert!(filter.may_exist(OsStr::new("caf\u{e9}.el")));
    assert_eq!(listings.len(), 1);
}

#[test]
fn changed_directory_is_listed_again() {
    let dir = fixture_dir("changed");
    touch(&dir.join("foo.el"));
    let mut listings = DirectoryListings::default();
    assert!(
        !listings
            .filter(&dir, settled_now())
            .may_exist(OsStr::new("bar.el"))
    );
    touch(&dir.join("bar.el"));
    restamp(&dir, 1_000_000);
    assert!(
        listings
            .filter(&dir, settled_now())
            .may_exist(OsStr::new("bar.el"))
    );
}

#[test]
fn unsettled_directory_probes_each_candidate() {
    let dir = fixture_dir("unsettled");
    touch(&dir.join("foo.el"));
    let mut listings = DirectoryListings::default();
    let filter = listings.filter(&dir, SystemTime::now());
    assert!(filter.may_exist(OsStr::new("bar.el")));
    assert_eq!(listings.len(), 0, "a young directory must not be cached");
}

#[test]
fn relative_directory_probes_each_candidate() {
    let mut listings = DirectoryListings::default();
    let filter = listings.filter(Path::new("relative/dir"), settled_now());
    assert!(filter.may_exist(OsStr::new("anything.el")));
    assert_eq!(listings.len(), 0);
}

#[test]
fn non_ascii_entry_disables_the_listing() {
    let dir = fixture_dir("non-ascii");
    touch(&dir.join("caf\u{e9}.el"));
    let mut listings = DirectoryListings::default();
    let filter = listings.filter(&dir, settled_now());
    assert!(filter.may_exist(OsStr::new("bar.el")));
    assert_eq!(listings.len(), 0);
}

#[test]
fn load_path_search_uses_and_revalidates_the_listing() {
    let dir = fixture_dir("search");
    touch(&dir.join("alpha.el"));
    touch(&dir.join("gamma.elc"));
    // Seed this thread's cache as if the directory had settled long ago.
    with_listings(|listings| {
        listings.filter(&dir, settled_now());
    });
    let load_path = [super::super::load_path_lisp_string(&dir)];
    let find = |name: &str| super::super::find_file_in_load_path(name, &load_path);
    assert_eq!(find("alpha"), Some(dir.join("alpha.el")));
    assert_eq!(find("gamma"), Some(dir.join("gamma.elc")));
    assert_eq!(find("beta"), None);
    // A file created after the listing changes the directory's stamp, so the
    // search must see it even though the stamp is too young to list again.
    // (The seeded listing pretended a seconds-old directory had settled; a
    // real one has, so its next change cannot share the listed timestamp --
    // the restamp models that.)
    touch(&dir.join("beta.el"));
    restamp(&dir, 2_000_000);
    assert_eq!(find("beta"), Some(dir.join("beta.el")));
    // And a deleted file is gone, listed or not.
    fs::remove_file(dir.join("alpha.el")).expect("remove fixture file");
    restamp(&dir, 3_000_000);
    assert_eq!(find("alpha"), None);
}

#[test]
fn listed_hit_is_still_a_real_probe() {
    let dir = fixture_dir("hit-probe");
    // A directory named like a candidate is in the listing but is not a
    // loadable file.
    fs::create_dir(dir.join("delta.el")).expect("create directory candidate");
    touch(&dir.join("delta"));
    with_listings(|listings| {
        listings.filter(&dir, settled_now());
    });
    let load_path = [super::super::load_path_lisp_string(&dir)];
    assert_eq!(
        super::super::find_file_in_load_path("delta", &load_path),
        Some(dir.join("delta"))
    );
}
