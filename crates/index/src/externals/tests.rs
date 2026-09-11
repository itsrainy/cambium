//! The externals tests that need crate-private items; everything that
//! uses only the public API lives in `crates/index/tests/externals_*.rs`.

use std::collections::BTreeSet;

use crate::loads::{FileText, LoadGraph, RepoMapping};

use super::inputs::string_literals;
use super::*;

fn m() -> RepoMapping {
    RepoMapping::from_dump("{\"\":\"\",\"envoy\":\"\",\"envoy_api\":\"envoy_api+\"}")
}

/// Both files present, and the bazelrc says WORKSPACE: the module file's
/// override is inert, and the repository is whatever the WORKSPACE
/// closure builds.
#[test]
fn a_module_file_bazel_does_not_read_derives_nothing() {
    fn with_rc(rc: Option<&'static str>) -> Files {
        let mut v = vec![
            (
                "MODULE.bazel",
                "bazel_dep(name = \"envoy_build_config\", version = \"1.0\")\n\
                 local_path_override(module_name = \"envoy_build_config\", path = \"mobile/envoy_build_config\")\n",
            ),
            ("WORKSPACE", "workspace(name = \"envoy\")\n"),
        ];
        if let Some(rc) = rc {
            v.push((".bazelrc", rc));
        }
        Files(v)
    }
    let identity = RepoMapping::identity();
    let bzlmod =
        RepoMapping::from_dump("{\"\":\"\",\"envoy_build_config\":\"envoy_build_config+\"}");
    assert!(!bzlmod_live(
        &with_rc(Some(
            "common --noenable_bzlmod\ncommon --enable_workspace\n"
        )),
        &bzlmod
    ));
    assert!(bzlmod_live(
        &with_rc(Some("build --enable_bzlmod\n")),
        &identity
    ));
    assert!(!bzlmod_live(&with_rc(None), &identity));
    assert!(bzlmod_live(&with_rc(None), &bzlmod));
    assert!(bzlmod_live(&Files(vec![("MODULE.bazel", "")]), &identity));
    assert_eq!(
        bazelrc_bzlmod("# --enable_bzlmod\ncommon --noenable_bzlmod\nbuild --enable_bzlmod\n"),
        Some(true)
    );
    // xla: the config-scoped line after the real one does not apply.
    assert_eq!(
        bazelrc_bzlmod("common --enable_bzlmod --noenable_workspace\ncommon:workspace --noenable_bzlmod --enable_workspace\n"),
        Some(true)
    );
    assert_eq!(bazelrc_bzlmod("build:ci --noenable_bzlmod\n"), None);

    let files = with_rc(Some("common --noenable_bzlmod\n"));
    let facts = read_live_module_facts(&files, &bzlmod);
    assert!(facts.local_paths.is_empty() && facts.deps.is_empty());
    let ri = inputs(&files, &bzlmod);
    assert!(
        !ri.path_maps.contains_key("envoy_build_config"),
        "{:?}",
        ri.path_maps
    );
}

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

#[test]
fn labels_become_patterns() {
    let m = m();
    // The file a label names, and the target it names. Never the label's
    // package subtree, which is what issue #8 was about.
    assert_eq!(
        main_repo_patterns("@envoy//:.github/config.yml", &m),
        vec![
            Pattern::Exact(".github/config.yml".into()),
            Pattern::Label("//:.github/config.yml".into()),
        ]
    );
    assert_eq!(
        main_repo_patterns("@envoy//changelogs", &m),
        vec![
            Pattern::Exact("changelogs/changelogs".into()),
            Pattern::Label("//changelogs:changelogs".into()),
        ]
    );
    assert_eq!(
        main_repo_patterns("//changelogs:summary", &m),
        vec![
            Pattern::Exact("changelogs/summary".into()),
            Pattern::Label("//changelogs:summary".into()),
        ]
    );
    // A `config_setting` key names a target and a file that is probably
    // not there; neither claims `build/**`.
    assert_eq!(
        main_repo_patterns("//build:some_flag", &m),
        vec![
            Pattern::Exact("build/some_flag".into()),
            Pattern::Label("//build:some_flag".into()),
        ]
    );
    // A visibility token names no file and creates no edge.
    assert!(main_repo_patterns("//some/team:__subpackages__", &m).is_empty());
    assert!(main_repo_patterns("//some/team:__pkg__", &m).is_empty());
    // Not the main repository.
    assert!(main_repo_patterns("@envoy_api//envoy:pkg", &m).is_empty());
    assert!(main_repo_patterns("not a label", &m).is_empty());
    assert!(main_repo_patterns("//", &m).is_empty());
    // Never a label at all: a format placeholder, or text out of a URL.
    assert!(main_repo_patterns("//%s:foo", &m).is_empty());
}

struct Files(Vec<(&'static str, &'static str)>);

impl FileText for Files {
    fn text(&self, path: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, t)| t.to_string())
    }
}

#[test]
fn a_repository_rules_labels_become_its_inputs() {
    let module =
        "envoy_repo_ext = use_extension(\"//bazel:extensions.bzl\", \"envoy_repo_extension\")\n\
                  use_repo(envoy_repo_ext, \"envoy_repo\")\n\
                  local_path_override(module_name = \"envoy_api\", path = \"api\")\n\
                  bazel_dep(name = \"envoy_api\", version = \"1.0\")\n";
    let files = Files(vec![
        ("MODULE.bazel", module),
        (
            "bazel/extensions.bzl",
            "load(\"//bazel:repo.bzl\", \"envoy_repo\")\n",
        ),
        (
            "bazel/repo.bzl",
            "X = attr.label(default = \"@envoy//:.github/config.yml\")\n\
             Y = \"@envoy//changelogs\"\n",
        ),
    ]);
    let m = m();
    let mut g = LoadGraph::new();
    let ws = g.workspace_closure(&files, &m);
    let facts = read_module_facts(&files);
    let ri = build_repo_inputs(&files, &m, &g, &ws, &facts);

    assert_eq!(
        ri.repos_for(".github/config.yml"),
        ["envoy_repo".to_string()].into_iter().collect()
    );
    // `@envoy//changelogs` names the target `//changelogs:changelogs`, not
    // the files under it: no path pattern covers the changelog entry, and
    // the graph answers for it instead (issue #8).
    assert!(ri.repos_for("changelogs/current/x.rst").is_empty());
    assert_eq!(
        ri.repos_for_label("//changelogs:changelogs"),
        ["envoy_repo".to_string()].into_iter().collect()
    );
    assert_eq!(
        ri.repos_for("api/envoy/config/v3/x.proto"),
        ["envoy_api".to_string()].into_iter().collect()
    );
    assert!(ri
        .repos_for("source/common/http/conn_manager.cc")
        .is_empty());
    // The `.bzl` files that make up the repository rule name the repo.
    assert_eq!(
        ri.bzl_repos["bazel/repo.bzl"],
        ["envoy_repo".to_string()].into_iter().collect()
    );
    // A label the rule resolves feeds it; neither of these is target-only.
    assert!(ri.target_only.is_empty(), "{:?}", ri.target_only);
}

