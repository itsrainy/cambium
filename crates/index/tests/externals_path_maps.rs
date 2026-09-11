//! Path maps: how main-repo paths land inside a repository, and which
//! Starlark shapes are certain enough to derive one from.

mod support;

use cambium_index::externals::{build_repo_inputs, read_module_facts, scan_bzl_path_maps, PathMap};
use cambium_index::{LoadGraph, RepoMapping};

use support::envoy_mapping;

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
        &envoy_mapping(),
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
        &envoy_mapping(),
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
        &envoy_mapping(),
    );
    assert!(maps.symlinks.is_empty(), "{:?}", maps.symlinks);
}

#[test]
fn a_local_path_override_is_the_repositorys_root() {
    let files = support::Files(vec![(
        "MODULE.bazel",
        "module(name = \"envoy\", version = \"1\")\n\
             bazel_dep(name = \"envoy_api\", version = \"1.0\")\n\
             local_path_override(module_name = \"envoy_api\", path = \"api\")\n",
    )]);
    let m = envoy_mapping();
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
    let files = support::Files(vec![
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
