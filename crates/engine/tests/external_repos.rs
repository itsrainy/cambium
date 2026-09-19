//! An edit that only matters through an external repository invalidates the
//! targets that depend on that repository, not the world.
//!
//! Three shapes, all driven with no Bazel process (the engine's handle points
//! at `/nonexistent/bazel`, so shelling out would fail the test rather than
//! pass it quietly):
//!
//! - a main-repo file a repository rule reads by label (`config.yml`,
//!   `data/**`) — the `@envoy_repo` shape;
//! - a `MODULE.bazel` dependency bump;
//! - a `.bzl` that defines a repository rule.
//!
//! plus a `MODULE.bazel.lock` and a legacy `WORKSPACE` that move on their own,
//! and the escalations that must survive: a toolchain-registering module, a
//! module Bazel 8 autoloads native rules from, a lockfile shape the parser
//! does not know, and a `WORKSPACE` statement it cannot classify.

use std::collections::BTreeMap;

use cambium_diff::{Change, Status};
use cambium_engine::externals::{FixedText, WorkspaceText};
use cambium_engine::{AffectedRequest, Code, Config};
use cambium_index::{IndexData, PackageUpdate, TargetRow};
use cambium_testkit::{base, engine, engine_with, req, Files, Sandbox, Scanning};

/// The base `MODULE.bazel`.
///
/// `gen` is a repository the main repository defines itself, in
/// `//bazel:ext.bzl`; `fmt` is an ordinary dependency; `rules_python`
/// registers toolchains.
const MODULE_BASE: &str = "module(name = \"demo\", version = \"1\")\n\
     bazel_dep(name = \"fmt\", version = \"1.0\")\n\
     bazel_dep(name = \"rules_python\", version = \"1.0\")\n\
     gen = use_extension(\"//bazel:ext.bzl\", \"gen\")\n\
     use_repo(gen, \"generated\")\n";

fn files() -> Files {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| m.insert(k.to_string(), v.to_string());
    put("MODULE.bazel", MODULE_BASE);
    // The repository rule's implementation, reached from MODULE.bazel through
    // `use_extension` and one `load()`. Both of its main-repo labels are
    // inputs of `@generated`.
    put("bazel/ext.bzl", "load(\"//bazel:repo.bzl\", \"r\")\n");
    // The repository rule proper: three labels it resolves, all inside the
    // declaration, and one call that names the repository they feed.
    // The BUILD file the rule *writes*: its `genrule` names a main-repo rule
    // target (`@demo//changelogs`, a filegroup over a glob), which is the
    // `@envoy_repo` shape — the repository's generated targets depend on it,
    // but the rule never reads it. The visibility spec and the
    // `config_setting` key beside it must yield no path pattern wider than the
    // file they literally name (issue #8).
    put(
        "bazel/repo.bzl",
        "def _impl(ctx):\n\
         \x20   ctx.file(\"BUILD\", '''\n\
         genrule(\n\
         \x20   name = \"project\",\n\
         \x20   tools = [\"@demo//changelogs\"],\n\
         \x20   visibility = [\"//some/team:__subpackages__\"],\n\
         )\n\
         config_setting(name = \"c\", flag_values = {\"//build:some_flag\": \"1\"})\n\
         ''')\n\
         \n\
         _gen = repository_rule(\n\
         \x20   implementation = _impl,\n\
         \x20   attrs = {\n\
         \x20       \"config\": attr.label(default = \"@demo//:config.yml\"),\n\
         \x20       \"tpl\": attr.label(default = \"@demo//tpl:helper.bzl\"),\n\
         \x20       \"data\": attr.label(default = \"@demo//data\"),\n\
         \x20   },\n\
         )\n\
         \n\
         def r():\n\
         \x20   _gen(name = \"generated\")\n",
    );
    put("app/BUILD", "load(\"//bazel:defs.bzl\", \"r\")\n");
    put("lib/BUILD", "load(\"//bazel:defs.bzl\", \"r\")\n");
    // Issue #4: a BUILD file that loads a dependency's `.bzl` *directly*,
    // with no main-repo `.bzl` in between. Its rules can change when `fmt`
    // does, even though nothing in it names a `@fmt` label.
    put("direct/BUILD", "load(\"@fmt//:defs.bzl\", \"f\")\n");
    // Both packages reach a `.bzl` inside `@generated` through this file, so
    // rule (2) has somewhere to land.
    put(
        "bazel/defs.bzl",
        "load(\"@generated//:defs.bzl\", \"g\")\nR = 1\n",
    );
    put("bazel/BUILD", "");
    put("changelogs/BUILD", "");
    Files(m)
}

