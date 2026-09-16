//! The layer's soundness checks, over real `bazel dump --skyframe=deps` output.
//!
//! The two fixtures are verbatim dumps of one small workspace, taken on Bazel
//! 9.2.0 and 7.7.0 with only the checkout path and the output base shortened.
//! The workspace is shaped to hold every chain the dirty stage has to follow:
//!
//! ```text
//! a/BUILD        filegroup(srcs = ["sub/x.cc"])   # an explicit sub-directory
//!                filegroup(srcs = glob(["*.txt"]))
//!                mylib(name = "m")                # from a/rules.bzl
//! a/rules.bzl    load(":helper.bzl", "H")         # loaded only by a .bzl
//! a/helper.bzl
//! a/sub/x.cc     a/one.txt
//! ```
//!
//! Keeping both majors matters: Bazel 7 spells the glob node `GLOB` (one per
//! pattern) and the ignore node `IGNORED_PACKAGE_PREFIXES`, Bazel 8 and 9 spell
//! them `GLOBS` and `IGNORED_SUBDIRECTORIES`. Every check below runs against
//! both, so a spelling that moves shows up as a failure rather than as a
//! silently empty walk.

use super::*;

const BAZEL9: &str = include_str!("fixtures/bazel9.dump");
const BAZEL7: &str = include_str!("fixtures/bazel7.dump");

/// Both fixtures, so every check runs on both majors.
fn layers() -> Vec<(&'static str, SkyLayer)> {
    [("bazel 9.2.0", BAZEL9), ("bazel 7.7.0", BAZEL7)]
        .into_iter()
        .map(|(name, text)| {
            let mut l = SkyLayer::new();
            let seen = l.apply_dump(text, &Paths::new("/ws", "/ob"));
            assert!(seen > 20, "{name}: only {seen} blocks");
            (name, l)
        })
        .collect()
}

/// The frozen tables, which is what a run actually walks.
fn tables() -> Vec<(&'static str, SkyTables)> {
    layers()
        .into_iter()
        .map(|(n, l)| (n, l.finish()))
        .collect()
}

fn view(t: &SkyTables) -> SkyRef<'_> {
    t.as_ref()
}

/// `(path, structural)` pairs, the shape [`seeds`] takes.
fn walk_of(v: SkyRef<'_>, changed: &[(&str, bool)]) -> SkyWalk {
    v.walk(&seeds(changed.iter().copied()))
}

/// Check (1). A new `BUILD` carving a sub-package out of its parent must dirty
/// the parent, and the dump has to be the thing that says so.
///
/// The workspace is deliberately the hard case: `a/BUILD` names `sub/x.cc` by
/// hand, and its only glob is `*.txt`, which does not descend. Under the rule
/// this layer replaces, the classifier dirtied the parent from the shape of
/// the path alone. Here `PACKAGE:a` depends on `PACKAGE_LOOKUP:a/sub` because
/// Bazel had to decide whether `sub/x.cc` was still `a`'s file, and that edge
/// is what the walk follows.
#[test]
fn a_new_sub_package_dirties_the_parent_it_carves_out_of() {
    for (name, t) in tables() {
        let w = walk_of(view(&t), &[("a/sub/BUILD", true)]);
        assert!(w.packages.contains("a"), "{name}: {w:?}");
        // And the new package itself, through the lookup that found nothing
        // there when the index was built.
        assert!(w.lookups.contains("a/sub"), "{name}: {w:?}");
    }
}

/// Check (2). Deleting the `BUILD` file returns the sub-directory's files to
/// the parent, which is the same edge read the other way: the seed is the same
/// `FILE_STATE`, so the walk cannot tell the two apart and does not try.
#[test]
fn a_deleted_build_file_returns_its_files_to_the_parent() {
    for (name, t) in tables() {
        let w = walk_of(view(&t), &[("a/sub/BUILD", true)]);
        assert!(w.packages.contains("a"), "{name}: {w:?}");
    }
}

