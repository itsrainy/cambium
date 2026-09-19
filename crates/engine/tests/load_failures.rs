//! A dirty package that failed to load, and the two directions of being wrong.
//!
//! Under `--keep_going` a package Bazel could not load comes back with no
//! targets, which on the wire is indistinguishable from a package whose
//! `BUILD` file was deleted. Applying that empty update tombstones every
//! target the package owned and clears every forward row they carried, and
//! nothing re-queries a package whose `BUILD` did not change, so the rows stay
//! gone for the rest of the lineage (issue #23). The engine carries such a
//! package's rows forward instead.
//!
//! That is right for a package something *else* dirtied, and wrong for a
//! package dirtied by an edit to its own `BUILD` file: there the carried rows
//! describe the package before the edit, and the targets the edit changed
//! would be reported unaffected. Those are tombstoned, which over-reports
//! rather than under-reports.

use std::path::Path;

use cambium_engine::query::Rows;
use cambium_engine::{AffectedRequest, Config, Engine, EngineError, PackageQuerier, RunOutcome};
use cambium_index::IndexRef;
use cambium_testkit::{commit_all, init_repo, synthetic_engine, write, DirQuerier};

/// A [`DirQuerier`] whose query "exited 3" with a load error naming `failing`:
/// that package's update travels in `unloaded` rather than in `updates`, which
/// is what the real `BazelQuerier` does with a package bazel named in a load
/// error.
struct FailsToLoad {
    inner: DirQuerier,
    failing: &'static str,
}

impl PackageQuerier for FailsToLoad {
    fn query(&self, packages: &[String]) -> Result<Rows, EngineError> {
        let mut updates = Vec::new();
        let mut unloaded = Vec::new();
        for p in packages {
            let mut u = self.inner.update(p);
            if p == self.failing {
                // A package that failed to load produced no targets, and its
                // BUILD file is still on disk.
                u.targets.clear();
                unloaded.push(u);
            } else {
                updates.push(u);
            }
        }
        Ok(Rows {
            updates,
            partial: Some(cambium_bazel::parse_load_errors(&format!(
                "ERROR: error loading package '{}': boom\n",
                self.failing
            ))),
            unloaded,
        })
    }

    fn universe(&self) -> Result<Rows, EngineError> {
        self.inner.universe()
    }

    fn workspace(&self) -> Option<&Path> {
        self.inner.workspace()
    }

    fn can_dump_loading_layer(&self) -> bool {
        self.inner.can_dump_loading_layer()
    }

    fn dump_loading_layer(
        &self,
        filter: &str,
    ) -> Result<(String, cambium_index::sky::Paths), EngineError> {
        self.inner.dump_loading_layer(filter)
    }
}

fn live(v: IndexRef<'_>, label: &str) -> bool {
    v.id_of(label).is_some_and(|id| !v.is_tombstone(id))
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    engine: Engine,
    base: String,
}

/// `lib` defines one target, `app` depends on it, and `other` is somewhere
/// else to edit.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    init_repo(&root);
    write(&root, "MODULE.bazel", "");
    write(&root, "lib/BUILD", "a lib/a.cc\n");
    write(&root, "lib/a.cc", "one\n");
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

fn run(f: &Fixture, head: &str, failing: &'static str) -> RunOutcome {
    let base_index = f
        .engine
        .full_build_with(&f.base, &DirQuerier::new(&f.root))
        .unwrap();
    assert!(live(base_index.view(), "//lib:a"));
    let changes = f.engine.repo().diff(&f.base, head).unwrap();
    let q = FailsToLoad {
        inner: DirQuerier::new(&f.root),
        failing,
    };
    f.engine
        .compute(
            base_index.view(),
            &changes,
            &q,
            &AffectedRequest {
                materialize: true,
                no_full_build: true,
                ..AffectedRequest::new(&f.base, head)
            },
        )
        .unwrap()
}

#[test]
fn a_package_dirtied_by_something_else_keeps_the_rows_the_index_has() {
    let f = fixture();
    write(&f.root, "lib/a.cc", "two\n");
    let head = commit_all(&f.root, "edit a source lib owns");

    let out = run(&f, &head, "lib");
    let index = out.index.expect("not globally invalidated");
    assert!(
        live(index.view(), "//lib:a"),
        "tombstoning it would clear every forward row it carried, and nothing \
         re-queries a package whose BUILD did not change"
    );
}

#[test]
fn a_package_whose_own_build_failed_to_load_is_tombstoned() {
    // The under-approximating direction. `lib/BUILD` is what this commit
    // edited, so the rows the index has for `lib` describe the package before
    // the edit: answering from them reports the targets the edit changed as
    // unaffected. Tombstoning is the wide answer, and the safe one.
    let f = fixture();
    write(&f.root, "lib/BUILD", "a lib/a.cc\nb lib/a.cc\n");
    let head = commit_all(&f.root, "edit lib/BUILD itself");

    let out = run(&f, &head, "lib");
    let index = out.index.expect("not globally invalidated");
    assert!(
        !live(index.view(), "//lib:a"),
        "the rows that predate the edit must not stand as the answer"
    );
    assert!(
        out.report.affected.iter().any(|t| t.label == "//app:app"),
        "the consumer loses its dependency and is affected: {:?}",
        out.report.affected
    );
}
