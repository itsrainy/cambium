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
                cambium_index::externals::Consumer::of_packages(
                    consumed.iter().map(|p| p.to_string()).collect()
                )
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

/// envoy's rename. The consumer table is Bazel's answer at the commit that
/// asked, and the walk carries it. Step 32 of envoy's history renamed
/// `com_github_grpc_grpc` to `grpc`: every re-queried package then spelled
/// its deps `@grpc//...`, and the carried table kept naming a repository no
/// label spelled, so rule (1) seeded nothing and the one target whose only
/// route into `@envoy_api` was a `@grpc` proto went missing at the next
/// overflow while a cold build at the same commit had it. The overflow
/// re-query is on the server that just loaded everything, so the table is
/// asked again there: the head index names the new repository, and the
/// next source edit under the indexed repository reaches the consumer.
#[test]
fn an_overflow_re_query_re_derives_the_consumer_table() {
    let dir = tree();
    let root = dir.path();
    let scanning = |grpc: &'static str, rows: fn(&str) -> PackageUpdate| {
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
            .with_repo_consumers("tsl", grpc, &["tsl/platform"])
            // Bazel's leaf answer knows either spelling; which one is asked
            // for is the index's business.
            .with_consumer_leaf("tsl", "@com_github_grpc_grpc//:grpc", &["tsl/platform"])
            .with_consumer_leaf("tsl", "@grpc//:grpc", &["tsl/platform"])
    };
    fn old_rows(pkg: &str) -> PackageUpdate {
        match pkg {
            "gapp" => PackageUpdate {
                package: pkg.to_string(),
                targets: vec![TargetRow::rule(
                    "//gapp:gapp",
                    "demo_lib",
                    vec!["@com_github_grpc_grpc//:grpc".into()],
                )],
                ..Default::default()
            },
            other => super_rows(other),
        }
    }
    fn new_rows(pkg: &str) -> PackageUpdate {
        match pkg {
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
        }
    }
    let consumer_of = |index: &IndexData| -> Vec<String> {
        let v = index.view();
        let tsl = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl").unwrap();
        v.repo_consumers(tsl).into_iter().map(|(c, _)| c).collect()
    };
    let leaf_map_of = |index: &IndexData| -> Vec<(String, Vec<String>)> {
        let v = index.view();
        let tsl = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl").unwrap();
        v.repo_consumers(tsl)
            .into_iter()
            .flat_map(|(_, used)| used.leaves.expect("a leaf map"))
            .map(|(l, ps)| (l, ps.into_iter().collect()))
            .collect()
    };
    // The base, at the commit before the rename.
    let base = engine()
        .full_build_with("c0", &scanning("com_github_grpc_grpc", old_rows))
        .unwrap();
    assert_eq!(consumer_of(&base), vec!["com_github_grpc_grpc".to_string()]);
    assert_eq!(
        leaf_map_of(&base),
        vec![(
            "@com_github_grpc_grpc//:grpc".to_string(),
            vec!["tsl/platform".to_string()]
        )]
    );

    // The rename: a workspace-closure change that overflows the dirty limit
    // and re-queries `//...`, under a limit of one so the overflow is certain.
    let mut cfg = cambium_engine::Config::default();
    cfg.limits.max_dirty_packages = 1;
    let renamed = scanning("grpc", new_rows);
    let rename = vec![
        Change::new(Status::Modified, "app/BUILD"),
        Change::new(Status::Modified, "lib/BUILD"),
    ];
    let out = cambium_testkit::engine_with(cfg)
        .compute_with_texts(
            base.view(),
            &rename,
            &renamed,
            &cambium_engine::AffectedRequest {
                materialize: true,
                ..req()
            },
            &FixedText(BTreeMap::new()),
        )
        .unwrap();
    assert!(out.report.stats.cold_rebuild, "{:?}", out.report.cold_rebuild_reason);
    assert!(!out.report.global, "{:?}", out.report.global_reason);
    let head = out.index.expect("the spliced index");
    assert_eq!(consumer_of(&head), vec!["grpc".to_string()], "re-derived on the overflow");
    // And its leaves under the head's spelling, which exists only once the
    // re-queried packages are spliced in: derived before the splice, the
    // index held no `@grpc//` label and the map would be empty.
    assert_eq!(
        leaf_map_of(&head),
        vec![("@grpc//:grpc".to_string(), vec!["tsl/platform".to_string()])],
        "the leaf map is re-derived under the new spelling"
    );
    assert!(
        renamed
            .leaf_questions()
            .iter()
            .any(|(r, ls)| r == "tsl" && ls == &vec!["@grpc//:grpc".to_string()]),
        "{:?}",
        renamed.leaf_questions()
    );

    // The next step: a source edit under the indexed repository, settled,
    // reaches `//gapp:gapp` through the leaf the new name spells.
    let edit = vec![Change::new(
        Status::Modified,
        "third_party/tsl/tsl/platform/x.cc",
    )];
    let r = engine()
        .compute_with_texts(head.view(), &edit, &renamed, &req(), &FixedText(BTreeMap::new()))
        .unwrap()
        .report;
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_consumer, 1);
    assert_eq!(r.stats.consumer_leaves_seeded, 1, "seeded by the map");
    let affected = r.affected_labels();
    assert!(affected.contains(&"//gapp:gapp".to_string()), "{affected:?}");
}