/// Check (3). `a/helper.bzl` is loaded by no `BUILD` file at all: only
/// `a/rules.bzl` loads it. The chain is `FILE_STATE -> FILE -> BZL_LOAD
/// (helper) -> BZL_LOAD (rules) -> PACKAGE:a`, and the `BZL_LOAD -> BZL_LOAD`
/// hop is the one the static load closure existed to model.
#[test]
fn a_bzl_loaded_only_by_another_bzl_reaches_the_packages_behind_it() {
    for (name, t) in tables() {
        let w = walk_of(view(&t), &[("a/helper.bzl", false)]);
        assert!(w.packages.contains("a"), "{name}: {w:?}");
        // And it is a package the *Starlark* half reached, which is what
        // marks its rules direct: a rule implementation can change without
        // any rule proto moving.
        assert!(w.bzl_packages.contains("a"), "{name}: {w:?}");
    }
}

/// The counterpart: a `BUILD` edit reaches the package without going through
/// any `BZL_LOAD`, so its rules are judged on their hashes as before.
#[test]
fn a_build_edit_is_not_a_starlark_reach() {
    for (name, t) in tables() {
        let w = walk_of(view(&t), &[("a/BUILD", false)]);
        assert!(w.packages.contains("a"), "{name}: {w:?}");
        assert!(w.bzl_packages.is_empty(), "{name}: {w:?}");
    }
}

/// Check (4). A file added to a directory a `glob` covers dirties the package
/// through the directory listing, with no rule about globs anywhere.
#[test]
fn a_file_added_to_a_globbed_directory_dirties_the_package() {
    for (name, t) in tables() {
        let w = walk_of(view(&t), &[("a/two.txt", true)]);
        assert!(w.packages.contains("a"), "{name}: {w:?}");
        // The path itself is new, so it has no `FILE_STATE` node: what
        // reached the package is the listing of the directory it landed in.
        assert_eq!(w.unknown_seeds, 1, "{name}: {w:?}");
    }
}

/// Check (5). Editing a source file's *contents* moves no loading-phase node.
/// Its consumers are reached through the rule layer's file rows, which is a
/// different question and a different table.
#[test]
fn a_source_content_change_dirties_nothing_in_the_layer() {
    for (name, t) in tables() {
        let w = walk_of(view(&t), &[("a/one.txt", false), ("a/sub/x.cc", false)]);
        assert!(w.packages.is_empty(), "{name}: {w:?}");
        assert!(w.lookups.is_empty(), "{name}: {w:?}");
    }
}

/// The counterpart: editing the `BUILD` file itself does dirty its package,
/// and only its package.
#[test]
fn a_modified_build_file_dirties_its_own_package() {
    for (name, t) in tables() {
        let w = walk_of(view(&t), &[("a/BUILD", false)]);
        assert_eq!(
            w.packages.iter().cloned().collect::<Vec<_>>(),
            vec!["a".to_string()],
            "{name}: {w:?}"
        );
    }
}

/// `.bazelignore` is the one singleton the layer keeps, and it keeps it
/// precisely so that editing the file says what it means: every package Bazel
/// read the file to load is dirty.
///
/// "Every package" has one exception, and it is Bazel's rather than the
/// layer's: under WORKSPACE, Bazel 7 synthesises a `//external` package from
/// the workspace file alone, and that package consults no ignore list because
/// no directory of the tree is involved in making it. The walk reports what
/// the dump says, so the exception shows up here instead of being smoothed
/// over.
#[test]
fn an_edit_to_bazelignore_reaches_every_package_bazel_read_a_directory_for() {
    for (name, t) in tables() {
        let v = view(&t);
        let w = walk_of(v, &[(".bazelignore", false)]);
        assert!(w.packages.contains("a"), "{name}: {w:?}");
        let loaded: BTreeSet<String> = (0..v.len() as u32)
            .filter(|&i| v.kind_of(i) == Kind::Package && is_main_package(v.text(i)))
            .map(|i| v.text(i).to_string())
            .collect();
        let missed: Vec<&String> = loaded.difference(&w.packages).collect();
        assert!(
            missed.iter().all(|p| p.as_str() == "external"),
            "{name}: {missed:?}"
        );
    }
}