/// ```text
/// //app:app -> //lib:lib, @@fmt+//:fmt
/// //lib:lib -> @generated//:thing
/// ```
fn rows(pkg: &str) -> PackageUpdate {
    let targets = match pkg {
        "app" => vec![TargetRow::rule(
            "//app:app",
            "demo_lib",
            vec!["//lib:lib".into(), "@@fmt+//:fmt".into()],
        )],
        "lib" => vec![TargetRow::rule(
            "//lib:lib",
            "demo_lib",
            vec!["@generated//:thing".into()],
        )],
        // The shape `bazel/bazel` uses all over `tools/`: the very `.bzl` that
        // the workspace closure reaches is *also* listed as a source of a
        // filegroup. Editing it must dirty this filegroup, on top of whatever
        // the repository it defines does elsewhere.
        "bazel" => vec![
            TargetRow::source_file("//bazel:repo.bzl"),
            TargetRow::rule(
                "//bazel:bzl_srcs",
                "filegroup",
                vec!["//bazel:repo.bzl".into()],
            ),
            TargetRow::rule(
                "//bazel:embedded_tools",
                "filegroup",
                vec!["//bazel:bzl_srcs".into()],
            ),
        ],
        "direct" => vec![TargetRow::rule("//direct:d", "demo_lib", Vec::new())],
        // `@envoy//changelogs`: a filegroup over `glob(["current/*.rst"])`.
        // Which files it globs is not knowable from the label, and is exactly
        // what these edges say.
        "changelogs" => vec![
            TargetRow::source_file("//changelogs:current/x.rst"),
            TargetRow::source_file("//changelogs:notes.txt"),
            TargetRow::rule(
                "//changelogs:changelogs",
                "filegroup",
                vec!["//changelogs:current/x.rst".into()],
            ),
        ],
        _ => Vec::new(),
    };
    PackageUpdate {
        package: pkg.to_string(),
        targets,
        ..Default::default()
    }
}

const PACKAGES: &[&str] = &["app", "lib", "bazel", "direct", "changelogs"];

/// What `bazel mod dump_repo_mapping ""` prints for this workspace: the
/// root module's own name maps to the empty canonical name (so `@demo//`
/// is the main repository), and `fmt` carries the `+` suffix Bazel 8 and 9
/// give a canonical name.
const REPO_MAPPING: &str =
    "{\"\":\"\",\"demo\":\"\",\"fmt\":\"fmt+\",\"generated\":\"generated+\"}";

fn scanning() -> Scanning {
    Scanning::new(PACKAGES, rows)
        .with_files(files())
        .with_repo_mapping(REPO_MAPPING)
}

/// `MODULE.bazel` unchanged between the two revisions unless a test says so.
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

/// The same two revisions, with `.bazelversion` pinned at the base. The
/// `Scanning` querier records no Bazel version in the index header, so the
/// pin is what decides whether the tree's Bazel autoloads native rules.
fn with_bazel(mut t: FixedText, version: &str) -> FixedText {
    t.0.insert(
        ("base".to_string(), ".bazelversion".to_string()),
        version.to_string(),
    );
    t
}

fn run(
    index: &IndexData,
    changes: &[Change],
    texts: &dyn WorkspaceText,
) -> cambium_engine::AffectedReport {
    run_on(&engine(), index, changes, texts)
}

/// As [`run`], on an engine the test holds, so its diagnostics can be read.
fn run_on(
    e: &Sandbox,
    index: &IndexData,
    changes: &[Change],
    texts: &dyn WorkspaceText,
) -> cambium_engine::AffectedReport {
    e.compute_with_texts(index.view(), changes, &scanning(), &req(), texts)
        .unwrap()
        .report
}

/// The one diagnostic that explains a global run, with the code asserted.
/// One: `MODULE.bazel` is a default invalidator as well, and a run whose
/// externals stage escalated on it reports the stage's cause alone.
fn cause(e: &Sandbox, code: Code) -> cambium_engine::Diagnostic {
    let diagnostics = e.diagnostics().drain();
    let causes: Vec<&cambium_engine::Diagnostic> = diagnostics
        .iter()
        .filter(|d| cambium_engine::GlobalReason::is_cause(d.code))
        .collect();
    let [cause] = causes[..] else {
        panic!("a global run names its cause once: {diagnostics:?}");
    };
    assert_eq!(cause.code, code, "{cause:?}");
    assert!(cause.help.is_some(), "{cause:?}");
    cause.clone()
}

fn changed(paths: &[&str]) -> Vec<Change> {
    paths
        .iter()
        .map(|p| Change::new(Status::Modified, *p))
        .collect()
}

fn affected(r: &cambium_engine::AffectedReport) -> Vec<String> {
    r.affected_labels()
}