/// The golden end-to-end fixture's `MODULE.bazel`: `use_repo_rule` names
/// the `.bzl` that defines `gen_repo`, and `bazel_dep` + `local_path_override`
/// declare a second repository. The `.bzl` label is a load edge, so it is
/// no path input of either; a change to the file is answered by its own
/// row, which names only the repository its rule instantiates.
#[test]
fn a_use_repo_rule_label_is_not_a_repository_input() {
    let files = Files(vec![
        (
            "MODULE.bazel",
            "module(name = \"root\", version = \"0.0.1\")\n\
             bazel_dep(name = \"mod_a\", version = \"0.0.1\")\n\
             local_path_override(\n    module_name = \"mod_a\",\n    path = \"mod_a\",\n)\n\
             gen_repo = use_repo_rule(\"//tools:gen.bzl\", \"gen_repo\")\n\
             gen_repo(\n    name = \"gen\",\n    config = \"@root//:config.txt\",\n)\n",
        ),
        (
            "tools/gen.bzl",
            "def _impl(rctx):\n\
             \x20   rctx.file(\"value.txt\", rctx.read(rctx.path(rctx.attr.config)))\n\n\
             gen_repo = repository_rule(\n\
             \x20   implementation = _impl,\n\
             \x20   attrs = {\"config\": attr.label(mandatory = True)},\n\
             )\n",
        ),
    ]);
    let ri = inputs(&files, &RepoMapping::identity());
    assert!(
        ri.repos_for("tools/gen.bzl").is_empty(),
        "{:?}",
        ri.repos_for("tools/gen.bzl")
    );
    assert_eq!(ri.repos_for("config.txt"), set(&["gen"]));
    assert_eq!(ri.bzl_repos["tools/gen.bzl"], set(&["gen"]));
}

/// `tools/workspace.bzl` loads every `third_party/*/init.bzl` and calls
/// them: its `load()` labels are edges, and the loaded file's own
/// repositories answer for a change to it. Read as inputs, each init file
/// would feed everything the root file can define.
#[test]
fn a_load_label_is_not_a_repository_input() {
    let files = Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"apollo\")\n\
             load(\"//tools:workspace.bzl\", \"apollo_repositories\")\n\
             apollo_repositories()\n",
        ),
        (
            "tools/workspace.bzl",
            "load(\"//third_party/pcl:init.bzl\", pcl = \"init\")\n\
             load(\"//third_party/absl:init.bzl\", absl = \"init\")\n\
             def apollo_repositories():\n\
             \x20   pcl()\n\
             \x20   absl()\n",
        ),
        (
            "third_party/pcl/init.bzl",
            "def init():\n\
             \x20   pcl_configure(name = \"local_config_pcl\", tpl = \"//third_party/pcl:BUILD.tpl\")\n",
        ),
        (
            "third_party/absl/init.bzl",
            "def init():\n\
             \x20   http_archive(name = \"com_google_absl\")\n",
        ),
    ]);
    let ri = inputs(&files, &RepoMapping::identity());
    assert!(
        ri.repos_for("third_party/pcl/init.bzl").is_empty(),
        "{:?}",
        ri.repos_for("third_party/pcl/init.bzl")
    );
    assert_eq!(
        ri.repos_for("third_party/pcl/BUILD.tpl"),
        set(&["local_config_pcl"])
    );
    // The file's own set is what answers a change to it; the loader's
    // set is kept alongside because a loader may read a constant from it.
    assert!(ri.bzl_repos["third_party/pcl/init.bzl"].contains("local_config_pcl"));
}

/// The lock a `WORKSPACE`-declared `npm_translate_lock` reads is written in
/// `WORKSPACE` itself, not in any `.bzl`. It is an input of `@npm` all the
/// same: a version bump in it rewrites every `node_modules` link target
/// the repository generates.
#[test]
fn a_label_written_in_workspace_is_a_repository_input() {
    let files = Files(vec![(
        "WORKSPACE",
        "workspace(name = \"sourcegraph\")\n\
         load(\"@aspect_rules_js//npm:repositories.bzl\", \"npm_translate_lock\")\n\
         npm_translate_lock(\n\
         \x20   name = \"npm\",\n\
         \x20   npmrc = \"//:.npmrc\",\n\
         \x20   pnpm_lock = \"//:pnpm-lock.yaml\",\n\
         )\n",
    )]);
    let ri = inputs(&files, &RepoMapping::identity());
    assert_eq!(ri.repos_for("pnpm-lock.yaml"), set(&["npm"]));
    assert_eq!(ri.repos_for(".npmrc"), set(&["npm"]));
    assert!(ri.target_only.is_empty(), "{:?}", ri.target_only);
}

/// `Label("//third_party/gpus/%s.tpl" % name)` inside a repository rule's
/// implementation: the directory before the placeholder is an input of
/// every repository the rule instantiates, as a prefix.
#[test]
fn a_format_string_label_is_a_prefix_input() {
    assert_eq!(
        main_repo_patterns("//third_party/gpus/%s.tpl", &RepoMapping::identity()),
        vec![Pattern::Prefix("third_party/gpus/".into())]
    );
    assert_eq!(
        main_repo_patterns("//third_party/gpus:{}.tpl", &RepoMapping::identity()),
        vec![Pattern::Prefix("third_party/gpus/".into())]
    );
    assert!(main_repo_patterns("//%s:foo", &RepoMapping::identity()).is_empty());
    let files = Files(vec![
        (
            "MODULE.bazel",
            "rocm = use_extension(\"//third_party/extensions:rocm.bzl\", \"rocm_ext\")\n\
             use_repo(rocm, \"local_config_rocm\")\n",
        ),
        (
            "third_party/extensions/rocm.bzl",
            "load(\"//third_party/gpus:rocm_configure.bzl\", \"rocm_configure\")\n\
             def _impl(ctx):\n\
             \x20   rocm_configure(name = \"local_config_rocm\")\n\
             rocm_ext = module_extension(implementation = _impl)\n",
        ),
        (
            "third_party/gpus/rocm_configure.bzl",
            "def _tpl_path(repository_ctx, labelname):\n\
             \x20   return repository_ctx.path(Label(\"//third_party/gpus/%s.tpl\" % labelname))\n\
             def _impl(repository_ctx):\n\
             \x20   repository_ctx.template(\"rocm/build_defs.bzl\", _tpl_path(repository_ctx, \"rocm:build_defs.bzl\"), {})\n\
             rocm_configure = repository_rule(implementation = _impl)\n",
        ),
    ]);
    let m =
        RepoMapping::from_dump("{\"\":\"\",\"local_config_rocm\":\"+rocm_ext+local_config_rocm\"}");
    let ri = inputs(&files, &m);
    assert_eq!(
        ri.repos_for("third_party/gpus/rocm/build_defs.bzl.tpl"),
        set(&["local_config_rocm"]),
        "{:?}",
        ri.patterns
    );
}

