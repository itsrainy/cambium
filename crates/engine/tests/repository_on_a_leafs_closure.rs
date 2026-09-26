//! A repository that moves moves every leaf whose closure runs through it.
//!
//! envoy's shape at `eea2ba5e2911`, box run F's step 161. The commit edits
//! `bazel/c-ares.patch`, an input of `@c-ares`, which Bazel refetches. The
//! main repository's rules name `@grpc//:grpc++`, and grpc links c-ares
//! (`bazel query 'somepath(//envoy/grpc:google_grpc_creds_interface,
//! @c-ares//...:*)'` runs `@grpc//:grpc++ -> ... -> @grpc//third_party:cares
//! -> @c-ares//:ares`), so every rule above `grpc++` is rebuilt. The index
//! held `grpc++` as a leaf with no way to know it runs through `@c-ares`, and
//! rule (1) seeded only the labels of `@c-ares` main-repo rules name: 145 of
//! the oracle's 604 labels were missing.
//!
//! Cut down:
//!
//! ```text
//! //app:app     -> @c//:lib      (whose closure runs through @r)
//! //other:other -> @d//:lib      (whose closure does not, until it does)
//! ```
//!
//! `@r` is fetched with a patch from this tree, and no rule names it.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::FixedText;
use cambium_engine::{AffectedReport, AffectedRequest, Config, RunOutcome};
use cambium_index::{IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{engine, engine_with, req, Files, Scanning};

const C_LIB: &str = "@c//:lib";
const D_LIB: &str = "@d//:lib";
const PATCH: &str = "third_party/r/fix.patch";

const MODULE: &str = "module(name = \"demo\", version = \"1\")\n\
     bazel_dep(name = \"c\", version = \"1.0\")\n\
     bazel_dep(name = \"d\", version = \"1.0\")\n\
     bazel_dep(name = \"q\", version = \"1.0\")\n\
     r_ext = use_extension(\"//third_party/r:workspace.bzl\", \"r_ext\")\n\
     use_repo(r_ext, \"r\")\n";

const REPO_MAPPING: &str = "{\"\":\"\",\"demo\":\"\",\"c\":\"c+\",\"d\":\"d+\",\
     \"q\":\"q+\",\"r\":\"+r_ext+r\"}";

/// The closures as Bazel prints the repositories on them.
const C: &str = "@c+";
const D: &str = "@d+";
const Q: &str = "@q+";
const R: &str = "@+r_ext+r";

const PACKAGES: &[&str] = &["app", "other", "third_party/r"];

fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "app" => vec![TargetRow::rule("//app:app", "demo_lib", vec![C_LIB.into()])],
        "other" => vec![TargetRow::rule(
            "//other:other",
            "demo_lib",
            vec![D_LIB.into()],
        )],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

fn files() -> Files {
    let mut m = BTreeMap::new();
    m.insert("MODULE.bazel".to_string(), MODULE.to_string());
    m.insert(
        "third_party/r/workspace.bzl".to_string(),
        "load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \"http_archive\")\n\
         \n\
         def _r_impl(mctx):\n\
         \x20   http_archive(\n\
         \x20       name = \"r\",\n\
         \x20       urls = [\"https://example.invalid/r.tar.gz\"],\n\
         \x20       patches = [\"//third_party/r:fix.patch\"],\n\
         \x20   )\n\
         \n\
         r_ext = module_extension(implementation = _r_impl)\n"
            .to_string(),
    );
    for p in PACKAGES {
        m.insert(format!("{p}/BUILD"), String::new());
    }
    Files(m)
}

/// Bazel's answer at the base: neither leaf comes back inside, `@c//:lib`
/// runs through `@r` and `@d//:lib` does not.
fn scanning() -> Scanning {
    Scanning::new(PACKAGES, rows)
        .with_files(files())
        .with_repo_mapping(REPO_MAPPING)
        .with_round_trip(C_LIB, &[])
        .with_round_trip_closure(C_LIB, &[C, R])
        .with_round_trip_closure(D_LIB, &[D])
}

/// Bazel's answer once `@d` has come to run through `@q` and `@r`.
fn scanning_d_through_r() -> Scanning {
    Scanning::new(PACKAGES, rows)
        .with_files(files())
        .with_repo_mapping(REPO_MAPPING)
        .with_round_trip(C_LIB, &[])
        .with_round_trip_closure(C_LIB, &[C])
        .with_round_trip_closure(D_LIB, &[D, Q, R])
}

fn texts(head_module: &str) -> FixedText {
    let mut m = BTreeMap::new();
    m.insert(
        ("base".to_string(), "MODULE.bazel".to_string()),
        MODULE.to_string(),
    );
    m.insert(
        ("head".to_string(), "MODULE.bazel".to_string()),
        head_module.to_string(),
    );
    // Bazel 7: no autoload question to answer for a version bump.
    m.insert(
        ("base".to_string(), ".bazelversion".to_string()),
        "7.4.1\n".to_string(),
    );
    FixedText(m)
}

fn edit(paths: &[&str]) -> Vec<Change> {
    paths
        .iter()
        .map(|p| Change::new(Status::Modified, *p))
        .collect()
}

fn run(index: &IndexData, q: &Scanning, changes: &[Change]) -> AffectedReport {
    let r = engine()
        .compute_with_texts(index.view(), changes, q, &req(), &texts(MODULE))
        .unwrap()
        .report;
    assert!(!r.global, "{:?}", r.global_reason);
    r
}

fn materialized_with(
    cfg: Config,
    index: &IndexData,
    q: &Scanning,
    changes: &[Change],
    head_module: &str,
) -> RunOutcome {
    let out = engine_with(cfg)
        .compute_with_texts(
            index.view(),
            changes,
            q,
            &AffectedRequest {
                materialize: true,
                ..req()
            },
            &texts(head_module),
        )
        .unwrap();
    assert!(!out.report.global, "{:?}", out.report.global_reason);
    out
}

/// The repositories the index records the leaf's closure as running
/// through, in the index's order (`@+r_ext+r` sorts first).
fn closure_of(index: &IndexData, leaf: &str) -> Vec<&'static str> {
    let v = index.view();
    let Some(id) = v.id_of(leaf) else {
        return Vec::new();
    };
    let mut out: Vec<&'static str> = v
        .leaf_closure()
        .filter(|(_, leaves)| leaves.contains(&id))
        .map(|(r, _)| [C, D, Q, R].into_iter().find(|k| *k == r).unwrap_or("?"))
        .collect();
    out.sort();
    out
}

