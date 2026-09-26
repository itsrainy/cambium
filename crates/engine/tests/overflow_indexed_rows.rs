//! An overflow's re-query refreshes the indexed repositories' rows too.
//!
//! Past `limits.max_dirty_packages` a run re-queries the universe and splices
//! all of it onto the base. The universe a cold build asks for is `//...:*`
//! plus one `@name//...:*` per indexed in-tree repository, and the dirty set
//! names that repository's packages like any other. A re-query of `//...:*`
//! alone left them out: their rows stayed as old as the last run that
//! re-queried them, while a cold build at the same commit had the head's.
//!
//! envoy, 73 carried steps from `4cf50c64` to `86c52279`: seven overflows
//! landed `api/` changes (the `googleapis`, `grpc` and `legacy_*` renames,
//! a `cc_proto_descriptor_library` definition move, two new packages), and
//! the rebuild oracle at the pin found 1634 `@envoy_api` rule-hash
//! mismatches, 4243 and 4411 one-sided edges and 23 missing nodes, every one
//! of them in `@envoy_api`. A stale row is a direct set decided against the
//! wrong hash, and an edge missing from it is a consumer never reached.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::FixedText;
use cambium_index::{IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{engine, req, Files, Scanning};

const DIR: &str = "third_party/tsl";

const REPO_BZL: &str = "def _tsl_impl(mctx):\n\
     \x20   tf_vendored(name = \"tsl\", path = \"third_party/tsl\")\n\
     tsl_extension = module_extension(implementation = _tsl_impl)\n";

const MODULE_BAZEL: &str = "module(name = \"xla\")\n\
     tsl_extension = use_extension(\"//third_party:repo.bzl\", \"tsl_extension\")\n\
     use_repo(tsl_extension, \"tsl\")\n";

const PROBED: &[&str] = &["", "tsl/platform"];

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("MODULE.bazel"), MODULE_BAZEL).unwrap();
    std::fs::create_dir_all(root.join(DIR).join("tsl/platform")).unwrap();
    std::fs::write(root.join(DIR).join("REPO.bazel"), "# the marker\n").unwrap();
    std::fs::write(root.join(DIR).join("BUILD"), "").unwrap();
    std::fs::write(root.join(DIR).join("tsl/platform/BUILD"), "").unwrap();
    dir
}

fn main_rows(pkg: &str) -> Vec<TargetRow> {
    match pkg {
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
        _ => Vec::new(),
    }
}

fn rule(label: &str, inputs: &[&str], hash: u8) -> TargetRow {
    let mut r = TargetRow::rule(
        label,
        "demo_lib",
        inputs.iter().map(|s| s.to_string()).collect(),
    );
    r.rule_hash = [hash; 32];
    r
}

/// The commit the base is built at: `dso_loader` is built from `x.cc`.
fn old_rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "@tsl//tsl/platform" => vec![
            rule(
                "@tsl//tsl/platform:dso_loader",
                &["@tsl//tsl/platform:x.cc"],
                1,
            ),
            TargetRow::source_file("@tsl//tsl/platform:x.cc"),
        ],
        other => main_rows(other),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

/// The head: the repository's `BUILD` gave `dso_loader` a second source,
/// `y.cc`, which moves its hash and adds an edge.
fn new_rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "@tsl//tsl/platform" => vec![
            rule(
                "@tsl//tsl/platform:dso_loader",
                &["@tsl//tsl/platform:x.cc", "@tsl//tsl/platform:y.cc"],
                2,
            ),
            TargetRow::source_file("@tsl//tsl/platform:x.cc"),
            TargetRow::source_file("@tsl//tsl/platform:y.cc"),
        ],
        other => main_rows(other),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

