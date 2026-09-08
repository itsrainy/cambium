//! Store layout, idempotent writes, and the content-stable payload.

use std::path::Path;

use cambium_engine::store::{ArtifactStore, DirStore};
use cambium_index::{
    format, new_lineage, IndexBuilder, IndexData, IndexFile, PackageUpdate, TargetRow,
};

fn index_at(commit: &str, extra_target: bool) -> IndexData {
    let mut b = IndexBuilder::new([7u8; 16]);
    b.set_bazel_version("test");
    let mut targets = vec![
        TargetRow::rule("//lib:core", "cc_library", vec!["//lib:a.cc".into()]),
        TargetRow::source_file("//lib:a.cc"),
    ];
    if extra_target {
        targets.push(TargetRow::rule("//lib:extra", "cc_library", vec![]));
    }
    b.apply_packages(&[PackageUpdate {
        package: "lib".into(),
        build_hash: [1; 32],
        bzl_hash: [0; 32],
        targets,
    }]);
    b.finish(commit)
}

#[test]
fn identical_graphs_at_different_commits_have_identical_payloads() {
    // The property the content-addressed store layout in docs/STORE.md will
    // depend on: a commit that does not change the graph — a README edit, a
    // comment, a revert — produces the same bytes.
    let a = index_at("aaaaaaaa", false);
    // The two indexes must have been built at different times for the
    // assertion to mean anything — `created_unix` is what would leak into the
    // payload if provenance were not held out of it. Set it rather than
    // sleeping past a second boundary to make it true.
    let mut b = index_at("bbbbbbbb", false);
    b.provenance.created_unix = a.provenance.created_unix + 3600;
    assert_ne!(a.provenance.created_unix, b.provenance.created_unix);

    assert_ne!(
        a.provenance.commit, b.provenance.commit,
        "the two indexes describe different commits"
    );
    assert_eq!(
        format::payload_bytes(&a).unwrap(),
        format::payload_bytes(&b).unwrap(),
        "provenance must not reach the payload"
    );

    // ...and a graph change does move them.
    let c = index_at("cccccccc", true);
    assert_ne!(
        format::payload_bytes(&a).unwrap(),
        format::payload_bytes(&c).unwrap(),
        "an added target must change the payload"
    );
}

#[test]
fn the_whole_file_still_records_which_commit_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idx.bin");
    let d = index_at("deadbeef", false);
    format::write_index(&path, &d).unwrap();

    let f = IndexFile::open(&path).unwrap();
    assert_eq!(f.provenance().commit, "deadbeef");
    assert!(f.provenance().created_unix > 0);
    assert_eq!(f.view().commit, "deadbeef");
    assert_eq!(f.view().lineage, [7u8; 16]);
    // ...and a round trip through deserialize keeps it.
    assert_eq!(f.deserialize().unwrap().provenance.commit, "deadbeef");
}

#[test]
fn store_keys_are_lineage_then_commit() {
    let key = format::index_key(&[0xab; 16], "cafebabe");
    assert_eq!(key, "indexes/abababababababababababababababab/cafebabe.bin");
    let (lineage, commit) = format::parse_index_key(&key).unwrap();
    assert_eq!(lineage, "abababababababababababababababab");
    assert_eq!(commit, "cafebabe");
    assert!(format::parse_index_key("indexes/onlyonepart.bin").is_none());
    assert!(format::parse_index_key("something/else.bin").is_none());
}

#[test]
fn dir_store_round_trips_nested_keys() {
    let dir = tempfile::tempdir().unwrap();
    let store = DirStore::new(dir.path().join("store"));
    let src = dir.path().join("idx.bin");
    format::write_index(&src, &index_at("aaaa", false)).unwrap();

    let key = format::index_key(&[7; 16], "aaaa");
    store.publish(&key, &src).unwrap();
    assert_eq!(store.list().unwrap(), vec![key.clone()]);
    assert!(
        dir.path().join("store").join(&key).exists(),
        "the key should be a real nested path on disk"
    );

    let dest = dir.path().join("fetched.bin");
    assert!(store.fetch(&key, &dest).unwrap());
    assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(&src).unwrap());
    assert!(!store
        .fetch(&format::index_key(&[9; 16], "nope"), &dest)
        .unwrap());
}