#[test]
fn the_index_stores_what_feeds_each_repository() {
    let (_e, index) = base(&scanning());
    let v = index.view();
    let repos: Vec<&str> = (0..v.num_repos()).map(|r| v.repo_name(r)).collect();
    assert_eq!(repos, ["generated"]);
    let named = |p: &str| -> Vec<&str> {
        v.repos_for_path(p)
            .into_iter()
            .map(|r| v.repo_name(r))
            .collect()
    };
    let named_label = |l: &str| -> Vec<&str> {
        v.repos_for_label_kinds(l)
            .into_iter()
            .map(|(r, _)| v.repo_name(r))
            .collect()
    };
    assert_eq!(named("config.yml"), ["generated"]);
    // `@demo//data` is `//data:data`: the file that label names, and the
    // target it names. Never `data/**` — issue #8.
    assert_eq!(named("data/data"), ["generated"]);
    assert_eq!(named_label("//data:data"), ["generated"]);
    assert!(named("data/thing.txt").is_empty());
    assert!(named("app/main.cc").is_empty());

    // A visibility token names no file and creates no edge, so it yields
    // nothing at all...
    assert!(named("some/team/anything/at/all").is_empty());
    assert!(named_label("//some/team:__subpackages__").is_empty());
    // ...and a `config_setting` key yields the target and the file it names,
    // and nothing that claims `build/**`.
    assert!(named("build/anything/at/all").is_empty());
    assert_eq!(named_label("//build:some_flag"), ["generated"]);
    assert_eq!(named("build/some_flag"), ["generated"]);
    // Nothing in the table is a subtree prefix any more.
    let prefixes: Vec<&str> = (0..v.repo_input_patterns.len() as u32)
        .filter(|&p| v.repo_input_prefix[p as usize])
        .map(|p| v.repo_input_patterns.get(p))
        .collect();
    assert!(prefixes.is_empty(), "{prefixes:?}");
    // The repository rule's own `.bzl` files name the repository they define,
    // which is what stops a change to them from being global.
    let repo_bzl = v.bzl_id("bazel/repo.bzl").unwrap();
    assert!(v.bzl_in_workspace(repo_bzl));
    assert_eq!(
        v.bzl_repos_of(repo_bzl)
            .iter()
            .map(|&r| v.repo_name(r))
            .collect::<Vec<_>>(),
        ["generated"]
    );
}

#[test]
fn a_repository_rule_input_reaches_only_the_repositorys_consumers() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["config.yml"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    // `//lib:lib` names `@generated//:thing`; `//app:app` reaches it by one hop.
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
    assert_eq!(
        affected(&r),
        vec!["//app:app".to_string(), "//lib:lib".to_string()]
    );
}

/// A `.bzl` the rule resolves is named *inside the call that names the
/// repository*, so per-occurrence attribution lands it on `@generated` — and
/// because a repository rule may write anything it reads into a `.bzl` of its
/// own, the packages whose load closure reaches into that repository are
/// re-queried. Narrowing attribution must not lose that edge.
#[test]
fn a_bzl_named_inside_a_repository_rules_call_dirties_its_loaders() {
    let (_e, index) = base(&scanning());
    let v = index.view();
    assert_eq!(
        v.repos_for_path("tpl/helper.bzl")
            .into_iter()
            .map(|r| v.repo_name(r))
            .collect::<Vec<_>>(),
        ["generated"]
    );
    let r = run(
        &index,
        &changed(&["tpl/helper.bzl"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.stats.repos_label_only, 0);
    assert!(
        r.dirty_packages.contains(&"app".to_string())
            && r.dirty_packages.contains(&"lib".to_string()),
        "{:?}",
        r.dirty_packages
    );
}

/// `attr.label(default = "@demo//data")` is the label `//data:data`, and a
/// repository rule can only ever *read* it — as the file `data/data`, since
/// `repository_ctx.path` rejects a rule target. An arbitrary other file in
/// that directory is not that file, and claiming `data/**` for it is issue #8.
#[test]
fn a_package_label_is_the_file_it_names_and_not_its_directory() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["data/thing.txt"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_input, 0);
    assert!(r.direct.is_empty(), "{:?}", r.direct);

    // The file the label does name still feeds the repository.
    let r = run(
        &index,
        &changed(&["data/data"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert_eq!(r.stats.repos_dirty_by_input, 1);
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
}

#[test]
fn an_unrelated_file_reaches_no_repository() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["notes.txt"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global);
    assert_eq!(r.stats.repos_dirty_by_input, 0);
    assert!(r.direct.is_empty());
}

#[test]
fn a_bazel_dep_bump_reaches_only_that_dependencys_consumers() {
    let (_e, index) = base(&scanning());
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"fmt\", version = \"1.0\")",
        "bazel_dep(name = \"fmt\", version = \"2.0\")",
    );
    let r = run(
        &index,
        &changed(&["MODULE.bazel"]),
        &texts(MODULE_BASE, &head),
    );
    assert!(
        !r.global,
        "MODULE.bazel is a default invalidator, but the stage handled it: {:?}",
        r.global_reason
    );
    assert_eq!(r.stats.repos_dirty_by_module, 1);
    // Only `//app:app` names `@@fmt+//:fmt`, and nothing depends on it.
    assert_eq!(r.direct, vec!["//app:app".to_string()]);
    assert_eq!(affected(&r), vec!["//app:app".to_string()]);
}

#[test]
fn a_build_files_direct_external_load_is_seen_by_rule_a() {
    // Issue #4. `//direct` loads `@fmt//:defs.bzl` from its BUILD file, so a
    // `fmt` bump must re-query it: the macro it loads can now emit different
    // rules. `//app` and `//lib` load only main-repo `.bzl` files and stay put.
    let (_e, index) = base(&scanning());
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"fmt\", version = \"1.0\")",
        "bazel_dep(name = \"fmt\", version = \"2.0\")",
    );
    let r = run(
        &index,
        &changed(&["MODULE.bazel"]),
        &texts(MODULE_BASE, &head),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(
        r.dirty_packages,
        vec!["direct".to_string()],
        "rule (a) must see a BUILD file's own external load"
    );
    // The re-query returned the same rows, so the label consumer is still the
    // only affected target.
    assert_eq!(affected(&r), vec!["//app:app".to_string()]);
}

#[test]
fn a_canonical_name_matches_the_apparent_one() {
    // `@@fmt+//:fmt` is how Bazel 8 and 9 spell it; the module names `fmt`.
    let (_e, index) = base(&scanning());
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"fmt\", version = \"1.0\")",
        "bazel_dep(name = \"fmt\", version = \"9.9\")",
    );
    let r = run(
        &index,
        &changed(&["MODULE.bazel"]),
        &texts(MODULE_BASE, &head),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.direct, vec!["//app:app".to_string()]);
}

/// On Bazel 7 the reason a `rules_python` bump is global is the toolchains it
/// registers; on 8 the autoload reason below is the more specific one.
#[test]
fn a_toolchain_module_bump_is_global() {
    let (e, index) = base(&scanning());
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"rules_python\", version = \"1.0\")",
        "bazel_dep(name = \"rules_python\", version = \"2.0\")",
    );
    let r = run_on(
        &e,
        &index,
        &changed(&["MODULE.bazel"]),
        &with_bazel(texts(MODULE_BASE, &head), "7.4.1"),
    );
    assert!(r.global);
    let reason = r.global_reason.unwrap();
    assert!(
        reason.contains("rules_python") && reason.contains("toolchain"),
        "{reason}"
    );
    cause(&e, Code::W014ExternalsGlobal);
}