// ------------------------------------------------ the consumer's leaf map
//
// envoy's step whose oracle is 649 labels answered 5035: the table dirtied
// `@grpc`, and rule (1) seeded all five of its leaves, of which only
// `@grpc//:grpc++` reaches `@envoy_api`. The tree below has two in-tree
// packages, `tsl/platform` depending on `tsl/base`, and two `@grpc` leaves,
// each linked by one main-repo target: `//gapp:gapp` on `@grpc//:grpc` and
// `//happ:happ` on `@grpc//:health`.

/// The packages of the two-package repository, as the probe prints them.
const PROBED2: &[&str] = &["", "tsl/base", "tsl/platform"];

fn tree2() -> tempfile::TempDir {
    let dir = tree();
    std::fs::create_dir_all(dir.path().join(DIR).join("tsl/base")).unwrap();
    std::fs::write(dir.path().join(DIR).join("tsl/base/BUILD"), "").unwrap();
    dir
}

fn leaf_rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "gapp" => vec![TargetRow::rule(
            "//gapp:gapp",
            "demo_lib",
            vec!["@grpc//:grpc".into()],
        )],
        "happ" => vec![TargetRow::rule(
            "//happ:happ",
            "demo_lib",
            vec!["@grpc//:health".into()],
        )],
        "@tsl//tsl/platform" => vec![
            TargetRow::rule(
                "@tsl//tsl/platform:dso_loader",
                "demo_lib",
                vec![
                    "@tsl//tsl/platform:x.cc".into(),
                    "@tsl//tsl/base:base".into(),
                ],
            ),
            TargetRow::source_file("@tsl//tsl/platform:x.cc"),
        ],
        "@tsl//tsl/base" => vec![
            TargetRow::rule(
                "@tsl//tsl/base:base",
                "demo_lib",
                vec!["@tsl//tsl/base:y.cc".into()],
            ),
            TargetRow::source_file("@tsl//tsl/base:y.cc"),
        ],
        other => return super_rows(other),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const LEAF_UNIVERSE: &[&str] = &[
    "app",
    "lib",
    "gapp",
    "happ",
    "@tsl//tsl/platform",
    "@tsl//tsl/base",
];