/// A def that implements a rule nothing instantiates, and is also the
/// helper an instantiated rule's implementation calls: its labels belong
/// to the caller's repositories, not to the universe.
#[test]
fn an_implementation_of_an_uninstantiated_rule_is_still_a_helper() {
    let files = Files(vec![
        (
            "MODULE.bazel",
            "rocm = use_extension(\"//third_party/extensions:rocm.bzl\", \"rocm_ext\")\n\
             use_repo(rocm, \"local_config_rocm\")\n",
        ),
        (
            "third_party/extensions/rocm.bzl",
            "load(\"//third_party/gpus:rocm_configure.bzl\", \"rocm_configure\")\n\
             def _impl(ctx):\n\
             \x20   rocm_configure(name = \"local_config_rocm\")\n\
             rocm_ext = module_extension(implementation = _impl)\n",
        ),
        (
            "third_party/gpus/rocm_configure.bzl",
            "def _create_local(repository_ctx):\n\
             \x20   repository_ctx.template(\"rocm/build_defs.bzl\", Label(\"//third_party/gpus/rocm:build_defs.bzl.tpl\"), {})\n\
             def _autoconf_impl(repository_ctx):\n\
             \x20   _create_local(repository_ctx)\n\
             remote_rocm_configure = repository_rule(implementation = _create_local)\n\
             rocm_configure = repository_rule(implementation = _autoconf_impl)\n",
        ),
    ]);
    let m =
        RepoMapping::from_dump("{\"\":\"\",\"local_config_rocm\":\"+rocm_ext+local_config_rocm\"}");
    let ri = inputs(&files, &m);
    assert_eq!(
        ri.repos_for("third_party/gpus/rocm/build_defs.bzl.tpl"),
        set(&["local_config_rocm"]),
        "{:?}",
        ri.patterns
    );
}

#[test]
fn a_label_inside_a_larger_literal_is_embedded() {
    let lits =
        string_literals("A = \"@envoy//:VERSION.txt\"\nB = 'tools = [\"@envoy//changelogs\"]'\n");
    let find = |text: &str| -> Vec<bool> {
        lits.iter()
            .filter(|(_, l)| l.text == text)
            .map(|(_, l)| l.embedded)
            .collect()
    };
    assert_eq!(find("@envoy//:VERSION.txt"), vec![false]);
    assert_eq!(find("@envoy//changelogs"), vec![true]);
}

/// envoy's `bazel/repo.bzl`: three `attr.label` defaults the rule reads,
/// and a BUILD file it writes whose `genrule` lists `@envoy//changelogs`.
/// The changelog entry is an input of the generated *target*; it cannot
/// have moved `compiler.bzl`, which every package's closure loads.
#[test]
fn a_label_inside_text_the_rule_writes_names_targets_not_bzl_files() {
    let module =
        "envoy_repo_ext = use_extension(\"//bazel:extensions.bzl\", \"envoy_repo_extension\")\n\
                  use_repo(envoy_repo_ext, \"envoy_repo\")\n";
    let repo_bzl = r#"
def _impl(ctx):
version = ctx.read(ctx.path(ctx.attr.envoy_version))
ctx.file("version.bzl", "VERSION = '%s'" % version)
ctx.file("BUILD", '''
genrule(
name = "project",
tools = ["@envoy//:VERSION.txt", "@envoy//changelogs", "@envoy//changelogs:summary"],
)
''')
ctx.file("defs.bzl", 'load("@envoy//bazel:helper.bzl", "h")')

_envoy_repo = repository_rule(
implementation = _impl,
attrs = {"envoy_version": attr.label(default = "@envoy//:VERSION.txt")},
)
"#;
    let files = Files(vec![
        ("MODULE.bazel", module),
        (
            "bazel/extensions.bzl",
            "load(\"//bazel:repo.bzl\", \"envoy_repo\")\n",
        ),
        ("bazel/repo.bzl", repo_bzl),
    ]);
    let m = m();
    let mut g = LoadGraph::new();
    let ws = g.workspace_closure(&files, &m);
    let facts = read_module_facts(&files);
    let ri = build_repo_inputs(&files, &m, &g, &ws, &facts);
    let envoy_repo: BTreeSet<String> = ["envoy_repo".to_string()].into_iter().collect();

    // Named by the generated BUILD file: the repository's targets depend
    // on `//changelogs:changelogs`, so rule (1) still has a repository to
    // resolve when the graph says a change reached that target...
    assert_eq!(ri.repos_for_label("//changelogs:changelogs"), envoy_repo);
    assert_eq!(ri.repos_for_label("//changelogs:summary"), envoy_repo);
    // ...but the changelog entry itself is no longer a path input: which
    // files that `filegroup` globs is the graph's answer, not a subtree
    // prefix's (issue #8).
    assert!(ri.repos_for("changelogs/current/x.rst").is_empty());
    // ...and no `.bzl` in the repository can have moved either way.
    assert!(ri
        .target_only
        .contains(&Pattern::Label("//changelogs:changelogs".into())));
    assert!(ri
        .target_only
        .contains(&Pattern::Exact("changelogs/summary".into())));
    // Both resolved by the rule and written into its BUILD file: it feeds
    // the rule, and `version.bzl` really does change with it.
    assert_eq!(ri.repos_for("VERSION.txt"), envoy_repo);
    assert!(!ri
        .target_only
        .contains(&Pattern::Exact("VERSION.txt".into())));
    // An embedded label that is itself a `.bzl`: the generated `defs.bzl`
    // loads it, so it stays a real input.
    assert_eq!(ri.repos_for("bazel/helper.bzl"), envoy_repo);
    assert!(!ri
        .target_only
        .contains(&Pattern::Exact("bazel/helper.bzl".into())));
}

// ---- the path mapping ----

#[test]
fn a_prefix_map_rewrites_and_an_exact_one_does_not() {
    let dir = PathMap {
        from: "api/".into(),
        to: String::new(),
        exhaustive: false,
    };
    assert_eq!(
        dir.translate("api/envoy/x.proto").as_deref(),
        Some("envoy/x.proto")
    );
    assert_eq!(
        dir.translate("api/bazel/defs.bzl").as_deref(),
        Some("bazel/defs.bzl")
    );
    assert_eq!(dir.translate("source/x.cc"), None);

    let nested = PathMap {
        from: "api/".into(),
        to: "sub/".into(),
        exhaustive: false,
    };
    assert_eq!(nested.translate("api/a/b").as_deref(), Some("sub/a/b"));

    let one = PathMap {
        from: "api/BUILD".into(),
        to: "BUILD".into(),
        exhaustive: false,
    };
    assert_eq!(one.translate("api/BUILD").as_deref(), Some("BUILD"));
    assert_eq!(one.translate("api/BUILD.bazel"), None);
}

#[test]
fn a_path_map_round_trips_through_the_index() {
    for pm in [
        PathMap {
            from: "api/".into(),
            to: String::new(),
            exhaustive: false,
        },
        PathMap {
            from: "a/b.txt".into(),
            to: "c/d.txt".into(),
            exhaustive: false,
        },
    ] {
        assert_eq!(PathMap::decode(&pm.encode()), Some(pm));
    }
    assert_eq!(PathMap::decode("no separator"), None);
}

#[test]
fn local_repository_calls_declare_their_own_mapping() {
    let maps = scan_bzl_path_maps(
        "def deps():\n    local_repository(name = \"api\", path = \"api\")\n\
         \x20   new_local_repository(name = \"x\", path = \"third_party/x/\", build_file = \"//bazel:x.BUILD\")\n\
         \x20   local_repository(name = \"outside\", path = \"../sibling\")\n",
        &m(),
    );
    assert_eq!(
        maps.named,
        vec![
            ("api".to_string(), PathMap::exhaustive_dir("api/".into())),
            (
                "x".to_string(),
                PathMap::exhaustive_dir("third_party/x/".into())
            ),
        ],
        "a path outside the repository cannot be matched against a diff"
    );
}

