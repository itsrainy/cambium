//! A path that leaves the main repository through a leaf and comes back.
//!
//! `bazelbuild/rules_shell` is the root module, and every test it declares
//! depends on `@bazel_tools//tools/test:collect_coverage`, which depends on
//! `@rules_shell//shell/runfiles:runfiles`: the main repository's own
//! runfiles library. The index held the coverage collector as a leaf with no
//! outgoing edges, so an edit to `shell/runfiles/runfiles.bash` reached no
//! test, where Bazel's `rdeps` has every one. The leaf now carries an edge to
//! each target inside the universe it reaches through targets outside it
//! (`crate::round_trip`), which is Bazel's answer, asked at cold build.
//!
//! The tree, cut down to the shape:
//!
//! ```text
//! //tests:env_test   -> //tests:env_test.sh, @bazel_tools//tools/test:collect_coverage
//! //tests:other_test -> @bazel_tools//tools/test:collect_coverage
//! //shell/runfiles:runfiles -> //shell/runfiles:runfiles.bash
//! //lib:lib -> @@fmt+//:fmt
//! //app:app -> //lib:lib
//! //tool:tool -> @@other+//:x
//! ```

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::FixedText;
use cambium_engine::{AffectedReport, AffectedRequest, Config, RunOutcome};
use cambium_index::{IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{engine, engine_with, req, Files, Scanning};

const COVERAGE: &str = "@bazel_tools//tools/test:collect_coverage";
const FMT: &str = "@@fmt+//:fmt";
const OTHER: &str = "@@other+//:x";
const RUNFILES: &str = "//shell/runfiles:runfiles";

const MODULE: &str = "module(name = \"rules_shell\", version = \"1\")\n\
     bazel_dep(name = \"fmt\", version = \"1.0\")\n";

const REPO_MAPPING: &str = "{\"\":\"\",\"rules_shell\":\"\",\"fmt\":\"fmt+\"}";

fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "shell/runfiles" => vec![
            TargetRow::source_file("//shell/runfiles:runfiles.bash"),
            TargetRow::rule(
                RUNFILES,
                "sh_library",
                vec!["//shell/runfiles:runfiles.bash".into()],
            ),
        ],
        "tests" => vec![
            TargetRow::source_file("//tests:env_test.sh"),
            TargetRow::rule(
                "//tests:env_test",
                "sh_test",
                vec!["//tests:env_test.sh".into(), COVERAGE.into()],
            ),
            TargetRow::rule("//tests:other_test", "sh_test", vec![COVERAGE.into()]),
        ],
        "lib" => vec![TargetRow::rule("//lib:lib", "demo_lib", vec![FMT.into()])],
        "tool" => vec![TargetRow::rule(
            "//tool:tool",
            "demo_lib",
            vec![OTHER.into()],
        )],
        "app" => vec![TargetRow::rule(
            "//app:app",
            "demo_lib",
            vec!["//lib:lib".into()],
        )],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

/// [`rows`], with `//shell/runfiles:runfiles` depending on a second library
/// of its package: a main-repo `BUILD` edit below the leaf's entry.
fn rows_with_extra(pkg: &str) -> PackageUpdate {
    match pkg {
        "shell/runfiles" => PackageUpdate {
            package: pkg.to_string(),
            targets: vec![
                TargetRow::source_file("//shell/runfiles:runfiles.bash"),
                TargetRow::source_file("//shell/runfiles:extra.bash"),
                TargetRow::rule(
                    "//shell/runfiles:extra",
                    "sh_library",
                    vec!["//shell/runfiles:extra.bash".into()],
                ),
                TargetRow::rule(
                    RUNFILES,
                    "sh_library",
                    vec![
                        "//shell/runfiles:runfiles.bash".into(),
                        "//shell/runfiles:extra".into(),
                    ],
                ),
            ],
            ..Default::default()
        },
        other => rows(other),
    }
}

/// [`rows`], with a new test that names a leaf no rule named before.
fn rows_with_new_leaf(pkg: &str) -> PackageUpdate {
    match pkg {
        "tests" => {
            let mut u = rows(pkg);
            u.targets.push(TargetRow::rule(
                "//tests:lint_test",
                "sh_test",
                vec!["@rules_lint//:lint".into()],
            ));
            u
        }
        other => rows(other),
    }
}

/// [`rows`], with no rule naming the coverage collector any more.
fn rows_without_coverage(pkg: &str) -> PackageUpdate {
    match pkg {
        "tests" => PackageUpdate {
            package: pkg.to_string(),
            targets: vec![
                TargetRow::source_file("//tests:env_test.sh"),
                TargetRow::rule(
                    "//tests:env_test",
                    "sh_test",
                    vec!["//tests:env_test.sh".into()],
                ),
            ],
            ..Default::default()
        },
        other => rows(other),
    }
}

const PACKAGES: &[&str] = &["shell/runfiles", "tests", "lib", "app", "tool"];

fn files() -> Files {
    let mut m = BTreeMap::new();
    m.insert("MODULE.bazel".to_string(), MODULE.to_string());
    for p in PACKAGES {
        m.insert(format!("{p}/BUILD"), String::new());
    }
    Files(m)
}

/// A querier over the tree whose rows are `rows`; Bazel answers the
/// round-trip question with nothing reaching the universe until a test says
/// otherwise with `with_round_trip`.
fn scanning_with(rows: fn(&str) -> PackageUpdate) -> Scanning {
    Scanning::new(PACKAGES, rows)
        .with_files(files())
        .with_repo_mapping(REPO_MAPPING)
        .with_round_trip(COVERAGE, &[])
}

/// Bazel's answer at `b1eed28`: the coverage collector reaches the runfiles
/// library and the shell toolchain type, which this index has no node for.
fn scanning() -> Scanning {
    scanning_with(rows).with_round_trip(COVERAGE, &[RUNFILES, "//shell:toolchain_type"])
}

fn edit(paths: &[&str]) -> Vec<Change> {
    paths
        .iter()
        .map(|p| Change::new(Status::Modified, *p))
        .collect()
}

fn run(index: &IndexData, q: &Scanning, changes: &[Change]) -> AffectedReport {
    let r = engine()
        .compute_with_texts(
            index.view(),
            changes,
            q,
            &req(),
            &FixedText(BTreeMap::new()),
        )
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
    texts: &FixedText,
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
            texts,
        )
        .unwrap();
    assert!(!out.report.global, "{:?}", out.report.global_reason);
    out
}