/// A path Bazel's loading phase never looked at reaches nothing, and the walk
/// counts it rather than guessing at it.
#[test]
fn a_path_the_loading_phase_never_saw_is_counted_not_guessed() {
    for (name, t) in tables() {
        let w = walk_of(view(&t), &[("README.md", false)]);
        assert!(w.packages.is_empty(), "{name}: {w:?}");
        assert_eq!(w.unknown_seeds, 1, "{name}: {w:?}");
    }
}

/// Packages of another repository are the externals stage's business, not a
/// package this repository can re-query.
#[test]
fn an_external_package_is_never_a_dirty_main_repo_package() {
    assert!(is_main_package("a/sub"));
    assert!(is_main_package(""));
    assert!(!is_main_package("@@rules_java_builtin//java"));
    assert!(!is_main_package("@foo//bar"));
}

/// The workspace root is erased, and a file under any other root is dropped.
///
/// A file in the output base is another repository's, and nothing a
/// main-repository diff can name; keeping such keys put this machine's own
/// directories in the payload and let the layer accumulate rows a cold build
/// at the same commit would not have. The rebuild oracle is what found that.
#[test]
fn the_workspace_root_is_erased_and_other_roots_are_dropped() {
    let mut l = SkyLayer::new();
    l.apply_dump(
        "FILE_STATE:[/ws]/[a/x]\n\nFILE_STATE:[/elsewhere]/[b/y]\n",
        &Paths::new("/ws", "/ob"),
    );
    let t = l.finish();
    let v = view(&t);
    assert_eq!(v.roots.get(WORKSPACE), "");
    assert_eq!(
        v.id_of(Kind::FileState, "a/x").map(|i| v.root[i as usize]),
        Some(WORKSPACE)
    );
    assert!(v.id_of(Kind::FileState, "b/y").is_none());
    assert_eq!(v.len(), 1);
}

/// The workspace Bazel writes back is not always the one it was handed.
///
/// Bazel resolves symbolic links and prints the real path in `[root]`. On
/// macOS that is the default for anything under `$TMPDIR` (`/var` is a link to
/// `/private/var`), which is where a `cambi profile` worktree lives. Matching
/// on the handed spelling alone dropped every rooted key as "another
/// repository's": the layer kept its `PACKAGE`, `PACKAGE_LOOKUP` and
/// `BZL_LOAD` rows and lost every `FILE_STATE`, so no changed path could enter
/// it and no `BUILD` edit re-queried its package. A tensorflow profile found
/// it (three `BUILD`-editing commits reported as changing nothing); this is
/// that dump, under that path shape.
#[test]
fn a_workspace_reached_through_a_symlink_is_still_this_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("ws");
    std::fs::create_dir(&real).unwrap();
    let real = std::fs::canonicalize(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_ne!(
        link.to_string_lossy(),
        real.to_string_lossy(),
        "the test needs the two spellings to differ"
    );

    // The real Bazel 7.7.0 WORKSPACE-mode dump, rooted where Bazel would root
    // it: at the resolved path.
    let text = BAZEL7.replace("/ws", &real.to_string_lossy());
    let mut l = SkyLayer::new();
    // ...while the run was told about the link.
    l.apply_dump(&text, &Paths::new(link.to_string_lossy(), "/ob"));
    assert_eq!(l.foreign_roots().count(), 0, "nothing was rejected");
    let t = l.finish();
    let v = view(&t);

    assert!(
        v.id_of(Kind::FileState, "a/BUILD").is_some(),
        "the seed a BUILD edit makes has to be in the layer"
    );
    assert!(v.id_of(Kind::Package, "a").is_some());
    let w = walk_of(v, &[("a/BUILD", false)]);
    assert!(w.packages.contains("a"), "{w:?}");
    // And the glob node, whose key embeds the root as text rather than as a
    // `[root]`, is this workspace's too.
    assert!(
        (0..v.len() as u32).any(|i| v.kind_of(i) == Kind::Glob),
        "the glob key embeds the resolved root and was dropped with it"
    );
}

