//! A package carried forward because it failed to load is re-queried the
//! next time anything dirties it, and heals then.
//!
//! xla's shape at an LLVM integrate (box run F, steps 12 and 174). `@raw` is
//! fetched with a patch from this tree, `@gen` is defined by a module
//! extension through a rule loaded from `@raw`, and `//tbl` loads a macro
//! from `@gen` (xla's 17 packages load `@llvm-project//mlir:tblgen.bzl`). An
//! edit to the patch dirties `@raw`, so `@gen`, so `//tbl` through its load
//! (rule (2) of the externals pass). When the new patch does not apply,
//! `//tbl` fails to load: its rows are carried rather than tombstoned, and a
//! later step that dirties it again, with the fetch working, has to re-query
//! it. Rule (2) skips *tombstoned* packages, so a package carried as
//! tombstoned would never have been asked again, which is how run F's
//! misses lasted until a global rebuild.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::FixedText;
use cambium_engine::inspect::PartialLoad;
use cambium_engine::{AffectedRequest, Code};
use cambium_index::{IndexData, IndexRef, PackageUpdate, TargetRow};
use cambium_testkit::{base, engine, req, Files, Scanning};

const MODULE_BAZEL: &str = "module(name = \"demo\", version = \"1\")\n\
     raw_ext = use_extension(\"//third_party/raw:workspace.bzl\", \"raw_ext\")\n\
     use_repo(raw_ext, \"raw\")\n\
     gen_ext = use_extension(\"//ext:ext.bzl\", \"gen_ext\")\n\
     use_repo(gen_ext, \"gen\")\n";

fn files() -> Files {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| m.insert(k.to_string(), v.to_string());
    put("MODULE.bazel", MODULE_BAZEL);
    put(
        "third_party/raw/workspace.bzl",
        "load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \"http_archive\")\n\
         \n\
         def _raw_impl(mctx):\n\
         \x20   http_archive(\n\
         \x20       name = \"raw\",\n\
         \x20       urls = [\"https://example.invalid/raw.tar.gz\"],\n\
         \x20       patches = [\"//third_party/raw:build.patch\"],\n\
         \x20   )\n\
         \n\
         raw_ext = module_extension(implementation = _raw_impl)\n",
    );
    put("third_party/raw/BUILD", "");
    put(
        "ext/ext.bzl",
        "load(\"@raw//x:configure.bzl\", \"configure\")\n\
         \n\
         def _gen_impl(mctx):\n\
         \x20   configure(name = \"gen\")\n\
         \n\
         gen_ext = module_extension(implementation = _gen_impl)\n",
    );
    put("ext/BUILD", "");
    // `xla/codegen/emitters/ir`: a package whose BUILD loads a macro from the
    // generated repository.
    put(
        "tbl/BUILD",
        "load(\"@gen//:tblgen.bzl\", \"gentbl\")\n\ngentbl(name = \"ir\")\n",
    );
    put("app/BUILD", "");
    put("other/BUILD", "");
    Files(m)
}

/// ```text
/// //tbl:ir   -> @gen//:lib
/// //app:test -> //tbl:ir        (xla_ops_test_cpu)
/// //other:x
/// ```
fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "tbl" => vec![TargetRow::rule(
            "//tbl:ir",
            "gentbl",
            vec!["@gen//:lib".into()],
        )],
        "app" => vec![TargetRow::rule(
            "//app:test",
            "cc_test",
            vec!["//tbl:ir".into()],
        )],
        "other" => vec![TargetRow::rule("//other:x", "cc_library", vec![])],
        _ => vec![],
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["app", "ext", "other", "tbl", "third_party/raw"];

const REPO_MAPPING: &str = "{\"\":\"\",\"demo\":\"\",\
     \"raw\":\"+raw_ext+raw\",\"gen\":\"+gen_ext+gen\"}";

fn scanning() -> Scanning {
    Scanning::new(PACKAGES, rows)
        .with_files(files())
        .with_repo_mapping(REPO_MAPPING)
}