fn materialized(index: &IndexData, q: &Scanning, changes: &[Change]) -> IndexData {
    materialized_with(
        Config::default(),
        index,
        q,
        changes,
        &FixedText(BTreeMap::new()),
    )
    .index
    .expect("the spliced index")
}

/// The labels a leaf's forward row names.
fn row_of(index: &IndexData, leaf: &str) -> Vec<String> {
    let v = index.view();
    let Some(id) = v.id_of(leaf) else {
        return Vec::new();
    };
    let mut row: Vec<String> = v
        .forward
        .row(id)
        .iter()
        .map(|&d| v.label(d).to_string())
        .collect();
    row.sort();
    row
}

fn tests_of(r: &AffectedReport) -> Vec<String> {
    r.affected_labels()
        .into_iter()
        .filter(|l| l.starts_with("//tests:"))
        .collect()
}

const RUNFILES_EDIT: &[&str] = &["shell/runfiles/runfiles.bash"];

#[test]
fn a_change_inside_reaches_the_rules_above_a_leaf_through_its_edge() {
    let q = scanning();
    let index = engine().full_build_with("c0", &q).unwrap();
    // One question, over every leaf the index holds, at cold build; the
    // toolchain type the index has no node for is dropped.
    let asked = q.round_trip_questions();
    assert_eq!(asked.len(), 1, "{asked:?}");
    let mut leaves = asked[0].clone();
    leaves.sort();
    let mut want = vec![COVERAGE.to_string(), FMT.to_string(), OTHER.to_string()];
    want.sort();
    assert_eq!(leaves, want);
    assert_eq!(row_of(&index, COVERAGE), vec![RUNFILES.to_string()]);
    assert!(row_of(&index, FMT).is_empty());

    let r = run(&index, &q, &edit(RUNFILES_EDIT));
    assert_eq!(
        tests_of(&r),
        vec![
            "//tests:env_test".to_string(),
            "//tests:other_test".to_string()
        ]
    );
    let affected = r.affected_labels();
    assert!(affected.contains(&RUNFILES.to_string()), "{affected:?}");
    // The leaf is a way back in, not a rule: never reported itself.
    assert!(!affected.contains(&COVERAGE.to_string()), "{affected:?}");
    assert!(!affected.contains(&"//app:app".to_string()), "{affected:?}");

    // Without Bazel's answer the leaf has no edge, and the edit reaches no
    // test: what every index before this change answered.
    let without = engine()
        .full_build_with("c0", &Scanning::new(PACKAGES, rows).with_files(files()))
        .unwrap();
    assert!(row_of(&without, COVERAGE).is_empty());
    assert!(tests_of(&run(&without, &q, &edit(RUNFILES_EDIT))).is_empty());
}

