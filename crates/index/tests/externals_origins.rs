//! What an external repository *is*, as far as this tree is concerned: the
//! directory it is built from, the tree BUILD file it overlays, or the BUILD
//! text its own rule writes. See `crates/index/src/externals/pattern.rs`.

mod support;

use cambium_index::externals::{OriginKind, PathMap, Pattern};
use cambium_index::RepoMapping;

use support::{inputs, Files};

/// envoy's `@quiche` before the bzlmod pin: the sources are fetched and the
/// BUILD file is `bazel/external/quiche.BUILD` of this tree.
///
/// Three things have to come out of that, and none of them did before: the
/// origin, a path map putting the file at the repository's package root, and
/// every main-repo label the BUILD file names as a *target-only* input of the
/// repository. The last is what closes the 32 recorded misses — a change that
/// reaches `//source/common/quic/platform:quic_base_impl_lib` moves
/// `@quiche`, and the repository fixpoint takes it from there.
fn envoy_overlay() -> Files {
    Files(vec![
        (
            "MODULE.bazel",
            "module(name = \"envoy\")\n\
             new_local_repository = use_repo_rule(\"//bazel:repo.bzl\", \"new_local_repository\")\n\
             new_local_repository(\n\
             \x20   name = \"quiche\",\n\
             \x20   build_file = \"@envoy//bazel/external:quiche.BUILD\",\n\
             \x20   patches = [\"//bazel/external:quiche.patch\"],\n\
             )\n",
        ),
        ("bazel/repo.bzl", "new_local_repository = 1\n"),
        (
            "bazel/external/quiche.BUILD",
            "cc_library(\n\
             \x20   name = \"quic_base_impl_lib\",\n\
             \x20   deps = [\"//source/common/quic/platform:quic_base_impl_lib\"],\n\
             )\n",
        ),
    ])
}

#[test]
fn an_overlay_repository_is_its_build_file() {
    let ri = inputs(&envoy_overlay(), &RepoMapping::identity());
    let o = &ri.origins["quiche"];
    assert_eq!(o.kind, OriginKind::Overlay);
    assert_eq!(o.dir, "bazel/external/quiche.BUILD");
    assert_eq!(o.evidence, "MODULE.bazel");
    // Neither column is written by any build at this step; A3 and Stage B
    // fill them.
    assert!(!o.confirmed);
    assert!(!o.indexed);

    // The BUILD file lands at the repository's package root, and the map is
    // not exhaustive: the sources come from somewhere this tree cannot see,
    // so a path it does not cover says nothing about the repository.
    let maps = &ri.path_maps["quiche"];
    assert!(maps.contains(&PathMap {
        from: "bazel/external/quiche.BUILD".to_string(),
        to: "BUILD.bazel".to_string(),
        exhaustive: false,
    }));
    assert!(!maps.iter().any(|m| m.exhaustive));
}

#[test]
fn the_labels_of_an_overlay_build_file_are_target_only_inputs() {
    let ri = inputs(&envoy_overlay(), &RepoMapping::identity());
    let label = Pattern::Label("//source/common/quic/platform:quic_base_impl_lib".to_string());
    assert_eq!(
        ri.patterns.get(&label).cloned().unwrap_or_default(),
        support::set(&["quiche"]),
        "the BUILD file's label is an input of the repository it builds"
    );
    // Target-only: a file of that package can move the repository's targets
    // and can never be a `.bzl` the repository exports.
    assert!(ri.target_only.contains(&label));
    assert_eq!(
        ri.repos_for_label("//source/common/quic/platform:quic_base_impl_lib"),
        support::set(&["quiche"])
    );
}

/// `patches` is a label too, and the reader must not mistake one for the
/// BUILD file: a patch at the package root would route every edit under it
/// as if it were the repository's BUILD.
#[test]
fn a_patch_label_mints_no_build_file_map() {
    let ri = inputs(&envoy_overlay(), &RepoMapping::identity());
    assert!(
        !ri.path_maps["quiche"]
            .iter()
            .any(|m| m.from.ends_with(".patch")),
        "a patch is a definition input, not the package's BUILD file"
    );
}

