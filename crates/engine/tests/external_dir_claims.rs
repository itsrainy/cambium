//! A repository rule that says which directory of this tree it is, and the
//! probe that decides whether to believe it.
//!
//! xla declares `@tsl` with `tf_vendored(name = "tsl", path =
//! "third_party/tsl")` inside a `module_extension` implementation. The
//! directory is a repository boundary, so `//...` stops there and every
//! change under it used to be global. The `path =` attribute says where the
//! repository is, but the rule behind it is arbitrary Starlark, so the claim
//! is checked before it is used: `bazel query '@tsl//...' --output=package`
//! has to list exactly the BUILD-bearing directories under `third_party/tsl`.
//! Confirmed, a change there is an input edit of one repository; refused, it
//! stays global and says why.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::diag::Code;
use cambium_engine::externals::FixedText;
use cambium_engine::AffectedReport;
use cambium_index::{IndexData, PackageUpdate, TargetRow};
use cambium_engine::PackageQuerier;
use cambium_testkit::{engine, req, Files, Scanning};

const DIR: &str = "third_party/tsl";

/// The extension whose implementation lambda holds the call.
const REPO_BZL: &str = "def _tsl_impl(mctx):\n\
     \x20   tf_vendored(name = \"tsl\", path = \"third_party/tsl\")\n\
     tsl_extension = module_extension(implementation = _tsl_impl)\n";

const MODULE_BAZEL: &str = "module(name = \"xla\")\n\
     tsl_extension = use_extension(\"//third_party:repo.bzl\", \"tsl_extension\")\n\
     use_repo(tsl_extension, \"tsl\")\n";

fn rows(pkg: &str) -> PackageUpdate {
    super_rows(pkg)
}

fn super_rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "lib" => vec![TargetRow::rule(
            "//lib:lib",
            "demo_lib",
            vec!["@tsl//tsl/platform:dso_loader".into()],
        )],
        "app" => vec![TargetRow::rule(
            "//app:app",
            "demo_lib",
            vec!["//lib:lib".into()],
        )],
        // The repository's own package, as a widened universe returns it:
        // the rule and the source file it is built from, under the labels
        // Bazel writes them with.
        "@tsl//tsl/platform" => vec![
            TargetRow::rule(
                "@tsl//tsl/platform:dso_loader",
                "demo_lib",
                vec!["@tsl//tsl/platform:x.cc".into()],
            ),
            TargetRow::source_file("@tsl//tsl/platform:x.cc"),
        ],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

/// The tree the boundary walk and the C2 comparison read: `third_party/tsl`
/// is a repository of its own with two packages in it.
fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("MODULE.bazel"), MODULE_BAZEL).unwrap();
    std::fs::create_dir_all(root.join("third_party/tsl/tsl/platform")).unwrap();
    std::fs::write(root.join(DIR).join("REPO.bazel"), "# the marker\n").unwrap();
    std::fs::write(root.join(DIR).join("BUILD"), "").unwrap();
    std::fs::write(root.join(DIR).join("tsl/platform/BUILD"), "").unwrap();
    dir
}

/// The tree's packages as the probe would print them, root first.
const PROBED: &[&str] = &["", "tsl/platform"];

fn scanning(root: &std::path::Path, packages: &[&str], load_error: bool) -> Scanning {
    let mut m = BTreeMap::new();
    m.insert("MODULE.bazel".to_string(), MODULE_BAZEL.to_string());
    m.insert("third_party/repo.bzl".to_string(), REPO_BZL.to_string());
    m.insert("app/BUILD".to_string(), String::new());
    m.insert("lib/BUILD".to_string(), String::new());
    Scanning::new(&["app", "lib", "@tsl//tsl/platform"], rows)
        .with_files(Files(m))
        .with_workspace(root)
        .with_repo_probe("tsl", packages, load_error)
}

fn run(index: &IndexData, root: &std::path::Path, packages: &[&str]) -> AffectedReport {
    let changes = vec![Change::new(Status::Modified, "third_party/tsl/tsl/platform/x.cc")];
    engine()
        .compute_with_texts(
            index.view(),
            &changes,
            &scanning(root, packages, false),
            &req(),
            &FixedText(BTreeMap::new()),
        )
        .unwrap()
        .report
}

#[test]
fn a_claim_bazels_own_package_list_agrees_with_maps_the_directory() {
    let dir = tree();
    let q = scanning(dir.path(), PROBED, false);
    let e = engine();
    let index = e.full_build_with("c0", &q).unwrap();
    // The probe ran, for the repository the claim names.
    assert_eq!(q.probed(), vec!["tsl".to_string()]);
    let v = index.view();
    let repo = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl").unwrap();
    let origin = v.repo_origin(repo);
    assert_eq!(origin.dir, "third_party/tsl/");
    assert!(origin.confirmed, "{origin:?}");
    assert_eq!(
        v.repo_path_maps(repo)
            .iter()
            .map(|m| m.from.clone())
            .collect::<Vec<_>>(),
        vec!["third_party/tsl/".to_string()]
    );

    // And so a change under the boundary moves the repository it is, rather
    // than the whole world.
    let r = run(&index, dir.path(), PROBED);
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    // A modified source file of a package the index holds re-queries
    // nothing, as it would in the main repository: the repository is settled
    // and its rule is reached through the file's own label. Before the
    // universe was widened there was no such package and every label of the
    // repository was seeded dirty instead; between that and this, its every
    // package was re-queried.
    assert_eq!(r.stats.repos_settled, 1);
    assert_eq!(r.stats.dirty_packages, 0, "{:?}", r.dirty_packages);
    assert_eq!(
        r.affected_labels(),
        vec![
            "//app:app".to_string(),
            "//lib:lib".to_string(),
            "@tsl//tsl/platform:dso_loader".to_string()
        ]
    );
}

