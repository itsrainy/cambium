//! A toolchain declaration moving is a whole-repository answer, wherever the
//! package is.
//!
//! Toolchain resolution happens in analysis and has no edge in the query
//! graph: nothing depends on a `toolchain()`, a `platform()` or a
//! `toolchain_type()` by label, so the reverse walk cannot reach the targets
//! one of them moving changes. The run reads the three classes off the splice
//! instead (`CAMBI-W023`), which is the rule the default invalidator globs
//! `tools/toolchains/**` and `platforms/**` used to stand in for. The globs
//! are gone, so a rule of any other class under those directories is an
//! ordinary rule now, which the last test pins. A `register_toolchains` /
//! `register_execution_platforms` call moving is the same gap and carries the
//! same code, under the wording the externals stage has always used.
//!
//! No Bazel anywhere: the engine's handle points at `/nonexistent/bazel`, and
//! the loading layer comes from the dump the in-memory tree would have
//! produced.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::FixedText;
use cambium_engine::{AffectedReport, Code, Diagnostic};
use cambium_index::{IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{base, req, Files, Sandbox, Scanning};

const MODULE_BASE: &str = "module(name = \"demo\")\nregister_toolchains(\"//tc:linux\")\n";

/// The tree behind the rows. `app` loads the `.bzl` under `tools/toolchains`,
/// so an edit there reaches a package the way any `.bzl` edit does.
fn files() -> Files {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| m.insert(k.to_string(), v.to_string());
    put("MODULE.bazel", MODULE_BASE);
    put("app/BUILD", "load(\"//tools/toolchains:cc.bzl\", \"X\")\n");
    put("plat/BUILD", "");
    put("tc/BUILD", "");
    put("tools/toolchains/BUILD", "");
    put("tools/toolchains/cc.bzl", "X = 1\n");
    put("types/BUILD", "");
    Files(m)
}

/// Which head a querier answers for.
#[derive(Clone, Copy)]
enum Head {
    Base,
    ToolchainAdded,
    PlatformChanged,
    TypeRemoved,
    OtherChanged,
}

/// ```text
/// //tc:cc (cc_toolchain) -> //tc:cfg (cc_toolchain_config)
/// //tc:linux (toolchain) -> //tc:cc                 [ToolchainAdded only]
/// //plat:linux (platform)                           [hash moves under PlatformChanged]
/// //types:lang (toolchain_type)                     [gone under TypeRemoved]
/// //tools/toolchains:cfg (cc_toolchain_config)      [hash moves under OtherChanged]
/// //app:app (cc_library)
/// ```
fn rows(pkg: &str, head: Head) -> PackageUpdate {
    let mut targets = match pkg {
        "tc" => vec![
            TargetRow::rule("//tc:cfg", "cc_toolchain_config", Vec::new()),
            TargetRow::rule("//tc:cc", "cc_toolchain", vec!["//tc:cfg".into()]),
        ],
        "plat" => vec![TargetRow::rule("//plat:linux", "platform", Vec::new())],
        "types" => vec![TargetRow::rule(
            "//types:lang",
            "toolchain_type",
            Vec::new(),
        )],
        "tools/toolchains" => vec![TargetRow::rule(
            "//tools/toolchains:cfg",
            "cc_toolchain_config",
            Vec::new(),
        )],
        "app" => vec![TargetRow::rule("//app:app", "cc_library", Vec::new())],
        _ => Vec::new(),
    };
    match (head, pkg) {
        (Head::ToolchainAdded, "tc") => targets.push(TargetRow::rule(
            "//tc:linux",
            "toolchain",
            vec!["//tc:cc".into()],
        )),
        (Head::PlatformChanged, "plat") => targets[0].rule_hash = [7; 32],
        (Head::TypeRemoved, "types") => targets.clear(),
        (Head::OtherChanged, "tools/toolchains") => targets[0].rule_hash = [7; 32],
        _ => {}
    }
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

fn base_rows(p: &str) -> PackageUpdate {
    rows(p, Head::Base)
}
fn toolchain_added(p: &str) -> PackageUpdate {
    rows(p, Head::ToolchainAdded)
}
fn platform_changed(p: &str) -> PackageUpdate {
    rows(p, Head::PlatformChanged)
}
fn type_removed(p: &str) -> PackageUpdate {
    rows(p, Head::TypeRemoved)
}
fn other_changed(p: &str) -> PackageUpdate {
    rows(p, Head::OtherChanged)
}

const PACKAGES: &[&str] = &["app", "plat", "tc", "tools/toolchains", "types"];

fn scanning(rows: fn(&str) -> PackageUpdate) -> Scanning {
    Scanning::new(PACKAGES, rows).with_files(files())
}

/// `MODULE.bazel` at the two revisions, and a `WORKSPACE` when a test says so.
fn texts(base_module: &str, head_module: &str) -> FixedText {
    let mut m = BTreeMap::new();
    m.insert(
        ("base".to_string(), "MODULE.bazel".to_string()),
        base_module.to_string(),
    );
    m.insert(
        ("head".to_string(), "MODULE.bazel".to_string()),
        head_module.to_string(),
    );
    FixedText(m)
}

fn with_workspace(mut t: FixedText, base_ws: &str, head_ws: &str) -> FixedText {
    t.0.insert(
        ("base".to_string(), "WORKSPACE".to_string()),
        base_ws.to_string(),
    );
    t.0.insert(
        ("head".to_string(), "WORKSPACE".to_string()),
        head_ws.to_string(),
    );
    t
}

fn unchanged() -> FixedText {
    texts(MODULE_BASE, MODULE_BASE)
}

/// The run over `changes`, answered by `head`'s rows, with the diagnostics it
/// raised.
fn run(
    e: &Sandbox,
    index: &IndexData,
    changes: &[Change],
    head: fn(&str) -> PackageUpdate,
    texts: &FixedText,
) -> (AffectedReport, Vec<Diagnostic>) {
    let r = e
        .compute_with_texts(index.view(), changes, &scanning(head), &req(), texts)
        .unwrap()
        .report;
    (r, e.diagnostics().drain())
}

fn modified(path: &str) -> Vec<Change> {
    vec![Change::new(Status::Modified, path)]
}

/// The run went global for the toolchain reason, and left behind the one
/// diagnostic `cambi explain` prints for it: the code, the reason verbatim at
/// the end of the message, and a help line. Returns the reason.
fn toolchain_cause(r: &AffectedReport, diags: &[Diagnostic]) -> String {
    assert!(r.global, "{r:?}");
    let reason = r
        .global_reason
        .clone()
        .expect("a global run carries its reason");
    let d = diags
        .iter()
        .find(|d| d.code == Code::W023ToolchainMoved)
        .unwrap_or_else(|| panic!("no CAMBI-W023 among {diags:?}"));
    assert!(d.message.ends_with(&reason), "{}", d.message);
    assert!(d.help.is_some(), "{d:?}");
    assert!(
        !diags.iter().any(|d| matches!(
            d.code,
            Code::W013InvalidatorMatched | Code::W014ExternalsGlobal
        )),
        "one cause, under one code: {diags:?}"
    );
    reason
}

#[test]
fn a_toolchain_rule_added_is_global() {
    let (e, index) = base(&scanning(base_rows));
    let (r, diags) = run(
        &e,
        &index,
        &modified("tc/BUILD"),
        toolchain_added,
        &unchanged(),
    );
    let reason = toolchain_cause(&r, &diags);
    assert!(
        reason.starts_with("//tc:linux (toolchain) was added;"),
        "{reason}"
    );
}

#[test]
fn a_platform_rule_changed_is_global() {
    let (e, index) = base(&scanning(base_rows));
    let (r, diags) = run(
        &e,
        &index,
        &modified("plat/BUILD"),
        platform_changed,
        &unchanged(),
    );
    let reason = toolchain_cause(&r, &diags);
    assert!(
        reason.starts_with("//plat:linux (platform) was changed;"),
        "{reason}"
    );
}

/// Gone from a package that still exists: the re-query returned the package
/// without it, and the splice tombstoned it. Its class is read off the base
/// index, since a tombstone has none.
#[test]
fn a_toolchain_type_rule_removed_is_global() {
    let (e, index) = base(&scanning(base_rows));
    let (r, diags) = run(
        &e,
        &index,
        &modified("types/BUILD"),
        type_removed,
        &unchanged(),
    );
    let reason = toolchain_cause(&r, &diags);
    assert!(
        reason.starts_with("//types:lang (toolchain_type) was removed;"),
        "{reason}"
    );
}

/// Gone with its package: the `BUILD` file was deleted, so the package was
/// never re-queried and every rule in it was tombstoned by the deletion.
#[test]
fn a_deleted_package_that_declared_a_toolchain_type_is_global() {
    let (e, index) = base(&scanning(base_rows));
    let mut head_tree = files();
    head_tree.0.remove("types/BUILD");
    let head =
        Scanning::new(&["app", "plat", "tc", "tools/toolchains"], base_rows).with_files(head_tree);
    let r = e
        .compute_with_texts(
            index.view(),
            &[Change::new(Status::Deleted, "types/BUILD")],
            &head,
            &req(),
            &unchanged(),
        )
        .unwrap()
        .report;
    let diags = e.diagnostics().drain();
    let reason = toolchain_cause(&r, &diags);
    assert!(
        reason.starts_with("//types:lang (toolchain_type) was removed;"),
        "{reason}"
    );
}

/// The same gap from the other side: the declaration stayed put and the
/// registration that offers it moved. Same code; the wording is the one the
/// externals stage has always reported, so the benchmark's reason histogram
/// keeps its bucket.
#[test]
fn a_moved_register_toolchains_call_carries_the_toolchain_code() {
    let (e, index) = base(&scanning(base_rows));
    let head_module = MODULE_BASE.replace("//tc:linux", "//tc:other");
    let (r, diags) = run(
        &e,
        &index,
        &modified("MODULE.bazel"),
        base_rows,
        &texts(MODULE_BASE, &head_module),
    );
    let reason = toolchain_cause(&r, &diags);
    assert_eq!(
        reason,
        "`register_toolchains` / `register_execution_platforms` changed; toolchain resolution \
         has no edge in the query graph, so no reverse walk can find what it moves"
    );
    let cause = diags
        .iter()
        .find(|d| d.code == Code::W023ToolchainMoved)
        .unwrap();
    assert_eq!(
        cause.location.as_ref().map(|l| l.path.as_str()),
        Some("MODULE.bazel")
    );
}

/// A legacy `WORKSPACE` registers through a rules-provided macro; the
/// statement diff names it, and the run codes it the same way.
#[test]
fn a_moved_workspace_registration_carries_the_toolchain_code() {
    let (e, index) = base(&scanning(base_rows));
    let ws = |version: &str| {
        format!(
            "load(\"@rules_go//go:deps.bzl\", \"go_register_toolchains\")\n\
             go_register_toolchains(version = \"{version}\")\n"
        )
    };
    let (r, diags) = run(
        &e,
        &index,
        &modified("WORKSPACE"),
        base_rows,
        &with_workspace(unchanged(), &ws("1"), &ws("2")),
    );
    let reason = toolchain_cause(&r, &diags);
    assert!(
        reason.starts_with("`WORKSPACE`: `go_register_toolchains(") && reason.contains("toolchain"),
        "{reason}"
    );
    let cause = diags
        .iter()
        .find(|d| d.code == Code::W023ToolchainMoved)
        .unwrap();
    assert_eq!(
        cause.location.as_ref().map(|l| l.path.as_str()),
        Some("WORKSPACE")
    );
}

/// The globs are gone. A rule of another class under `tools/toolchains/` is
/// an ordinary rule: its own hash moving makes it direct and nothing else,
/// and a `.bzl` edit there reaches the packages that load it, as any `.bzl`
/// edit does, and goes no wider.
#[test]
fn another_class_under_tools_toolchains_is_no_longer_global() {
    let (e, index) = base(&scanning(base_rows));

    let (r, diags) = run(
        &e,
        &index,
        &modified("tools/toolchains/BUILD"),
        other_changed,
        &unchanged(),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert!(
        !diags.iter().any(|d| matches!(
            d.code,
            Code::W013InvalidatorMatched | Code::W023ToolchainMoved
        )),
        "{diags:?}"
    );
    assert_eq!(r.direct, vec!["//tools/toolchains:cfg".to_string()]);
    assert_eq!(
        r.affected_labels(),
        vec!["//tools/toolchains:cfg".to_string()]
    );

    let (r, diags) = run(
        &e,
        &index,
        &modified("tools/toolchains/cc.bzl"),
        base_rows,
        &unchanged(),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert!(
        !diags.iter().any(|d| matches!(
            d.code,
            Code::W013InvalidatorMatched | Code::W023ToolchainMoved
        )),
        "{diags:?}"
    );
    assert!(
        r.direct.contains(&"//app:app".to_string()),
        "the loader's rules are direct, by the layer's BZL_LOAD edge: {:?}",
        r.direct
    );
    assert!(
        !r.affected_labels()
            .iter()
            .any(|l| l.starts_with("//plat:") || l.starts_with("//types:")),
        "nothing outside the loader is reached: {:?}",
        r.affected_labels()
    );
}
