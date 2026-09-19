//! Per-occurrence attribution: which repositories a label in a
//! workspace-closure file feeds, and what a change to one such file moves.

mod support;

use std::collections::BTreeSet;

use cambium_index::externals::{
    build_repo_inputs, read_live_module_facts, read_module_facts, Pattern,
};
use cambium_index::{LoadGraph, RepoMapping};

#[test]
fn an_unattributable_workspace_bzl_falls_back_to_every_repository() {
    // `setup` comes from a repository this closure cannot read into, so
    // the module-level call to it is opaque and the file keeps the wide
    // answer. Without that call the same file would be read in full and
    // its constant would be an input of nothing; see
    // [`a_readable_file_that_defines_nothing_is_not_the_universe`].
    let files = support::Files(vec![
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
    let ri = build_repo_inputs(&files, &m, &g, &ws, &facts, &[]);
    assert_eq!(
        ri.repos_for("data/thing.txt"),
        ["r".to_string()].into_iter().collect()
    );
}

/// envoy at 7ba51266cc60: `//bazel:extensions.bzl` declares five module
/// extensions, and `bazel/repo.bzl` — which it loads — declares one
/// repository rule whose `attr.label` defaults name three main-repo files.
/// Per file, all three fed all six repositories, including `envoy_repo`,
/// whose generated `compiler.bzl` sits in eleven hundred package load
/// closures; per occurrence they feed `envoy_repo` alone.
#[test]
fn the_bzlmod_shape_attributes_an_attr_default_to_its_own_rule() {
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &m);

    assert_eq!(
        ri.repos_for(".github/config.yml"),
        support::set(&["envoy_repo"])
    );
    assert_eq!(
        ri.repos_for("source/extensions/extensions_build_config.bzl"),
        support::set(&["envoy_build_config"])
    );
    assert_eq!(
        ri.repos_for("bazel/external/cargo/remote/defs.bzl"),
        support::set(&["cu"])
    );
    // The generated BUILD file's `tools` still name the repository — as a
    // target, whose own files the graph knows — and still cannot have
    // moved a `.bzl` inside it.
    assert_eq!(
        ri.repos_for_label("//changelogs:changelogs"),
        support::set(&["envoy_repo"])
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
    let files = support::Files(vec![
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
    let m = support::corpus_mapping();
    let ri = support::inputs(&files, &m);

    assert_eq!(
        ri.repos_for("tools/base/requirements.txt"),
        support::set(&["base_pip3"])
    );
    assert_eq!(
        ri.repos_for("tools/dev/requirements.txt"),
        support::set(&["dev_pip3"])
    );
    // A patch passed positionally to a forwarding wrapper.
    assert_eq!(
        ri.repos_for("bazel/protobuf.patch"),
        support::set(&["com_google_protobuf"])
    );
    // `local_repository` is the whole of `@envoy_api`, so an edit under
    // `api/` that is not a `.bzl` the closure loads cannot have moved one.
    assert_eq!(
        ri.repos_for("api/envoy/x.proto"),
        support::set(&["envoy_api"])
    );
    assert!(ri.path_maps["envoy_api"]
        .iter()
        .all(|pm| pm.exhaustive && pm.from == "api/"));
    // And a `.bzl` change names a set rather than nothing at all.
    assert_eq!(
        ri.bzl_repos["bazel/python_dependencies.bzl"],
        support::set(&["base_pip3", "dev_pip3"])
    );
}

/// A name built by string arithmetic cannot be evaluated, so the enclosing
/// scope keeps the whole-file answer rather than a guess. The def is
/// called from `WORKSPACE`, so it is live; see
/// [`a_def_nothing_calls_reads_nothing_and_a_called_one_keeps_the_fallback`].
#[test]
fn a_dynamic_name_falls_back_to_the_file_set() {
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &m);
    assert_eq!(ri.repos_for("bazel/x.patch"), support::set(&["a", "b"]));
}

/// apollo's `clean_dep(dep)` is `str(Label(dep))`: a helper whose only
/// call forwards its parameter into a pure builtin. `Label(dep)` names no
/// repository, so the helper is not a wrapper, and
/// `build_file = clean_dep("//third_party/absl:absl.BUILD")` is an input
/// of `com_google_absl` rather than a repository called
/// `//third_party/absl:absl.BUILD`.
#[test]
fn a_helper_that_forwards_into_a_builtin_is_not_a_wrapper() {
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &m);
    assert_eq!(
        ri.repos_for("third_party/absl/absl.BUILD"),
        support::set(&["com_google_absl"])
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
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &m);
    assert_eq!(ri.repos_for("conf/settings.yml"), support::set(&["a", "b"]));
}

/// A manual probe, not an assertion: prints what the attribution derives for
/// a real checkout, so a surprising row in a replay can be chased to the
/// occurrence that produced it. Run against a working tree of a corpus
/// subject:
///
/// ```text
/// CAMBIUM_CORPUS_DIR=/somewhere cargo test -p cambium-index \
///     -- --ignored --nocapture corpus_attribution
/// ```
///
/// Environment:
///
/// - `CAMBIUM_CORPUS_DIR` — the checkout to read (required).
/// - `CAMBIUM_CORPUS_PATHS` — comma-separated main-repo paths to probe; the
///   default is a handful of envoy's known inputs.
/// - `CAMBIUM_CORPUS_ALIAS`, `CAMBIUM_CORPUS_INDEX`, `CAMBIUM_CORPUS_MAP_PICK`,
///   `CAMBIUM_CORPUS_MAP_LIMIT` — the repository mapping; see
///   [`support::corpus_mapping`].
#[test]
#[ignore = "needs a working tree of the corpus; see the doc comment"]
fn corpus_attribution() {
    let dir = std::env::var("CAMBIUM_CORPUS_DIR").expect("set CAMBIUM_CORPUS_DIR");
    let files = support::Dir(dir.into());
    let m = support::corpus_mapping();
    println!(
        "mapping: empty {}, identity {}",
        m.is_empty(),
        m.is_identity()
    );
    let ri = support::inputs(&files, &m);
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

/// apollo's shape: `tools/workspace.bzl` loads a hundred
/// `third_party/*/init.bzl` files and calls each one's `repo()`. Its own
/// set is therefore everything apollo declares — a change to it really
/// does move the world — but that must not be handed to each init file.
/// Calling `pcl()` does exactly what `pcl`'s `init()` does, so the init
/// file answers for its own repository and no other.
#[test]
fn a_loaded_file_answers_for_what_its_own_symbol_does() {
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
    let ri = support::inputs(&files, &RepoMapping::identity());

    // The root file declares everything, and says so.
    let root = &ri.bzl_repos["tools/workspace.bzl"];
    assert!(
        root.contains("local_config_pcl") && root.contains("com_google_absl"),
        "{root:?}"
    );
    // Each init file answers for its own repository alone.
    assert_eq!(
        ri.bzl_repos["third_party/pcl/init.bzl"],
        support::set(&["local_config_pcl"])
    );
    assert_eq!(
        ri.bzl_repos["third_party/absl/init.bzl"],
        support::set(&["com_google_absl"])
    );
    // And so does the rule's own `.bzl`, whose `attr.label` default is an
    // input of that repository and nothing else.
    assert_eq!(
        ri.bzl_repos["third_party/pcl/pcl_configure.bzl"],
        support::set(&["local_config_pcl"])
    );
    assert_eq!(
        ri.repos_for("third_party/pcl/BUILD.tpl"),
        support::set(&["local_config_pcl"])
    );
    assert_eq!(
        ri.repos_for("third_party/absl/absl.BUILD"),
        support::set(&["com_google_absl"])
    );
}

/// A constant read across files. `tools/platform/common.bzl` exports it,
/// one init file uses it inside a call that names its repository outright,
/// and that use site is the whole of what a change to the constant can
/// move.
#[test]
fn a_constant_is_answered_by_where_it_is_read() {
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &RepoMapping::identity());
    assert_eq!(
        ri.bzl_repos["tools/platform/common.bzl"],
        support::set(&["local_config_pcl"]),
        "the file is only read where `local_config_pcl` is declared"
    );
    // A label written at the constant file's module level is an input of
    // the same set, not of every repository in the universe.
    assert_eq!(
        ri.repos_for("tools/platform/sysroot"),
        support::set(&["local_config_pcl"])
    );
}

/// apollo's `_find_cuda_config(repository_ctx)`: a helper that declares
/// nothing, reads one main-repo file, and is called from a repository
/// rule's implementation. Its body belongs to the repositories that rule
/// creates.
#[test]
fn a_helper_that_declares_nothing_is_answered_by_its_callers() {
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &RepoMapping::identity());
    assert_eq!(
        ri.repos_for("third_party/gpus/find_cuda_config.py.gz.base64"),
        support::set(&["local_config_cuda"])
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
    let files = support::Files(vec![
        ("MODULE.bazel", module),
        ("bazel/ext.bzl", "E = 1\n"),
        ("bazel/loose.bzl", loose),
    ]);
    let ri = support::inputs(&files, &m);
    assert!(
        ri.repos_for("conf/settings.yml").is_empty(),
        "{:?}",
        ri.patterns
    );

    // Both worlds live, as Bazel 7 reads them: the bazelrc says so, or the
    // WORKSPACE alone would make the module file inert.
    let files = support::Files(vec![
        ("MODULE.bazel", module),
        (".bazelrc", "common --enable_bzlmod\n"),
        (
            "WORKSPACE",
            "load(\"//bazel:loose.bzl\", \"loose\")\nloose(None)\n",
        ),
        ("bazel/ext.bzl", "E = 1\n"),
        ("bazel/loose.bzl", loose),
    ]);
    let ri = support::inputs(&files, &m);
    assert_eq!(ri.repos_for("conf/settings.yml"), support::set(&["a", "b"]));
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
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &RepoMapping::identity().with_main_alias("av"));
    assert!(
        ri.repos_for("cloud/common/dbfx/go_default_library")
            .is_empty(),
        "{:?}",
        ri.patterns
    );
    assert!(!ri.repos.contains("sqlgen_files"), "{:?}", ri.repos);
    assert_eq!(ri.repos, support::set(&["io_bazel_rules_go", "boost"]));
}

/// Issue #9: the macro's loader has nothing the workspace runs — a
/// constant and a def nothing calls — and nothing about it is unreadable.
/// The rounds learn nothing about such a file, and it used to be handed
/// the fallback for that alone; every file it imported a symbol from then
/// inherited the universe. A file read in full and found to define nothing
/// defines nothing.
#[test]
fn a_readable_file_that_defines_nothing_is_not_the_universe() {
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &m);
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
        support::set(&["io_bazel_rules_go"]),
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
    let files = support::Files(vec![
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
    let ri = support::inputs(&files, &m);
    assert!(
        !ri.patterns.keys().any(Pattern::is_prefix),
        "{:?}",
        ri.patterns
    );
    assert!(ri.repos_for("build/config/x.tpl").is_empty());
}

/// apollo's `cuda_configure`: its implementation declares `cuda-lib` by
/// name beside the `local_config_cuda` it was instantiated as. Instantiating
/// the rule creates both, so the file that calls it — and the constant it
/// reads — answer for both, and a template the implementation resolves is
/// an input of both.
#[test]
fn a_rules_implementation_declares_its_side_effects() {
    let files = support::Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"apollo\")\n\
             load(\"//third_party/gpus:init.bzl\", \"init\")\n\
             init()\n",
        ),
        (
            "third_party/gpus/init.bzl",
            "load(\"//third_party/gpus:cuda_configure.bzl\", \"cuda_configure\")\n\
             \n\
             def init():\n\
             \x20   cuda_configure(name = \"local_config_cuda\")\n",
        ),
        (
            "third_party/gpus/cuda_configure.bzl",
            "def _impl(repository_ctx):\n\
             \x20   repository_ctx.template(\"BUILD\", Label(\"//third_party/gpus:cuda.BUILD.tpl\"))\n\
             \x20   http_archive(name = \"cuda-lib\")\n\
             \n\
             cuda_configure = repository_rule(implementation = _impl)\n",
        ),
    ]);
    let ri = support::inputs(&files, &RepoMapping::identity());
    assert_eq!(
        ri.bzl_repos["third_party/gpus/init.bzl"],
        support::set(&["local_config_cuda", "cuda-lib"])
    );
    assert_eq!(
        ri.repos_for("third_party/gpus/cuda.BUILD.tpl"),
        support::set(&["local_config_cuda", "cuda-lib"])
    );
}