/// No package owns a leaf, so no splice rebuilds its row: a re-query of an
/// unrelated package carries it, and so does a re-query of the package whose
/// rules name the leaf. Neither asks Bazel again.
#[test]
fn the_edge_survives_a_splice_of_any_package() {
    let q = scanning();
    let index = engine().full_build_with("c0", &q).unwrap();
    let unrelated = materialized(&index, &q, &edit(&["app/BUILD"]));
    assert_eq!(row_of(&unrelated, COVERAGE), vec![RUNFILES.to_string()]);
    let consumer = materialized(&unrelated, &q, &edit(&["tests/BUILD"]));
    assert_eq!(row_of(&consumer, COVERAGE), vec![RUNFILES.to_string()]);
    assert_eq!(q.round_trip_questions().len(), 1, "the cold build's alone");

    let r = run(&consumer, &q, &edit(RUNFILES_EDIT));
    assert_eq!(
        tests_of(&r),
        vec![
            "//tests:env_test".to_string(),
            "//tests:other_test".to_string()
        ]
    );
}

/// A `BUILD` edit below the leaf's entry changes what the leaf reaches
/// through the main repository, and the index's own rows say so: the edge
/// stops at the first node inside, so nothing about it went stale and
/// nothing is asked. The next edit to what the entry now depends on reaches
/// the tests.
#[test]
fn a_build_edit_below_the_entry_needs_no_new_answer() {
    let q = scanning();
    let index = engine().full_build_with("c0", &q).unwrap();
    let after = scanning_with(rows_with_extra)
        .with_round_trip(COVERAGE, &[RUNFILES, "//shell:toolchain_type"]);
    let head = materialized(&index, &after, &edit(&["shell/runfiles/BUILD"]));
    assert!(
        after.round_trip_questions().is_empty(),
        "{:?}",
        after.round_trip_questions()
    );
    assert_eq!(row_of(&head, COVERAGE), vec![RUNFILES.to_string()]);

    let r = run(&head, &after, &edit(&["shell/runfiles/extra.bash"]));
    assert_eq!(
        tests_of(&r),
        vec![
            "//tests:env_test".to_string(),
            "//tests:other_test".to_string()
        ]
    );
}