#[test]
fn a_literal_symlink_is_a_mapping_and_a_computed_one_is_not() {
    let maps = scan_bzl_path_maps(
        "def _impl(ctx):\n\
         \x20   ctx.symlink(ctx.path(Label(\"//api:BUILD\")), \"BUILD\")\n\
         \x20   ctx.symlink(\"api/bazel/\", \"bazel\")\n\
         \x20   rctx.symlink(target = Label(\"//api:versioning.txt\"), link_name = \"versioning.txt\")\n",
        &m(),
    );
    let symlinks: Vec<PathMap> = maps.symlinks.iter().map(|(_, p)| p.clone()).collect();
    assert_eq!(
        symlinks,
        vec![
            PathMap {
                from: "api/BUILD".into(),
                to: "BUILD".into(),
                exhaustive: false,
            },
            PathMap {
                from: "api/bazel/".into(),
                to: "bazel/".into(),
                exhaustive: false,
            },
            PathMap {
                from: "api/versioning.txt".into(),
                to: "versioning.txt".into(),
                exhaustive: false,
            },
        ]
    );
}

/// The shape envoy's `bazel/api_binding.bzl` used before it moved to
/// `local_path_override`. Deriving a mapping from it would be a guess, and
/// a wrong mapping *hides* a `.bzl` edit; no mapping merely keeps the old,
/// wider answer.
#[test]
fn a_computed_symlink_source_yields_no_mapping() {
    let maps = scan_bzl_path_maps(
        "def _impl(ctx):\n\
         \x20   for d in DIRS:\n\
         \x20       ctx.symlink(ctx.path(ctx.attr.envoy_root).dirname.get_child(ctx.attr.reldir).get_child(d), d)\n\
         \x20   ctx.symlink(Label(\"//api\"), \"\")\n\
         \x20   ctx.symlink(Label(\"@other//api:BUILD\"), \"BUILD\")\n\
         \x20   ctx.symlink(\"../outside\", \"x\")\n",
        &m(),
    );
    assert!(maps.symlinks.is_empty(), "{:?}", maps.symlinks);
}

#[test]
fn a_local_path_override_is_the_repositorys_root() {
    let files = Files(vec![(
        "MODULE.bazel",
        "module(name = \"envoy\", version = \"1\")\n\
             bazel_dep(name = \"envoy_api\", version = \"1.0\")\n\
             local_path_override(module_name = \"envoy_api\", path = \"api\")\n",
    )]);
    let m = m();
    let mut g = LoadGraph::new();
    let ws = g.workspace_closure(&files, &m);
    let facts = read_module_facts(&files);
    let ri = build_repo_inputs(&files, &m, &g, &ws, &facts);
    assert_eq!(
        ri.path_maps["envoy_api"],
        [PathMap::exhaustive_dir("api/".into())]
            .into_iter()
            .collect()
    );
}

/// A `.bzl` that could not be attributed to a repository must not hand its
/// symlink mapping to every repository the main repository defines.
#[test]
fn an_unattributable_bzls_symlinks_map_nothing() {
    let files = Files(vec![
        (
            "MODULE.bazel",
            "e = use_extension(\"//bazel:ext.bzl\", \"e\")\nuse_repo(e, \"r\")\n\
             load(\"//bazel:loose.bzl\", \"x\")\n",
        ),
        ("bazel/ext.bzl", ""),
        (
            "bazel/loose.bzl",
            "def _impl(ctx):\n    ctx.symlink(\"data/\", \"d\")\n",
        ),
    ]);
    let m = RepoMapping::identity();
    let mut g = LoadGraph::new();
    let ws = g.workspace_closure(&files, &m);
    let facts = read_module_facts(&files);
    let ri = build_repo_inputs(&files, &m, &g, &ws, &facts);
    assert!(ri.path_maps.is_empty(), "{:?}", ri.path_maps);
}

#[test]
fn an_unattributable_workspace_bzl_falls_back_to_every_repository() {
    // `setup` comes from a repository this closure cannot read into, so
    // the module-level call to it is opaque and the file keeps the wide
    // answer. Without that call the same file would be read in full and
    // its constant would be an input of nothing; see
    // [`a_readable_file_that_defines_nothing_is_not_the_universe`].
    let files = Files(vec![
        (
            "MODULE.bazel",
            "e = use_extension(\"//bazel:ext.bzl\", \"e\")\nuse_repo(e, \"r\")\n\
             load(\"//bazel:loose.bzl\", \"x\")\n",
        ),
        ("bazel/ext.bzl", ""),
        (
            "bazel/loose.bzl",
            "load(\"@rules_x//x:defs.bzl\", \"setup\")\n\
             P = \"//data:thing.txt\"\n\
             x = setup(P)\n",
        ),
    ]);
    let m = RepoMapping::identity();
    let mut g = LoadGraph::new();
    let ws = g.workspace_closure(&files, &m);
    let facts = read_module_facts(&files);
    let ri = build_repo_inputs(&files, &m, &g, &ws, &facts);
    assert_eq!(
        ri.repos_for("data/thing.txt"),
        ["r".to_string()].into_iter().collect()
    );
}

// ---- per-occurrence attribution ----

/// A working tree on disk, for the corpus check below.
struct Dir(std::path::PathBuf);

impl FileText for Dir {
    fn text(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.0.join(path)).ok()
    }
}