/// envoy's grpc route. `//gapp:gapp` links `@grpc//:grpc`, a leaf with no
/// outgoing edges; grpc's rules are built from `@tsl//tsl/platform`'s
/// targets, which only Bazel can say (`consumer_repos`). A modified source
/// file of that package settles `@tsl` and re-queries nothing, and the
/// consumer table is what carries the change on to `@grpc`'s consumers: the
/// table names `grpc` dirty, rule (1) seeds `@grpc//:grpc`, and `//gapp:gapp`
/// is reached. A consumer that depends on none of the packages that moved
/// stays clean.
#[test]
fn a_consumer_of_a_moved_indexed_repository_is_reached_through_its_leaf() {
    let dir = tree();
    let root = dir.path();
    let rows = |pkg: &str| match pkg {
        "gapp" => PackageUpdate {
            package: pkg.to_string(),
            targets: vec![TargetRow::rule(
                "//gapp:gapp",
                "demo_lib",
                vec!["@grpc//:grpc".into()],
            )],
            ..Default::default()
        },
        other => super_rows(other),
    };
    let scanning = |consumed: &[&str]| {
        let mut m = BTreeMap::new();
        m.insert("MODULE.bazel".to_string(), MODULE_BAZEL.to_string());
        m.insert("third_party/repo.bzl".to_string(), REPO_BZL.to_string());
        m.insert("app/BUILD".to_string(), String::new());
        m.insert("lib/BUILD".to_string(), String::new());
        m.insert("gapp/BUILD".to_string(), String::new());
        Scanning::new(&["app", "lib", "gapp", "@tsl//tsl/platform"], rows)
            .with_files(Files(m))
            .with_workspace(root)
            .with_repo_probe("tsl", PROBED, false)
            .with_repo_consumers("tsl", "grpc", consumed)
    };
    let changes = vec![Change::new(
        Status::Modified,
        "third_party/tsl/tsl/platform/x.cc",
    )];
    for (consumed, reached) in [(&["tsl/platform"][..], true), (&["tsl/other"][..], false)] {
        let q = scanning(consumed);
        let index = engine().full_build_with("c0", &q).unwrap();
        let v = index.view();
        let tsl = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl").unwrap();
        assert_eq!(
            v.repo_consumers(tsl),
            vec![(
                "grpc".to_string(),
                consumed.iter().map(|p| p.to_string()).collect()
            )],
            "the table is in the index"
        );
        let r = engine()
            .compute_with_texts(v, &changes, &q, &req(), &FixedText(BTreeMap::new()))
            .unwrap()
            .report;
        assert!(!r.global, "{:?}", r.global_reason);
        assert_eq!(r.stats.repos_settled, 1);
        assert_eq!(r.stats.dirty_packages, 0, "{:?}", r.dirty_packages);
        assert_eq!(r.stats.repos_dirty_by_consumer, usize::from(reached), "{consumed:?}");
        let affected = r.affected_labels();
        assert_eq!(
            affected.contains(&"//gapp:gapp".to_string()),
            reached,
            "{consumed:?}: {affected:?}"
        );
        // The rest of the answer is the one the settled repository gives.
        assert!(affected.contains(&"//app:app".to_string()), "{affected:?}");
    }
}

/// A settled repository is decided from the shape of every path that
/// dirtied it, and any one of them can unsettle it: a `BUILD` file or a
/// `.bzl` defines rules, an added file can enter a `glob`, and a path outside
/// the directory came through some other route. Each keeps rule (0), the
/// repository's own package re-queried.
#[test]
fn a_repository_is_settled_only_by_modified_source_files_of_its_packages() {
    let dir = tree();
    let root = dir.path();
    let q = scanning(root, PROBED, false);
    let index = engine().full_build_with("c0", &q).unwrap();
    let run = |changes: Vec<Change>| {
        engine()
            .compute_with_texts(
                index.view(),
                &changes,
                &scanning(root, PROBED, false),
                &req(),
                &FixedText(BTreeMap::new()),
            )
            .unwrap()
            .report
    };
    let pkg = vec!["@tsl//tsl/platform".to_string()];
    for (why, changes) in [
        (
            "a BUILD edit",
            vec![Change::new(Status::Modified, "third_party/tsl/tsl/platform/BUILD")],
        ),
        (
            "an added source file",
            vec![Change::new(Status::Added, "third_party/tsl/tsl/platform/y.cc")],
        ),
        (
            "a source edit beside a .bzl edit",
            vec![
                Change::new(Status::Modified, "third_party/tsl/tsl/platform/x.cc"),
                Change::new(Status::Modified, "third_party/tsl/tsl/platform/defs.bzl"),
            ],
        ),
    ] {
        let r = run(changes);
        assert!(!r.global, "{why}: {:?}", r.global_reason);
        assert_eq!(r.stats.repos_settled, 0, "{why}");
        assert_eq!(r.dirty_packages, pkg, "{why}: {:?}", r.dirty_packages);
    }
}