/// Issue #25. `protobuf` is an ordinary dependency, and on Bazel 8 it is also
/// where a bare `proto_library` comes from, through a load no BUILD file
/// writes: the graph has no edge from the packages using it, so a bump is
/// global, under a code of its own. The `bazel` subject's nearest step,
/// `3c82b1a2..a3db5085` ("Update bazelbuild/bazel's rules_cc to version
/// 0.2.19", `.bazelversion` 9.1.1), is not this variant: it moves
/// `bazel_features` and `rules_cc` in `MODULE.bazel`, the first localises,
/// and the second is global for the toolchains `rules_cc` registers
/// (`CAMBI-W014`). Two reasons, either of which is enough: Bazel 8 keeps
/// `cc_library` in its own builtins and autoloads nothing from `rules_cc`,
/// and Bazel 9 (which that step pins) disabled autoloads altogether, so
/// every rule `@rules_cc` gives a package arrives through a `load()` the
/// package writes.
#[test]
fn an_autoload_provider_bump_is_global_on_bazel_8() {
    let (e, index) = base(&scanning());
    let module = format!("{MODULE_BASE}bazel_dep(name = \"protobuf\", version = \"29.0\")\n");
    let head = module.replace("29.0", "30.0");
    let r = run_on(
        &e,
        &index,
        &changed(&["MODULE.bazel"]),
        &with_bazel(texts(&module, &head), "8.4.2"),
    );
    assert!(r.global);
    let reason = r.global_reason.clone().unwrap();
    assert!(
        reason.contains("protobuf") && reason.contains("autoloads"),
        "{reason}"
    );
    let d = cause(&e, Code::W024AutoloadProvider);
    assert!(d.message.ends_with(&reason), "{d:?}");
    assert!(d.help.unwrap().contains("Bazel 8"));

    // A version bump is the same move whichever file carries it: the
    // lockfile re-resolving `protobuf` under an unchanged `MODULE.bazel`.
    let lock = |h: &str| {
        format!(
            "{{\"lockFileVersion\": 24, \"selectedYankedVersions\": {{}}, \
             \"moduleExtensions\": {{}}, \"registryFileHashes\": {{\
             \"https://r/modules/protobuf/29.0/source.json\": \"{h}\"}}}}"
        )
    };
    let r = run_on(
        &e,
        &index,
        &changed(&["MODULE.bazel.lock"]),
        &with_bazel(texts_with_lock(&lock("a"), &lock("b")), "8.4.2"),
    );
    assert!(r.global, "{:?}", r.global_reason);
    assert!(r.global_reason.unwrap().contains("autoloads"));
    cause(&e, Code::W024AutoloadProvider);
}