#[test]
fn publishing_the_same_commit_twice_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = DirStore::new(dir.path().join("store"));
    let key = format::index_key(&[7; 16], "aaaa");

    // Two workers that computed the same index publish it independently. The
    // payload is deterministic, so they are writing the same bytes; what must
    // not happen is a partial file or a duplicate key.
    let first = dir.path().join("a.bin");
    let second = dir.path().join("b.bin");
    format::write_index(&first, &index_at("aaaa", false)).unwrap();
    format::write_index(&second, &index_at("aaaa", false)).unwrap();

    store.publish(&key, &first).unwrap();
    store.publish(&key, &second).unwrap();
    assert_eq!(store.list().unwrap(), vec![key.clone()]);

    let landed = dir.path().join("store").join(&key);
    let f = IndexFile::open(&landed).unwrap();
    assert_eq!(f.view().commit, "aaaa");
    assert_eq!(
        std::fs::read(&landed).unwrap()[..64],
        std::fs::read(&first).unwrap()[..64],
        "the published framing should match what was written"
    );
    // No temp files left behind.
    let leftovers: Vec<_> = std::fs::read_dir(
        dir.path()
            .join("store/indexes/07070707070707070707070707070707"),
    )
    .unwrap()
    .flatten()
    .filter(|e| e.file_name().to_string_lossy().contains("publishing"))
    .collect();
    assert!(leftovers.is_empty(), "temp files should be renamed away");
}

#[test]
fn concurrent_publishers_of_the_same_commit_do_not_corrupt_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let key = format::index_key(&[7; 16], "aaaa");
    let src = dir.path().join("idx.bin");
    format::write_index(&src, &index_at("aaaa", false)).unwrap();

    std::thread::scope(|s| {
        for _ in 0..8 {
            let root = root.clone();
            let key = key.clone();
            let src = src.clone();
            s.spawn(move || {
                DirStore::new(root).publish(&key, &src).unwrap();
            });
        }
    });

    let landed = root.join(&key);
    let f = IndexFile::open(&landed).unwrap();
    assert_eq!(f.view().commit, "aaaa");
    assert_eq!(f.view().num_nodes(), 2);
}

#[test]
fn listing_still_finds_artifacts_from_the_old_flat_layout() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    std::fs::create_dir_all(&root).unwrap();
    let legacy = format::index_file_name(&[7; 16], "aaaa");
    std::fs::write(root.join(&legacy), b"x").unwrap();

    let store = DirStore::new(root);
    assert_eq!(store.list().unwrap(), vec![legacy]);
}

#[test]
fn gc_walks_the_nested_layout() {
    let dir = tempfile::tempdir().unwrap();
    let store = DirStore::new(dir.path().join("store"));
    let src = dir.path().join("idx.bin");
    format::write_index(&src, &index_at("aaaa", false)).unwrap();
    let keep = format::index_key(&[7; 16], "aaaa");
    let drop = format::index_key(&[7; 16], "bbbb");
    store.publish(&keep, &src).unwrap();
    store.publish(&drop, &src).unwrap();

    assert_eq!(store.gc(30, &keep).unwrap(), 0, "nothing is old enough yet");
    assert_eq!(store.gc(0, &keep).unwrap(), 1);
    assert_eq!(store.list().unwrap(), vec![keep]);
}