fn texts() -> FixedText {
    let mut m = BTreeMap::new();
    for rev in ["base", "head"] {
        m.insert(
            (rev.to_string(), "MODULE.bazel".to_string()),
            MODULE_BAZEL.to_string(),
        );
    }
    m.insert(
        ("base".to_string(), ".bazelversion".to_string()),
        "7.4.1\n".to_string(),
    );
    FixedText(m)
}

/// One step over `index`, `path` modified, with `q` answering the queries.
fn step(index: &IndexData, path: &str, q: &Scanning) -> cambium_engine::RunOutcome {
    engine()
        .compute_with_texts(
            index.view(),
            &[Change::new(Status::Modified, path)],
            q,
            &AffectedRequest {
                materialize: true,
                ..req()
            },
            &texts(),
        )
        .unwrap()
}

fn live(v: IndexRef<'_>, label: &str) -> bool {
    v.id_of(label).is_some_and(|id| !v.is_tombstone(id))
}

fn tombstoned_package(v: IndexRef<'_>, package: &str) -> bool {
    (0..v.num_packages()).any(|p| v.package_name(p) == package && v.package_tombstoned(p))
}

fn w028(q: &Scanning) -> usize {
    q.diags()
        .drain()
        .into_iter()
        .filter(|d| d.code == Code::W028PackageLoadFailed)
        .count()
}

/// The partial state as a reader of the published file sees it: written,
/// read back, and turned into what `cambi index stats` and the publish check
/// read.
fn partial_on_disk(index: &IndexData) -> PartialLoad {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("i.bin");
    cambium_index::format::write_index(&path, index).unwrap();
    let f = cambium_index::IndexFile::open(&path).unwrap();
    PartialLoad::from(f.view())
}

fn queried(q: &Scanning, package: &str) -> bool {
    q.queries().iter().flatten().any(|p| p == package)
}

/// The step that breaks the fetch: `//tbl` is dirtied through its load,
/// fails to load, keeps its rows, and its consumer is answered.
fn broken_step() -> (IndexData, cambium_engine::RunOutcome) {
    let (_e, index) = base(&scanning());
    let q = scanning().failing_to_load(&["tbl"]);
    let out = step(&index, "third_party/raw/build.patch", &q);
    assert!(!out.report.global, "{:?}", out.report.global_reason);
    assert!(
        queried(&q, "tbl"),
        "rule (2) dirties //tbl through its load"
    );
    assert_eq!(w028(&q), 1);
    (index, out)
}

#[test]
fn a_package_that_failed_to_load_is_carried_and_its_consumers_answered() {
    let (_base, out) = broken_step();
    let affected = out.report.affected_labels();
    for label in ["//tbl:ir", "//app:test"] {
        assert!(
            affected.contains(&label.to_string()),
            "{label} is reached through the carried rows: {affected:?}"
        );
    }
    let index = out.index.expect("not globally invalidated");
    let v = index.view();
    assert!(live(v, "//tbl:ir"), "carried, not tombstoned");
    assert!(!tombstoned_package(v, "tbl"));
    let partial = partial_on_disk(&index);
    assert!(partial.partial, "the carried rows are older than this head");
    assert_eq!(partial.packages, vec!["tbl".to_string()]);
    assert_eq!(partial.packages_in_error, 1);
}

/// The next step that dirties `//tbl` again, with the fetch working, asks
/// Bazel for it (rule (2) does not skip a carried package, because carrying
/// does not tombstone it), gets its rows, and says nothing about a load
/// failure: the W028 was the broken step's alone.
#[test]
fn the_next_step_that_dirties_a_carried_package_re_queries_it() {
    let (_base, out) = broken_step();
    let index = out.index.expect("not globally invalidated");
    let q = scanning();
    let healed = step(&index, "third_party/raw/build.patch", &q);
    assert!(queried(&q, "tbl"), "{:?}", q.queries());
    assert_eq!(w028(&q), 0);
    assert!(healed
        .report
        .affected_labels()
        .contains(&"//app:test".to_string()));
    let healed = healed.index.expect("not globally invalidated");
    assert!(live(healed.view(), "//tbl:ir"));
    let partial = partial_on_disk(&healed);
    assert!(
        !partial.partial,
        "the only package in error loaded again, so nothing is: {partial:?}"
    );
    assert!(partial.packages.is_empty());
}