/// Under Bazel 7 nothing is autoloaded, and the same bump reaches only the
/// consumers of `@protobuf`, of which this graph has none.
#[test]
fn an_autoload_provider_is_an_ordinary_dependency_on_bazel_7() {
    let (_e, index) = base(&scanning());
    let module = format!("{MODULE_BASE}bazel_dep(name = \"protobuf\", version = \"29.0\")\n");
    let head = module.replace("29.0", "30.0");
    let r = run(
        &index,
        &changed(&["MODULE.bazel"]),
        &with_bazel(texts(&module, &head), "7.6.1"),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_module, 1);
    assert!(r.direct.is_empty(), "{:?}", r.direct);
}

/// No `.bazelversion` and no version in the index header: the Bazel could be
/// 8, so the answer is the one that over-reports. `rules_python` here would
/// have been global for its toolchains anyway; the code says which reason.
#[test]
fn a_provider_bump_under_an_unknown_bazel_version_is_read_as_bazel_8() {
    let (e, index) = base(&scanning());
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"rules_python\", version = \"1.0\")",
        "bazel_dep(name = \"rules_python\", version = \"2.0\")",
    );
    let r = run_on(
        &e,
        &index,
        &changed(&["MODULE.bazel"]),
        &texts(MODULE_BASE, &head),
    );
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("autoloads"));
    cause(&e, Code::W024AutoloadProvider);
}

/// Bazel 9 autoloads nothing at all: it disabled autoloads and its
/// `exports.bzl` defines no rule to autoload, so every `py_binary` is an
/// explicit `load()` with an edge, and a provider bump is as ordinary as on
/// 7.
#[test]
fn a_provider_bump_on_bazel_9_is_an_ordinary_dependency() {
    let (_e, index) = base(&scanning());
    let module = format!("{MODULE_BASE}bazel_dep(name = \"protobuf\", version = \"29.0\")\n");
    let head = module.replace("29.0", "30.0");
    let r = run(
        &index,
        &changed(&["MODULE.bazel"]),
        &with_bazel(texts(&module, &head), "9.2.0"),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_module, 1);
}

/// The same two revisions, with a `.bazelrc` at the base.
fn with_bazelrc(mut t: FixedText, text: &str) -> FixedText {
    t.0.insert(
        ("base".to_string(), ".bazelrc".to_string()),
        text.to_string(),
    );
    t
}

/// `--incompatible_autoload_externally` in `.bazelrc` decides over the
/// version where Bazel reads the flag: empty turns autoload off on 8, which
/// is what the diagnostic's help says to do, and a value naming a provider
/// turns it on for a 7.4 tree opting in early. Bazel 9 does not read it:
/// autoloads are disabled there, so a line left over from an 8 tree leaves
/// the bump localised.
#[test]
fn the_bazelrc_flag_decides_autoload_where_bazel_reads_it() {
    let (e, index) = base(&scanning());
    let module = format!("{MODULE_BASE}bazel_dep(name = \"protobuf\", version = \"29.0\")\n");
    let head = module.replace("29.0", "30.0");
    let off = with_bazelrc(
        with_bazel(texts(&module, &head), "8.4.2"),
        "common --incompatible_autoload_externally=\n",
    );
    let r = run(&index, &changed(&["MODULE.bazel"]), &off);
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_module, 1);
    let opted_in = |version: &str| {
        with_bazelrc(
            with_bazel(texts(&module, &head), version),
            "build --jobs=8\n\
             common --incompatible_autoload_externally=+@rules_python,@com_google_protobuf\n",
        )
    };
    let r = run_on(&e, &index, &changed(&["MODULE.bazel"]), &opted_in("7.4.1"));
    assert!(r.global, "{:?}", r.global_reason);
    cause(&e, Code::W024AutoloadProvider);
    let r = run(&index, &changed(&["MODULE.bazel"]), &opted_in("9.2.0"));
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_module, 1);
}

/// `rules_android` is on Bazel 8's default autoload list (`android_library`
/// and friends) and registers no toolchains, so without the autoload rule
/// its bump would reach explicit loaders alone.
#[test]
fn a_rules_android_bump_is_global_on_bazel_8() {
    let (e, index) = base(&scanning());
    let module = format!("{MODULE_BASE}bazel_dep(name = \"rules_android\", version = \"0.6.0\")\n");
    let head = module.replace("0.6.0", "0.6.1");
    let r = run_on(
        &e,
        &index,
        &changed(&["MODULE.bazel"]),
        &with_bazel(texts(&module, &head), "8.4.2"),
    );
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("rules_android"));
    cause(&e, Code::W024AutoloadProvider);
    let r = run(
        &index,
        &changed(&["MODULE.bazel"]),
        &with_bazel(texts(&module, &head), "7.6.1"),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_module, 1);
}