fn inputs(files: &dyn FileText, m: &RepoMapping) -> RepoInputs {
    let mut g = LoadGraph::new();
    let ws = g.workspace_closure(files, m);
    let facts = read_live_module_facts(files, m);
    build_repo_inputs(files, m, &g, &ws, &facts)
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// envoy at 7ba51266cc60: `//bazel:extensions.bzl` declares five module
/// extensions, and `bazel/repo.bzl` — which it loads — declares one
/// repository rule whose `attr.label` defaults name three main-repo files.
/// Per file, all three fed all six repositories, including `envoy_repo`,
/// whose generated `compiler.bzl` sits in eleven hundred package load
/// closures; per occurrence they feed `envoy_repo` alone.
#[test]
fn the_bzlmod_shape_attributes_an_attr_default_to_its_own_rule() {
    let files = Files(vec![
        (
            "MODULE.bazel",
            "module(name = \"envoy\", version = \"1\")\n\
             repo_ext = use_extension(\"//bazel:extensions.bzl\", \"envoy_repo_extension\")\n\
             use_repo(repo_ext, \"envoy_repo\")\n\
             cfg_ext = use_extension(\"//bazel:extensions.bzl\", \"envoy_build_config_ext\")\n\
             use_repo(cfg_ext, \"envoy_build_config\")\n\
             cargo_ext = use_extension(\"//bazel:extensions.bzl\", \"wasm_cargo\")\n\
             use_repo(cargo_ext, \"cu\")\n",
        ),
        (
            "bazel/extensions.bzl",
            "load(\"//bazel/external/cargo/remote:crates.bzl\", \"crate_repositories\")\n\
             load(\":envoy_build_config.bzl\", \"default_envoy_build_config\")\n\
             load(\":repo.bzl\", \"envoy_repo\")\n\
             \n\
             def _envoy_repo_impl(module_ctx):\n\
             \x20   envoy_repo()\n\
             \n\
             def _envoy_build_config_impl(module_ctx):\n\
             \x20   default_envoy_build_config(name = \"envoy_build_config\")\n\
             \n\
             def _wasm_cargo_impl(module_ctx):\n\
             \x20   crate_repositories()\n\
             \n\
             envoy_repo_extension = module_extension(implementation = _envoy_repo_impl)\n\
             envoy_build_config_ext = module_extension(implementation = _envoy_build_config_impl)\n\
             wasm_cargo = module_extension(implementation = _wasm_cargo_impl)\n",
        ),
        (
            "bazel/repo.bzl",
            "def _envoy_repo_impl(repository_ctx):\n\
             \x20   repository_ctx.file(\"BUILD\", '''\n\
             genrule(name = \"project\", tools = [\"@envoy//changelogs\"])\n\
             ''')\n\
             \n\
             _envoy_repo = repository_rule(\n\
             \x20   implementation = _envoy_repo_impl,\n\
             \x20   attrs = {\n\
             \x20       \"envoy_ci_config\": attr.label(default = \"@envoy//:.github/config.yml\"),\n\
             \x20   },\n\
             )\n\
             \n\
             def envoy_repo():\n\
             \x20   _envoy_repo(name = \"envoy_repo\")\n",
        ),
        (
            "bazel/envoy_build_config.bzl",
            "def _impl(ctx):\n\
             \x20   ctx.symlink(ctx.attr.config, \"extensions_build_config.bzl\")\n\
             \n\
             default_envoy_build_config = repository_rule(\n\
             \x20   implementation = _impl,\n\
             \x20   attrs = {\"config\": attr.label(\
             default = \"@envoy//source/extensions:extensions_build_config.bzl\")},\n\
             )\n",
        ),
        (
            "bazel/external/cargo/remote/crates.bzl",
            "def crate_repositories():\n\
             \x20   maybe(\n\
             \x20       crates_vendor_remote_repository,\n\
             \x20       name = \"cu\",\n\
             \x20       defs_module = Label(\"@envoy//bazel/external/cargo/remote:defs.bzl\"),\n\
             \x20   )\n",
        ),
    ]);
    let m = RepoMapping::from_dump("{\"\":\"\",\"envoy\":\"\"}");
    let ri = inputs(&files, &m);

    assert_eq!(ri.repos_for(".github/config.yml"), set(&["envoy_repo"]));
    assert_eq!(
        ri.repos_for("source/extensions/extensions_build_config.bzl"),
        set(&["envoy_build_config"])
    );
    assert_eq!(
        ri.repos_for("bazel/external/cargo/remote/defs.bzl"),
        set(&["cu"])
    );
    // The generated BUILD file's `tools` still name the repository — as a
    // target, whose own files the graph knows — and still cannot have
    // moved a `.bzl` inside it.
    assert_eq!(
        ri.repos_for_label("//changelogs:changelogs"),
        set(&["envoy_repo"])
    );
    assert!(ri.repos_for("changelogs/current/x.rst").is_empty());
    assert!(ri
        .target_only
        .contains(&Pattern::Label("//changelogs:changelogs".into())));
}

/// Pre-bzlmod envoy (issue #6): no `MODULE.bazel` at all, so every file
/// used to fall back to the empty set — no repository named, hence a
/// global invalidation. `envoy_dependencies()` reaches a forwarding
/// wrapper, and `pip_parse` is called with a literal name in a helper.
#[test]
fn the_workspace_shape_attributes_a_requirements_lock_to_one_pip() {
    let files = Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"envoy\")\n\
             load(\"//bazel:repositories.bzl\", \"envoy_dependencies\")\n\
             envoy_dependencies()\n\
             load(\"//bazel:python_dependencies.bzl\", \"envoy_python_dependencies\")\n\
             envoy_python_dependencies()\n",
        ),
        (
            "bazel/repositories.bzl",
            "load(\"@envoy_api//bazel:envoy_http_archive.bzl\", \"envoy_http_archive\")\n\
             \n\
             def external_http_archive(name, **kwargs):\n\
             \x20   envoy_http_archive(name, locations = REPOSITORY_LOCATIONS, **kwargs)\n\
             \n\
             def _com_google_protobuf():\n\
             \x20   external_http_archive(\n\
             \x20       \"com_google_protobuf\",\n\
             \x20       patches = [\"@envoy//bazel:protobuf.patch\"],\n\
             \x20   )\n\
             \n\
             def envoy_dependencies():\n\
             \x20   local_repository(name = \"envoy_api\", path = \"api\")\n\
             \x20   _com_google_protobuf()\n",
        ),
        (
            "bazel/python_dependencies.bzl",
            "load(\"@rules_python//python:pip.bzl\", \"pip_parse\")\n\
             \n\
             def envoy_python_dependencies():\n\
             \x20   pip_parse(\n\
             \x20       name = \"base_pip3\",\n\
             \x20       requirements_lock = \"@envoy//tools/base:requirements.txt\",\n\
             \x20   )\n\
             \x20   pip_parse(\n\
             \x20       name = \"dev_pip3\",\n\
             \x20       requirements_lock = \"@envoy//tools/dev:requirements.txt\",\n\
             \x20   )\n",
        ),
    ]);
    let alias = std::env::var("CAMBIUM_CORPUS_ALIAS").unwrap_or_else(|_| "envoy".into());
    // `CAMBIUM_CORPUS_INDEX=<file>`: the mapping a replay's `--rederive`
    // builds, from the stored index's canonical column, instead of the
    // bare identity.
    let m = match std::env::var("CAMBIUM_CORPUS_INDEX") {
        Ok(f) => {
            let file = crate::IndexFile::open(std::path::Path::new(&f)).expect("index file");
            let v = file.view();
            let limit: usize = std::env::var("CAMBIUM_CORPUS_MAP_LIMIT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(usize::MAX);
            let pick = std::env::var("CAMBIUM_CORPUS_MAP_PICK").unwrap_or_default();
            RepoMapping::from_canonical(
                (0..v.num_repos())
                    .filter(|&r| pick.is_empty() || v.repo_name(r).contains(pick.as_str()))
                    .take(limit)
                    .map(|r| (v.repo_name(r), v.repo_canonical.get(r))),
            )
        }
        Err(_) => RepoMapping::identity(),
    }
    .with_main_alias(&alias);
    println!(
        "mapping: empty {}, identity {}",
        m.is_empty(),
        m.is_identity()
    );
    let ri = inputs(&files, &m);

    assert_eq!(
        ri.repos_for("tools/base/requirements.txt"),
        set(&["base_pip3"])
    );
    assert_eq!(
        ri.repos_for("tools/dev/requirements.txt"),
        set(&["dev_pip3"])
    );
    // A patch passed positionally to a forwarding wrapper.
    assert_eq!(
        ri.repos_for("bazel/protobuf.patch"),
        set(&["com_google_protobuf"])
    );
    // `local_repository` is the whole of `@envoy_api`, so an edit under
    // `api/` that is not a `.bzl` the closure loads cannot have moved one.
    assert_eq!(ri.repos_for("api/envoy/x.proto"), set(&["envoy_api"]));
    assert!(ri.path_maps["envoy_api"]
        .iter()
        .all(|pm| pm.exhaustive && pm.from == "api/"));
    // And a `.bzl` change names a set rather than nothing at all.
    assert_eq!(
        ri.bzl_repos["bazel/python_dependencies.bzl"],
        set(&["base_pip3", "dev_pip3"])
    );
}

