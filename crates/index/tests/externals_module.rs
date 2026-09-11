//! `MODULE.bazel`: what the statement reader records, and what a diff of
//! two revisions names.

use std::collections::BTreeSet;

use cambium_index::externals::{diff_module, scan_module, ModuleChange};

#[test]
fn parses_bazel_deps_and_overrides() {
    let f = scan_module(
        "module(name = \"envoy\", version = \"1.0\")\n\
         bazel_dep(name = \"envoy_api\", version = \"1.40.0-dev\")\n\
         bazel_dep(name = \"apple_support\", version = \"2.3.0\", repo_name = \"build_bazel_apple_support\")\n\
         local_path_override(\n    module_name = \"envoy_api\",\n    path = \"api\",\n)\n",
    );
    assert!(f.unparsed.is_empty(), "{:?}", f.unparsed);
    assert_eq!(f.deps["envoy_api"].version, "1.40.0-dev");
    assert_eq!(f.deps["build_bazel_apple_support"].module, "apple_support");
    assert_eq!(f.local_paths["envoy_api"], "api/");
}

#[test]
fn parses_extensions_tags_and_use_repo() {
    let f = scan_module(
        "pip = use_extension(\"@rules_python//python/extensions:pip.bzl\", \"pip\")\n\
         pip.parse(\n    hub_name = \"base_pip3\",\n    requirements_lock = \"//tools/base:requirements.txt\",\n)\n\
         use_repo(pip, \"base_pip3\", alias = \"fuzzing_pip3\")\n",
    );
    assert!(f.unparsed.is_empty(), "{:?}", f.unparsed);
    let e = f
        .extensions
        .values()
        .find(|e| e.name == "pip")
        .expect("extension");
    assert_eq!(e.tags.len(), 1);
    assert!(e.repos.contains("base_pip3"));
    assert!(e.repos.contains("fuzzing_pip3"));
    assert!(e.repos.contains("alias"));
    assert!(e
        .labels
        .iter()
        .any(|l| l == "//tools/base:requirements.txt"));
}

#[test]
fn parses_use_repo_rule_instantiations() {
    let f = scan_module(
        "http_archive = use_repo_rule(\"//bazel:repo.bzl\", \"http_archive\")\n\
         http_archive(name = \"thing\", build_file = \"//bazel:thing.BUILD\")\n",
    );
    assert!(f.unparsed.is_empty(), "{:?}", f.unparsed);
    let r = f.repo_rules.values().next().unwrap();
    assert_eq!(r.bzl, "//bazel:repo.bzl");
    assert!(r.repos.contains("thing"));
    assert!(r.labels.iter().any(|l| l == "//bazel:thing.BUILD"));
}

/// `bazel/bazel` writes two of these; before they were understood, every
/// `MODULE.bazel` edit in that repository went global.
#[test]
fn a_comprehension_of_tag_calls_belongs_to_its_extension() {
    let t = "maven = use_extension(\"@rules_jvm_external//:extensions.bzl\", \"maven\")\n\
             use_repo(maven, \"maven\")\n\
             [\n    maven.artifact(\n        group = group,\n        artifact = artifact,\n    )\n\
             \x20   for group, artifact in [c.split(\":\") for c in [\"a:b\", \"c:d\"]]\n]\n";
    let f = scan_module(t);
    assert!(f.unparsed.is_empty(), "{:?}", f.unparsed);
    let e = f.extensions.values().next().unwrap();
    assert_eq!(e.tags.len(), 1);
    // And a change inside the comprehension is seen as a change to it.
    let g = scan_module(&t.replace("\"a:b\"", "\"a:z\""));
    assert_eq!(
        diff_module(&f, &g),
        ModuleChange::Repos(["maven".to_string()].into_iter().collect())
    );
}

/// `override_repo` and `inject_repo` used to land in `unparsed`, which is
/// global; xla writes two of them.
#[test]
fn parses_override_repo_and_inject_repo() {
    let f = scan_module(
        "cc = use_extension(\"@rules_cc//cc:extensions.bzl\", \"cc_configure_extension\")\n\
         override_repo(cc, local_config_cc = \"my_config_cc\")\n\
         override_repo(cc, \"other_config\")\n\
         inject_repo(cc, \"tools\")\n",
    );
    assert!(f.unparsed.is_empty(), "{:?}", f.unparsed);
    let e = f.extensions.values().next().expect("extension");
    assert_eq!(e.overrides.len(), 3);
    // They say nothing about which repositories the main module asked the
    // extension for.
    assert!(e.repos.is_empty());
}

/// The overridden name resolves to a different repository, so everything
/// the extension generates is built against something else.
#[test]
fn a_changed_override_repo_moves_the_extensions_repositories() {
    let base = "cc = use_extension(\"@rules_cc//cc:extensions.bzl\", \"ext\")\n\
         use_repo(cc, \"local_config_cc\")\n\
         override_repo(cc, local_config_cc = \"my_config_cc\")\n";
    for head in [
        // Changed.
        base.replace("my_config_cc", "other_config_cc"),
        // Removed.
        base.replace(
            "override_repo(cc, local_config_cc = \"my_config_cc\")\n",
            "",
        ),
        // Added, on top of the same base.
        format!("{base}inject_repo(cc, \"extra\")\n"),
    ] {
        assert_eq!(
            diff_module(&scan_module(base), &scan_module(&head)),
            ModuleChange::Repos(["local_config_cc".to_string()].into_iter().collect()),
            "{head}"
        );
    }
}