/// envoy's `@envoy_repo`: the rule writes the BUILD file itself.
#[test]
fn a_repository_that_writes_its_own_build_file_is_generated() {
    let files = Files(vec![
        (
            "MODULE.bazel",
            "module(name = \"envoy\")\n\
             ext = use_extension(\"//bazel:ext.bzl\", \"ext\")\n\
             use_repo(ext, \"envoy_repo\")\n",
        ),
        (
            "bazel/ext.bzl",
            "def _impl(rctx):\n\
             \x20   rctx.file(\"BUILD\", \"genrule(name = 'project', tools = ['@envoy//changelogs'])\")\n\
             \n\
             envoy_repo = repository_rule(implementation = _impl)\n\
             \n\
             def _ext_impl(mctx):\n\
             \x20   envoy_repo(name = \"envoy_repo\")\n\
             \n\
             ext = module_extension(implementation = _ext_impl)\n",
        ),
    ]);
    let ri = inputs(&files, &RepoMapping::identity());
    let o = &ri.origins["envoy_repo"];
    assert_eq!(o.kind, OriginKind::Generated);
    assert_eq!(o.evidence, "bazel/ext.bzl");
    assert_eq!(
        o.dir, "",
        "a generated repository is no directory of this tree"
    );
    // The label inside the BUILD text it writes stays what it was: an input
    // of the repository's targets and of nothing else.
    assert!(ri
        .target_only
        .contains(&Pattern::Label("//changelogs:changelogs".to_string())));
}

/// A literal that is not a BUILD file name claims nothing: a rule writing
/// `rctx.file("defs.bzl", ...)` is not a repository whose BUILD this tree
/// holds.
#[test]
fn only_a_build_file_name_makes_a_repository_generated() {
    let files = Files(vec![
        (
            "MODULE.bazel",
            "module(name = \"envoy\")\n\
             ext = use_extension(\"//bazel:ext.bzl\", \"ext\")\n\
             use_repo(ext, \"gen\")\n",
        ),
        (
            "bazel/ext.bzl",
            "def _impl(rctx):\n\
             \x20   rctx.file(\"defs.bzl\", \"X = ['@envoy//changelogs']\")\n\
             \n\
             gen = repository_rule(implementation = _impl)\n\
             \n\
             def _ext_impl(mctx):\n\
             \x20   gen(name = \"gen\")\n\
             \n\
             ext = module_extension(implementation = _ext_impl)\n",
        ),
    ]);
    let ri = inputs(&files, &RepoMapping::identity());
    assert_eq!(
        ri.origins.get("gen").map(|o| o.kind),
        None,
        "nothing was derived about its shape"
    );
}

/// `local_path_override` is the shape that already had a path map; it now
/// says what the repository is as well.
#[test]
fn a_local_path_override_is_a_directory_of_this_tree() {
    let files = Files(vec![(
        "MODULE.bazel",
        "module(name = \"envoy\")\n\
         bazel_dep(name = \"envoy_api\", version = \"0.0.0\")\n\
         local_path_override(module_name = \"envoy_api\", path = \"api\")\n\
         bazel_dep(name = \"fmt\", version = \"1.0\")\n",
    )]);
    let ri = inputs(&files, &RepoMapping::identity());
    let o = &ri.origins["envoy_api"];
    assert_eq!(o.kind, OriginKind::Dir);
    assert_eq!(o.dir, "api/");
    assert_eq!(o.evidence, "MODULE.bazel");
    // A plain `bazel_dep` says nothing about this tree, so it gets neither a
    // row nor an origin.
    assert!(!ri.repos.contains("fmt"));
    assert!(!ri.origins.contains_key("fmt"));
}

/// A `local_repository` written in a closure `.bzl` names its own repository
/// and its own directory, and the evidence says which file said so.
#[test]
fn a_closure_local_repository_is_a_directory_of_this_tree() {
    let files = Files(vec![
        (
            "WORKSPACE",
            "workspace(name = \"main\")\nload(\"//bazel:deps.bzl\", \"deps\")\ndeps()\n",
        ),
        (
            "bazel/deps.bzl",
            "def deps():\n\
             \x20   local_repository(name = \"vend\", path = \"vendored\")\n",
        ),
    ]);
    let ri = inputs(&files, &RepoMapping::identity());
    let o = &ri.origins["vend"];
    assert_eq!(o.kind, OriginKind::Dir);
    assert_eq!(o.dir, "vendored/");
    assert_eq!(o.evidence, "bazel/deps.bzl");
}