/// A name built by string arithmetic cannot be evaluated, so the enclosing
/// scope keeps the whole-file answer rather than a guess. The def is
/// called from `WORKSPACE`, so it is live; see
/// [`a_def_nothing_calls_reads_nothing_and_a_called_one_keeps_the_fallback`].
#[test]
fn a_dynamic_name_falls_back_to_the_file_set() {
    let files = Files(vec![
        (
            "MODULE.bazel",
            "module(name = \"demo\", version = \"1\")\n\
             e = use_extension(\"//bazel:ext.bzl\", \"e\")\n\
             use_repo(e, \"a\", \"b\")\n",
        ),
        (".bazelrc", "common --enable_bzlmod\n"),
        (
            "WORKSPACE",
            "load(\"//bazel:loose.bzl\", \"loose\")\nloose()\n",
        ),
        ("bazel/ext.bzl", "E = 1\n"),
        (
            "bazel/loose.bzl",
            "def loose():\n\
             \x20   for p in PLATFORMS:\n\
             \x20       http_archive(name = \"proto_\" + p, patches = [\"//bazel:x.patch\"])\n",
        ),
    ]);
    let m = RepoMapping::from_dump("{\"\":\"\",\"demo\":\"\"}");
    let ri = inputs(&files, &m);
    assert_eq!(ri.repos_for("bazel/x.patch"), set(&["a", "b"]));
}

/// apollo's `clean_dep(dep)` is `str(Label(dep))`: a helper whose only
/// call forwards its parameter into a pure builtin. `Label(dep)` names no
/// repository, so the helper is not a wrapper, and
/// `build_file = clean_dep("//third_party/absl:absl.BUILD")` is an input
/// of `com_google_absl` rather than a repository called
/// `//third_party/absl:absl.BUILD`.
#[test]
fn a_helper_that_forwards_into_a_builtin_is_not_a_wrapper() {
    let files = Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"apollo\")\n\
             load(\"//third_party/absl:workspace.bzl\", absl = \"repo\")\n\
             absl()\n",
        ),
        (
            "third_party/absl/workspace.bzl",
            "def clean_dep(dep):\n\
             \x20   return str(Label(dep))\n\
             \n\
             def repo():\n\
             \x20   native.new_local_repository(\n\
             \x20       name = \"com_google_absl\",\n\
             \x20       build_file = clean_dep(\"//third_party/absl:absl.BUILD\"),\n\
             \x20       path = \"/opt/apollo/absl/\",\n\
             \x20   )\n",
        ),
    ]);
    let m = RepoMapping::identity();
    let ri = inputs(&files, &m);
    assert_eq!(
        ri.repos_for("third_party/absl/absl.BUILD"),
        set(&["com_google_absl"])
    );
    assert!(
        !ri.repos.iter().any(|r| r.starts_with("//")),
        "{:?}",
        ri.repos
    );
}

/// A label written at module level is attributed the way it always was:
/// the file's owner set. Nothing about the occurrence says more.
#[test]
fn a_module_level_label_keeps_the_old_answer() {
    let files = Files(vec![
        (
            "MODULE.bazel",
            "module(name = \"demo\", version = \"1\")\n\
             e = use_extension(\"//bazel:ext.bzl\", \"e\")\n\
             use_repo(e, \"a\", \"b\")\n",
        ),
        (
            "bazel/ext.bzl",
            "CONF = attr.label(default = \"@demo//conf:settings.yml\")\n",
        ),
    ]);
    let m = RepoMapping::from_dump("{\"\":\"\",\"demo\":\"\"}");
    let ri = inputs(&files, &m);
    assert_eq!(ri.repos_for("conf/settings.yml"), set(&["a", "b"]));
}

#[test]
fn an_exhaustive_path_map_round_trips_through_the_index() {
    let pm = PathMap::exhaustive_dir("api/".into());
    assert_eq!(PathMap::decode(&pm.encode()), Some(pm.clone()));
    assert!(pm.encode().ends_with("\u{0}!"));
    // The two-field form every index written before the marker existed
    // holds reads back as the wide answer.
    assert_eq!(
        PathMap::decode("api/\u{0}"),
        Some(PathMap {
            from: "api/".into(),
            to: String::new(),
            exhaustive: false,
        })
    );
}

/// Run against a working tree of the envoy corpus:
///
/// ```text
/// CAMBIUM_CORPUS_DIR=/somewhere cargo test -p cambium-index \
///     -- --ignored --nocapture corpus_attribution
/// ```
#[test]
#[ignore = "needs a working tree of the corpus; see the doc comment"]
fn corpus_attribution() {
    let dir = std::env::var("CAMBIUM_CORPUS_DIR").expect("set CAMBIUM_CORPUS_DIR");
    let files = Dir(dir.into());
    let alias = std::env::var("CAMBIUM_CORPUS_ALIAS").unwrap_or_else(|_| "envoy".into());
    // `CAMBIUM_CORPUS_INDEX=<file>`: the mapping a replay's `--rederive`
    // builds, from the stored index's canonical column, instead of the
    // bare identity.
    let m = match std::env::var("CAMBIUM_CORPUS_INDEX") {
        Ok(f) => {
            let file = crate::IndexFile::open(std::path::Path::new(&f)).expect("index file");
            let v = file.view();
            let limit: usize = std::env::var("CAMBIUM_CORPUS_MAP_LIMIT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(usize::MAX);
            let pick = std::env::var("CAMBIUM_CORPUS_MAP_PICK").unwrap_or_default();
            RepoMapping::from_canonical(
                (0..v.num_repos())
                    .filter(|&r| pick.is_empty() || v.repo_name(r).contains(pick.as_str()))
                    .take(limit)
                    .map(|r| (v.repo_name(r), v.repo_canonical.get(r))),
            )
        }
        Err(_) => RepoMapping::identity(),
    }
    .with_main_alias(&alias);
    println!(
        "mapping: empty {}, identity {}",
        m.is_empty(),
        m.is_identity()
    );
    let ri = inputs(&files, &m);
    println!("repos in the universe: {}", ri.repos.len());
    let facts = read_live_module_facts(&files, &m);
    println!(
        "module facts: {} deps, {} extensions, unparsed {:?}",
        facts.deps.len(),
        facts.extensions.len(),
        facts.unparsed
    );
    for (k, e) in &facts.extensions {
        if e.bzl.contains("rocm") || e.bzl.contains("third_party.bzl") {
            println!("   extension {k}: {} {} -> {:?}", e.bzl, e.name, e.repos);
        }
    }
    let probe = std::env::var("CAMBIUM_CORPUS_PATHS").unwrap_or_else(|_| {
        ".github/config.yml,source/extensions/extensions_build_config.bzl,\
         bazel/external/cargo/remote/defs.bzl,tools/base/requirements.txt,\
         api/envoy/config/core/v3/base.proto"
            .to_string()
    });
    for p in probe.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        for (pat, repos) in &ri.patterns {
            if pat.matches(p) {
                println!("   pattern {pat:?} -> {repos:?}");
            }
        }
        let repos = ri.repos_for(p);
        println!(
            "{p}: {} repo(s) {:?}",
            repos.len(),
            repos.iter().take(12).collect::<Vec<_>>()
        );
    }
    for (repo, maps) in &ri.path_maps {
        println!(
            "path map {repo}: {:?}",
            maps.iter()
                .map(|m| (m.from.as_str(), m.to.as_str(), m.exhaustive))
                .collect::<Vec<_>>()
        );
    }
    for p in probe.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some(r) = ri.bzl_repos.get(p) {
            println!(
                "bzl_repos {p}: {} repo(s) {:?}",
                r.len(),
                r.iter().take(8).collect::<Vec<_>>()
            );
        }
    }
}

