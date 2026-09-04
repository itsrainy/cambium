//! The `index` query family, over a hand-built index with a known shape.
//!
//! These queries are what a service shelling out to the binary will call, so
//! the assertions are about the answers being *the same answers a run would
//! give* — same reachability, same ownership rules — not merely well-formed.

use cambium_engine::inspect::{self, IndexQuery, QueryOptions};
use cambium_index::{new_lineage, IndexBuilder, IndexData, NodeKind, PackageUpdate, TargetRow};

/// ```text
///   //lib:core   <- //app:main <- //app:main_test
///        ^                ^
///   lib/a.cc          app/m.cc
///   //lib:helper -----'
/// ```
fn sample() -> IndexData {
    fn pkg(name: &str, targets: Vec<TargetRow>) -> PackageUpdate {
        PackageUpdate {
            package: name.to_string(),
            build_hash: *blake3::hash(name.as_bytes()).as_bytes(),
            bzl_hash: [0; 32],
            targets,
        }
    }
    let mut b = IndexBuilder::new(new_lineage());
    b.set_bazel_version("test");
    b.apply_packages(&[
        pkg(
            "lib",
            vec![
                TargetRow::rule("//lib:core", "cc_library", vec!["//lib:a.cc".into()]),
                TargetRow::rule("//lib:helper", "cc_library", vec!["//lib:h.cc".into()]),
                TargetRow::source_file("//lib:a.cc"),
                TargetRow::source_file("//lib:h.cc"),
            ],
        ),
        pkg(
            "app",
            vec![
                TargetRow::rule(
                    "//app:main",
                    "cc_binary",
                    vec![
                        "//lib:core".into(),
                        "//lib:helper".into(),
                        "//app:m.cc".into(),
                    ],
                ),
                {
                    let mut t =
                        TargetRow::rule("//app:main_test", "cc_test", vec!["//app:main".into()]);
                    t.is_test = true;
                    t
                },
                {
                    let mut t =
                        TargetRow::rule("//app:manual_test", "cc_test", vec!["//app:main".into()]);
                    t.is_test = true;
                    t.manual = true;
                    t
                },
                TargetRow::source_file("//app:m.cc"),
            ],
        ),
    ]);
    b.finish("cafe")
}

fn opts() -> QueryOptions {
    QueryOptions::default()
}

fn transitive() -> QueryOptions {
    QueryOptions {
        transitive: true,
        ..Default::default()
    }
}

#[test]
fn rdeps_one_hop_and_transitive() {
    let d = sample();
    let q = IndexQuery::new(d.view());

    let direct = q.rdeps("//lib:core", &opts()).unwrap();
    assert_eq!(direct.targets, vec!["//app:main"]);
    assert!(!direct.transitive);

    let all = q.rdeps("//lib:core", &transitive()).unwrap();
    // Transitive rdeps include the subject itself, matching `bazel query rdeps`.
    assert_eq!(
        all.targets,
        vec!["//app:main", "//app:main_test", "//lib:core"]
    );
    assert_eq!(all.count, all.targets.len());
}

