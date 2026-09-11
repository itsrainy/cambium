//! Spec 9.1: the store keys indexes by git tree hash, so commits with the same
//! content — rebased, squashed, cherry-picked — share one index.
//!
//! Real git, no Bazel: the index itself is hand-built, and what is under test
//! is which key a commit resolves to.

use cambium_engine::{Config, Engine};
use cambium_index::{format, IndexBuilder, IndexData, IndexFile, PackageUpdate, TargetRow};
use cambium_testkit::{commit_all, git, init_repo, synthetic_engine, write};

fn index_for(commit: &str, config_hash: [u8; 32]) -> IndexData {
    index_in_lineage(commit, [3u8; 16], config_hash)
}

fn index_in_lineage(commit: &str, lineage: [u8; 16], config_hash: [u8; 32]) -> IndexData {
    let mut b = IndexBuilder::new(lineage);
    b.set_bazel_version("test");
    b.set_config_hash(config_hash);
    b.apply_packages(&[PackageUpdate {
        package: "lib".into(),
        build_hash: [1; 32],
        bzl_hash: [0; 32],
        targets: vec![
            TargetRow::rule("//lib:core", "cc_library", vec!["//lib:a.cc".into()]),
            TargetRow::source_file("//lib:a.cc"),
        ],
    }]);
    b.finish(commit)
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    engine: Engine,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    init_repo(&root);
    write(&root, "MODULE.bazel", "");
    write(&root, "lib/a.cc", "one");

    let mut cfg = Config::default();
    cfg.baseline.store = cambium_engine::config::StoreConfig::Dir {
        path: dir.path().join("store"),
    };
    let engine = synthetic_engine(&root, cfg);
    Fixture {
        _dir: dir,
        root,
        engine,
    }
}

impl Fixture {
    fn publish(&self, sha: &str) -> Vec<String> {
        let idx = index_for(sha, self.engine.cfg.hash());
        self.publish_idx(sha, idx)
    }

    fn publish_idx(&self, sha: &str, idx: IndexData) -> Vec<String> {
        let path = self
            .root
            .join(format!("../{}-{sha}.bin", idx.header.lineage[0]));
        format::write_index(&path, &idx).unwrap();
        self.engine
            .publish_index(&path, &idx.header.lineage, sha)
            .unwrap()
    }

    fn keys(&self) -> Vec<String> {
        self.engine.store.as_ref().unwrap().list().unwrap()
    }
}

#[test]
fn keys_are_lineage_then_tree_with_a_commit_pointer() {
    let f = fixture();
    let a = commit_all(&f.root, "one");
    let tree = f.engine.repo.tree_of(&a).unwrap();
    assert_ne!(tree, a);

    let names = f.publish(&a);
    assert_eq!(
        names,
        vec![
            format::index_key(&[3; 16], &tree),
            format::commit_pointer_key(&[3; 16], &a),
        ]
    );
    let mut keys = f.keys();
    keys.sort();
    let mut want = names.clone();
    want.sort();
    assert_eq!(keys, want, "the pointer is listed alongside the index");

    let found = f.engine.fetch_baseline(&a).unwrap().expect("found by sha");
    assert_eq!(IndexFile::open(&found).unwrap().view().commit, a);
}

/// Two commits whose trees are identical — here, one with an empty commit on
/// top, the shape of a rebase that changed nothing — resolve to one blob.
#[test]
fn commits_with_identical_trees_share_one_index() {
    let f = fixture();
    let a = commit_all(&f.root, "one");
    f.publish(&a);

    // A different sha, the same tree: an amended message, a rebase onto the
    // same parent, an empty commit.
    let b = commit_all(&f.root, "one, again");
    assert_ne!(a, b);
    assert_eq!(
        f.engine.repo.tree_of(&a).unwrap(),
        f.engine.repo.tree_of(&b).unwrap()
    );

    let found = f
        .engine
        .fetch_baseline(&b)
        .unwrap()
        .expect("b's tree is a's tree, so a's index serves b");
    assert_eq!(
        IndexFile::open(&found).unwrap().view().commit,
        a,
        "the blob a published is the one b resolves to"
    );

    // And a branch rebased onto a different parent, but with the same content
    // as `a`, hits too — the parent is not part of the tree.
    git(&f.root, &["checkout", "-q", "-b", "other", &a]);
    let c = commit_all(&f.root, "an unrelated empty commit");
    git(&f.root, &["checkout", "-q", "-b", "rebased", &c]);
    std::fs::write(f.root.join("lib/a.cc"), "one").unwrap();
    let d = commit_all(&f.root, "same content, different history");
    assert_eq!(
        f.engine.repo.tree_of(&d).unwrap(),
        f.engine.repo.tree_of(&a).unwrap()
    );
    assert!(f.engine.fetch_baseline(&d).unwrap().is_some());

    // Only one blob was ever written.
    let blobs = f
        .keys()
        .into_iter()
        .filter(|k| k.starts_with("indexes/"))
        .count();
    assert_eq!(blobs, 1);
}

