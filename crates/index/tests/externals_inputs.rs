//! Repository inputs: which main-repo paths and targets feed which external
//! repositories, and which world Bazel reads in a tree that carries both.

mod support;

use std::collections::BTreeSet;

use cambium_index::externals::{
    bazelrc_bzlmod, build_repo_inputs, bzlmod_live, main_repo_patterns, read_live_module_facts,
    read_module_facts, read_module_facts_in, Mode, Pattern,
};
use cambium_index::{LoadGraph, RepoMapping};

use support::envoy_mapping;

/// Both files present, and the bazelrc says WORKSPACE: the module file's
/// override is inert, and the repository is whatever the WORKSPACE
/// closure builds.
#[test]
fn a_module_file_bazel_does_not_read_derives_nothing() {
    fn with_rc(rc: Option<&'static str>) -> support::Files {
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
        support::Files(v)
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
    assert!(bzlmod_live(
        &support::Files(vec![("MODULE.bazel", "")]),
        &identity
    ));
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
    let ri = support::inputs(&files, &bzlmod);
    assert!(
        !ri.path_maps.contains_key("envoy_build_config"),
        "{:?}",
        ri.path_maps
    );
}

/// The three states [`bzlmod_live`] collapses to a bool, on the same
/// fixture: a bazelrc that turns bzlmod off is WORKSPACE mode whatever the
/// mapping says; bzlmod on beside a `WORKSPACE` is both worlds; bzlmod on
/// with no `WORKSPACE` is bzlmod alone.
#[test]
fn the_mode_is_decided_once() {
    let identity = RepoMapping::identity();
    let bzlmod = RepoMapping::from_dump("{\"\":\"\",\"x\":\"x+\"}");
    let with = |files: Vec<(&'static str, &'static str)>, m: &RepoMapping| {
        let files = support::Files(files);
        let mode = Mode::detect(&files, m);
        assert_eq!(mode.bzlmod_live(), bzlmod_live(&files, m));
        assert_eq!(
            read_module_facts_in(&files, mode),
            read_live_module_facts(&files, m)
        );
        mode
    };
    let module = ("MODULE.bazel", "bazel_dep(name = \"x\", version = \"1\")\n");
    let workspace = ("WORKSPACE", "");
    assert_eq!(
        with(
            vec![
                module,
                workspace,
                (".bazelrc", "common --noenable_bzlmod\n")
            ],
            &bzlmod
        ),
        Mode::Workspace
    );
    assert_eq!(with(vec![module, workspace], &identity), Mode::Workspace);
    assert_eq!(with(vec![module, workspace], &bzlmod), Mode::Both);
    assert_eq!(
        with(
            vec![module, workspace, (".bazelrc", "build --enable_bzlmod\n")],
            &identity
        ),
        Mode::Both
    );
    assert_eq!(with(vec![module], &identity), Mode::Bzlmod);
    assert_eq!(with(vec![], &identity), Mode::Bzlmod);
    assert!(with(vec![module], &identity).bzlmod_live());
    assert!(!Mode::Workspace.bzlmod_live());
}

#[test]
fn labels_become_patterns() {
    let m = envoy_mapping();
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

#[test]
fn a_repository_rules_labels_become_its_inputs() {
    let module =
        "envoy_repo_ext = use_extension(\"//bazel:extensions.bzl\", \"envoy_repo_extension\")\n\
                  use_repo(envoy_repo_ext, \"envoy_repo\")\n\
                  local_path_override(module_name = \"envoy_api\", path = \"api\")\n\
                  bazel_dep(name = \"envoy_api\", version = \"1.0\")\n";
    let files = support::Files(vec![
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
    let m = envoy_mapping();
    let mut g = LoadGraph::new();
    let ws = g.workspace_closure(&files, &m);
    let facts = read_module_facts(&files);
    let ri = build_repo_inputs(&files, &m, &g, &ws, &facts, &[]);

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
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &RepoMapping::identity());
    assert!(
        ri.repos_for("tools/gen.bzl").is_empty(),
        "{:?}",
        ri.repos_for("tools/gen.bzl")
    );
    assert_eq!(ri.repos_for("config.txt"), support::set(&["gen"]));
    assert_eq!(ri.bzl_repos["tools/gen.bzl"], support::set(&["gen"]));
}

/// `tools/workspace.bzl` loads every `third_party/*/init.bzl` and calls
/// them: its `load()` labels are edges, and the loaded file's own
/// repositories answer for a change to it. Read as inputs, each init file
/// would feed everything the root file can define.
#[test]
fn a_load_label_is_not_a_repository_input() {
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &RepoMapping::identity());
    assert!(
        ri.repos_for("third_party/pcl/init.bzl").is_empty(),
        "{:?}",
        ri.repos_for("third_party/pcl/init.bzl")
    );
    assert_eq!(
        ri.repos_for("third_party/pcl/BUILD.tpl"),
        support::set(&["local_config_pcl"])
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
    let files = support::Files(vec![(
        "WORKSPACE",
        "workspace(name = \"sourcegraph\")\n\
         load(\"@aspect_rules_js//npm:repositories.bzl\", \"npm_translate_lock\")\n\
         npm_translate_lock(\n\
         \x20   name = \"npm\",\n\
         \x20   npmrc = \"//:.npmrc\",\n\
         \x20   pnpm_lock = \"//:pnpm-lock.yaml\",\n\
         )\n",
    )]);
    let ri = support::inputs(&files, &RepoMapping::identity());
    assert_eq!(ri.repos_for("pnpm-lock.yaml"), support::set(&["npm"]));
    assert_eq!(ri.repos_for(".npmrc"), support::set(&["npm"]));
    assert!(ri.target_only.is_empty(), "{:?}", ri.target_only);
}

/// `Label("//third_party/gpus/%s.tpl" % name)` inside a repository rule's
/// implementation: the literal text before the placeholder is an input of
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
    // tensorflow's `Label("//third_party/systemlibs%s.tpl" % tpl)`, where the
    // placeholder carries the `:`. The prefix stops at the literal text, not
    // at the directory above it: `third_party/` would claim the vendored
    // `third_party/xla` as an input of a configure repository.
    assert_eq!(
        main_repo_patterns("//third_party/systemlibs%s.tpl", &RepoMapping::identity()),
        vec![Pattern::Prefix("third_party/systemlibs".into())]
    );
    assert_eq!(
        main_repo_patterns("//third_party/gpus:cuda_%s.tpl", &RepoMapping::identity()),
        vec![Pattern::Prefix("third_party/gpus/cuda_".into())]
    );
    assert!(main_repo_patterns("//%s:foo", &RepoMapping::identity()).is_empty());
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &m);
    assert_eq!(
        ri.repos_for("third_party/gpus/rocm/build_defs.bzl.tpl"),
        support::set(&["local_config_rocm"]),
        "{:?}",
        ri.patterns
    );
}

/// A def that implements a rule nothing instantiates, and is also the
/// helper an instantiated rule's implementation calls: its labels belong
/// to the caller's repositories, not to the universe.
#[test]
fn an_implementation_of_an_uninstantiated_rule_is_still_a_helper() {
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &m);
    assert_eq!(
        ri.repos_for("third_party/gpus/rocm/build_defs.bzl.tpl"),
        support::set(&["local_config_rocm"]),
        "{:?}",
        ri.patterns
    );
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
    let files = support::Files(vec![
        ("MODULE.bazel", module),
        (
            "bazel/extensions.bzl",
            "load(\"//bazel:repo.bzl\", \"envoy_repo\")\n",
        ),
        ("bazel/repo.bzl", repo_bzl),
    ]);
    let m = envoy_mapping();
    let mut g = LoadGraph::new();
    let ws = g.workspace_closure(&files, &m);
    let facts = read_module_facts(&files);
    let ri = build_repo_inputs(&files, &m, &g, &ws, &facts, &[]);
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

/// Adjacent string literals are one string — `"@envoy//tools/base:"
/// "requirements.txt"` is the label Bazel resolves, and a `name` written
/// the same way is the name of the repository. The token-level reader saw
/// two literals, neither of which was a label, and a name it could not
/// evaluate; the tree joins them, so the lock is an input of `base_pip3`
/// and nothing else. Sound in both directions: the joined value is the
/// only one Bazel ever sees.
#[test]
fn adjacent_literals_are_one_string() {
    let files = support::Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"envoy\")\n\
             load(\"//bazel:py.bzl\", \"py\")\n\
             py()\n",
        ),
        (
            "bazel/py.bzl",
            "def py():\n\
             \x20   pip_parse(\n\
             \x20       name = \"base_\" \"pip3\",\n\
             \x20       requirements_lock = \"@envoy//tools/base:\"\n\
             \x20                           \"requirements.txt\",\n\
             \x20   )\n",
        ),
    ]);
    let ri = support::inputs(&files, &RepoMapping::identity().with_main_alias("envoy"));
    assert_eq!(
        ri.repos_for("tools/base/requirements.txt"),
        support::set(&["base_pip3"])
    );
    assert_eq!(ri.repos, support::set(&["base_pip3"]));
}

/// A repository build setting written in an rc file the workspace `.bazelrc`
/// imports is a setting of this build, and the target it names is an input of
/// that repository. envoy keeps families of flags in `bazel/*.rc`, and
/// reading the top-level file alone answered nothing through `@quiche`.
#[test]
fn a_setting_in_an_imported_rc_file_is_an_input_of_its_repository() {
    let files = support::Files(vec![
        (
            "MODULE.bazel",
            "module(name = \"envoy\")\nbazel_dep(name = \"quiche\", version = \"1.0\")\n",
        ),
        (".bazelrc", "import %workspace%/bazel/quiche.rc\n"),
        (
            "bazel/quiche.rc",
            "build --@quiche//:quic_base_impl_lib=//source/common/quic/platform:quic_base_impl_lib\n",
        ),
    ]);
    let m = RepoMapping::from_dump("{\"\":\"\",\"envoy\":\"\",\"quiche\":\"quiche+\"}");
    let ri = support::inputs(&files, &m);
    assert!(ri.repos.contains("quiche"), "{:?}", ri.repos);
    assert_eq!(
        ri.repos_for_label("//source/common/quic/platform:quic_base_impl_lib"),
        ["quiche".to_string()].into_iter().collect::<BTreeSet<_>>()
    );
}
