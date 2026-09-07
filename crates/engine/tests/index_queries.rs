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
    // answer `cambi affected` gives for that file.
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
        + b.label_sorted
        + b.rule_class_table
        + b.forward_offsets
        + b.forward_edges
        + b.reverse_offsets
        + b.reverse_edges
        + b.rule_hashes
        + b.node_columns
        + b.package_table
        + b.bzl_closure;
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
    assert!(
        b.label_total_share > b.label_table_share,
        "the sort index is part of what labels cost: {b:?}"
    );

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

#[test]
fn the_label_packing_projection_adds_up() {
    let d = sample();
    let bytes = cambium_index::format::to_bytes(&d).unwrap();
    let v = d.view();
    let q = IndexQuery::new(v);
    let s = q.stats(bytes.len() as u64);
    let lp = &s.bytes.label_packing;

    assert_eq!(
        lp.packed_label_table + lp.prefix_bytes,
        s.bytes.label_table,
        "packing removes exactly the prefix bytes and nothing else"
    );

    // The projection is arithmetic over the labels, so it can be checked
    // against the labels directly rather than trusted.
    let mut prefix = 0u64;
    let mut unpackable = 0u32;
    for id in 0..v.num_nodes() {
        let label = v.label(id);
        if v.package[id as usize] == cambium_index::NO_ID {
            unpackable += 1;
            continue;
        }
        let colon = label
            .rfind(':')
            .expect("a label with a package has a colon");
        prefix += colon as u64 + 1;
        // What packing keeps is enough to rebuild the label, which is the whole
        // reason the external API can go on returning full ones.
        let pkg = v.pkg_names.get(v.package[id as usize]);
        let rebuilt = format!("//{pkg}:{}", &label[colon + 1..]);
        assert_eq!(rebuilt, label, "a packed label must round-trip");
    }
    assert_eq!(lp.prefix_bytes, prefix);
    assert_eq!(lp.unpackable_nodes, unpackable);
}

/// The measurement item 7 turns on: how much of an index is labels, and how
/// much of that a `(package_id, relative_name)` encoding would give back.
///
/// The numbers for the `large` and `xl` presets are in `BENCHMARKS.md`; this
/// only pins that the two quantities stay coherent with each other, since the
/// decision not to pack rests on them.
#[test]
fn the_label_share_and_the_saving_are_consistent() {
    let d = sample();
    let bytes = cambium_index::format::to_bytes(&d).unwrap();
    let s = IndexQuery::new(d.view()).stats(bytes.len() as u64);
    let b = &s.bytes;

    assert!(
        b.label_packing.file_saving_share < b.label_total_share,
        "packing cannot save more than labels cost: {b:?}"
    );
    let residual = (b.label_table + b.label_sorted - b.label_packing.prefix_bytes) as f64
        / b.file_total as f64;
    assert!(
        (residual - (b.label_total_share - b.label_packing.file_saving_share)).abs() < 1e-9,
        "the shares must describe the same bytes: {b:?}"
    );
}

/// Longer package paths mean a bigger label table, and `BENCHMARKS.md`
/// extrapolates from the synthetic presets to a real repository on exactly that
/// basis: every byte added to a package path lands once in each of its nodes'
/// labels and once in the package name table, and nowhere else.
///
/// That is an arithmetic claim about the file layout, and the decision not to
/// pack labels rests on it, so it is checked rather than asserted.
#[test]
fn label_share_grows_with_package_path_length_as_predicted() {
    fn index_with_prefix(prefix: &str) -> IndexData {
        let mut b = IndexBuilder::new(new_lineage());
        b.set_bazel_version("test");
        let pkgs: Vec<PackageUpdate> = (0..8)
            .map(|i| {
                let name = format!("{prefix}p{i}");
                let targets = (0..5)
                    .flat_map(|t| {
                        let src = format!("//{name}:s{t}.cc");
                        vec![
                            TargetRow::rule(
                                format!("//{name}:t{t}"),
                                "cc_library",
                                vec![src.clone()],
                            ),
                            TargetRow::source_file(&src),
                        ]
                    })
                    .collect();
                PackageUpdate {
                    package: name,
                    build_hash: [0; 32],
                    bzl_hash: [0; 32],
                    targets,
                }
            })
            .collect();
        b.apply_packages(&pkgs);
        b.finish("cafe")
    }

    let measure = |prefix: &str| {
        let d = index_with_prefix(prefix);
        let bytes = cambium_index::format::to_bytes(&d).unwrap();
        IndexQuery::new(d.view()).stats(bytes.len() as u64)
    };

    let base = measure("");
    // Ten more bytes on every package path, as a deep source tree would have.
    let deeper = measure("src/main/");
    let d = "src/main/".len() as u64;

    let n = base.counts.nodes as u64;
    let p = base.counts.packages as u64;
    assert_eq!(n, deeper.counts.nodes as u64, "same graph, longer names");
    assert_eq!(p, deeper.counts.packages as u64);

    assert_eq!(
        deeper.bytes.label_table,
        base.bytes.label_table + d * n,
        "each added byte appears once per label"
    );
    assert_eq!(
        deeper.bytes.package_table,
        base.bytes.package_table + d * p,
        "and once per package name"
    );
    assert_eq!(
        deeper.bytes.label_packing.prefix_bytes,
        base.bytes.label_packing.prefix_bytes + d * n,
        "all of which packing would give back"
    );

    // The predicted share, which is what BENCHMARKS.md extrapolates with.
    let predicted = (base.bytes.label_table + base.bytes.label_sorted + d * n) as f64
        / (base.bytes.file_total + d * (n + p)) as f64;
    assert!(
        (predicted - deeper.bytes.label_total_share).abs() < 0.005,
        "predicted {predicted:.4} vs measured {:.4}",
        deeper.bytes.label_total_share
    );
    assert!(
        deeper.bytes.label_total_share > base.bytes.label_total_share,
        "longer paths make labels a bigger share, which is the whole point"
    );
}