#[test]
fn a_legacy_workspace_edit_that_cannot_be_classified_is_global() {
    let (_e, index) = base(&scanning());
    // An assignment at the top level: not a shape the statement diff reads.
    let r = run(
        &index,
        &changed(&["WORKSPACE"]),
        &texts_with_workspace(
            &format!("{WORKSPACE_BASE}VERSION = \"1\"\n"),
            &format!("{WORKSPACE_BASE}VERSION = \"2\"\n"),
        ),
    );
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("classify"));
}

/// A legacy `WORKSPACE` that declares the same repository the module file's
/// extension does, so the graph has somewhere to send it.
const WORKSPACE_BASE: &str = "load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \
     \"http_archive\")\n\
     http_archive(\n    name = \"generated\",\n    sha256 = \"aaa\",\n)\n\
     http_archive(\n    name = \"unused\",\n    sha256 = \"bbb\",\n)\n";

fn texts_with_workspace(base_ws: &str, head_ws: &str) -> FixedText {
    let mut m = texts(MODULE_BASE, MODULE_BASE).0;
    m.insert(
        ("base".to_string(), "WORKSPACE".to_string()),
        base_ws.to_string(),
    );
    m.insert(
        ("head".to_string(), "WORKSPACE".to_string()),
        head_ws.to_string(),
    );
    FixedText(m)
}

/// The sourcegraph shape: one `http_archive` in `WORKSPACE` moved, and only
/// the targets that depend on that repository are affected.
#[test]
fn a_legacy_workspace_declaration_names_its_repository() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["WORKSPACE"]),
        &texts_with_workspace(WORKSPACE_BASE, &WORKSPACE_BASE.replace("aaa", "zzz")),
    );
    assert!(
        !r.global,
        "a WORKSPACE edit used to be global on its own: {:?}",
        r.global_reason
    );
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
}

/// The other declaration in the same file moving reaches nothing, which is the
/// whole point of diffing the file rather than invalidating on it.
#[test]
fn an_unrelated_workspace_declaration_reaches_nothing() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["WORKSPACE"]),
        &texts_with_workspace(WORKSPACE_BASE, &WORKSPACE_BASE.replace("bbb", "zzz")),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert!(r.direct.is_empty());
}

/// Bazel 8 autoloads from a `WORKSPACE`-declared `com_google_protobuf` (its
/// name for protobuf there) as it does from the module, so the declaration
/// moving is global there too, and on Bazel 7 it is the one repository it
/// names.
#[test]
fn a_workspace_declaration_of_an_autoload_provider_is_global_on_bazel_8() {
    let (e, index) = base(&scanning());
    let ws = |sha: &str| {
        format!(
            "{WORKSPACE_BASE}http_archive(\n    name = \"com_google_protobuf\",\n    \
             sha256 = \"{sha}\",\n)\n"
        )
    };
    let r = run_on(
        &e,
        &index,
        &changed(&["WORKSPACE"]),
        &with_bazel(texts_with_workspace(&ws("ccc"), &ws("ddd")), "8.4.2"),
    );
    assert!(r.global);
    let reason = r.global_reason.unwrap();
    assert!(
        reason.contains("WORKSPACE") && reason.contains("com_google_protobuf"),
        "{reason}"
    );
    cause(&e, Code::W024AutoloadProvider);

    let r = run(
        &index,
        &changed(&["WORKSPACE"]),
        &with_bazel(texts_with_workspace(&ws("ccc"), &ws("ddd")), "7.4.1"),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert!(r.direct.is_empty());
}

/// The `WORKSPACE` route asks the same question as the module route, so a
/// 7.4 tree whose `.bazelrc` opts into autoloads is global there too, and the
/// diagnostic says "the Bazel here", not a version this tree is not running.
#[test]
fn a_workspace_declaration_of_a_provider_follows_the_bazelrc_flag() {
    let (e, index) = base(&scanning());
    let ws = |sha: &str| {
        format!(
            "{WORKSPACE_BASE}http_archive(\n    name = \"com_google_protobuf\",\n    \
             sha256 = \"{sha}\",\n)\n"
        )
    };
    let opted_in = with_bazelrc(
        with_bazel(texts_with_workspace(&ws("ccc"), &ws("ddd")), "7.4.1"),
        "common --incompatible_autoload_externally=@com_google_protobuf\n",
    );
    let r = run_on(&e, &index, &changed(&["WORKSPACE"]), &opted_in);
    assert!(r.global, "{:?}", r.global_reason);
    let reason = r.global_reason.unwrap();
    assert!(
        reason.contains("the Bazel here autoloads"),
        "the reason must not name a version this tree is not running: {reason}"
    );
    cause(&e, Code::W024AutoloadProvider);
}

/// Toolchain registration has no edge in the graph wherever it is written.
#[test]
fn a_workspace_toolchain_registration_is_global() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["WORKSPACE"]),
        &texts_with_workspace(
            &format!("{WORKSPACE_BASE}go_register_toolchains(version = \"1\")\n"),
            &format!("{WORKSPACE_BASE}go_register_toolchains(version = \"2\")\n"),
        ),
    );
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("toolchain"));
}

