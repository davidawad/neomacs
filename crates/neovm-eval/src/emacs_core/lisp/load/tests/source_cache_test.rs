//! The source compile cache: a hit loads the cached `.elc` under the SOURCE's
//! names, and everything that disqualifies a file leaves `load` as it was.

use super::*;
use crate::emacs_core::eval::Context;
use neovm_value::value::Value;

const COOKIE: &str = ";;; -*- lexical-binding: t -*-\n";

struct Fixture {
    _dir: tempfile::TempDir,
    cache: PathBuf,
    source: PathBuf,
}

fn fixture(first_line: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("fixture dir");
    let source = dir.path().join("probe.el");
    fs::write(&source, format!("{first_line}(setq sc-from 'source)\n")).expect("write source");
    let cache = dir.path().join("cache");
    Fixture {
        _dir: dir,
        cache,
        source,
    }
}

fn context(cache: Option<&Path>) -> Context {
    let mut eval = Context::new(crate::test_utils::unit_runtime());
    if let Some(cache) = cache {
        eval.store_variable_by_name(CACHE_VAR, Value::string(cache.to_string_lossy()));
    }
    eval
}

fn var(eval: &Context, name: &str) -> Value {
    eval.visible_variable_value_or_nil(name)
}

/// Write the cache entry `load` would find for `fixture`, holding `elc_text`.
fn publish(eval: &Context, fixture: &Fixture, elc_text: &str) -> PathBuf {
    let config = config(eval).expect("cache on");
    let (elc, meta_path, _, meta) = entry_paths_for_test(&config, &fixture.source).expect("entry");
    fs::create_dir_all(elc.parent().expect("shard")).expect("shard dir");
    fs::write(&elc, elc_text).expect("write elc");
    fs::write(meta_path, meta).expect("write meta");
    elc
}

fn entry_files(eval: &Context, fixture: &Fixture) -> (PathBuf, PathBuf, PathBuf) {
    let config = config(eval).expect("cache on");
    let (elc, meta, failed, _) = entry_paths_for_test(&config, &fixture.source).expect("entry");
    (elc, meta, failed)
}

#[test]
fn a_cache_hit_runs_the_compiled_file_under_the_source_names() {
    let f = fixture(COOKIE);
    let mut eval = context(Some(&f.cache));
    publish(
        &eval,
        &f,
        "(setq sc-from 'cache sc-file load-file-name sc-true load-true-file-name)\n",
    );
    super::super::load_file(&mut eval, &f.source).expect("load");
    assert_eq!(var(&eval, "sc-from"), Value::symbol("cache"));
    let source = f.source.to_string_lossy();
    assert_eq!(var(&eval, "sc-file").as_utf8_str(), Some(&*source));
    assert_eq!(var(&eval, "sc-true").as_utf8_str(), Some(&*source));
    let history = list_to_vec(&var(&eval, "load-history")).expect("load-history");
    let first = list_to_vec(&history[0]).expect("entry");
    assert_eq!(first[0].as_utf8_str(), Some(&*source));
}

#[test]
fn a_stale_meta_is_ignored_and_the_source_loads() {
    let f = fixture(COOKIE);
    let mut eval = context(Some(&f.cache));
    let elc = publish(&eval, &f, "(setq sc-from 'cache)\n");
    fs::write(elc.with_extension("meta"), "source\twrong\n").expect("stale meta");
    super::super::load_file(&mut eval, &f.source).expect("load");
    assert_eq!(var(&eval, "sc-from"), Value::symbol("source"));
}

#[test]
fn a_changed_source_misses_the_old_entry() {
    let f = fixture(COOKIE);
    let mut eval = context(Some(&f.cache));
    publish(&eval, &f, "(setq sc-from 'cache)\n");
    fs::write(
        &f.source,
        format!("{COOKIE}(setq sc-from 'edited-source)\n"),
    )
    .expect("edit");
    super::super::load_file(&mut eval, &f.source).expect("load");
    assert_eq!(var(&eval, "sc-from"), Value::symbol("edited-source"));
}

#[test]
fn a_miss_queues_one_compile_and_the_flush_reaps_it() {
    let f = fixture(COOKIE);
    let mut eval = context(Some(&f.cache));
    let (elc, _, failed) = entry_files(&eval, &f);
    super::super::load_file(&mut eval, &f.source).expect("first load");
    super::super::load_file(&mut eval, &f.source).expect("second load");
    assert_eq!(var(&eval, "sc-from"), Value::symbol("source"));
    assert!(queued_for_test(&elc), "the miss queues the file");
    // The child here is the test binary, which cannot compile: it dies
    // without a verdict and the reaper records that, so the file is not retried.
    assert_eq!(flush(&eval, Duration::from_secs(60)), 0);
    assert!(
        failed.exists(),
        "a child without output leaves a failed marker"
    );
}

#[test]
fn a_failed_marker_suppresses_the_compile() {
    let f = fixture(COOKIE);
    let mut eval = context(Some(&f.cache));
    let (elc, _, failed) = entry_files(&eval, &f);
    fs::create_dir_all(failed.parent().expect("shard")).expect("shard dir");
    fs::write(&failed, "failed\n").expect("marker");
    super::super::load_file(&mut eval, &f.source).expect("load");
    assert!(!queued_for_test(&elc));
}

#[test]
fn files_without_a_lexical_binding_cookie_are_never_queued() {
    for first_line in ["", ";;; -*- lexical-binding: nil -*-\n"] {
        let f = fixture(first_line);
        let mut eval = context(Some(&f.cache));
        let (elc, _, _) = entry_files(&eval, &f);
        super::super::load_file(&mut eval, &f.source).expect("load");
        assert!(!queued_for_test(&elc), "{first_line:?} must not be queued");
    }
}

#[test]
fn an_excluded_directory_never_probes_or_queues() {
    let f = fixture(COOKIE);
    let mut eval = context(Some(&f.cache));
    let parent = f
        .source
        .parent()
        .expect("parent")
        .to_string_lossy()
        .into_owned();
    eval.store_variable_by_name(EXCLUDE_VAR, Value::list(vec![Value::string(parent)]));
    assert!(entry_paths_for_test(&config(&eval).expect("on"), &f.source).is_none());
    super::super::load_file(&mut eval, &f.source).expect("load");
    assert_eq!(var(&eval, "sc-from"), Value::symbol("source"));
}

#[test]
fn the_cache_is_off_unless_the_variable_names_a_directory() {
    let f = fixture(COOKIE);
    let on = context(Some(&f.cache));
    publish(&on, &f, "(setq sc-from 'cache)\n");
    let mut off = context(None);
    assert!(config(&off).is_none());
    super::super::load_file(&mut off, &f.source).expect("load");
    assert_eq!(var(&off, "sc-from"), Value::symbol("source"));
}

#[test]
fn clear_deletes_the_cache_directory() {
    let f = fixture(COOKIE);
    let eval = context(Some(&f.cache));
    publish(&eval, &f, "(setq sc-from 'cache)\n");
    assert!(f.cache.exists());
    assert!(clear(&eval));
    assert!(!f.cache.exists());
}