/// The shape of the failure, so the guard above it has something to report: a
/// dump whose file keys are rooted somewhere else leaves a layer with no
/// `FILE_STATE` at all, and names the root it did use.
#[test]
fn a_dump_rooted_elsewhere_keeps_no_file_keys_and_says_where_they_were() {
    let mut l = SkyLayer::new();
    l.apply_dump(&BAZEL7.replace("/ws", "/somewhere/else"), &Paths::new("/ws", "/ob"));
    assert_eq!(l.count_of(Kind::FileState), 0);
    assert_eq!(l.foreign_roots().collect::<Vec<_>>(), ["/somewhere/else"]);
}

/// Another repository's packages, globs and ignore list are dropped with it.
/// A reverse walk that reached one would stop there anyway, and the engine
/// acts on a moved external repository through the externals stage.
///
/// `BZL_LOAD` is the exception, and deliberately: an external `.bzl` can be a
/// main-repository *file* (a `local_path_override` on a module whose sources
/// are in this tree), with main-repository packages above it.
#[test]
fn another_repositorys_packages_are_dropped_and_its_bzl_loads_are_not() {
    let mut l = SkyLayer::new();
    l.apply_dump(
        "PACKAGE:a\n\nPACKAGE:@@ext//pkg\n\nPACKAGE_LOOKUP:@@ext//pkg\n\n\
         IGNORED_SUBDIRECTORIES:@@\n\nIGNORED_PACKAGE_PREFIXES:@@rules_java_builtin\n\n\
         GLOBS:<GlobsKey packageRoot = /ws, packageIdentifier = a>\n\n\
         GLOBS:<GlobsKey packageRoot = /ob/external/ext, packageIdentifier = pkg>\n\n\
         BZL_LOAD:KeyForBuild{label=@@ext//:x.bzl, isBuildPrelude=false}\n",
        &Paths::new("/ws", "/ob"),
    );
    let t = l.finish();
    let v = view(&t);
    assert!(v.id_of(Kind::Package, "a").is_some());
    assert!(v.id_of(Kind::Package, "@@ext//pkg").is_none());
    assert!(v.id_of(Kind::PackageLookup, "@@ext//pkg").is_none());
    assert!(v.id_of(Kind::Ignored, "@@").is_some());
    assert!(v.id_of(Kind::Ignored, "@@rules_java_builtin").is_none());
    let globs: Vec<&str> = (0..v.len() as u32)
        .filter(|&i| v.kind_of(i) == Kind::Glob)
        .map(|i| v.text(i))
        .collect();
    assert_eq!(globs.len(), 1, "{globs:?}");
    assert!(globs[0].contains(WORKSPACE_MARK), "{globs:?}");
    assert_eq!(
        (0..v.len() as u32)
            .filter(|&i| v.kind_of(i) == Kind::BzlLoad)
            .count(),
        1,
        "an external .bzl can be a main-repository file"
    );
}