/// A leaf a splice adds is asked about in the splice that writes it, and
/// alone: here a new test names `@rules_lint//:lint`, whose closure reaches
/// the runfiles library too.
#[test]
fn a_leaf_a_splice_adds_is_asked_for_in_that_splice() {
    let q = scanning();
    let index = engine().full_build_with("c0", &q).unwrap();
    let after = scanning_with(rows_with_new_leaf)
        .with_round_trip(COVERAGE, &[RUNFILES])
        .with_round_trip("@rules_lint//:lint", &[RUNFILES]);
    let head = materialized(&index, &after, &edit(&["tests/BUILD"]));
    assert_eq!(
        after.round_trip_questions(),
        vec![vec!["@rules_lint//:lint".to_string()]]
    );
    assert_eq!(
        row_of(&head, "@rules_lint//:lint"),
        vec![RUNFILES.to_string()]
    );
    assert_eq!(row_of(&head, COVERAGE), vec![RUNFILES.to_string()]);

    let r = run(&head, &after, &edit(RUNFILES_EDIT));
    assert!(
        tests_of(&r).contains(&"//tests:lint_test".to_string()),
        "{r:?}"
    );

    // An overlay, which is never written, does not ask.
    let overlay = scanning_with(rows_with_new_leaf).with_round_trip(COVERAGE, &[RUNFILES]);
    let _ = run(&index, &overlay, &edit(&["tests/BUILD"]));
    assert!(overlay.round_trip_questions().is_empty());
}

/// An overflow re-queries `//...` and asks for every leaf again, as the
/// consumer table is asked again: here the answer at head has `@@fmt+//:fmt`
/// reaching the runfiles library where at cold build it reached nothing, and
/// has the coverage collector gone (no rule names it at head, and Bazel no
/// longer resolves it), so its edge is retired rather than carried.
#[test]
fn an_overflow_re_derives_every_leaf_and_retires_one_that_is_gone() {
    let q = scanning();
    let index = engine().full_build_with("c0", &q).unwrap();
    assert_eq!(row_of(&index, COVERAGE), vec![RUNFILES.to_string()]);

    let mut cfg = Config::default();
    cfg.limits.max_dirty_packages = 1;
    let after = Scanning::new(PACKAGES, rows_without_coverage)
        .with_files(files())
        .with_repo_mapping(REPO_MAPPING)
        .with_round_trip(FMT, &[RUNFILES]);
    let out = materialized_with(
        cfg,
        &index,
        &after,
        &edit(&["tests/BUILD", "lib/BUILD"]),
        &FixedText(BTreeMap::new()),
    );
    assert!(
        out.report.stats.cold_rebuild,
        "{:?}",
        out.report.cold_rebuild_reason
    );
    let head = out.index.expect("the spliced index");
    let asked = after.round_trip_questions();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].contains(&COVERAGE.to_string()), "{asked:?}");
    assert!(row_of(&head, COVERAGE).is_empty(), "retired");
    assert_eq!(row_of(&head, FMT), vec![RUNFILES.to_string()]);

    let r = run(&head, &after, &edit(RUNFILES_EDIT));
    let affected = r.affected_labels();
    assert!(affected.contains(&"//lib:lib".to_string()), "{affected:?}");
    assert!(affected.contains(&"//app:app".to_string()), "{affected:?}");
}

fn module_texts(head: &str) -> FixedText {
    let mut m = BTreeMap::new();
    m.insert(
        ("base".to_string(), "MODULE.bazel".to_string()),
        MODULE.to_string(),
    );
    m.insert(
        ("head".to_string(), "MODULE.bazel".to_string()),
        head.to_string(),
    );
    FixedText(m)
}