// ---- what a change to one workspace `.bzl` moves ----

/// apollo's shape: `tools/workspace.bzl` loads a hundred
/// `third_party/*/init.bzl` files and calls each one's `repo()`. Its own
/// set is therefore everything apollo declares — a change to it really
/// does move the world — but that must not be handed to each init file.
/// Calling `pcl()` does exactly what `pcl`'s `init()` does, so the init
/// file answers for its own repository and no other.
#[test]
fn a_loaded_file_answers_for_what_its_own_symbol_does() {
    let files = Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"apollo\")\n\
             load(\"//tools:workspace.bzl\", \"apollo_repositories\")\n\
             apollo_repositories()\n",
        ),
        (
            "tools/workspace.bzl",
            "load(\"//third_party/pcl:init.bzl\", pcl = \"init\")\n\
             load(\"//third_party/absl:init.bzl\", absl = \"init\")\n\
             \n\
             def apollo_repositories():\n\
             \x20   pcl()\n\
             \x20   absl()\n",
        ),
        (
            "third_party/pcl/init.bzl",
            "load(\"//third_party/pcl:pcl_configure.bzl\", \"pcl_configure\")\n\
             \n\
             def init():\n\
             \x20   pcl_configure(name = \"local_config_pcl\")\n",
        ),
        (
            "third_party/pcl/pcl_configure.bzl",
            "def _impl(repository_ctx):\n\
             \x20   repository_ctx.template(\"BUILD\", repository_ctx.attr.tpl)\n\
             \n\
             pcl_configure = repository_rule(\n\
             \x20   implementation = _impl,\n\
             \x20   attrs = {\"tpl\": attr.label(default = \"//third_party/pcl:BUILD.tpl\")},\n\
             )\n",
        ),
        (
            "third_party/absl/init.bzl",
            "def init():\n\
             \x20   http_archive(name = \"com_google_absl\", build_file = \"//third_party/absl:absl.BUILD\")\n",
        ),
    ]);
    let ri = inputs(&files, &RepoMapping::identity());

    // The root file declares everything, and says so.
    let root = &ri.bzl_repos["tools/workspace.bzl"];
    assert!(
        root.contains("local_config_pcl") && root.contains("com_google_absl"),
        "{root:?}"
    );
    // Each init file answers for its own repository alone.
    assert_eq!(
        ri.bzl_repos["third_party/pcl/init.bzl"],
        set(&["local_config_pcl"])
    );
    assert_eq!(
        ri.bzl_repos["third_party/absl/init.bzl"],
        set(&["com_google_absl"])
    );
    // And so does the rule's own `.bzl`, whose `attr.label` default is an
    // input of that repository and nothing else.
    assert_eq!(
        ri.bzl_repos["third_party/pcl/pcl_configure.bzl"],
        set(&["local_config_pcl"])
    );
    assert_eq!(
        ri.repos_for("third_party/pcl/BUILD.tpl"),
        set(&["local_config_pcl"])
    );
    assert_eq!(
        ri.repos_for("third_party/absl/absl.BUILD"),
        set(&["com_google_absl"])
    );
}

/// A constant read across files. `tools/platform/common.bzl` exports it,
/// one init file uses it inside a call that names its repository outright,
/// and that use site is the whole of what a change to the constant can
/// move.
#[test]
fn a_constant_is_answered_by_where_it_is_read() {
    let files = Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"apollo\")\n\
             load(\"//third_party/pcl:init.bzl\", pcl = \"init\")\n\
             pcl()\n\
             load(\"//third_party/absl:init.bzl\", absl = \"init\")\n\
             absl()\n",
        ),
        (
            "tools/platform/common.bzl",
            "ARCH = \"x86_64\"\nSYSROOT = \"//tools/platform:sysroot\"\n",
        ),
        (
            "third_party/pcl/init.bzl",
            "load(\"//tools/platform:common.bzl\", \"ARCH\")\n\
             \n\
             def init():\n\
             \x20   http_archive(name = \"local_config_pcl\", arch = ARCH)\n",
        ),
        (
            "third_party/absl/init.bzl",
            "def init():\n\
             \x20   http_archive(name = \"com_google_absl\")\n",
        ),
    ]);
    let ri = inputs(&files, &RepoMapping::identity());
    assert_eq!(
        ri.bzl_repos["tools/platform/common.bzl"],
        set(&["local_config_pcl"]),
        "the file is only read where `local_config_pcl` is declared"
    );
    // A label written at the constant file's module level is an input of
    // the same set, not of every repository in the universe.
    assert_eq!(
        ri.repos_for("tools/platform/sysroot"),
        set(&["local_config_pcl"])
    );
}

/// apollo's `_find_cuda_config(repository_ctx)`: a helper that declares
/// nothing, reads one main-repo file, and is called from a repository
/// rule's implementation. Its body belongs to the repositories that rule
/// creates.
#[test]
fn a_helper_that_declares_nothing_is_answered_by_its_callers() {
    let files = Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"apollo\")\n\
             load(\"//third_party/gpus:cuda_configure.bzl\", \"cuda_configure\")\n\
             cuda_configure(name = \"local_config_cuda\")\n\
             load(\"//third_party/absl:init.bzl\", \"init\")\n\
             init()\n",
        ),
        (
            "third_party/gpus/cuda_configure.bzl",
            "def _find_cuda_config(repository_ctx):\n\
             \x20   return repository_ctx.read(\
             Label(\"//third_party/gpus:find_cuda_config.py.gz.base64\"))\n\
             \n\
             def _impl(repository_ctx):\n\
             \x20   _find_cuda_config(repository_ctx)\n\
             \n\
             cuda_configure = repository_rule(implementation = _impl)\n",
        ),
        (
            "third_party/absl/init.bzl",
            "def init():\n\
             \x20   http_archive(name = \"com_google_absl\")\n",
        ),
    ]);
    let ri = inputs(&files, &RepoMapping::identity());
    assert_eq!(
        ri.repos_for("third_party/gpus/find_cuda_config.py.gz.base64"),
        set(&["local_config_cuda"])
    );
}

