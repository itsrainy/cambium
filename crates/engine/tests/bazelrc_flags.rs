//! A `.bazelrc` edit that flips a Starlark build setting reaches what reads the
//! setting, not the world.
//!
//! The graph is the shape the feature exists for:
//!
//! ```text
//! //bazel:crypto            bool_flag
//! //bazel:crypto_boringssl  config_setting(flag_values = {"//bazel:crypto": ...})
//! //lib:tls                 cc_library that select()s on it
//! //app:app                 depends on //lib:tls
//! //lib:plain               cc_library that does not
//! ```
//!
//! Real git (the engine reads both sides of the rc file with `git show`), rows
//! and Starlark from a querier, and a Bazel binary that does not exist.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::{FixedText, WorkspaceText};
use cambium_engine::{AffectedRequest, Config, Engine};
use cambium_index::{PackageUpdate, TargetRow};
use cambium_testkit::{
    commit_all, init_repo, scratch, synthetic_engine, write, Files, Scanning, TempDir,
};

const MODULE: &str = "module(name = \"envoy\", version = \"1\")\n";

/// The `.bzl` every BUILD file loads. `transition` is what test two adds.
const DEFS: &str = "def library(**kwargs):\n    pass\n";

const DEFS_WITH_TRANSITION: &str = "def _impl(settings, attr):\n    return settings\n\
     crypto_transition = transition(\n    implementation = _impl,\n\
     \x20   inputs = [\"@envoy//bazel:crypto\"],\n    outputs = [],\n)\n";

fn files(defs: &str) -> Files {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| m.insert(k.to_string(), v.to_string());
    put("MODULE.bazel", MODULE);
    put("bazel/defs.bzl", defs);
    put("bazel/BUILD", "load(\"//bazel:defs.bzl\", \"library\")\n");
    put("lib/BUILD", "load(\"//bazel:defs.bzl\", \"library\")\n");
    put("app/BUILD", "load(\"//bazel:defs.bzl\", \"library\")\n");
    put("q/BUILD", "load(\"//bazel:defs.bzl\", \"library\")\n");
    Files(m)
}

fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        // The setting itself, and the config_setting whose `flag_values`
        // names it — a label-keyed attribute, so `bazel query` writes the
        // edge as a rule input.
        "bazel" => vec![
            TargetRow::rule("//bazel:crypto", "bool_flag", Vec::new()),
            TargetRow::rule("//bazel:mode", "string_flag", Vec::new()),
            TargetRow::rule(
                "//bazel:crypto_boringssl",
                "config_setting",
                vec!["//bazel:crypto".into()],
            ),
        ],
        // `//lib:tls` selects on the config_setting; `//lib:plain` does not.
        "lib" => vec![
            TargetRow::rule(
                "//lib:tls",
                "cc_library",
                vec!["//bazel:crypto_boringssl".into()],
            ),
            TargetRow::rule("//lib:plain", "cc_library", Vec::new()),
        ],
        // A main-repo rule naming a target in an external repository: the
        // shape an external Starlark setting is resolved through.
        "q" => vec![TargetRow::rule(
            "//q:q",
            "cc_library",
            vec!["@@quiche+//:ssl_lib".into()],
        )],
        "app" => vec![TargetRow::rule(
            "//app:app",
            "cc_library",
            vec!["//lib:tls".into()],
        )],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["bazel", "lib", "app", "q"];

const REPO_MAPPING: &str = "{\"\":\"\",\"envoy\":\"\",\"quiche\":\"quiche+\"}";

fn scanning(defs: &str) -> Scanning {
    Scanning::new(PACKAGES, rows)
        .with_files(files(defs))
        .with_repo_mapping(REPO_MAPPING)
}

/// A repository whose `.bazelrc` says `base_rc` and then `head_rc`, with the
/// engine over it and the two shas.
fn fixture(base_rc: &str, head_rc: &str) -> (TempDir, Engine, String, String) {
    let dir = scratch();
    let root = dir.path().to_path_buf();
    init_repo(&root);
    write(&root, "MODULE.bazel", MODULE);
    write(&root, ".bazelrc", base_rc);
    let base = commit_all(&root, "base");
    write(&root, ".bazelrc", head_rc);
    let head = commit_all(&root, "head");
    let engine = synthetic_engine(&root, Config::default());
    (dir, engine, base, head)
}

fn texts(base: &str, head: &str) -> FixedText {
    let mut m = BTreeMap::new();
    for rev in [base, head] {
        m.insert(
            (rev.to_string(), "MODULE.bazel".to_string()),
            MODULE.to_string(),
        );
    }
    FixedText(m)
}