fn scanning(root: &std::path::Path, rows: fn(&str) -> PackageUpdate) -> Scanning {
    let mut m = BTreeMap::new();
    m.insert("MODULE.bazel".to_string(), MODULE_BAZEL.to_string());
    m.insert("third_party/repo.bzl".to_string(), REPO_BZL.to_string());
    m.insert("app/BUILD".to_string(), String::new());
    m.insert("lib/BUILD".to_string(), String::new());
    Scanning::new(&["app", "lib", "@tsl//tsl/platform"], rows)
        .with_files(Files(m))
        .with_workspace(root)
        .with_repo_probe("tsl", PROBED, false)
}

/// `(hash, deps)` of one rule, by label.
fn row(index: &IndexData, label: &str) -> ([u8; 32], Vec<String>) {
    let v = index.view();
    let id = v
        .id_of(label)
        .unwrap_or_else(|| panic!("{label} is not in the index"));
    assert!(!v.is_tombstone(id), "{label} is tombstoned");
    let mut deps: Vec<String> = v
        .forward
        .row(id)
        .iter()
        .map(|&d| v.label(d).to_string())
        .collect();
    deps.sort();
    (v.hash[id as usize], deps)
}

#[test]
fn an_overflow_re_query_refreshes_the_indexed_repositorys_rows() {
    let dir = tree();
    let root = dir.path();
    let base = engine()
        .full_build_with("c0", &scanning(root, old_rows))
        .unwrap();
    assert_eq!(base.view().indexed_repos(), vec!["tsl".to_string()]);
    assert_eq!(row(&base, "@tsl//tsl/platform:dso_loader").0, [1; 32]);

    // A commit that edits two main-repo packages and the repository's own
    // package, under a limit of one so the dirty set certainly overflows.
    let mut cfg = cambium_engine::Config::default();
    cfg.limits.max_dirty_packages = 1;
    let changes = vec![
        Change::new(Status::Modified, "app/BUILD"),
        Change::new(Status::Modified, "lib/BUILD"),
        Change::new(Status::Modified, "third_party/tsl/tsl/platform/BUILD"),
    ];
    let head_q = scanning(root, new_rows);
    let out = cambium_testkit::engine_with(cfg)
        .compute_with_texts(
            base.view(),
            &changes,
            &head_q,
            &cambium_engine::AffectedRequest {
                materialize: true,
                ..req()
            },
            &FixedText(BTreeMap::new()),
        )
        .unwrap();
    assert!(
        out.report.stats.cold_rebuild,
        "{:?}",
        out.report.cold_rebuild_reason
    );
    assert!(!out.report.global, "{:?}", out.report.global_reason);
    assert!(
        out.report
            .dirty_packages
            .iter()
            .any(|p| p == "@tsl//tsl/platform"),
        "the dirty set names the repository's package: {:?}",
        out.report.dirty_packages
    );
    let head = out.index.expect("the spliced index");

    // The row is the head's, as a cold build at the same commit has it.
    let cold = engine().full_build_with("c1", &head_q).unwrap();
    let want = row(&cold, "@tsl//tsl/platform:dso_loader");
    assert_eq!(want.0, [2; 32]);
    assert_eq!(
        row(&head, "@tsl//tsl/platform:dso_loader"),
        want,
        "the overflow re-queried the indexed repository's package"
    );
    // And the rule whose hash moved is direct on this very step, so what
    // consumes it is reported.
    assert!(
        out.report
            .direct
            .iter()
            .any(|d| d == "@tsl//tsl/platform:dso_loader"),
        "{:?}",
        out.report.direct
    );
    let affected = out.report.affected_labels();
    assert!(affected.contains(&"//app:app".to_string()), "{affected:?}");

    // The next step edits the source the head added. Answered from a row
    // without the `y.cc` edge, it would reach nothing.
    let edit = vec![Change::new(
        Status::Modified,
        "third_party/tsl/tsl/platform/y.cc",
    )];
    let r = engine()
        .compute_with_texts(
            head.view(),
            &edit,
            &head_q,
            &req(),
            &FixedText(BTreeMap::new()),
        )
        .unwrap()
        .report;
    assert!(!r.global, "{:?}", r.global_reason);
    let affected = r.affected_labels();
    assert!(affected.contains(&"//app:app".to_string()), "{affected:?}");
}