#[test]
fn rdeps_respects_manual_and_tests_only() {
    let d = sample();
    let q = IndexQuery::new(d.view());

    let default = q.rdeps("//app:main", &transitive()).unwrap();
    assert!(
        !default.targets.contains(&"//app:manual_test".to_string()),
        "manual targets are excluded by default: {:?}",
        default.targets
    );

    let with_manual = q
        .rdeps(
            "//app:main",
            &QueryOptions {
                transitive: true,
                include_manual: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(with_manual
        .targets
        .contains(&"//app:manual_test".to_string()));

    let tests = q
        .rdeps(
            "//app:main",
            &QueryOptions {
                transitive: true,
                tests_only: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(tests.targets, vec!["//app:main_test"]);
}

#[test]
fn deps_one_hop_and_transitive() {
    let d = sample();
    let q = IndexQuery::new(d.view());

    let direct = q.deps("//app:main", &opts()).unwrap();
    assert_eq!(
        direct.targets,
        vec!["//app:m.cc", "//lib:core", "//lib:helper"]
    );

    let all = q.deps("//app:main_test", &transitive()).unwrap();
    // The transitive form is filtered to rules, like the affected walk.
    assert_eq!(
        all.targets,
        vec![
            "//app:main",
            "//app:main_test",
            "//lib:core",
            "//lib:helper"
        ]
    );
}

#[test]
fn deps_and_rdeps_are_the_same_edges_read_both_ways() {
    // One traversal serves both directions; this is the property that keeps
    // them from drifting.
    let d = sample();
    let q = IndexQuery::new(d.view());
    let deps = q.deps("//app:main", &opts()).unwrap();
    for dep in &deps.targets {
        let back = q.rdeps(dep, &opts()).unwrap();
        assert!(
            back.targets.contains(&"//app:main".to_string()),
            "{dep} is a dep of //app:main but does not list it as an rdep"
        );
    }
}

#[test]
fn path_reports_the_dependency_chain() {
    let d = sample();
    let q = IndexQuery::new(d.view());
    let p = q.path("//app:main_test", "//lib:core").unwrap();
    assert!(p.found);
    assert_eq!(p.path, vec!["//app:main_test", "//app:main", "//lib:core"]);
}

#[test]
fn path_hints_when_the_arguments_are_the_wrong_way_round() {
    let d = sample();
    let q = IndexQuery::new(d.view());
    let p = q.path("//lib:core", "//app:main_test").unwrap();
    assert!(!p.found);
    assert!(p.path.is_empty());
    let hint = p.reversed_hint.expect("a reverse path exists");
    assert!(hint.contains("try swapping"), "{hint}");
}

#[test]
fn path_between_unconnected_targets_has_no_hint() {
    let d = sample();
    let q = IndexQuery::new(d.view());
    let p = q.path("//lib:core", "//lib:helper").unwrap();
    assert!(!p.found);
    assert!(p.reversed_hint.is_none());
}

#[test]
fn consumers_maps_paths_through_the_same_ownership_rule_as_a_run() {
    let d = sample();
    let q = IndexQuery::new(d.view());
    let r = q
        .consumers(
            &[
                "lib/a.cc".to_string(),
                "app/m.cc".to_string(),
                "nowhere/x.cc".to_string(),
            ],
            &opts(),
        )
        .unwrap();
    assert_eq!(r.files[0].package.as_deref(), Some("lib"));
    assert_eq!(r.files[0].label.as_deref(), Some("//lib:a.cc"));
    assert_eq!(r.files[0].consumers, vec!["//lib:core"]);
    assert_eq!(r.files[1].consumers, vec!["//app:main"]);
    // A path under no package is reported, not silently dropped.
    assert_eq!(r.files[2].package, None);
    assert!(r.files[2].consumers.is_empty());
}

#[test]
fn consumers_transitive_matches_the_affected_walk() {
    let d = sample();
    let q = IndexQuery::new(d.view());
    let r = q
        .consumers(&["lib/a.cc".to_string()], &transitive())
        .unwrap();
    // Editing lib/a.cc affects //lib:core and everything above it — the same
    // answer `cambium affected` gives for that file.
    assert_eq!(
        r.files[0].consumers,
        vec!["//app:main", "//app:main_test", "//lib:core"]
    );
}

#[test]
fn owner_resolves_longest_prefix_and_reports_misses() {
    let d = sample();
    let q = IndexQuery::new(d.view());
    let r = q
        .owner(&[
            "lib/a.cc".to_string(),
            "lib/sub/deep.cc".to_string(),
            "unowned/x".to_string(),
        ])
        .unwrap();
    assert_eq!(r.paths[0].package.as_deref(), Some("lib"));
    assert!(r.paths[0].in_index);
    // A file under lib/ that no rule references still belongs to //lib.
    assert_eq!(r.paths[1].package.as_deref(), Some("lib"));
    assert_eq!(r.paths[1].label.as_deref(), Some("//lib:sub/deep.cc"));
    assert!(!r.paths[1].in_index);
    assert_eq!(r.paths[2].package, None);
}

#[test]
fn targets_lists_a_package_and_filters_by_kind() {
    let d = sample();
    let q = IndexQuery::new(d.view());

    let all = q.targets("lib", None).unwrap();
    assert_eq!(all.count, 4);

    let rules = q.targets("lib", Some(NodeKind::Rule)).unwrap();
    assert_eq!(
        rules
            .targets
            .iter()
            .map(|t| t.label.as_str())
            .collect::<Vec<_>>(),
        vec!["//lib:core", "//lib:helper"]
    );
    assert!(rules.targets.iter().all(|t| t.kind == "rule"));

    let srcs = q.targets("lib", Some(NodeKind::SourceFile)).unwrap();
    assert_eq!(srcs.count, 2);
}

#[test]
fn unknown_subjects_are_errors_rather_than_empty_answers() {
    let d = sample();
    let q = IndexQuery::new(d.view());
    assert!(q.rdeps("//nope:nope", &opts()).is_err());
    assert!(q.deps("//nope:nope", &opts()).is_err());
    assert!(q.path("//nope:nope", "//lib:core").is_err());
    assert!(q.targets("nope", None).is_err());
    assert!(inspect::parse_kind("banana").is_err());
}

#[test]
fn stats_accounts_for_every_byte_and_describes_the_shape() {
    let d = sample();
    let bytes = cambium_index::format::to_bytes(&d).unwrap();
    let q = IndexQuery::new(d.view());
    let s = q.stats(bytes.len() as u64);

    assert_eq!(s.counts.nodes, d.view().num_nodes());
    assert_eq!(s.counts.rules, 5);
    assert_eq!(s.counts.tests, 2);
    assert_eq!(s.counts.source_files, 3);
    assert_eq!(s.counts.packages, 2);

    let b = &s.bytes;
    let accounted = b.label_table
        + b.rule_class_table
        + b.forward_offsets
        + b.forward_edges
        + b.reverse_offsets
        + b.reverse_edges
        + b.rule_hashes
        + b.node_columns
        + b.package_table;
    assert_eq!(
        accounted as i64 + b.framing,
        b.file_total as i64,
        "the component sizes plus framing must equal the file"
    );
    assert!(b.framing >= 0, "framing should not be negative: {b:?}");
    assert!(b.label_table > 0);
    assert_eq!(
        b.files_csr, 0,
        "the files CSR is a view over reverse, not a stored array"
    );
    assert!(b.label_table_share > 0.0 && b.label_table_share < 1.0);

    // //app:main is depended on by two tests; nothing has a higher in-degree.
    assert_eq!(s.in_degree.max, 2);
    assert_eq!(s.in_degree.max_label, "//app:main");
    assert!(s.in_degree.mean > 0.0);
}

#[test]
fn every_result_carries_the_schema_version() {
    use cambium_engine::report::SCHEMA_VERSION;
    let d = sample();
    let q = IndexQuery::new(d.view());
    assert_eq!(
        q.rdeps("//lib:core", &opts()).unwrap().schema,
        SCHEMA_VERSION
    );
    assert_eq!(
        q.deps("//app:main", &opts()).unwrap().schema,
        SCHEMA_VERSION
    );
    assert_eq!(
        q.path("//app:main", "//lib:core").unwrap().schema,
        SCHEMA_VERSION
    );
    assert_eq!(
        q.consumers(&["lib/a.cc".into()], &opts()).unwrap().schema,
        SCHEMA_VERSION
    );
    assert_eq!(
        q.owner(&["lib/a.cc".into()]).unwrap().schema,
        SCHEMA_VERSION
    );
    assert_eq!(q.targets("lib", None).unwrap().schema, SCHEMA_VERSION);
    assert_eq!(q.stats(1).schema, SCHEMA_VERSION);
}