/// Builds the base index and answers the `.bazelrc` diff over it.
fn run(base_rc: &str, head_rc: &str, defs: &str) -> cambium_engine::AffectedReport {
    let (_d, engine, base, head) = fixture(base_rc, head_rc);
    let index = engine
        .full_build_with(&base, &scanning(defs))
        .unwrap();
    let req = AffectedRequest {
        materialize: true,
        no_full_build: true,
        include_manual: true,
        ..AffectedRequest::new(&base, &head)
    };
    let changes = vec![Change::new(Status::Modified, ".bazelrc")];
    let t = texts(&base, &head);
    engine
        .compute_with_texts(
            index.view(),
            &changes,
            &scanning(defs),
            &req,
            &t as &dyn WorkspaceText,
        )
        .unwrap()
        .report
}

#[test]
fn flipping_a_setting_reaches_only_what_reads_it() {
    let r = run(
        "build --jobs=8\nbuild --@envoy//bazel:crypto=openssl\n",
        "build --jobs=8\nbuild --@envoy//bazel:crypto=boringssl\n",
        DEFS,
    );
    assert!(!r.global, "{:?}", r.global_reason);
    // A flag value changes no rule's definition, so nothing is re-queried.
    assert!(r.dirty_packages.is_empty(), "{:?}", r.dirty_packages);
    assert_eq!(r.stats.bazelrc_flag_nodes, 1);
    assert_eq!(r.bazelrc_flags, vec!["@envoy//bazel:crypto".to_string()]);
    // The config_setting is direct; the library that selects on it and the
    // binary above that follow from the ordinary reverse walk. `//lib:plain`
    // does not.
    assert_eq!(r.direct, vec!["//bazel:crypto_boringssl".to_string()]);
    assert_eq!(
        r.affected_labels(),
        vec![
            "//app:app".to_string(),
            "//bazel:crypto_boringssl".to_string(),
            "//lib:tls".to_string(),
        ]
    );
}

#[test]
fn the_main_repositorys_own_name_is_the_same_setting_as_a_bare_label() {
    let r = run(
        "build --//bazel:crypto=openssl\n",
        "build --//bazel:crypto=boringssl\n",
        DEFS,
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.bazelrc_flag_nodes, 1);
    assert!(r.affected_labels().contains(&"//lib:tls".to_string()));
}

#[test]
fn a_bzl_that_names_the_setting_keeps_the_global() {
    let r = run(
        "build --@envoy//bazel:crypto=openssl\n",
        "build --@envoy//bazel:crypto=boringssl\n",
        DEFS_WITH_TRANSITION,
    );
    assert!(r.global);
    let why = r.global_reason.clone().unwrap();
    assert!(why.contains("bazel/defs.bzl"), "{why}");
    assert!(why.contains("@envoy//bazel:crypto"), "{why}");
    assert_eq!(r.stats.bazelrc_flag_nodes, 0);
}

#[test]
fn a_setting_scoped_to_a_config_keeps_the_global() {
    let r = run(
        "build:ci --@envoy//bazel:crypto=openssl\n",
        "build:ci --@envoy//bazel:crypto=boringssl\n",
        DEFS,
    );
    assert!(r.global);
    let why = r.global_reason.clone().unwrap();
    assert!(why.contains("--config"), "{why}");
}

#[test]
fn a_setting_no_rule_names_affects_nothing() {
    let r = run(
        "build --//bazel:unused=1\n",
        "build --//bazel:unused=2\n",
        DEFS,
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.bazelrc_flag_nodes, 0);
    assert_eq!(r.bazelrc_flags, vec!["//bazel:unused".to_string()]);
    assert!(r.affected.is_empty(), "{:?}", r.affected);
}

#[test]
fn a_boolean_setting_written_as_a_negation_is_the_same_setting() {
    let r = run(
        "build --//bazel:crypto\n",
        "build --no//bazel:crypto\n",
        DEFS,
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.bazelrc_flag_nodes, 1);
    assert!(r.affected_labels().contains(&"//lib:tls".to_string()));
}

#[test]
fn a_setting_in_an_external_repository_keeps_the_global() {
    // `@quiche//:ssl_lib` is a target in a repository the index does not
    // cover: the config_settings and transitions that read it live there, and
    // the main-repo rules that depend on them do so through ordinary targets.
    // A main-repo rule naming the setting (`//q:q` does) is only part of the
    // picture, so the run stays global and says why.
    let r = run(
        "build --@quiche//:ssl_lib=@envoy//bazel:ssl\n",
        "build --@quiche//:ssl_lib=@envoy//bazel:boring\n",
        DEFS,
    );
    assert!(r.global, "{:?}", r.bazelrc_flags);
    let why = r.global_reason.clone().unwrap();
    assert!(why.contains("quiche"), "{why}");
    assert!(why.contains("external repository"), "{why}");
    assert_eq!(r.stats.bazelrc_flag_nodes, 0);
}