/// A step that does not dirty the carried package does not ask for it, so
/// its W028 is not repeated on every later step of the lineage while the
/// fetch stays broken; the rows it carries stand.
#[test]
fn a_step_that_does_not_dirty_a_carried_package_leaves_it_alone() {
    let (_base, out) = broken_step();
    let index = out.index.expect("not globally invalidated");
    let q = scanning().failing_to_load(&["tbl"]);
    let later = step(&index, "other/BUILD", &q);
    assert!(!queried(&q, "tbl"), "{:?}", q.queries());
    assert_eq!(w028(&q), 0);
    let later = later.index.expect("not globally invalidated");
    assert!(live(later.view(), "//tbl:ir"));
    let partial = partial_on_disk(&later);
    assert!(
        partial.partial,
        "tbl was not asked about and is still carried"
    );
    assert_eq!(partial.packages, vec!["tbl".to_string()]);
}

/// The whole lineage in one run, as a walk sees it: the fetch breaks at n,
/// step n+1 dirties something else while it is still broken, and step n+2
/// dirties `//tbl` again with the fetch working. The partial state names
/// `//tbl` through n+1 and is gone at n+2, and W028 is raised at n alone.
#[test]
fn a_carried_package_survives_an_unrelated_step_and_heals_after_it() {
    let (_base, out) = broken_step();
    let n = out.index.expect("not globally invalidated");

    let q = scanning().failing_to_load(&["tbl"]);
    let next = step(&n, "other/BUILD", &q);
    assert!(!queried(&q, "tbl"), "{:?}", q.queries());
    assert_eq!(w028(&q), 0);
    let next = next.index.expect("not globally invalidated");
    assert!(live(next.view(), "//tbl:ir"));
    assert_eq!(partial_on_disk(&next).packages, vec!["tbl".to_string()]);

    let q = scanning();
    let healed = step(&next, "third_party/raw/build.patch", &q);
    assert!(queried(&q, "tbl"), "{:?}", q.queries());
    assert_eq!(w028(&q), 0);
    assert!(healed
        .report
        .affected_labels()
        .contains(&"//app:test".to_string()));
    let healed = healed.index.expect("not globally invalidated");
    assert!(live(healed.view(), "//tbl:ir"));
    let partial = partial_on_disk(&healed);
    assert!(!partial.partial, "{partial:?}");
}

/// The step that carries `//tbl` answers from rows older than this head, so
/// as an answer someone acts on it is everything, under `CAMBI-W028` naming
/// the package, unless the configuration expects `//tbl` to fail or the
/// caller asked for the answer anyway. The index it writes is the same
/// either way; only the answer changes.
#[test]
fn an_answer_that_carries_a_package_is_everything_unless_expected() {
    let (_base, out) = broken_step();
    assert_eq!(
        out.partial.as_ref().map(|p| p.carried.clone()),
        Some(vec!["tbl".to_string()]),
        "the run knows what it carried"
    );
    let e = engine();
    let gated = e.trust_partial(&req(), out);
    assert!(gated.report.global);
    assert!(gated.report.affected.is_empty());
    let reason = gated.report.global_reason.clone().unwrap();
    assert!(reason.contains("tbl"), "{reason}");
    let said = e.diagnostics().drain();
    let cause = said
        .iter()
        .find(|d| cambium_engine::GlobalReason::explains(d, Some(&reason)))
        .expect("the cause is said");
    assert_eq!(cause.code, Code::W028PackageLoadFailed);

    // Asked for, or expected: the carried answer stands.
    let (_base, out) = broken_step();
    let allowed = AffectedRequest {
        allow_partial: true,
        ..req()
    };
    assert!(!e.trust_partial(&allowed, out).report.global);
    let (_base, out) = broken_step();
    let mut cfg = e.config().clone();
    cfg.baseline.publish_partial =
        cambium_engine::config::PartialPolicy::Expected(vec!["//tbl".into()]);
    let e = cambium_testkit::engine_with(cfg);
    assert!(!e.trust_partial(&req(), out).report.global);
}