/// A helper nothing in the closure calls says nothing about itself, and
/// keeps the fallback rather than being attributed to no repository.
/// A def nothing in the workspace runs reads nothing: `loose` is imported
/// into `MODULE.bazel` and never called, so its label is a rule attribute
/// at most, not a repository input. The same def called from `WORKSPACE`
/// at module level is live, instantiates nothing itself, and keeps the
/// fallback — the conservative answer for a helper whose caller this
/// analysis cannot place.
#[test]
fn a_def_nothing_calls_reads_nothing_and_a_called_one_keeps_the_fallback() {
    let module = "module(name = \"demo\", version = \"1\")\n\
                  e = use_extension(\"//bazel:ext.bzl\", \"e\")\n\
                  use_repo(e, \"a\", \"b\")\n\
                  load(\"//bazel:loose.bzl\", \"loose\")\n";
    let loose = "def loose(ctx):\n\
                 \x20   return ctx.read(Label(\"//conf:settings.yml\"))\n";
    let m = RepoMapping::from_dump("{\"\":\"\",\"demo\":\"\"}");
    let files = Files(vec![
        ("MODULE.bazel", module),
        ("bazel/ext.bzl", "E = 1\n"),
        ("bazel/loose.bzl", loose),
    ]);
    let ri = inputs(&files, &m);
    assert!(
        ri.repos_for("conf/settings.yml").is_empty(),
        "{:?}",
        ri.patterns
    );

    // Both worlds live, as Bazel 7 reads them: the bazelrc says so, or the
    // WORKSPACE alone would make the module file inert.
    let files = Files(vec![
        ("MODULE.bazel", module),
        (".bazelrc", "common --enable_bzlmod\n"),
        (
            "WORKSPACE",
            "load(\"//bazel:loose.bzl\", \"loose\")\nloose(None)\n",
        ),
        ("bazel/ext.bzl", "E = 1\n"),
        ("bazel/loose.bzl", loose),
    ]);
    let ri = inputs(&files, &m);
    assert_eq!(ri.repos_for("conf/settings.yml"), set(&["a", "b"]));
}

/// A BUILD-file macro that a load chain dragged into the workspace
/// closure: `tools/rules/go.bzl` is loaded from a workspace `.bzl` and
/// itself loads `sqlgen.bzl`, whose `go_sql_gen` macro names
/// `//cloud/common/dbfx:go_default_library` in a `deps` list. Nothing in
/// the workspace calls the macro, so the label is not a repository input
/// and its `name =` literals are not repositories. Read with the file's
/// fallback, that one label fed every repository in a 3,831-repository
/// universe, and the walk-time closure over it took a 900k-target graph.
#[test]
fn a_macro_only_build_files_call_feeds_no_repository() {
    let files = Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"av\")\n\
             load(\"//tools:tools_workspace.bzl\", \"tools_workspace\")\n\
             tools_workspace()\n",
        ),
        (
            "tools/tools_workspace.bzl",
            "load(\"//tools/rules:go.bzl\", \"go_versions\")\n\
             def tools_workspace():\n\
             \x20   http_archive(name = \"io_bazel_rules_go\", urls = go_versions())\n\
             \x20   http_archive(name = \"boost\")\n",
        ),
        (
            "tools/rules/go.bzl",
            "load(\"//tools/rules:sqlgen.bzl\", \"go_sql_gen\")\n\
             def go_versions():\n\
             \x20   return [\"https://example/go.zip\"]\n",
        ),
        (
            "tools/rules/sqlgen.bzl",
            "def go_sql_gen(name, srcs):\n\
             \x20   go_library(\n\
             \x20       name = name + \"_lib\",\n\
             \x20       srcs = srcs,\n\
             \x20       deps = [\"//cloud/common/dbfx:go_default_library\"],\n\
             \x20   )\n\
             \x20   native.filegroup(name = \"sqlgen_files\")\n",
        ),
    ]);
    let ri = inputs(&files, &RepoMapping::identity().with_main_alias("av"));
    assert!(
        ri.repos_for("cloud/common/dbfx/go_default_library")
            .is_empty(),
        "{:?}",
        ri.patterns
    );
    assert!(!ri.repos.contains("sqlgen_files"), "{:?}", ri.repos);
    assert_eq!(ri.repos, set(&["io_bazel_rules_go", "boost"]));
}

/// Issue #9: the macro's loader has nothing the workspace runs — a
/// constant and a def nothing calls — and nothing about it is unreadable.
/// The rounds learn nothing about such a file, and it used to be handed
/// the fallback for that alone; every file it imported a symbol from then
/// inherited the universe. A file read in full and found to define nothing
/// defines nothing.
#[test]
fn a_readable_file_that_defines_nothing_is_not_the_universe() {
    let files = Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"av\")\n\
             load(\"//tools:tools_workspace.bzl\", \"tools_workspace\")\n\
             tools_workspace()\n",
        ),
        (
            "tools/tools_workspace.bzl",
            "load(\"//tools/rules:go.bzl\", \"GO_VERSION\")\n\
             def tools_workspace():\n\
             \x20   http_archive(name = \"io_bazel_rules_go\", urls = [GO_VERSION])\n\
             \x20   http_archive(name = \"boost\")\n",
        ),
        (
            "tools/rules/go.bzl",
            "load(\"//tools/rules:sqlgen.bzl\", \"go_sql_gen\")\n\
             GO_VERSION = \"1.22\"\n\
             def go_binary_wrapper(name):\n\
             \x20   go_sql_gen(name = name, srcs = [\"//cloud/common/dbfx:schema.sql\"])\n",
        ),
        (
            "tools/rules/sqlgen.bzl",
            "def go_sql_gen(name, srcs):\n\
             \x20   go_library(\n\
             \x20       name = name + \"_lib\",\n\
             \x20       srcs = srcs,\n\
             \x20       deps = [\"//cloud/common/dbfx:go_default_library\"],\n\
             \x20   )\n",
        ),
    ]);
    let m = RepoMapping::identity().with_main_alias("av");
    let ri = inputs(&files, &m);
    assert!(
        ri.repos_for("cloud/common/dbfx/go_default_library")
            .is_empty(),
        "{:?}",
        ri.patterns
    );
    assert!(
        ri.repos_for("cloud/common/dbfx/schema.sql").is_empty(),
        "{:?}",
        ri.patterns
    );
    // A change to the constant's file moves the one instantiation that
    // reads it; the macro file moves nothing.
    assert_eq!(
        ri.bzl_repos["tools/rules/go.bzl"],
        set(&["io_bazel_rules_go"]),
        "{:?}",
        ri.bzl_repos
    );
    assert!(
        ri.bzl_repos
            .get("tools/rules/sqlgen.bzl")
            .map_or(true, BTreeSet::is_empty),
        "{:?}",
        ri.bzl_repos
    );
}

/// Issue #9's other half: a format-string label whose occurrence is on the
/// fallback — here inside a def that calls something the closure cannot
/// resolve — must not become a prefix over its directory for the whole
/// universe. It is no input at all, as before the prefix existed.
#[test]
fn a_format_string_label_on_the_fallback_is_no_prefix() {
    let files = Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"av\")\n\
             load(\"//build:cfg.bzl\", \"configure\")\n\
             configure()\n",
        ),
        (
            "build/cfg.bzl",
            "load(\"@rules_x//x:defs.bzl\", \"x_register\")\n\
             def configure():\n\
             \x20   http_archive(name = \"a\")\n\
             \x20   http_archive(name = \"b\")\n\
             \x20   x_register(version = \"1\")\n\
             \x20   tpl = Label(\"//build/config/%s.tpl\" % \"x\")\n",
        ),
    ]);
    let m = RepoMapping::identity().with_main_alias("av");
    let ri = inputs(&files, &m);
    assert!(
        !ri.patterns.keys().any(Pattern::is_prefix),
        "{:?}",
        ri.patterns
    );
    assert!(ri.repos_for("build/config/x.tpl").is_empty());
}