/// What a leaf reaches is a function of the repositories outside the
/// universe alone, so a run in which one of them moved asks at head. Here
/// `fmt` is bumped, and at the new version `@@other+//:x`, whose closure runs
/// through `@fmt`, reaches the runfiles library; the same commit edits it.
/// `@@other+` did not move, so nothing seeds its leaf, and the base's rows
/// have no path from it: `//tool:tool` would be missed. The answer at head
/// has the path, and the index the run writes keeps it without asking twice.
#[test]
fn a_repository_outside_moving_is_answered_at_head() {
    let q = scanning();
    let index = engine().full_build_with("c0", &q).unwrap();
    assert!(row_of(&index, OTHER).is_empty());

    let bumped = MODULE.replace("version = \"1.0\"", "version = \"2.0\"");
    let changes = edit(&["MODULE.bazel", "shell/runfiles/runfiles.bash"]);
    let after = scanning().with_round_trip(OTHER, &[RUNFILES]);
    let out = materialized_with(
        Config::default(),
        &index,
        &after,
        &changes,
        &module_texts(&bumped),
    );
    assert_eq!(out.report.stats.repos_dirty_by_module, 1);
    let affected = out.report.affected_labels();
    for l in [
        "//tool:tool",
        "//lib:lib",
        "//tests:env_test",
        "//tests:other_test",
    ] {
        assert!(affected.contains(&l.to_string()), "{l}: {affected:?}");
    }
    assert_eq!(
        after.round_trip_questions().len(),
        1,
        "{:?}",
        after.round_trip_questions()
    );
    let head = out.index.expect("the spliced index");
    assert_eq!(row_of(&head, OTHER), vec![RUNFILES.to_string()]);

    // Answered from the base's rows, the same run misses `//tool:tool`.
    let silent = Scanning::new(PACKAGES, rows)
        .with_files(files())
        .with_repo_mapping(REPO_MAPPING);
    let r = engine()
        .compute_with_texts(
            index.view(),
            &changes,
            &silent,
            &req(),
            &module_texts(&bumped),
        )
        .unwrap()
        .report;
    assert!(!r.affected_labels().contains(&"//tool:tool".to_string()));

    // With the runfiles edit alone nothing outside moved: no question.
    let quiet = scanning();
    let _ = run(&index, &quiet, &edit(RUNFILES_EDIT));
    assert!(quiet.round_trip_questions().is_empty());
}

const PROBE: &str = "//shell/runfiles:probe";
const SH_BINARY_BZL: &str = "//shell/private:sh_binary.bzl";

/// [`rows`], with a filegroup `//shell/runfiles:probe` nothing inside names.
fn rows_with_probe(pkg: &str) -> PackageUpdate {
    match pkg {
        "shell/runfiles" => {
            let mut u = rows(pkg);
            u.targets
                .push(TargetRow::source_file("//shell/runfiles:probe.txt"));
            u.targets.push(TargetRow::rule(
                PROBE,
                "filegroup",
                vec!["//shell/runfiles:probe.txt".into()],
            ));
            u
        }
        other => rows(other),
    }
}

/// Bazel's answer with the probe: the coverage collector's class, defined in
/// the main repository's `shell/private/sh_binary.bzl`, which the collector's
/// package loads.
fn probing(entries: &[&str]) -> Scanning {
    scanning_with(rows_with_probe)
        .with_round_trip(COVERAGE, entries)
        .with_round_trip_via(&[SH_BINARY_BZL])
}

