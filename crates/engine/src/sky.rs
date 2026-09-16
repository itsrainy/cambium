//! Taking the Skyframe loading layer off a Bazel server, and walking it.
//!
//! The layer itself lives in `cambium_index::sky`; this is the engine's half:
//! when the dump runs, what a failure means, and how a diff becomes the seeds
//! the walk starts from.
//!
//! # When the dump runs
//!
//! Always on the server that has just loaded the packages, never on a fresh
//! one. A cold build dumps after `bazel query //...:*`, so the graph holds the
//! whole repository. An incremental run dumps after the splice query, so the
//! graph holds exactly the packages that run re-queried, and applying that
//! dump refreshes their rows and touches nothing else. There is no
//! package-name filter on the second dump and none is needed: a server only
//! holds what it loaded.
//!
//! # A failure is a failure
//!
//! A dump that does not run is an error with Bazel's reason in it. It is not a
//! quiet fall back to the rules the layer replaced, and it is not an index
//! written without a layer: such an index reads as "this repository's loading
//! phase touched nothing", and every later run spliced onto it would answer
//! that no package is dirty. See the fallback elimination principle in
//! README.md and docs/DIAGNOSTICS.md.

use cambium_diff::{Change, Status};
use cambium_index::sky::{self, Kind, SkyLayer};

use crate::error::{EngineError, Result};
use crate::query::PackageQuerier;

/// Builds or refreshes the loading layer from a dump on `q`'s server.
///
/// `onto` is the layer carried over from the base index, whose rows the dump
/// replaces one by one; `None` builds a fresh one, which is what a cold build
/// does.
///
/// A row source with no Bazel to ask gets `onto` back unchanged: a cold build
/// through such a source produces an index with no layer (and the next run
/// over that index says so), and a splice keeps the layer the base had rather
/// than dropping it, which would make the *published* index one no later run
/// could be answered from. Only the queriers with no filesystem behind them
/// are in that position; the real one always dumps, and a dump that fails is
/// an error.
pub(crate) fn build_layer(
    q: &dyn PackageQuerier,
    onto: Option<SkyLayer>,
    filter: &str,
) -> Result<Option<SkyLayer>> {
    if !q.can_dump_loading_layer() {
        tracing::debug!(
            carried = onto.is_some(),
            "this row source has no Bazel server, so the Skyframe loading layer is \
             whatever the base index already carried"
        );
        return Ok(onto);
    }
    let started = std::time::Instant::now();
    let cold = onto.is_none();
    let (text, paths) = q.dump_loading_layer(filter)?;
    let bytes = text.len();
    let mut layer = onto.unwrap_or_default();
    let blocks = layer.apply_dump(&text, &paths);
    tracing::debug!(
        blocks,
        bytes,
        nodes = layer.num_nodes(),
        edges = layer.num_edges(),
        ms = started.elapsed().as_millis(),
        "read the Skyframe loading layer off the server"
    );
    if blocks == 0 {
        // A server that loaded something and dumped nothing is not a layer
        // with no rows in it; it is a dump that did not do what it says.
        return Err(EngineError::BazelQuery {
            what: "dump --skyframe=deps".into(),
            code: None,
            stderr: format!(
                "the dump returned {bytes} bytes and no Skyframe nodes; the loading layer \
                 would be empty, which no later run can be answered from"
            ),
        });
    }
    // A layer with no `FILE_STATE` node under the workspace is a layer no
    // changed path can enter: every seed is a `FILE_STATE` or a
    // `DIRECTORY_LISTING_STATE`, so the walk would reach nothing, re-query
    // nothing, and report nothing, for every commit after this one.
    //
    // It happens when the `[root]` the dump writes is not the workspace this
    // run was told about, and the only way that is visible is to look: Bazel
    // resolves symbolic links and prints the real path, so a workspace under
    // `$TMPDIR` on macOS (`/var` is a link to `/private/var`) writes keys the
    // run does not recognise as its own. Saying so with the roots in hand is
    // the difference between a failed run and a silently empty answer.
    //
    // Checked on a cold build, where the dump is the whole loading phase and
    // must hold this workspace's files. A refresh is filtered to the packages
    // its run re-queried and may legitimately carry no file key at all, so it
    // is the cold build that establishes the lineage's roots; every later run
    // asks the same handle for the same workspace.
    if cold && layer.count_of(Kind::FileState) == 0 {
        let roots: Vec<&str> = layer.foreign_roots().take(4).collect();
        return Err(EngineError::BazelQuery {
            what: "dump --skyframe=deps".into(),
            code: None,
            stderr: format!(
                "the loading layer holds no file keys of this workspace ({}), so no changed \
                 path could reach a package through it. The dump rooted its file keys at {:?}. \
                 A root that does not match is usually a symlinked workspace path: bazel \
                 resolves it and prints the real one",
                paths.workspace(),
                roots
            ),
        });
    }
    Ok(Some(layer))
}

/// The Skyframe keys a diff moved.
///
/// The only reading of a changed path anywhere in the dirty stage, and it is
/// one bit wide: did this change add or remove a *name* in a directory, or
/// only rewrite a file's contents? Everything after that is edges Bazel
/// recorded.
pub(crate) fn seeds_of(changes: &[&Change]) -> std::collections::BTreeSet<(Kind, String)> {
    sky::seeds(changes.iter().map(|c| {
        (
            c.path.as_str(),
            // A rename reaches the diff as a delete plus an add, so both ends
            // move a directory's listing and both are structural here.
            matches!(c.status, Status::Added | Status::Deleted),
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_add_or_a_delete_moves_a_directory_listing() {
        let changes = [
            Change::new(Status::Modified, "a/x.cc"),
            Change::new(Status::Added, "b/y.cc"),
        ];
        let refs: Vec<&Change> = changes.iter().collect();
        let s = seeds_of(&refs);
        assert!(s.contains(&(Kind::FileState, "a/x.cc".into())));
        assert!(s.contains(&(Kind::FileState, "b/y.cc".into())));
        // The modified file's directory did not gain or lose a name.
        assert!(!s.contains(&(Kind::DirectoryListingState, "a".into())));
        assert!(s.contains(&(Kind::DirectoryListingState, "b".into())));
        assert!(s.contains(&(Kind::DirectoryListingState, String::new())));
    }

    #[test]
    fn a_type_change_is_a_content_change() {
        // A file that became a symlink still occupies the same name, so no
        // directory listing moved; its own file state did.
        let changes = [Change::new(Status::TypeChanged, "a/x.cc")];
        let refs: Vec<&Change> = changes.iter().collect();
        let s = seeds_of(&refs);
        assert_eq!(s, [(Kind::FileState, "a/x.cc".to_string())].into());
    }
}
