//! What makes the local conversion reload the zone rules: a changed `TZ`, or,
//! with `TZ` unset, a replaced or rewritten `/etc/localtime`.

use super::*;
use std::io::Write;

fn write_file(path: &std::path::Path, bytes: &[u8]) {
    let mut file = std::fs::File::create(path).expect("create zone file");
    file.write_all(bytes).expect("write zone file");
    file.sync_all().expect("sync zone file");
}

#[test]
fn zone_file_identity_follows_a_symlink_to_its_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let utc = dir.path().join("UTC");
    let tokyo = dir.path().join("Tokyo");
    let link = dir.path().join("localtime");
    write_file(&utc, b"utc zone");
    write_file(&tokyo, b"tokyo zone, a different length");
    std::os::unix::fs::symlink(&utc, &link).expect("link");
    let link = link.to_str().expect("utf-8 path");

    let before = ZoneFileId::of(link).expect("stat through the link");
    assert_eq!(ZoneFileId::of(link), Some(before), "stable while untouched");

    // `ln -sf Tokyo localtime`: the way a system changes its zone.
    let relink = dir.path().join("relink");
    std::os::unix::fs::symlink(&tokyo, &relink).expect("relink");
    std::fs::rename(&relink, link).expect("replace the link");
    let after = ZoneFileId::of(link).expect("stat through the new link");
    assert_ne!(after, before, "a repointed link is a different zone file");
}

#[test]
fn zone_file_identity_notices_a_rewrite_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("localtime");
    write_file(&file, b"first");
    let path = file.to_str().expect("utf-8 path");
    let before = ZoneFileId::of(path).expect("stat");

    // Same length, same inode: only the timestamps can tell.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_file(&file, b"other");
    assert_ne!(ZoneFileId::of(path), Some(before));
}

#[test]
fn a_missing_zone_file_is_a_state_of_its_own() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("localtime");
    let path = file.to_str().expect("utf-8 path");
    assert_eq!(ZoneFileId::of(path), None);
    write_file(&file, b"appeared");
    assert!(ZoneFileId::of(path).is_some());
}

#[test]
fn loaded_zone_record_distinguishes_unset_from_empty_and_from_a_value() {
    let unset = LoadedZone {
        tz: None,
        zone_file: None,
    };
    let empty = LoadedZone {
        tz: Some(std::ffi::OsString::new()),
        zone_file: None,
    };
    let named = LoadedZone {
        tz: Some("Asia/Tokyo".into()),
        zone_file: None,
    };
    assert_ne!(unset, empty);
    assert_ne!(empty, named);
    assert_ne!(unset, named);
}