/// A `MODULE.bazel.lock` around one extension, whose `gen` extension
/// generates the repository `generated` with `spec` as its definition.
fn lock(spec: &str, digest: &str) -> String {
    format!(
        "{{\"lockFileVersion\": 24, \"registryFileHashes\": {{}}, \
         \"selectedYankedVersions\": {{}}, \"moduleExtensions\": {{\
         \"//bazel:ext.bzl%gen\": {{\"general\": {{\
         \"bzlTransitiveDigest\": \"{digest}\", \
         \"generatedRepoSpecs\": {{\"generated\": {{\"url\": \"{spec}\"}}}}}}}}}}}}"
    )
}

/// The same two revisions, with a lockfile on each side as well.
fn texts_with_lock(base_lock: &str, head_lock: &str) -> FixedText {
    let mut m = texts(MODULE_BASE, MODULE_BASE).0;
    m.insert(
        ("base".to_string(), "MODULE.bazel.lock".to_string()),
        base_lock.to_string(),
    );
    m.insert(
        ("head".to_string(), "MODULE.bazel.lock".to_string()),
        head_lock.to_string(),
    );
    FixedText(m)
}

/// The envoy shape: the lockfile moves on its own because an extension was
/// re-evaluated, and exactly one repository's definition came out different.
#[test]
fn a_lockfile_moving_on_its_own_names_the_repositories_that_moved() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["MODULE.bazel.lock"]),
        &texts_with_lock(&lock("a", "d"), &lock("b", "d")),
    );
    assert!(
        !r.global,
        "the lone lockfile used to be an invalidator: {:?}",
        r.global_reason
    );
    assert_eq!(r.stats.repos_dirty_by_module, 1);
    // `//lib:lib` names `@generated//:thing`; `//app:app` reaches it.
    assert_eq!(r.direct, vec!["//lib:lib".to_string()]);
}

/// Only the extension's implementation digest moved: the extension was
/// re-evaluated and produced exactly the same repositories.
#[test]
fn a_lockfile_whose_digests_alone_moved_dirties_nothing() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["MODULE.bazel.lock"]),
        &texts_with_lock(&lock("a", "d1"), &lock("a", "d2")),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_module, 0);
    assert!(r.direct.is_empty());
}

#[test]
fn a_lockfile_cambium_cannot_read_is_global() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["MODULE.bazel.lock"]),
        &texts_with_lock(
            &lock("a", "d"),
            &lock("a", "d").replace("{\"lockFileVersion\"", "{\"novel\": 1, \"lockFileVersion\""),
        ),
    );
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("novel"));
}

#[test]
fn a_lockfile_moving_with_the_module_file_adds_nothing() {
    let (_e, index) = base(&scanning());
    let head = MODULE_BASE.replace(
        "bazel_dep(name = \"fmt\", version = \"1.0\")",
        "bazel_dep(name = \"fmt\", version = \"9.9\")",
    );
    let r = run(
        &index,
        &changed(&["MODULE.bazel", "MODULE.bazel.lock"]),
        &texts(MODULE_BASE, &head),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.direct, vec!["//app:app".to_string()]);
}

#[test]
fn a_repository_rules_bzl_names_its_repository_instead_of_going_global() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["bazel/repo.bzl"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(
        !r.global,
        "a workspace-level .bzl used to be global: {:?}",
        r.global_reason
    );
    assert_eq!(r.stats.repos_dirty_by_bzl, 1);
    assert_eq!(
        r.direct,
        vec!["//bazel:bzl_srcs".to_string(), "//lib:lib".to_string()],
        "the repository consequence and the source-file consequence are both owed"
    );
}

/// The soundness hole the bazel subject found on `9a7ba209..cc5c15fa`.
///
/// `tools/build_defs/repo/utils.bzl` is loaded from `MODULE.bazel` (through
/// `http.bzl`) *and* listed in the `srcs` of fifteen filegroups in its own
/// package. Once the externals stage started claiming such a file as a
/// repository-defining input, and item 4 stopped sending it through
/// `rbuildfiles`, nothing left marked its ordinary source-file consumers: the
/// filegroups were `cambium_missing` against bazel-diff. Before those items
/// the edit went global, which hid the miss rather than fixing it.
#[test]
fn a_workspace_closure_bzl_still_reaches_the_filegroups_that_list_it() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["bazel/repo.bzl"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    let a = affected(&r);
    assert!(
        a.contains(&"//bazel:bzl_srcs".to_string()),
        "the filegroup consuming the .bzl as a source must be affected: {a:?}"
    );
    assert!(
        a.contains(&"//bazel:embedded_tools".to_string()),
        "and everything above it: {a:?}"
    );
    // Without losing the repository consequence.
    assert!(a.contains(&"//lib:lib".to_string()), "{a:?}");
}