/// A querier over the two-package tree whose consumer table names `grpc` as
/// consuming both packages, pooled, and whose leaf map is `leaves`.
fn leaf_scanning(
    root: &std::path::Path,
    universe: &'static [&'static str],
    leaves: &[(&str, &[&str])],
) -> Scanning {
    let mut m = BTreeMap::new();
    m.insert("MODULE.bazel".to_string(), MODULE_BAZEL.to_string());
    m.insert("third_party/repo.bzl".to_string(), REPO_BZL.to_string());
    for p in ["app", "lib", "gapp", "happ"] {
        m.insert(format!("{p}/BUILD"), String::new());
    }
    let mut q = Scanning::new(universe, leaf_rows)
        .with_files(Files(m))
        .with_workspace(root)
        .with_repo_probe("tsl", PROBED2, false)
        .with_repo_consumers("tsl", "grpc", &["tsl/base", "tsl/platform"]);
    for (leaf, pkgs) in leaves {
        q = q.with_consumer_leaf("tsl", leaf, pkgs);
    }
    q
}

fn leaves_of(index: &IndexData) -> Option<BTreeMap<String, std::collections::BTreeSet<String>>> {
    let v = index.view();
    let tsl = (0..v.num_repos()).find(|&r| v.repo_name(r) == "tsl").unwrap();
    let cs = v.repo_consumers(tsl);
    assert_eq!(cs.len(), 1, "{cs:?}");
    assert_eq!(cs[0].0, "grpc");
    cs[0].1.leaves.clone()
}

fn map(entries: &[(&str, &[&str])]) -> BTreeMap<String, std::collections::BTreeSet<String>> {
    entries
        .iter()
        .map(|(l, ps)| (l.to_string(), ps.iter().map(|p| p.to_string()).collect()))
        .collect()
}

fn run_leaves(index: &IndexData, q: &Scanning, changes: Vec<Change>) -> AffectedReport {
    let r = engine()
        .compute_with_texts(index.view(), &changes, q, &req(), &FixedText(BTreeMap::new()))
        .unwrap()
        .report;
    assert!(!r.global, "{:?}", r.global_reason);
    r
}

fn edit(path: &str) -> Vec<Change> {
    vec![Change::new(Status::Modified, path)]
}

/// Two leaves of one consumer, one whose deps reach the package that moved
/// and one that reaches nothing of the repository: only the first is seeded,
/// and the consumer of the second is not affected. Before the map both were,
/// which is the 5035 against 649.
#[test]
fn only_the_leaf_whose_closure_reaches_what_moved_is_seeded() {
    let dir = tree2();
    let root = dir.path();
    let leaves: &[(&str, &[&str])] = &[
        ("@grpc//:grpc", &["tsl/base", "tsl/platform"]),
        ("@grpc//:health", &[]),
    ];
    let q = leaf_scanning(root, LEAF_UNIVERSE, leaves);
    let index = engine().full_build_with("c0", &q).unwrap();
    // The map is in the index, both leaves known, and it was asked for in one
    // question over every leaf of the consumer.
    assert_eq!(leaves_of(&index), Some(map(leaves)));
    assert_eq!(
        q.leaf_questions(),
        vec![(
            "tsl".to_string(),
            vec!["@grpc//:grpc".to_string(), "@grpc//:health".to_string()]
        )]
    );

    let r = run_leaves(&index, &q, edit("third_party/tsl/tsl/platform/x.cc"));
    assert_eq!(r.stats.repos_settled, 1);
    assert_eq!(r.stats.repos_dirty_by_consumer, 1);
    assert_eq!(r.stats.consumer_leaves_seeded, 1);
    let affected = r.affected_labels();
    assert!(affected.contains(&"//gapp:gapp".to_string()), "{affected:?}");
    assert!(!affected.contains(&"//happ:happ".to_string()), "{affected:?}");
    assert!(affected.contains(&"//app:app".to_string()), "{affected:?}");
}

/// A leaf whose map names only another package of the repository is not
/// seeded when this one moves, although the consumer's pooled packages
/// include it: the pooled set is every consumer rule's, the map is the leaf's.
#[test]
fn a_leaf_keyed_to_another_package_is_not_seeded() {
    let dir = tree2();
    let root = dir.path();
    // `tsl/platform` depends on `tsl/base`, not the other way round, so an
    // edit to `tsl/platform` moves nothing `@grpc//:grpc` reaches.
    let q = leaf_scanning(
        root,
        LEAF_UNIVERSE,
        &[("@grpc//:grpc", &["tsl/base"]), ("@grpc//:health", &[])],
    );
    let index = engine().full_build_with("c0", &q).unwrap();
    let r = run_leaves(&index, &q, edit("third_party/tsl/tsl/platform/x.cc"));
    assert_eq!(r.stats.consumer_leaves_seeded, 0);
    assert_eq!(r.stats.repos_dirty_by_consumer, 0);
    let affected = r.affected_labels();
    assert!(!affected.contains(&"//gapp:gapp".to_string()), "{affected:?}");
    assert!(!affected.contains(&"//happ:happ".to_string()), "{affected:?}");
}