/// The proxy has to be one a `use_extension` bound, or the statement means
/// something this parser does not know.
#[test]
fn an_override_repo_on_an_unknown_proxy_is_unparsed() {
    let f = scan_module("override_repo(mystery, \"a\")\n");
    assert!(!f.unparsed.is_empty());
}

#[test]
fn an_unrecognised_statement_is_recorded() {
    let f = scan_module("if x:\n    bazel_dep(name = \"a\")\n");
    assert!(!f.unparsed.is_empty());
}

#[test]
fn a_version_bump_names_one_repository() {
    let base = scan_module("bazel_dep(name = \"fmt\", version = \"1.0\")\n");
    let head = scan_module("bazel_dep(name = \"fmt\", version = \"2.0\")\n");
    assert_eq!(
        diff_module(&base, &head),
        ModuleChange::Repos(["fmt".to_string()].into_iter().collect())
    );
}

#[test]
fn a_toolchain_module_bump_is_global() {
    let base = scan_module("bazel_dep(name = \"rules_python\", version = \"1.0\")\n");
    let head = scan_module("bazel_dep(name = \"rules_python\", version = \"2.0\")\n");
    match diff_module(&base, &head) {
        ModuleChange::Global(r) => assert!(r.contains("rules_python"), "{r}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn register_toolchains_is_global() {
    let base = scan_module("register_toolchains(\"//a:b\")\n");
    let head = scan_module("register_toolchains(\"//a:c\")\n");
    assert!(matches!(diff_module(&base, &head), ModuleChange::Global(_)));
}

/// The unparsed statement is global when it is the edit, and inert when
/// it merely sits beside one.
#[test]
fn an_unparsed_statement_is_global_only_when_it_changed() {
    let t = |v: &str| {
        format!(
            "load(\"//bazel:vendored.bzl\", \"vendored\")\nvendored()\n\
             bazel_dep(name = \"fmt\", version = \"{v}\")\n"
        )
    };
    assert_eq!(
        diff_module(&scan_module(&t("1.0")), &scan_module(&t("2.0"))),
        ModuleChange::Repos(["fmt".to_string()].into_iter().collect())
    );
    let edited = t("1.0").replace("vendored()", "vendored(x = 1)");
    assert!(matches!(
        diff_module(&scan_module(&t("1.0")), &scan_module(&edited)),
        ModuleChange::Global(m) if m.contains("vendored") && m.contains("x")
    ));
}

#[test]
fn an_unchanged_module_names_nothing() {
    let t = "bazel_dep(name = \"fmt\", version = \"1.0\")\nregister_toolchains(\"//a:b\")\n";
    assert_eq!(
        diff_module(&scan_module(t), &scan_module(t)),
        ModuleChange::Repos(BTreeSet::new())
    );
}

#[test]
fn a_changed_tag_names_the_extensions_repositories() {
    let base = scan_module(
        "pip = use_extension(\"@rules_python//python/extensions:pip.bzl\", \"pip\")\n\
         pip.parse(hub_name = \"base_pip3\", requirements_lock = \"//tools:a.txt\")\n\
         use_repo(pip, \"base_pip3\")\n",
    );
    let head = scan_module(
        "pip = use_extension(\"@rules_python//python/extensions:pip.bzl\", \"pip\")\n\
         pip.parse(hub_name = \"base_pip3\", requirements_lock = \"//tools:b.txt\")\n\
         use_repo(pip, \"base_pip3\")\n",
    );
    assert_eq!(
        diff_module(&base, &head),
        ModuleChange::Repos(["base_pip3".to_string()].into_iter().collect())
    );
}

/// The precision question `bazel/bazel`'s `5c47f233..bfd201d1` raised: a
/// version bump next to two extensions that between them name half a dozen
/// repositories must name the one repository, not all of them. (What
/// widened that change set's answer was not this function but the input map —
/// `//:extensions.bzl` calls `ctx.path(Label("//:MODULE.bazel"))`, so
/// every repository it defines really is declared to depend on the file.)
#[test]
fn a_version_bump_does_not_name_the_other_extensions_repositories() {
    let text = |v: &str| {
        format!(
            "bazel_dep(name = \"fmt\", version = \"{v}\")\n\
             maven = use_extension(\"@rules_jvm_external//:extensions.bzl\", \"maven\")\n\
             maven.install(artifacts = [\"g:a:1\"])\n\
             use_repo(maven, \"maven\", \"unpinned_maven\")\n\
             deps = use_extension(\"//:extensions.bzl\", \"build_deps\")\n\
             use_repo(deps, \"debian_java_deps\", \"debian_cc_deps\")\n"
        )
    };
    assert_eq!(
        diff_module(&scan_module(&text("1.0")), &scan_module(&text("2.0"))),
        ModuleChange::Repos(["fmt".to_string()].into_iter().collect())
    );
}

/// And a changed tag names its own extension's repositories only.
#[test]
fn a_changed_tag_leaves_the_other_extensions_alone() {
    let text = |a: &str| {
        format!(
            "maven = use_extension(\"@rules_jvm_external//:extensions.bzl\", \"maven\")\n\
             maven.install(artifacts = [\"{a}\"])\n\
             use_repo(maven, \"maven\", \"unpinned_maven\")\n\
             deps = use_extension(\"//:extensions.bzl\", \"build_deps\")\n\
             use_repo(deps, \"debian_java_deps\", \"debian_cc_deps\")\n"
        )
    };
    assert_eq!(
        diff_module(&scan_module(&text("g:a:1")), &scan_module(&text("g:a:2"))),
        ModuleChange::Repos(
            ["maven".to_string(), "unpinned_maven".to_string()]
                .into_iter()
                .collect()
        )
    );
}