#[test]
fn an_absolute_path_inside_an_unrooted_key_is_replaced_with_a_placeholder() {
    let mut l = SkyLayer::new();
    l.apply_dump(
        "GLOBS:<GlobsKey packageRoot = /ws, packageIdentifier = a>\n\
         \nBZL_LOAD:KeyForBuild{label=@@x//:f.bzl}\n\
         \nIGNORED_SUBDIRECTORIES:@@\n",
        &Paths::new("/ws", "/ob"),
    );
    let t = l.finish();
    let v = view(&t);
    let globs: Vec<&str> = (0..v.len() as u32)
        .filter(|&i| v.kind_of(i) == Kind::Glob)
        .map(|i| v.text(i))
        .collect();
    assert_eq!(globs.len(), 1);
    assert!(globs[0].contains(WORKSPACE_MARK), "{globs:?}");
    assert!(!globs[0].contains("/ws"), "{globs:?}");
}

/// The two Bazel majors spell three of the kinds differently and mean the same
/// graph by them. The layer's own node counts per kind therefore have to
/// match, kind for kind, over the same workspace.
#[test]
fn the_two_majors_produce_the_same_kinds_over_the_same_workspace() {
    let ts = tables();
    let (n9, t9) = &ts[0];
    let (n7, t7) = &ts[1];
    for (v, name) in [(view(t9), n9), (view(t7), n7)] {
        let kinds: BTreeSet<Kind> = v.counts().into_iter().map(|(k, _)| k).collect();
        for want in [
            Kind::FileState,
            Kind::File,
            Kind::DirectoryListingState,
            Kind::DirectoryListing,
            Kind::Glob,
            Kind::PackageLookup,
            Kind::Package,
            Kind::BzlLoad,
            Kind::Ignored,
        ] {
            assert!(kinds.contains(&want), "{name} has no {}", want.name());
        }
    }
}

/// A refresh replaces a node's row rather than merging into it, so an edge
/// that no longer exists goes away. That is what makes the incremental run's
/// filtered re-dump an exact update instead of an accumulating one.
#[test]
fn a_refresh_replaces_a_row_and_leaves_the_rest_alone() {
    let paths = Paths::new("/ws", "/ob");
    let mut l = SkyLayer::new();
    l.apply_dump(BAZEL9, &paths);
    let before = l.finish();
    let n_before = view(&before).rdeps.row_len(
        view(&before)
            .id_of(Kind::PackageLookup, "a/sub")
            .expect("a/sub lookup"),
    );
    assert_eq!(n_before, 1, "PACKAGE:a is the one node that consulted it");

    let mut l = SkyLayer::new();
    l.apply_dump(BAZEL9, &paths);
    // The package stopped naming `sub/x.cc`, so it no longer looks the
    // directory up and no longer globs.
    l.apply_dump(
        "PACKAGE:a\n  Group 1:\n    PACKAGE_LOOKUP:a\n\n  Group 2:\n    FILE:[/ws]/[a/BUILD]\n\
         \n  Group 3:\n    BZL_LOAD:KeyForBuild{label=//a:rules.bzl, isBuildPrelude=false}\n",
        &paths,
    );
    let after = l.finish();
    let v = view(&after);
    assert_eq!(
        v.rdeps
            .row_len(v.id_of(Kind::PackageLookup, "a/sub").expect("a/sub lookup")),
        0,
        "the refreshed PACKAGE:a row dropped the lookup"
    );
    // A new BUILD under `a/sub` no longer reaches `a`, which is exactly what
    // the refreshed dump says.
    let w = walk_of(v, &[("a/sub/BUILD", true)]);
    assert!(!w.packages.contains("a"), "{w:?}");
    // Everything the refresh did not name is untouched.
    assert!(walk_of(v, &[("a/helper.bzl", false)]).packages.contains("a"));
}

/// A deleted package's `PACKAGE` row is retired, so nothing reaches it and it
/// names no package; its `PACKAGE_LOOKUP` survives, because the directory is
/// still a directory and a later commit re-adding the `BUILD` file moves it.
#[test]
fn a_deleted_package_tombstones_its_row() {
    let mut l = SkyLayer::new();
    l.apply_dump(BAZEL9, &Paths::new("/ws", "/ob"));
    l.delete_package("a");
    let t = l.finish();
    let v = view(&t);
    assert!(v.id_of(Kind::Package, "a").is_none());
    let w = walk_of(v, &[("a/BUILD", false)]);
    assert!(w.packages.is_empty(), "{w:?}");
    assert!(w.lookups.contains("a"), "{w:?}");
}