/// The map is the cold build's answer, and an edge inside the repository can
/// arrive after it: here the map says `@grpc//:grpc` reaches `tsl/platform`
/// only, and the index says `tsl/platform` depends on `tsl/base`. An edit to
/// `tsl/base` reaches the leaf through that edge, so it is seeded; the reverse
/// rows of the repository's own nodes are what say so.
#[test]
fn an_edge_inside_the_repository_carries_a_change_to_the_leaf_above_it() {
    let dir = tree2();
    let root = dir.path();
    let q = leaf_scanning(
        root,
        LEAF_UNIVERSE,
        &[("@grpc//:grpc", &["tsl/platform"]), ("@grpc//:health", &[])],
    );
    let index = engine().full_build_with("c0", &q).unwrap();
    let r = run_leaves(&index, &q, edit("third_party/tsl/tsl/base/y.cc"));
    assert_eq!(r.stats.repos_settled, 1);
    assert_eq!(r.stats.consumer_leaves_seeded, 1);
    let affected = r.affected_labels();
    assert!(affected.contains(&"//gapp:gapp".to_string()), "{affected:?}");
    assert!(!affected.contains(&"//happ:happ".to_string()), "{affected:?}");
}

/// A `BUILD` edit under the repository unsettles it, and what moved is then
/// every package: each leaf that reaches anything of the repository is
/// seeded, and one that reaches nothing still is not.
#[test]
fn an_unsettled_repository_seeds_every_leaf_that_reaches_it() {
    let dir = tree2();
    let root = dir.path();
    let q = leaf_scanning(
        root,
        LEAF_UNIVERSE,
        &[("@grpc//:grpc", &["tsl/base"]), ("@grpc//:health", &[])],
    );
    let index = engine().full_build_with("c0", &q).unwrap();
    let r = run_leaves(&index, &q, edit("third_party/tsl/tsl/platform/BUILD"));
    assert_eq!(r.stats.repos_settled, 0);
    assert_eq!(r.stats.consumer_leaves_seeded, 1);
    let affected = r.affected_labels();
    assert!(affected.contains(&"//gapp:gapp".to_string()), "{affected:?}");
    assert!(!affected.contains(&"//happ:happ".to_string()), "{affected:?}");
}

/// A leaf the map does not know is seeded whenever the repository moves: it
/// was named after the map was derived (here a package added by a later
/// commit links it), and nothing says what it reaches. The leaf the map does
/// know, and knows reaches only `tsl/base`, stays unseeded.
#[test]
fn a_leaf_named_after_the_map_was_derived_is_seeded() {
    let dir = tree2();
    let root = dir.path();
    const BEFORE: &[&str] = &["app", "lib", "gapp", "@tsl//tsl/platform", "@tsl//tsl/base"];
    let leaves: &[(&str, &[&str])] = &[("@grpc//:grpc", &["tsl/base"]), ("@grpc//:health", &[])];
    let base = engine()
        .full_build_with("c0", &leaf_scanning(root, BEFORE, leaves))
        .unwrap();
    // Only `@grpc//:grpc` was a label then, so only it is known.
    assert_eq!(leaves_of(&base), Some(map(&[("@grpc//:grpc", &["tsl/base"])])));

    // The commit that adds `//happ:happ` and with it the label
    // `@grpc//:health`, spliced without an overflow, so the map is carried.
    let q = leaf_scanning(root, LEAF_UNIVERSE, leaves);
    let out = engine()
        .compute_with_texts(
            base.view(),
            &[Change::new(Status::Added, "happ/BUILD")],
            &q,
            &cambium_engine::AffectedRequest {
                materialize: true,
                ..req()
            },
            &FixedText(BTreeMap::new()),
        )
        .unwrap();
    assert!(!out.report.stats.cold_rebuild);
    let head = out.index.expect("the spliced index");
    assert!(head.view().id_of("@grpc//:health").is_some());
    assert_eq!(leaves_of(&head), leaves_of(&base), "carried, not re-derived");

    let r = run_leaves(&head, &q, edit("third_party/tsl/tsl/platform/x.cc"));
    assert_eq!(r.stats.consumer_leaves_seeded, 1);
    let affected = r.affected_labels();
    assert!(affected.contains(&"//happ:happ".to_string()), "{affected:?}");
    assert!(!affected.contains(&"//gapp:gapp".to_string()), "{affected:?}");
}