/// The reviewer's reproduction on rules_shell, cut down. The coverage
/// collector is an `sh_binary`, and `sh_binary`'s class is the root
/// module's own `.bzl`: an edit there giving the class an implicit
/// `_rt_probe = //shell/runfiles:probe` moves what the leaf reaches, and no
/// `BUILD` file, package or repository outside the universe moved. The
/// closure's loads are what the edges were derived through, so the edit
/// asks at head, the index the splice writes carries the new edge, and the
/// next edit to the probe reaches the tests. One run over both edits
/// answers the same.
#[test]
fn a_main_repo_bzl_the_closure_loads_asks_again() {
    let a = probing(&[RUNFILES]);
    let index = engine().full_build_with("c0", &a).unwrap();
    assert_eq!(row_of(&index, COVERAGE), vec![RUNFILES.to_string()]);
    assert_eq!(
        index.view().round_trip_via().collect::<Vec<_>>(),
        vec![SH_BINARY_BZL]
    );

    let bzl_edit = edit(&["shell/private/sh_binary.bzl"]);
    let b = probing(&[PROBE, RUNFILES]);
    let out = materialized_with(
        Config::default(),
        &index,
        &b,
        &bzl_edit,
        &FixedText(BTreeMap::new()),
    );
    assert_eq!(
        b.round_trip_questions().len(),
        1,
        "{:?}",
        b.round_trip_questions()
    );
    let head = out.index.expect("the spliced index");
    assert_eq!(
        row_of(&head, COVERAGE),
        vec![PROBE.to_string(), RUNFILES.to_string()]
    );
    assert_eq!(
        head.view().round_trip_via().collect::<Vec<_>>(),
        vec![SH_BINARY_BZL]
    );
    let probe_edit = edit(&["shell/runfiles/probe.txt"]);
    let r = run(&head, &b, &probe_edit);
    assert_eq!(
        tests_of(&r),
        vec![
            "//tests:env_test".to_string(),
            "//tests:other_test".to_string()
        ]
    );

    // Both edits in one run: the answer at head has the path, and the
    // overlay walks it.
    let both = probing(&[PROBE, RUNFILES]);
    let r = run(
        &index,
        &both,
        &edit(&["shell/private/sh_binary.bzl", "shell/runfiles/probe.txt"]),
    );
    assert_eq!(both.round_trip_questions().len(), 1);
    assert!(
        tests_of(&r).contains(&"//tests:env_test".to_string()),
        "{r:?}"
    );

    // Without the via entry the edit asks nothing and the edge is never
    // written: the answer the branch gave before, one label for the probe.
    let blind_q = scanning_with(rows_with_probe).with_round_trip(COVERAGE, &[RUNFILES]);
    let blind = engine().full_build_with("c0", &blind_q).unwrap();
    let after = scanning_with(rows_with_probe).with_round_trip(COVERAGE, &[PROBE, RUNFILES]);
    let head = materialized(&blind, &after, &bzl_edit);
    assert!(after.round_trip_questions().is_empty());
    assert!(tests_of(&run(&head, &after, &probe_edit)).is_empty());

    // A main-repo file the closure does not load asks nothing.
    let quiet = probing(&[RUNFILES]);
    let _ = run(&index, &quiet, &edit(&["shell/private/sh_library.bzl"]));
    assert!(quiet.round_trip_questions().is_empty());
}

/// An entry the index has no node for (`//shell:marker` here, in a package
/// the index does not hold yet) is dropped from the row, and its package
/// is what the edges were derived through: when the package moves, the
/// leaf is asked about again and the entry, now a node, becomes an edge.
#[test]
fn an_entry_with_no_node_is_asked_for_when_its_package_moves() {
    const MARKER: &str = "//shell:marker";
    let q = scanning_with(rows).with_round_trip(COVERAGE, &[RUNFILES, MARKER]);
    let index = engine().full_build_with("c0", &q).unwrap();
    assert_eq!(row_of(&index, COVERAGE), vec![RUNFILES.to_string()]);
    assert_eq!(
        index.view().round_trip_via().collect::<Vec<_>>(),
        vec!["//shell"]
    );
    fn with_shell(pkg: &str) -> PackageUpdate {
        match pkg {
            "shell" => PackageUpdate {
                package: pkg.to_string(),
                targets: vec![TargetRow::rule(MARKER, "filegroup", vec![])],
                ..Default::default()
            },
            other => rows(other),
        }
    }
    const WITH_SHELL: &[&str] = &["shell/runfiles", "tests", "lib", "app", "tool", "shell"];
    let mut f = files();
    f.0.insert("shell/BUILD".to_string(), String::new());
    let after = Scanning::new(WITH_SHELL, with_shell)
        .with_files(f)
        .with_repo_mapping(REPO_MAPPING)
        .with_round_trip(COVERAGE, &[RUNFILES, MARKER]);
    let head = materialized(&index, &after, &[Change::new(Status::Added, "shell/BUILD")]);
    assert_eq!(after.round_trip_questions().len(), 1);
    assert_eq!(
        row_of(&head, COVERAGE),
        vec![RUNFILES.to_string(), MARKER.to_string()]
    );
    assert!(head.view().round_trip_via().next().is_none());
}