/// The frozen node order is a function of the layer's content, not of the
/// order the dump's blocks arrived in. Two indexes built from the same graph
/// must have byte-identical payloads; see `crates/engine/tests/store_layout.rs`.
#[test]
fn the_stored_order_does_not_depend_on_the_dumps_order() {
    let paths = Paths::new("/ws", "/ob");
    let forward = {
        let mut l = SkyLayer::new();
        l.apply_dump(BAZEL9, &paths);
        l.finish()
    };
    // The same blocks, in reverse order.
    let reversed_text = {
        let mut blocks: Vec<String> = Vec::new();
        let mut cur = String::new();
        for line in BAZEL9.lines() {
            let opens_a_block =
                !line.is_empty() && !line.starts_with(' ') && !line.starts_with('\t');
            if opens_a_block && !cur.is_empty() {
                blocks.push(std::mem::take(&mut cur));
            }
            cur.push_str(line);
            cur.push('\n');
        }
        blocks.push(cur);
        blocks.reverse();
        blocks.concat()
    };
    let backward = {
        let mut l = SkyLayer::new();
        l.apply_dump(&reversed_text, &paths);
        l.finish()
    };
    assert_eq!(forward.key_bytes, backward.key_bytes);
    assert_eq!(forward.kind, backward.kind);
    assert_eq!(forward.rdep_offsets, backward.rdep_offsets);
    assert_eq!(forward.rdep_edges, backward.rdep_edges);
}

/// A layer read back out of an index is the layer that was written.
#[test]
fn a_layer_round_trips_through_its_stored_form() {
    let mut l = SkyLayer::new();
    l.apply_dump(BAZEL9, &Paths::new("/ws", "/ob"));
    let t = l.finish();
    let back = SkyLayer::from_view(view(&t)).expect("present");
    let again = back.finish();
    assert_eq!(t.key_bytes, again.key_bytes);
    assert_eq!(t.kind, again.kind);
    assert_eq!(t.root, again.root);
    assert_eq!(t.rdep_offsets, again.rdep_offsets);
    assert_eq!(t.rdep_edges, again.rdep_edges);
}

#[test]
fn a_layer_that_was_never_built_answers_nothing_rather_than_empty() {
    let v = SkyRef::EMPTY;
    assert!(!v.present);
    assert_eq!(walk_of(v, &[("a/BUILD", true)]), SkyWalk::default());
}

#[test]
fn seeds_are_the_file_state_and_the_listings_an_add_or_delete_moves() {
    let s = seeds([("a/b/c.txt", false)]);
    assert_eq!(s, [(Kind::FileState, "a/b/c.txt".to_string())].into());
    let s = seeds([("a/b/c.txt", true)]);
    assert!(s.contains(&(Kind::DirectoryListingState, "a/b".to_string())));
    assert!(s.contains(&(Kind::DirectoryListingState, "a".to_string())));
    assert!(s.contains(&(Kind::DirectoryListingState, String::new())));
    // The ancestor directories' own file state is *not* seeded: it did not
    // move, and seeding it would walk the `FILE` parent chain into every
    // package above the change.
    assert!(!s.contains(&(Kind::FileState, "a/b".to_string())));
}