fn app_and_other(r: &AffectedReport) -> (bool, bool) {
    let a = r.affected_labels();
    (
        a.contains(&"//app:app".to_string()),
        a.contains(&"//other:other".to_string()),
    )
}

/// The under-report itself. `@r` moves by its input alone, no label of it is
/// in the graph, and the rule above the leaf whose closure runs through it
/// is affected; the rule above the leaf whose closure does not is not.
#[test]
fn an_input_of_a_repository_on_a_leafs_closure_reaches_the_rules_above_it() {
    let q = scanning();
    let index = engine().full_build_with("c0", &q).unwrap();
    assert!(
        (0..index.view().num_nodes()).all(|id| !index.view().label(id).contains("r//")),
        "no label of `@r` is in the graph"
    );
    // The index records which leaf runs through which repository.
    assert_eq!(closure_of(&index, C_LIB), vec![R, C]);
    assert_eq!(closure_of(&index, D_LIB), vec![D]);

    let r = run(&index, &q, &edit(&[PATCH]));
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.stats.closure_leaves_seeded, 1);
    assert_eq!(
        app_and_other(&r),
        (true, false),
        "{:?}",
        r.affected_labels()
    );
}

/// No package owns a leaf, so no splice rewrites what its closure runs
/// through: a re-query of the package whose rule names it, and of an
/// unrelated one, both carry it, and neither asks Bazel again.
#[test]
fn the_closure_survives_a_splice_of_any_package() {
    let q = scanning();
    let index = engine().full_build_with("c0", &q).unwrap();
    let one = materialized_with(
        Config::default(),
        &index,
        &q,
        &edit(&["other/BUILD"]),
        MODULE,
    )
    .index
    .expect("the spliced index");
    let two = materialized_with(Config::default(), &one, &q, &edit(&["app/BUILD"]), MODULE)
        .index
        .expect("the spliced index");
    assert_eq!(closure_of(&two, C_LIB), vec![R, C]);
    assert_eq!(q.round_trip_questions().len(), 1, "the cold build's alone");
    let r = run(&two, &q, &edit(&[PATCH]));
    assert_eq!(
        app_and_other(&r),
        (true, false),
        "{:?}",
        r.affected_labels()
    );
}