/// The engine's fetch path has to find an index without knowing its lineage.
#[test]
fn the_engine_finds_a_baseline_by_commit_alone() {
    use cambium_engine::{Config, Engine};

    let dir = tempfile::tempdir().unwrap();
    let store_root = dir.path().join("store");
    let mut cfg = Config::default();
    cfg.baseline.store = cambium_engine::config::StoreConfig::Dir {
        path: store_root.clone(),
    };
    let engine = Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(dir.path().to_path_buf()),
        cambium_bazel::Bazel::at(dir.path().to_path_buf(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(dir.path().join("cache"));

    let src = dir.path().join("idx.bin");
    let mut d = index_at("feedface", false);
    // Only an index built with this configuration counts as a baseline.
    d.header.config_hash = engine.cfg.hash();
    format::write_index(&src, &d).unwrap();
    engine
        .publish_index(&src, &d.header.lineage, "feedface")
        .unwrap();

    let found = engine
        .fetch_baseline("feedface")
        .unwrap()
        .expect("published index should be found by commit");
    assert_eq!(IndexFile::open(&found).unwrap().view().commit, "feedface");
    assert!(engine.fetch_baseline("nosuchcommit").unwrap().is_none());
}

/// Documented in docs/STORE.md; asserted here so the docs cannot drift.
#[test]
fn a_store_key_is_a_safe_relative_path() {
    let key = format::index_key(&new_lineage(), "abc123");
    assert!(!key.starts_with('/'), "keys are relative: {key}");
    assert!(!key.contains(".."), "keys never escape the root: {key}");
    assert!(
        Path::new(&key).components().count() == 3,
        "indexes/<lineage>/<commit>.bin is three components: {key}"
    );
}

/// The same round trip, through a `command` store over a nested directory.
///
/// This is the shape every real deployment uses — `aws s3 cp` and friends —
/// and it was broken end to end: `list` returned only the basename of each
/// key, so `fetch_baseline`, which looks for a key ending `/{tree}.bin` and
/// for a `commits/` pointer, matched nothing, while `publish_index` skipped
/// writing the lineage-less copies *because* the store could list. A listable
/// command store therefore published indexes that could never be found again.
///
/// The store is three shell scripts. `list` prints `aws s3 ls --recursive`'s
/// verbose format over a URL-ish prefix, because that is the output that has
/// to parse, not a tidy one.
#[test]
fn a_command_store_over_a_nested_directory_round_trips() {
    use cambium_engine::{Config, Engine};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("bucket/cambium");
    std::fs::create_dir_all(&root).unwrap();

    let script = |name: &str, body: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, format!("#!/bin/sh\nset -e\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p.display().to_string()
    };
    let r = root.display();
    let fetch = script(
        "fetch.sh",
        &format!("[ -f \"{r}/$1\" ] || exit 7\ncp \"{r}/$1\" \"$2\""),
    );
    let push = script(
        "push.sh",
        &format!("mkdir -p \"$(dirname \"{r}/$1\")\"\ncp \"$2\" \"{r}/$1\""),
    );
    // `<date> <time> <size> s3://bucket/cambium/<key>`, one per line.
    let list = script(
        "list.sh",
        &format!(
            "cd \"{r}\" 2>/dev/null || exit 0\n\
             find . -type f | sed 's|^\\./||' | \
             while read -r k; do echo \"2026-09-08 12:00:00 1 s3://bucket/cambium/$k\"; done"
        ),
    );

    let mut cfg = Config::default();
    cfg.baseline.store = cambium_engine::config::StoreConfig::Command {
        fetch_cmd: vec![fetch, "{name}".into(), "{path}".into()],
        push_cmd: vec![push, "{name}".into(), "{path}".into()],
        list_cmd: vec![list],
        // The fetch script says 7 for "no such key", so any other failure is a
        // store error rather than a silently missing baseline.
        not_found_exit: Some(7),
    };
    let engine = Engine::with_parts(
        cfg,
        cambium_diff::Repo::new(dir.path().to_path_buf()),
        cambium_bazel::Bazel::at(dir.path().to_path_buf(), Some("/nonexistent/bazel")),
    )
    .with_cache_dir(dir.path().join("cache"));

    let src = dir.path().join("idx.bin");
    let mut d = index_at("feedface", false);
    d.header.config_hash = engine.cfg.hash();
    format::write_index(&src, &d).unwrap();
    let written = engine
        .publish_index(&src, &d.header.lineage, "feedface")
        .unwrap();
    assert!(
        written.iter().all(|k| k.contains('/')),
        "a listable store gets the nested keys only: {written:?}"
    );

    let found = engine
        .fetch_baseline("feedface")
        .unwrap()
        .expect("a published index must be findable through a listing store");
    assert_eq!(IndexFile::open(&found).unwrap().view().commit, "feedface");
    assert!(engine.fetch_baseline("nosuchcommit").unwrap().is_none());
}