/// The `@envoy_repo` shape, answered by the graph rather than by a subtree
/// prefix (issue #8).
///
/// `@generated`'s rule writes a BUILD file whose `genrule` lists
/// `@demo//changelogs` in `tools`, so the repository's targets depend on the
/// filegroup `//changelogs:changelogs`. Which files that filegroup globs is
/// not knowable from the label — the old code guessed `changelogs/**`, which
/// on a real repository claimed 91% of the packages — but the graph has the
/// edges: the reverse walk from `changelogs/current/x.rst` reaches the
/// filegroup, the filegroup is a recorded label input of `@generated`, so
/// `@generated`'s targets moved and `//lib:lib`, which names one, is direct.
#[test]
fn a_changed_file_reaching_a_repositorys_rule_label_moves_that_repository() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["changelogs/current/x.rst"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    // Nothing in the diff named a repository: the walk did.
    assert_eq!(r.stats.repos_dirty_by_input, 0);
    assert_eq!(r.stats.repos_dirty_by_target, 1);
    let a = affected(&r);
    assert!(a.contains(&"//changelogs:changelogs".to_string()), "{a:?}");
    assert!(a.contains(&"//lib:lib".to_string()), "{a:?}");
    assert!(a.contains(&"//app:app".to_string()), "and above it: {a:?}");
    assert!(
        r.direct.contains(&"//lib:lib".to_string()),
        "the repository's label consumer is direct, not merely affected: {:?}",
        r.direct
    );
    // Rule (1) only: a repository rule cannot read a rule target's output, so
    // nothing it wrote — no `.bzl` it exports — can have moved, and no
    // package's load closure is re-queried on that account.
    assert!(
        !r.dirty_packages.contains(&"app".to_string()),
        "{:?}",
        r.dirty_packages
    );
}

/// The same directory, a file the filegroup's glob does not match. The graph
/// has no edge from it, so no repository moved — which is the whole difference
/// between the prefix and the label.
#[test]
fn a_file_the_glob_does_not_match_moves_nothing() {
    let (_e, index) = base(&scanning());
    let r = run(
        &index,
        &changed(&["changelogs/notes.txt"]),
        &texts(MODULE_BASE, MODULE_BASE),
    );
    assert!(!r.global, "{:?}", r.global_reason);
    assert_eq!(r.stats.repos_dirty_by_target, 0);
    assert_eq!(r.stats.repos_dirty_by_input, 0);
    assert!(affected(&r).is_empty(), "{:?}", affected(&r));
}

/// `--explain` names the repository and the target that carried it.
#[test]
fn the_direct_reason_names_the_repository_and_the_label() {
    let (_e, index) = base(&scanning());
    let e = engine();
    let r = e
        .compute_with_texts(
            index.view(),
            &changed(&["changelogs/current/x.rst"]),
            &scanning(),
            &AffectedRequest {
                explain: true,
                ..req()
            },
            &texts(MODULE_BASE, MODULE_BASE),
        )
        .unwrap()
        .report;
    let x = r.explanations.expect("--explain was asked for");
    let lib = x
        .iter()
        .find(|e| e.direct == "//lib:lib")
        .expect("//lib:lib is direct");
    assert_eq!(
        lib.reason,
        cambium_engine::DirectReason::ExternalRepoTarget {
            repo: "generated".into(),
            via: "//changelogs:changelogs".into(),
        }
    );
}

#[test]
fn an_unparseable_module_file_is_global() {
    let (_e, index) = base(&scanning());
    let head = format!("{MODULE_BASE}if x:\n    bazel_dep(name = \"q\")\n");
    let r = run(
        &index,
        &changed(&["MODULE.bazel"]),
        &texts(MODULE_BASE, &head),
    );
    assert!(r.global);
    assert!(r.global_reason.unwrap().contains("cannot parse"));
}

#[test]
fn turning_the_stage_off_restores_whole_world_invalidation() {
    let e = engine();
    let index = e.full_build_with("c0", &scanning()).unwrap();
    let mut cfg = Config::default();
    cfg.externals.enabled = false;
    let e = engine_with(cfg);
    let out = e
        .compute_with_texts(
            index.view(),
            &changed(&["MODULE.bazel"]),
            &scanning(),
            &req(),
            &texts(MODULE_BASE, MODULE_BASE),
        )
        .unwrap();
    assert!(out.report.global);
    assert!(out.report.global_reason.unwrap().contains("invalidator"));
}