/// An overflow re-queries `//...` and asks for every leaf again, closures
/// with the edges: at head `@c//:lib` no longer runs through `@r` and
/// `@d//:lib` does, and the index the run writes says so.
#[test]
fn an_overflow_re_derives_every_closure() {
    let q = scanning();
    let index = engine().full_build_with("c0", &q).unwrap();
    let mut cfg = Config::default();
    cfg.limits.max_dirty_packages = 1;
    let after = scanning_d_through_r();
    let out = materialized_with(
        cfg,
        &index,
        &after,
        &edit(&["app/BUILD", "other/BUILD"]),
        MODULE,
    );
    assert!(
        out.report.stats.cold_rebuild,
        "{:?}",
        out.report.cold_rebuild_reason
    );
    let head = out.index.expect("the spliced index");
    assert_eq!(after.round_trip_questions().len(), 1);
    assert_eq!(closure_of(&head, C_LIB), vec![C]);
    assert_eq!(closure_of(&head, D_LIB), vec![R, D, Q]);
    let r = run(&head, &after, &edit(&[PATCH]));
    assert_eq!(
        app_and_other(&r),
        (false, true),
        "{:?}",
        r.affected_labels()
    );
}

/// A leaf whose closure gains a repository at head. `@q` is bumped, and at
/// the new version `@d//:lib` runs through `@q` (and `@r`). No label of `@q`
/// is in the graph and the base's closures do not name it; the answer at
/// head, which the run asks for because a repository outside moved, does,
/// so `//other:other` is affected in that run. The index the run writes
/// keeps the new closure, and the next edit to `@r`'s patch reaches
/// `//other:other` from it.
#[test]
fn a_closure_that_gains_a_repository_at_head_is_seeded_and_kept() {
    let q = scanning();
    let index = engine().full_build_with("c0", &q).unwrap();
    let bumped = MODULE.replace(
        "bazel_dep(name = \"q\", version = \"1.0\")",
        "bazel_dep(name = \"q\", version = \"2.0\")",
    );
    let after = scanning_d_through_r();
    let out = materialized_with(
        Config::default(),
        &index,
        &after,
        &edit(&["MODULE.bazel"]),
        &bumped,
    );
    assert_eq!(out.report.stats.repos_dirty_by_module, 1);
    assert_eq!(after.round_trip_questions().len(), 1, "asked at head, once");
    assert_eq!(
        app_and_other(&out.report),
        (false, true),
        "{:?}",
        out.report.affected_labels()
    );
    let head = out.index.expect("the spliced index");
    assert_eq!(closure_of(&head, D_LIB), vec![R, D, Q]);

    let r = run(&head, &after, &edit(&[PATCH]));
    assert_eq!(
        app_and_other(&r),
        (false, true),
        "{:?}",
        r.affected_labels()
    );

    // Answered from the base's closures alone (Bazel not asked at head),
    // the bump reaches nothing: the answer before the head question.
    let silent = Scanning::new(PACKAGES, rows)
        .with_files(files())
        .with_repo_mapping(REPO_MAPPING);
    let r = engine()
        .compute_with_texts(
            index.view(),
            &edit(&["MODULE.bazel"]),
            &silent,
            &req(),
            &texts(&bumped),
        )
        .unwrap()
        .report;
    assert_eq!(
        app_and_other(&r),
        (false, false),
        "{:?}",
        r.affected_labels()
    );
}

/// A repository no leaf runs through seeds nothing beyond its own labels,
/// and a change that moves no repository asks nothing.
#[test]
fn a_repository_on_no_closure_seeds_nothing() {
    let q = scanning();
    let index = engine().full_build_with("c0", &q).unwrap();
    let r = run(&index, &q, &edit(&["app/README.md"]));
    assert_eq!(r.stats.closure_leaves_seeded, 0);
    assert_eq!(app_and_other(&r), (false, false));
    assert_eq!(q.round_trip_questions().len(), 1, "the cold build's alone");
}