#[test]
fn a_claim_bazel_disagrees_with_is_refused_and_the_directory_stays_global() {
    let dir = tree();
    // Bazel says the repository has a package the directory does not: the
    // rule put the repository somewhere else, or read `path` as something
    // other than the repository's root.
    let probed = &["", "tsl/platform", "elsewhere"];
    let q = scanning(dir.path(), probed, false);
    let e = engine();
    let index = e.full_build_with("c0", &q).unwrap();
    let v = index.view();
    let repo = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl");
    assert!(
        repo.is_none_or(|r| !v.repo_origin(r).confirmed && v.repo_path_maps(r).is_empty()),
        "an unconfirmed claim minted a mapping"
    );
    let codes: Vec<Code> = q.diagnostics().unwrap().drain().iter().map(|d| d.code).collect();
    assert!(
        codes.contains(&Code::W026RepoOriginUnconfirmed),
        "{codes:?}"
    );

    let r = run(&index, dir.path(), probed);
    assert!(r.global, "{:?}", r.direct);
}

#[test]
fn a_repository_that_did_not_load_is_not_indexed_and_says_so() {
    let dir = tree();
    // C1: the package list is there but a package of the repository failed to
    // load, so the list is an undercount and cannot confirm anything.
    let q = scanning(dir.path(), PROBED, true);
    let e = engine();
    let index = e.full_build_with("c0", &q).unwrap();
    let v = index.view();
    let repo = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl");
    assert!(repo.is_none_or(|r| !v.repo_origin(r).confirmed));
    let codes: Vec<Code> = q.diagnostics().unwrap().drain().iter().map(|d| d.code).collect();
    assert!(codes.contains(&Code::W025RepoNotIndexed), "{codes:?}");
}

#[test]
fn a_repository_over_the_cap_keeps_the_wide_answer() {
    let dir = tree();
    let q = scanning(dir.path(), PROBED, false);
    let cfg = cambium_engine::Config {
        externals: cambium_engine::config::Externals {
            index_max_packages: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let e = cambium_testkit::engine_with(cfg);
    let index = e.full_build_with("c0", &q).unwrap();
    let v = index.view();
    let repo = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl");
    assert!(repo.is_none_or(|r| !v.repo_origin(r).confirmed));
    let codes: Vec<Code> = q.diagnostics().unwrap().drain().iter().map(|d| d.code).collect();
    assert!(codes.contains(&Code::W025RepoNotIndexed), "{codes:?}");
}

/// Phase 5 stage 3: the repository's rules are answered by its own packages,
/// not by dirtying every label of it.
///
/// A file under the directory is a file of two repositories at once. Its
/// main-repo label reaches nothing here, because `//...` stops at the
/// boundary; the edge that matters is `@tsl//tsl/platform:x.cc`, which the
/// rule in that package is built from. Before the universe was widened, that
/// rule was a leaf and the repository's own dirtiness stood in for it, which
/// is sound and answers every label of the repository at once. Now the edge
/// is real and the answer is the reachable part of it -- so this asserts the
/// soundness half, that the consumers are still reached.
#[test]
fn a_change_under_an_indexed_repository_reaches_its_rules_by_edge() {
    let dir = tree();
    let root = dir.path();
    let q = scanning(root, PROBED, false);
    let e = engine();
    let index = e.full_build_with("c0", &q).unwrap();
    let v = index.view();

    // The universe was widened by it, and the rows are there.
    assert!(
        v.indexed_repos().contains(&"tsl".to_string()),
        "{:?}",
        v.indexed_repos()
    );
    let names: Vec<&str> = (0..v.num_packages()).map(|p| v.package_name(p)).collect();
    assert!(names.contains(&"@tsl//tsl/platform"), "{names:?}");

    // A content edit to the repository's own source file.
    let changes = vec![Change::new(
        Status::Modified,
        "third_party/tsl/tsl/platform/x.cc",
    )];
    let report = engine()
        .compute_with_texts(
            v,
            &changes,
            &scanning(root, PROBED, false),
            &req(),
            &FixedText(BTreeMap::new()),
        )
        .unwrap()
        .report;

    let affected: Vec<&str> = report.affected.iter().map(|s| s.label.as_str()).collect();
    assert!(
        affected.contains(&"@tsl//tsl/platform:dso_loader"),
        "the rule built from the changed file is affected: {affected:?}"
    );
    // And back out into the main repository through the edge that used to be
    // a leaf.
    assert!(
        affected.contains(&"//lib:lib") && affected.contains(&"//app:app"),
        "the round trip back into `//...` is carried: {affected:?}"
    );
}
