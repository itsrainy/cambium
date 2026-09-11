//! A deleted BUILD file is not always a deleted package.
//!
//! Bazel calls a directory a package if it holds `BUILD` *or* `BUILD.bazel`,
//! and prefers `BUILD` when both are there. Repositories mid-migration between
//! the two names have plenty of directories holding both, so `git rm
//! lib/BUILD` in one of them is not a deletion at all: the package survives,
//! Bazel starts reading `BUILD.bazel`, and the targets it defines are the ones
//! that exist at head.
//!
//! The diff cannot see that on its own — `lib/BUILD.bazel` did not change, so
//! it is not in the change list — and the classifier, working from the change
//! list alone, marks the package deleted. Left there, the splice would
//! tombstone every target in it, `affected` would report the package's
//! consumers as losing a dependency, and the published head index would be
//! missing a package that is plainly on disk. The engine's answer is to check
//! the head workspace before believing the deletion.
//!
//! Real git, no Bazel: [`DirQuerier`] reads BUILD files the way Bazel does,
//! including the `BUILD` over `BUILD.bazel` preference, so what these tests
//! assert about the rows is what Bazel would have supplied.

use std::path::PathBuf;

use cambium_engine::{AffectedRequest, Config, Engine, RunOutcome};
use cambium_index::IndexRef;
use cambium_testkit::{commit_all, git, init_repo, synthetic_engine, write, DirQuerier};

/// Whether the index still has `label` as a live target. Splicing never
/// removes a node — a deleted target is tombstoned so the walk can still hop
/// through it — so "is it gone" is a question about the tombstone bit, not
/// about the id.
fn live(v: IndexRef<'_>, label: &str) -> bool {
    v.id_of(label).is_some_and(|id| !v.is_tombstone(id))
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    engine: Engine,
    base: String,
}

/// Base: `lib` has both BUILD files, and `app` depends on the target `BUILD`
/// defines. `BUILD.bazel` defines a different target, so which file was read
/// is legible in the index.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    init_repo(&root);
    write(&root, "MODULE.bazel", "");
    write(&root, "lib/BUILD", "a\n");
    write(&root, "lib/BUILD.bazel", "z\n");
    write(&root, "app/BUILD", "app //lib:a\n");
    let base = commit_all(&root, "base");

    let engine = synthetic_engine(&root, Config::default());
    Fixture {
        _dir: dir,
        root,
        engine,
        base,
    }
}

fn req(base: &str, head: &str) -> AffectedRequest {
    AffectedRequest {
        base: base.into(),
        head: head.into(),
        materialize: true,
        no_full_build: true,
        ..AffectedRequest::new(base, head)
    }
}

/// Builds the base index from a checkout at `base`, then computes base..head
/// against the working tree, which is where head is.
fn run(f: &Fixture, head: &str) -> RunOutcome {
    let bq = f.engine.bazel_querier(&f.base).unwrap();
    let base_index = f
        .engine
        .full_build_with(&f.base, &DirQuerier::new(bq.workspace_path()))
        .unwrap();
    assert!(
        live(base_index.view(), "//lib:a"),
        "at base, `BUILD` is the file Bazel reads"
    );
    drop(bq);

    let changes = f.engine.repo.diff(&f.base, head).unwrap();
    f.engine
        .compute(
            base_index.view(),
            &changes,
            &DirQuerier::new(&f.root),
            &req(&f.base, head),
        )
        .unwrap()
}

#[test]
fn deleting_one_of_two_build_files_leaves_the_package_alive() {
    let f = fixture();
    git(&f.root, &["rm", "-q", "lib/BUILD"]);
    let head = commit_all(&f.root, "lib/BUILD goes away; lib/BUILD.bazel stays");

    let out = run(&f, &head);
    let index = out.index.expect("the run must not be globally invalidated");
    let v = index.view();

    assert!(
        live(v, "//lib:z"),
        "`lib` survives on BUILD.bazel and must be re-queried, not tombstoned"
    );
    assert!(
        !live(v, "//lib:a"),
        "the target `BUILD` defined is gone with it"
    );
    assert!(
        live(v, "//app:app"),
        "the consumer is untouched by all this"
    );
    assert!(
        out.report.dirty_packages.contains(&"lib".to_string()),
        "the package is dirty — its rows changed — just not deleted: {:?}",
        out.report.dirty_packages
    );
    // `//app:app` loses `//lib:a`, so it is affected either way. What the
    // survivor rule buys is that it is affected for the true reason, off an
    // index that still has `lib` in it.
    assert!(out.report.affected.iter().any(|t| t.label == "//app:app"));
}

#[test]
fn deleting_every_build_file_really_does_delete_the_package() {
    let f = fixture();
    git(&f.root, &["rm", "-q", "lib/BUILD", "lib/BUILD.bazel"]);
    let head = commit_all(&f.root, "lib goes away");

    let out = run(&f, &head);
    let v_owned = out.index.expect("not globally invalidated");
    let v = v_owned.view();

    assert!(!live(v, "//lib:a"));
    assert!(
        !live(v, "//lib:z"),
        "nothing is left to read; the package is gone"
    );
    assert!(out.report.affected.iter().any(|t| t.label == "//app:app"));
}

#[test]
fn a_rename_between_the_two_names_is_not_a_deletion_either() {
    // The other half of the same story, and the one the classifier can settle
    // on its own: a delete plus an add of the same package in one diff.
    let f = fixture();
    git(&f.root, &["rm", "-q", "lib/BUILD.bazel"]);
    write(&f.root, "lib/BUILD", "a\nz\n");
    let head = commit_all(&f.root, "fold BUILD.bazel into BUILD");

    let out = run(&f, &head);
    let index = out.index.expect("not globally invalidated");
    let v = index.view();
    assert!(live(v, "//lib:a"));
    assert!(live(v, "//lib:z"));
}