/// A consumer with no map is the answer before the map: every leaf seeded.
/// That is what an index from before format 14 carries, and what a leaf
/// question Bazel would not answer leaves behind.
#[test]
fn a_consumer_with_no_leaf_map_seeds_every_leaf() {
    let dir = tree2();
    let root = dir.path();
    let q = leaf_scanning(root, LEAF_UNIVERSE, &[]);
    let index = engine().full_build_with("c0", &q).unwrap();
    assert_eq!(leaves_of(&index), None);
    let r = run_leaves(&index, &q, edit("third_party/tsl/tsl/platform/x.cc"));
    assert_eq!(r.stats.repos_dirty_by_consumer, 1);
    assert_eq!(r.stats.consumer_leaves_seeded, 0);
    let affected = r.affected_labels();
    assert!(affected.contains(&"//gapp:gapp".to_string()), "{affected:?}");
    assert!(affected.contains(&"//happ:happ".to_string()), "{affected:?}");
}

/// [`leaf_scanning`], each leaf also naming what outside the repository its
/// closure runs through (`Consumer::via`).
fn leaf_scanning_via(
    root: &std::path::Path,
    universe: &'static [&'static str],
    leaves: &[(&str, &[&str], &[&str])],
) -> Scanning {
    let mut q = leaf_scanning(root, universe, &[]);
    for (leaf, pkgs, via) in leaves {
        q = q.with_consumer_leaf_via("tsl", leaf, pkgs, via);
    }
    q
}

fn materialized(index: &IndexData, q: &Scanning, changes: Vec<Change>) -> cambium_engine::RunOutcome {
    engine()
        .compute_with_texts(
            index.view(),
            &changes,
            q,
            &cambium_engine::AffectedRequest {
                materialize: true,
                ..req()
            },
            &FixedText(BTreeMap::new()),
        )
        .unwrap()
}