/// A squash-merge produces a new commit whose tree equals the PR tip's tree, so
/// an index published for the PR tip serves the merge commit on main.
#[test]
fn a_squash_merge_hits_the_pre_merge_tree() {
    let f = fixture();
    let base = commit_all(&f.root, "base");

    git(&f.root, &["checkout", "-q", "-b", "pr", &base]);
    std::fs::write(f.root.join("lib/a.cc"), "two").unwrap();
    commit_all(&f.root, "pr 1");
    std::fs::write(f.root.join("lib/b.cc"), "new").unwrap();
    let tip = commit_all(&f.root, "pr 2");
    f.publish(&tip);

    git(&f.root, &["checkout", "-q", "main"]);
    git(&f.root, &["merge", "-q", "--squash", "pr"]);
    let squashed = commit_all(&f.root, "squash: pr");
    assert_ne!(squashed, tip);
    assert_eq!(
        f.engine.repo.tree_of(&squashed).unwrap(),
        f.engine.repo.tree_of(&tip).unwrap()
    );

    let found = f
        .engine
        .fetch_baseline(&squashed)
        .unwrap()
        .expect("the squash commit resolves to the pre-merge tree");
    assert_eq!(IndexFile::open(&found).unwrap().view().commit, tip);
    // The base commit's tree is different and was never published.
    assert!(f.engine.fetch_baseline(&base).unwrap().is_none());
}

/// The pointer exists for a caller that has a sha git cannot resolve locally —
/// a shallow clone, a service without the repository — and it still lands on
/// the tree-keyed blob.
#[test]
fn a_sha_git_cannot_resolve_follows_the_pointer() {
    let f = fixture();
    let a = commit_all(&f.root, "one");
    f.publish(&a);

    // An engine over an empty directory: `rev-parse` fails, and the sha is
    // all it has.
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.baseline.store = f.engine.cfg.baseline.store.clone();
    let blind = synthetic_engine(dir.path(), cfg);
    let found = blind
        .fetch_baseline(&a)
        .unwrap()
        .expect("resolved through commits/<lineage>/<sha>");
    assert_eq!(IndexFile::open(&found).unwrap().view().commit, a);
    assert!(blind
        .fetch_baseline("0000000000000000000000000000000000000000")
        .unwrap()
        .is_none());
}

/// A store outlives configuration changes: every `baseline build` mints a
/// lineage, and a config edit (or a rule-hash version bump) strands the old
/// one. The blob built with the current configuration must win whichever
/// lineage lists first, and when only stale blobs exist there is no baseline.
#[test]
fn a_stale_lineage_never_wins_over_a_compatible_one() {
    let f = fixture();
    let a = commit_all(&f.root, "one");
    let good = f.engine.cfg.hash();
    let mut stale = good;
    stale[0] ^= 0xff;

    // Lineage 0x01 sorts before 0x03 in the listing and is stale.
    f.publish_idx(&a, index_in_lineage(&a, [1u8; 16], stale));
    assert!(
        f.engine.fetch_baseline(&a).unwrap().is_none(),
        "a stale blob is not a baseline"
    );

    f.publish(&a);
    let found = f
        .engine
        .fetch_baseline(&a)
        .unwrap()
        .expect("compatible blob");
    let view = IndexFile::open(&found).unwrap();
    assert_eq!(view.view().lineage, [3u8; 16]);
    assert_eq!(view.view().config_hash, good);
}

/// A blob written by an older build of cambium — a format bump, not a
/// config edit — is stale in the same way, and the resolver steps past it.
#[test]
fn a_blob_in_an_unreadable_format_is_skipped_not_fatal() {
    let f = fixture();
    let a = commit_all(&f.root, "one");
    let names = f.publish(&a);
    let store = f.engine.store.as_ref().unwrap();
    let dir = match &f.engine.cfg.baseline.store {
        cambium_engine::config::StoreConfig::Dir { path } => path.clone(),
        _ => unreachable!(),
    };
    let blob = dir.join(&names[0]);
    // Published blobs are zstd frames, so rewrite this one raw before
    // reaching into the header: what is under test is the version check, and
    // a raw blob in the store is itself a case worth covering.
    let idx = cambium_index::IndexFile::open(&blob)
        .unwrap()
        .deserialize()
        .unwrap();
    cambium_index::format::write_index_with(&blob, &idx, cambium_index::Compression::None).unwrap();
    let mut bytes = std::fs::read(&blob).unwrap();
    // Magic is the first 8 bytes; the version follows it.
    bytes[8] ^= 0xff;
    std::fs::write(&blob, bytes).unwrap();
    assert!(store.list().is_ok());
    assert!(
        f.engine.fetch_baseline(&a).unwrap().is_none(),
        "an unreadable blob is not a baseline, and is not an error"
    );
}

/// Nor is a blob that will not decompress — a half-finished upload, a file
/// some other tool truncated. The walk steps past that too.
#[test]
fn a_blob_that_will_not_decompress_is_skipped_not_fatal() {
    let f = fixture();
    let a = commit_all(&f.root, "one");
    let names = f.publish(&a);
    let dir = match &f.engine.cfg.baseline.store {
        cambium_engine::config::StoreConfig::Dir { path } => path.clone(),
        _ => unreachable!(),
    };
    let blob = dir.join(&names[0]);
    let bytes = std::fs::read(&blob).unwrap();
    assert!(cambium_index::is_zstd(&bytes), "published blobs are frames");
    std::fs::write(&blob, &bytes[..bytes.len() / 2]).unwrap();
    assert!(
        f.engine.fetch_baseline(&a).unwrap().is_none(),
        "a frame that will not decompress is not a baseline, and is not an error"
    );
}