/// The refresh filter, checked against the keys it has to let through and the
/// ones it has to keep out. The real thing is checked against a real server in
/// `crates/index/tests/sky_layer.rs`; this is the shape.
#[test]
fn the_refresh_filter_covers_what_re_querying_a_package_can_move() {
    let f = refresh_filter(&["a".to_string(), "b/c".to_string()]).unwrap();
    // A run that re-queried nothing asks for nothing.
    assert!(refresh_filter(&[]).is_none());
    // And one that re-queried most of the repository asks for everything
    // rather than building a megabyte of regex.
    let many: Vec<String> = (0..MAX_FILTERED_PACKAGES + 1).map(|i| format!("p{i}")).collect();
    assert!(refresh_filter(&many).is_none());

    let re = regex::Regex::new(&f).expect("a valid regex");
    for key in [
        "PACKAGE:a",
        "PACKAGE:b/c",
        "PACKAGE_LOOKUP:a",
        "PACKAGE_LOOKUP:a/sub",
        "BZL_LOAD:KeyForBuild{label=//x:y.bzl}",
        "IGNORED_SUBDIRECTORIES:@@",
        "FILE:[/ws]/[a/BUILD]",
        "FILE_STATE:[/ws]/[a/BUILD]",
        "FILE:[/ws]/[far/away/rules.bzl]",
        "DIRECTORY_LISTING:[/ws]/[a/sub]",
        "DIRECTORY_LISTING_STATE:[/ws]/[b/c/deep/er]",
        "GLOBS:<GlobsKey packageRoot = /ws, packageIdentifier = a, globRequests = []>",
        "GLOB:<GlobDescriptor packageName=a packageRoot=/ws subdir= pattern=*.txt>",
    ] {
        assert!(re.is_match(key), "should match: {key}");
    }
    for key in [
        // Another package's rows are not this refresh's to rewrite.
        "PACKAGE:other",
        "PACKAGE:ab",
        "FILE:[/ws]/[other/BUILD]",
        "DIRECTORY_LISTING:[/ws]/[other]",
        // A package whose name merely starts with a dirty one's.
        "GLOBS:<GlobsKey packageRoot = /ws, packageIdentifier = ab, globRequests = []>",
        "GLOB:<GlobDescriptor packageName=ab packageRoot=/ws subdir= pattern=*.txt>",
    ] {
        assert!(!re.is_match(key), "should not match: {key}");
    }
    // No comma anywhere: the flag is a comma-separated list of regexes, so a
    // comma inside one would split it in half.
    assert!(!f.contains(','), "{f}");
}

/// A package name is a literal, not a pattern: `.` and `+` are legal in one.
#[test]
fn a_package_name_with_a_metacharacter_is_escaped() {
    let f = refresh_filter(&["a.b".to_string()]).unwrap();
    let re = regex::Regex::new(&f).unwrap();
    assert!(re.is_match("PACKAGE:a.b"));
    assert!(!re.is_match("PACKAGE:axb"));
}

#[test]
fn every_stored_kind_byte_reads_back_as_itself() {
    for k in [
        Kind::FileState,
        Kind::File,
        Kind::DirectoryListingState,
        Kind::DirectoryListing,
        Kind::Glob,
        Kind::PackageLookup,
        Kind::Package,
        Kind::BzlLoad,
        Kind::Ignored,
        Kind::Tombstone,
    ] {
        assert_eq!(Kind::from_u8(k as u8), Some(k), "{}", k.name());
    }
    assert_eq!(Kind::from_u8(MAX_KIND + 1), None);
    // Both majors' spellings land on one kind.
    assert_eq!(Kind::from_dump("GLOB"), Some(Kind::Glob));
    assert_eq!(Kind::from_dump("GLOBS"), Some(Kind::Glob));
    assert_eq!(Kind::from_dump("IGNORED_SUBDIRECTORIES"), Some(Kind::Ignored));
    assert_eq!(
        Kind::from_dump("IGNORED_PACKAGE_PREFIXES"),
        Some(Kind::Ignored)
    );
    // And the singletons that would join every package to every other do not.
    assert_eq!(Kind::from_dump("PRECOMPUTED"), None);
    assert_eq!(Kind::from_dump("STARLARK_BUILTINS"), None);
    assert_eq!(Kind::from_dump("REPOSITORY_MAPPING"), None);
}