/// The map is a closure, and a `BUILD` edit on it moves it. quiche's leaves
/// reach `@envoy_api` only through `@envoy//source/common/quic/platform:*`,
/// main-repo rules; here `@grpc//:health` reaches nothing of `@tsl` at the
/// cold build, through `//lib`. A commit edits `lib/BUILD` so that it now
/// does (Bazel's answer at head says `tsl/base`), and nothing overflows. The
/// splice asks for the map again, and the next edit under `tsl/base` seeds
/// the leaf. Carried as it was, the map said "reaches nothing" and
/// `//happ:happ` was missed, which a pooled table seeding every leaf would
/// not have done.
#[test]
fn a_build_edit_on_a_leafs_closure_re_derives_the_map() {
    let dir = tree2();
    let root = dir.path();
    let before = leaf_scanning_via(
        root,
        LEAF_UNIVERSE,
        &[
            ("@grpc//:grpc", &["tsl/platform"], &["@grpc"]),
            ("@grpc//:health", &[], &["@grpc", "//lib"]),
        ],
    );
    let base = engine().full_build_with("c0", &before).unwrap();
    assert_eq!(
        leaves_of(&base),
        Some(map(&[("@grpc//:grpc", &["tsl/platform"]), ("@grpc//:health", &[])]))
    );

    let after = leaf_scanning_via(
        root,
        LEAF_UNIVERSE,
        &[
            ("@grpc//:grpc", &["tsl/platform"], &["@grpc"]),
            ("@grpc//:health", &["tsl/base"], &["@grpc", "//lib"]),
        ],
    );
    let out = materialized(&base, &after, edit("lib/BUILD"));
    assert!(!out.report.stats.cold_rebuild);
    assert!(!out.report.global, "{:?}", out.report.global_reason);
    assert_eq!(after.leaf_questions().len(), 1, "{:?}", after.leaf_questions());
    let head = out.index.expect("the spliced index");
    assert_eq!(
        leaves_of(&head),
        Some(map(&[("@grpc//:grpc", &["tsl/platform"]), ("@grpc//:health", &["tsl/base"])])),
        "asked for again, not carried"
    );

    let r = run_leaves(&head, &after, edit("third_party/tsl/tsl/base/y.cc"));
    let affected = r.affected_labels();
    assert!(affected.contains(&"//happ:happ".to_string()), "{affected:?}");
    // The base's map, which the splice used to carry, misses it.
    let stale = run_leaves(&base, &after, edit("third_party/tsl/tsl/base/y.cc"));
    assert!(
        !stale.affected_labels().contains(&"//happ:happ".to_string()),
        "the test holds the old map to the new closure"
    );
}

/// The same `BUILD` edit in the same commit as the edit it gives a path to:
/// no splice has asked yet, so the run asks Bazel at head, reads each leaf
/// as reaching what it reaches at either end, and seeds by that. The splice
/// then takes the same answer rather than ask a second time.
#[test]
fn a_build_edit_on_a_leafs_closure_is_answered_at_head_in_its_own_run() {
    let dir = tree2();
    let root = dir.path();
    let before = leaf_scanning_via(
        root,
        LEAF_UNIVERSE,
        &[
            ("@grpc//:grpc", &["tsl/platform"], &["@grpc"]),
            ("@grpc//:health", &[], &["@grpc", "//lib"]),
        ],
    );
    let base = engine().full_build_with("c0", &before).unwrap();
    let after = leaf_scanning_via(
        root,
        LEAF_UNIVERSE,
        &[
            ("@grpc//:grpc", &["tsl/platform"], &["@grpc"]),
            ("@grpc//:health", &["tsl/base"], &["@grpc", "//lib"]),
        ],
    );
    let both = vec![
        Change::new(Status::Modified, "lib/BUILD"),
        Change::new(Status::Modified, "third_party/tsl/tsl/base/y.cc"),
    ];
    let out = materialized(&base, &after, both.clone());
    let r = &out.report;
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.consumer_leaves_seeded, 2, "narrowed by the map at head, not dirty");
    let affected = r.affected_labels();
    assert!(affected.contains(&"//happ:happ".to_string()), "{affected:?}");
    assert!(affected.contains(&"//gapp:gapp".to_string()), "{affected:?}");
    assert_eq!(after.leaf_questions().len(), 1, "asked once: {:?}", after.leaf_questions());
    assert_eq!(
        leaves_of(out.index.as_ref().expect("the spliced index")),
        Some(map(&[("@grpc//:grpc", &["tsl/platform"]), ("@grpc//:health", &["tsl/base"])]))
    );
    // Without the `BUILD` edit the map stands, `//lib` did not move, and
    // nothing is asked.
    let quiet = leaf_scanning_via(root, LEAF_UNIVERSE, &[]);
    let r = run_leaves(&base, &quiet, edit("third_party/tsl/tsl/base/y.cc"));
    assert!(!r.affected_labels().contains(&"//happ:happ".to_string()));
    assert!(quiet.leaf_questions().is_empty());
}

/// A leaf that reached a package at base and no longer does at head was cut
/// from it by the `BUILD` edit, which moves the leaf too: what it reached at
/// either end is what it is held against.
#[test]
fn a_suspect_leaf_is_held_against_what_it_reached_at_base_as_well() {
    let dir = tree2();
    let root = dir.path();
    let before = leaf_scanning_via(
        root,
        LEAF_UNIVERSE,
        &[("@grpc//:grpc", &[], &["@grpc"]), ("@grpc//:health", &["tsl/base"], &["//lib"])],
    );
    let base = engine().full_build_with("c0", &before).unwrap();
    let after = leaf_scanning_via(
        root,
        LEAF_UNIVERSE,
        &[("@grpc//:grpc", &[], &["@grpc"]), ("@grpc//:health", &[], &["//lib"])],
    );
    let r = run_leaves(
        &base,
        &after,
        vec![
            Change::new(Status::Modified, "lib/BUILD"),
            Change::new(Status::Modified, "third_party/tsl/tsl/base/y.cc"),
        ],
    );
    assert_eq!(r.stats.consumer_leaves_seeded, 1);
    let affected = r.affected_labels();
    assert!(affected.contains(&"//happ:happ".to_string()), "{affected:?}");
    assert!(!affected.contains(&"//gapp:gapp".to_string()), "{affected:?}");
}

/// A suspect map Bazel will not answer for at head is no map: the pooled
/// packages decide, and every leaf of the consumer is seeded.
#[test]
fn a_suspect_map_with_no_answer_at_head_is_read_as_no_map() {
    let dir = tree2();
    let root = dir.path();
    let before = leaf_scanning_via(
        root,
        LEAF_UNIVERSE,
        &[
            ("@grpc//:grpc", &["tsl/platform"], &["@grpc"]),
            ("@grpc//:health", &[], &["@grpc", "//lib"]),
        ],
    );
    let base = engine().full_build_with("c0", &before).unwrap();
    // No leaf answer at all for `tsl`.
    let silent = leaf_scanning(root, LEAF_UNIVERSE, &[]);
    let r = run_leaves(
        &base,
        &silent,
        vec![
            Change::new(Status::Modified, "lib/BUILD"),
            Change::new(Status::Modified, "third_party/tsl/tsl/base/y.cc"),
        ],
    );
    assert_eq!(silent.leaf_questions().len(), 1);
    assert_eq!(r.stats.repos_dirty_by_consumer, 1);
    assert_eq!(r.stats.consumer_leaves_seeded, 0, "the consumer is dirty, not narrowed");
    let affected = r.affected_labels();
    assert!(affected.contains(&"//happ:happ".to_string()), "{affected:?}");
    assert!(affected.contains(&"//gapp:gapp".to_string()), "{affected:?}");
}

/// A `.bzl` under the repository unsettles it, and a consumer whose `BUILD`
/// files load it can have its rules changed by it, a leaf that reached
/// nothing of the repository included: every leaf is seeded. A `BUILD` edit
/// cannot give such a leaf a path, which is the test above this one's
/// neighbour (`an_unsettled_repository_seeds_every_leaf_that_reaches_it`).
#[test]
fn a_starlark_edit_under_the_repository_seeds_every_leaf() {
    let dir = tree2();
    let root = dir.path();
    std::fs::write(root.join(DIR).join("tsl/defs.bzl"), "").unwrap();
    let q = leaf_scanning(
        root,
        LEAF_UNIVERSE,
        &[("@grpc//:grpc", &["tsl/base"]), ("@grpc//:health", &[])],
    );
    let index = engine().full_build_with("c0", &q).unwrap();
    let r = engine()
        .compute_with_texts(
            index.view(),
            &edit("third_party/tsl/tsl/defs.bzl"),
            &q,
            &req(),
            &FixedText(BTreeMap::new()),
        )
        .unwrap()
        .report;
    if r.global {
        panic!("a .bzl under the repository went global: {:?}", r.global_reason);
    }
    assert_eq!(r.stats.repos_settled, 0);
    assert_eq!(r.stats.consumer_leaves_seeded, 2);
    let affected = r.affected_labels();
    assert!(affected.contains(&"//gapp:gapp".to_string()), "{affected:?}");
    assert!(affected.contains(&"//happ:happ".to_string()), "{affected:?}");
}
