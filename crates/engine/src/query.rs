//! Running Bazel queries and turning the results into index rows.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cambium_bazel::{convert, ConvertOptions, ParsedTarget};
use cambium_bazel::{Bazel, BazelError, QueryOutcome};
use cambium_index::PackageUpdate;
use rayon::prelude::*;

/// Reads BUILD files so packages can record a content hash.
///
/// Prefers the working tree, which is what a CI worker has checked out, and
/// falls back to `git show` when the revision being indexed is not the one in
/// the worktree.
pub struct FileSource {
    root: PathBuf,
    fallback_rev: Option<String>,
}

impl FileSource {
    pub fn worktree(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            fallback_rev: None,
        }
    }

    pub fn with_fallback_rev(mut self, rev: Option<String>) -> Self {
        self.fallback_rev = rev;
        self
    }

    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        if let Ok(b) = std::fs::read(self.root.join(path)) {
            return Some(b);
        }
        let rev = self.fallback_rev.as_ref()?;
        cambium_diff::Repo::new(self.root.clone()).read_at(rev, path)
    }

    /// blake3 of the package's BUILD file, or zero if it cannot be read.
    pub fn build_hash(&self, package: &str) -> [u8; 32] {
        for name in ["BUILD.bazel", "BUILD"] {
            let path = if package.is_empty() {
                name.to_string()
            } else {
                format!("{package}/{name}")
            };
            if let Some(b) = self.read(&path) {
                return *blake3::hash(&b).as_bytes();
            }
        }
        [0; 32]
    }
}

impl cambium_index::FileText for FileSource {
    fn text(&self, path: &str) -> Option<String> {
        // Starlark is UTF-8 by definition; a file that is not is not one we
        // can scan, and `from_utf8_lossy` would only invent loads.
        self.read(path).and_then(|b| String::from_utf8(b).ok())
    }
}

/// The source of package rows for a run.
///
/// Everything above this trait — dirty sets, splices, direct sets, the walk —
/// is pure graph work with no Bazel in it. Putting the query behind a trait is
/// what lets the property tests in spec 12.4 run the real engine over a
/// synthetic graph, without Bazel and at sizes Bazel could never reach.
pub trait PackageQuerier {
    /// Rows for the named packages. A requested package with no targets must
    /// still come back with an empty update, so its old rows are tombstoned —
    /// and when that happened because the package failed to *load*, the
    /// answer says so in `partial`.
    fn query(&self, packages: &[String]) -> Result<Rows, crate::error::EngineError>;

    /// Rows for the whole repository (cold build), plus whether Bazel managed
    /// to load all of it.
    fn universe(&self) -> Result<Rows, crate::error::EngineError>;

    /// The universe, widened by the in-tree external repositories stage 1
    /// confirmed (`crate::repos`).
    ///
    /// Defaulted to [`PackageQuerier::universe`], which is the right answer
    /// for every querier that is not driving a real Bazel: a synthetic row
    /// source has no external repositories to widen by, and a caller that
    /// passes some to one is asking a question it cannot answer.
    fn universe_with(&self, repos: &[String]) -> Result<Rows, crate::error::EngineError> {
        let _ = repos;
        self.universe()
    }

    /// Version string recorded in the index header.
    fn version(&self) -> String {
        String::new()
    }

    /// Starlark source for the static `.bzl` load closure, read at the
    /// revision these rows describe.
    ///
    /// A querier that returns `None` produces an index with no closure in it,
    /// and the engine then treats any `.bzl` change as global rather than
    /// guessing — see `Header::bzl_closure`.
    fn load_source(&self) -> Option<&dyn cambium_index::FileText> {
        None
    }

    /// The repository mapping to canonicalise external load labels with.
    /// Identity is always safe: nothing walks into an external repository.
    fn repo_mapping(&self) -> cambium_index::RepoMapping {
        cambium_index::RepoMapping::identity()
    }

    /// Every Starlark file of the checkout these rows describe, as
    /// repo-relative paths: BUILD, BUILD.bazel, `*.bzl`, and the workspace
    /// files.
    ///
    /// Only the witness for a changed `.bzl` the stored closure has no node
    /// for reads this, so it costs nothing on the common path. `None` means
    /// the querier cannot enumerate its tree at all (there is none), and the
    /// witness does not run.
    ///
    /// The default enumerates [`PackageQuerier::workspace`], which is the
    /// checkout the rows came from — not the working tree, which may be on a
    /// different revision.
    fn starlark_files(&self) -> Option<Vec<String>> {
        self.workspace().map(cambium_index::loads::starlark_files)
    }

    /// Whether this row source can produce a Skyframe loading-layer dump.
    ///
    /// False for a source with no Bazel behind it (the synthetic querier, a
    /// canned one in a test), and the engine then builds an index with no
    /// layer — which the next run refuses to answer a change from, rather
    /// than reading an absent layer as "nothing is dirty".
    fn can_dump_loading_layer(&self) -> bool {
        false
    }

    /// `bazel dump --skyframe=deps --skykey_filter=<filter>`, on the server
    /// these rows came from, with the paths needed to keep this machine's own
    /// directories out of the stored keys.
    ///
    /// The filter is the caller's: a cold build asks for the whole loading
    /// phase, a refresh for what re-querying its packages can have moved. It
    /// matters because a long-lived server holds whatever it has ever loaded,
    /// so a refresh on the server a cold build filled would otherwise dump the
    /// entire repository on every step.
    ///
    /// Only ever called when [`PackageQuerier::can_dump_loading_layer`] is
    /// true, and an error here fails the run: see
    /// [`cambium_bazel::Bazel::dump_skyframe`].
    fn dump_loading_layer(
        &self,
        filter: &str,
    ) -> Result<(String, cambium_index::sky::Paths), crate::error::EngineError> {
        let _ = filter;
        Err(crate::error::EngineError::BazelQuery {
            what: "dump --skyframe=deps".into(),
            code: None,
            stderr: "this row source has no Bazel server to dump".into(),
        })
    }

    /// Asks Bazel to load the whole repository once more, so that the loading
    /// layer the next dump prints is the whole one.
    ///
    /// Only ever called when a cold dump came back with the loading graph's
    /// shape and none of its package values (see [`crate::sky::build_layer`]).
    /// The call is warm: the server has just loaded these packages and nothing
    /// on disk has moved, so it re-reads nothing. `false` means this row
    /// source has no server to ask, and the layer stands or falls on the dump
    /// already taken.
    fn reload_packages(&self, repos: &[String]) -> bool {
        let _ = repos;
        false
    }

    /// The same, for named packages rather than the whole repository: the
    /// refresh path's repair, where only the packages the splice query
    /// re-queried can have been lost.
    fn reload_named(&self, packages: &[String]) -> bool {
        let _ = packages;
        false
    }

    /// `bazel query '//external:*' --output=build` over the checkout these
    /// rows describe, for [`cambium_index::externals::external_dir_repos`] to
    /// read the nested repositories' names out of.
    ///
    /// Only a cold build in the WORKSPACE world asks, and only to learn which
    /// repository a directory `//...` stopped at is. `None` — no Bazel behind
    /// this querier, or the query failed — leaves those directories unmapped,
    /// which is a global invalidation when one of them changes, not a wrong
    /// answer.
    fn external_rules(&self) -> Option<String> {
        None
    }

    /// Asks Bazel which packages each of these repositories has.
    ///
    /// One `bazel query '@R//...' --output=package` per repository, at cold
    /// build only, and before the universe query (see
    /// [`cambium_bazel::Bazel::probe_repo`]). The answer confirms or refuses
    /// what the tree claims a repository is; a querier with no Bazel behind
    /// it answers nothing and every claim stays unconfirmed, which is the
    /// wide behaviour the engine has today.
    fn probe_repos(&self, _repos: &[String]) -> Vec<cambium_bazel::RepoProbe> {
        Vec::new()
    }

    /// Asks Bazel which external repositories consume each of these indexed
    /// repositories, and through which of their packages
    /// ([`cambium_bazel::Bazel::consumer_bridge`]). At cold build only, after
    /// the loading-layer dump. A querier with no Bazel behind it knows of no
    /// consumer, and a change under the repository then reaches only what
    /// the index's own edges reach.
    fn consumer_repos(
        &self,
        _indexed: &[String],
    ) -> BTreeMap<String, BTreeMap<String, BTreeSet<String>>> {
        BTreeMap::new()
    }

    /// The directory whose files these rows describe, when there is one.
    ///
    /// The engine reads the tree directly in one place — deciding whether a
    /// deleted `BUILD` really deleted the package — and that read has to happen
    /// in the same checkout the rows came from, not in whatever the working
    /// tree happens to be on. A querier with no filesystem behind it (the
    /// synthetic one) returns `None` and the engine falls back to its own
    /// workspace.
    fn workspace(&self) -> Option<&Path> {
        None
    }

    /// Where a diagnostic raised while reading this querier's checkout goes.
    ///
    /// The index-building side of a run reads files as well as rows, and some
    /// of what it finds there is actionable rather than merely logged. A
    /// querier with no collector behind it (the synthetic and test ones)
    /// returns `None` and the finding stays a `tracing` line.
    fn diagnostics(&self) -> Option<&crate::diag::Diagnostics> {
        None
    }
}

/// The real thing: `bazel query` for a package set.
///
/// Owns its `Bazel` rather than borrowing one: the handle for a revision the
/// working tree is not on points at a temporary worktree that lives and dies
/// with the querier (see [`crate::revision`]), and a borrow could not express
/// that.
pub struct BazelQuerier {
    pub bazel: Bazel,
    pub opts: ConvertOptions,
    pub files: FileSource,
    /// Where a tombstoned package or a partial `//...` is reported. Empty by
    /// default, so a test that builds one by hand need not care.
    pub diags: crate::diag::Diagnostics,
}

impl PackageQuerier for BazelQuerier {
    fn diagnostics(&self) -> Option<&crate::diag::Diagnostics> {
        Some(&self.diags)
    }

    fn query(&self, packages: &[String]) -> Result<Rows, crate::error::EngineError> {
        let mut q = query_packages(&self.bazel, packages, &self.opts, &self.files)?;
        // Exit 3 is `--keep_going` finishing with errors, and the packages
        // missing from the output are of two kinds. A package this
        // configuration declares deleted, or whose `BUILD` file is gone, does
        // not exist here: its empty update tombstones its rows and its
        // consumers become direct via `DeletedDep`. Envoy's `.bazelrc` lists
        // `--deleted_packages` that still have BUILD files on disk; that is
        // where the first kind shows up in practice. A package that merely
        // failed to load is handled below, and keeps what the index has.
        //
        // The load errors travel with the rows so the index built from them
        // is flagged partial, exactly as a cold build's would be: a package
        // that failed to load because a registry was unreachable is missing
        // from the index, not deleted from the repository, and a `baseline
        // update` that published the result without saying so would leave
        // every later run silently short of it.
        if q.outcome.exit_code == Some(3) {
            let partial = partial_of(&q.outcome);
            // A package that failed to load is not a package that was
            // deleted, and the difference is not recoverable later: an empty
            // update tombstones every target it owned and clears every
            // forward row they carried, so the consumers that reached them
            // stop reaching them -- for the rest of the lineage, because
            // nothing re-queries a package whose `BUILD` did not change. That
            // is what took `//xla/python/ifrt/ir` and its seventy targets, and
            // `//xla/codegen/emitters/transforms` with them, out of every
            // later step of a 300-step walk (issue #23).
            //
            // So a requested package Bazel *named in a load error* keeps its
            // rows, and it keeps them whether it came back with no targets at
            // all or with the few `--keep_going` managed to create before the
            // error: a truncated update is not a smaller package, it is the
            // same package half read, and applying it tombstones everything
            // the error swallowed.
            //
            // Three kinds stay tombstoned. A package whose `BUILD` is gone was
            // deleted. A package this invocation declared deleted
            // (`--deleted_packages`) does not exist in this configuration,
            // however many other packages failed to load in the same batch --
            // envoy's `.bazelrc` is full of them, with their BUILD files still
            // on disk, so "has a BUILD file" cannot be the test. And a package
            // that loaded and is simply empty now was emptied by an edit.
            let declared_deleted: BTreeSet<&str> = q
                .outcome
                .deleted_packages
                .iter()
                .map(String::as_str)
                .collect();
            let errored: BTreeSet<&str> = partial
                .iter()
                .flat_map(|e| e.packages.iter().map(String::as_str))
                .collect();
            // Bazel's stderr is read for the first `MAX_ERROR_PACKAGES` names
            // of each kind, so past that the names cannot decide anything. A
            // batch that lost count falls back to the shape -- no targets and
            // a `BUILD` file on disk -- which keeps stale rows for a declared
            // deleted package but never clears rows an error swallowed.
            let names_lost = partial
                .as_ref()
                .is_some_and(|e| e.count as usize > e.packages.len());
            let mut unloaded = Vec::new();
            if partial.is_some() {
                let mut kept = Vec::with_capacity(q.updates.len());
                for u in q.updates {
                    let name = u.package.as_str();
                    let failed_to_load = u.build_hash != [0; 32]
                        && !declared_deleted.contains(name)
                        && (errored.contains(name) || (names_lost && u.targets.is_empty()));
                    if failed_to_load {
                        unloaded.push(u);
                    } else {
                        kept.push(u);
                    }
                }
                q.updates = kept;
            }
            if !unloaded.is_empty() {
                tracing::warn!(
                    packages = unloaded.len(),
                    first = ?unloaded.iter().take(5).map(|u| &u.package).collect::<Vec<_>>(),
                    "bazel named these dirty packages in a load error; carrying their \
                     rows forward rather than tombstoning them"
                );
                self.diags.emit(
                    crate::diag::Diagnostic::warning(
                        crate::diag::Code::W028PackageLoadFailed,
                        format!(
                            "{} dirty package(s) failed to load under --keep_going, so the \
                             rows the index already had for them stand and this run answers \
                             from them",
                            unloaded.len()
                        ),
                    )
                    .with_help(
                        "those rows are as old as the last query that loaded the package; \
                         fix the load error bazel reported to refresh them",
                    ),
                );
            }
            // Only the carried-forward packages that came back empty were
            // ever candidates for tombstoning; one that came back truncated
            // is counted among the packages Bazel was seen to load.
            let carried_empty = unloaded.iter().filter(|u| u.targets.is_empty()).count();
            let tombstoned = packages
                .len()
                .saturating_sub(q.packages_seen)
                .saturating_sub(carried_empty);
            if tombstoned > 0 {
                tracing::warn!(
                    requested = packages.len(),
                    loaded = q.packages_seen,
                    declared_deleted = ?q.outcome.deleted_packages,
                    stderr = %q.outcome.stderr_tail,
                    "some dirty packages did not load under --keep_going and have no BUILD \
                     file; tombstoning them, as a cold build would"
                );
                self.diags.emit(
                    crate::diag::Diagnostic::warning(
                        crate::diag::Code::W005PackagesTombstoned,
                        format!(
                            "{tombstoned} of {} dirty packages did not load under --keep_going \
                             and were tombstoned, as a cold build would drop them; their \
                             consumers are reported as affected",
                            packages.len()
                        ),
                    )
                    .with_help(
                        "if those packages should load, fix the load error bazel reported; \
                         run with --log warn to see its stderr",
                    ),
                );
            }
            return Ok(Rows {
                updates: q.updates,
                partial,
                unloaded,
            });
        }
        // Any other failure is not a partial answer: if a requested package
        // produced nothing while Bazel reported failure, the honest answer is
        // "we do not know", and the safe one is "everything" (spec 7.1).
        if !q.outcome.success && q.packages_seen < packages.len() {
            return Err(crate::error::EngineError::BazelQuery {
                what: format!("{} dirty packages", packages.len()),
                code: q.outcome.exit_code,
                stderr: q.outcome.stderr_tail,
            });
        }
        Ok(q.updates.into())
    }

    fn external_rules(&self) -> Option<String> {
        self.bazel.external_rules()
    }

    fn probe_repos(&self, repos: &[String]) -> Vec<cambium_bazel::RepoProbe> {
        repos.iter().filter_map(|r| self.bazel.probe_repo(r)).collect()
    }

    fn consumer_repos(
        &self,
        indexed: &[String],
    ) -> BTreeMap<String, BTreeMap<String, BTreeSet<String>>> {
        indexed
            .iter()
            .filter_map(|r| {
                let cs = self.bazel.consumer_bridge(r)?;
                (!cs.is_empty()).then(|| (r.clone(), cs.into_iter().collect()))
            })
            .collect()
    }

    fn can_dump_loading_layer(&self) -> bool {
        true
    }

    fn dump_loading_layer(
        &self,
        filter: &str,
    ) -> Result<(String, cambium_index::sky::Paths), crate::error::EngineError> {
        let text = self.bazel.dump_skyframe(filter).map_err(|e| {
            crate::error::EngineError::BazelQuery {
                what: "dump --skyframe=deps".into(),
                code: None,
                stderr: e.to_string(),
            }
        })?;
        let paths = cambium_index::sky::Paths::new(
            self.bazel.workspace().to_string_lossy(),
            self.bazel.output_base().unwrap_or_default(),
        );
        Ok((text, paths))
    }

    fn reload_named(&self, packages: &[String]) -> bool {
        match self.bazel.load_packages(packages) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(error = %e, "could not ask bazel to load these packages again");
                false
            }
        }
    }

    fn reload_packages(&self, repos: &[String]) -> bool {
        match self.bazel.load_universe(repos) {
            Ok(()) => true,
            Err(e) => {
                // Not fatal in itself: the caller checks the dump that follows
                // and fails the run if it is still not a loading layer.
                tracing::debug!(error = %e, "could not ask bazel to load the repository again");
                false
            }
        }
    }

    fn universe(&self) -> Result<Rows, crate::error::EngineError> {
        self.universe_with(&[])
    }

    fn universe_with(&self, repos: &[String]) -> Result<Rows, crate::error::EngineError> {
        let q = query_universe(&self.bazel, &self.opts, &self.files, repos)?;
        // Exit 3 is accepted here for the same reason `query` accepts it, and
        // for consistency with it: the incremental path tombstones a package
        // that fails to load *because* a cold build would not have it either.
        // That argument only holds if the cold build really does go ahead
        // without those packages, so it does — and records that it did, in the
        // index's provenance, rather than pretending the answer was complete.
        // Anything other than 0 or 3 is still a hard failure: it means Bazel
        // did not finish, and a truncated stream is not a partial answer.
        if !q.outcome.success && q.outcome.exit_code != Some(3) {
            return Err(crate::error::EngineError::BazelQuery {
                what: cambium_bazel::universe_expression(repos),
                code: q.outcome.exit_code,
                stderr: q.outcome.stderr_tail,
            });
        }
        let partial = partial_of(&q.outcome).inspect(|e| {
            tracing::warn!(
                packages_in_error = e.count,
                loaded = q.updates.len(),
                first = ?e.packages,
                "bazel query //... finished with errors under --keep_going; the index will be PARTIAL"
            );
            self.diags.emit(
                crate::diag::Diagnostic::warning(
                    crate::diag::Code::W006PartialQuery,
                    format!(
                        "`bazel query //...` finished with errors under --keep_going \
                         ({} package error lines), so the index is partial and every run \
                         spliced onto it is short of those packages",
                        e.count
                    ),
                )
                .with_help(
                    "fix the load errors, or name the packages whose absence is expected \
                     in baseline.publish_partial (--allow-partial for one run)",
                ),
            );
        });
        Ok(Rows {
            updates: q.updates,
            partial,
            // A cold build asked for the whole repository, not a package
            // list: there is no carried row to keep for what did not load.
            unloaded: Vec::new(),
        })
    }

    fn version(&self) -> String {
        self.bazel.version()
    }

    fn load_source(&self) -> Option<&dyn cambium_index::FileText> {
        Some(&self.files)
    }

    fn repo_mapping(&self) -> cambium_index::RepoMapping {
        // A failed command is WORKSPACE mode or an old Bazel, where identity
        // is the right mapping and `cambium_bazel` already logs it. A command
        // that succeeded and printed something unreadable is not: the module
        // graph has a mapping and it was lost.
        let Some(dump) = self.bazel.repo_mapping_dump() else {
            return cambium_index::RepoMapping::identity();
        };
        cambium_index::RepoMapping::try_from_dump(&dump).unwrap_or_else(|why| {
            tracing::warn!("{why}; falling back to the identity repository mapping");
            self.diags.emit(
                crate::diag::Diagnostic::warning(
                    crate::diag::Code::W022RepoMappingUnparsed,
                    format!(
                        "{why}, so external repositories are named by their apparent names \
                         and a load through a renamed `bazel_dep` can be attributed to the \
                         wrong repository"
                    ),
                )
                .with_help(
                    "run `bazel mod dump_repo_mapping \"\"` in the workspace: its stdout must be \
                     one JSON object. File the output with cambium if Bazel printed one and \
                     this still fired",
                ),
            );
            cambium_index::RepoMapping::identity()
        })
    }

    fn workspace(&self) -> Option<&Path> {
        Some(self.bazel.workspace())
    }
}

/// Whether a finished query left the index incomplete, and with what.
///
/// Exit 3 is `--keep_going` finishing with errors, and the errors are not all
/// the same thing:
///
/// - A package that *failed to load* — an unreachable registry, a broken
///   `.bzl` — is missing from an answer that should have had it. The index is
///   partial and has to say so.
/// - A package the invocation *declared deleted* (`--deleted_packages`) is not
///   missing: it does not exist in this configuration. A `bazel query //...`
///   never names it and exits 0, so a cold build's index is complete without
///   it — and an incremental query that asked for it by name, because a file
///   under it changed, must reach the same verdict. Its empty update
///   tombstones whatever rows the base had for it, exactly as a deletion
///   would.
///
/// So exit 3 whose only complaint is declared-deleted packages is a complete
/// answer. Any *unexplained* exit 3 stays partial: zero recognised error lines
/// means a stderr shape this does not know, and calling that complete would
/// silently under-report for every run spliced onto the published index.
pub fn partial_of(outcome: &QueryOutcome) -> Option<cambium_bazel::LoadErrors> {
    if outcome.exit_code != Some(3) {
        return None;
    }
    if outcome.load_errors.is_empty() && !outcome.deleted_packages.is_empty() {
        return None;
    }
    Some(outcome.load_errors.clone())
}

/// A query's answer: every package Bazel could load, and — when it could not
/// load them all — what it said about the ones it dropped.
#[derive(Debug)]
pub struct Rows {
    pub updates: Vec<PackageUpdate>,
    /// `Some` when the query exited 3 under `--keep_going`.
    pub partial: Option<cambium_bazel::LoadErrors>,
    /// Packages that were asked for and that Bazel failed to load: it named
    /// them in a load error, and they are neither gone from disk nor declared
    /// deleted by this configuration. Their updates travel here rather than in
    /// `updates`, so nothing applies them; the rows the base has for them
    /// stand, and the run treats them as packages it did not re-query. A run
    /// that knows better -- the package's own `BUILD` is what this commit
    /// edited -- can still apply one, and does (`run::query`).
    pub unloaded: Vec<PackageUpdate>,
}

impl From<Vec<PackageUpdate>> for Rows {
    fn from(updates: Vec<PackageUpdate>) -> Self {
        Self {
            updates,
            partial: None,
            unloaded: Vec::new(),
        }
    }
}

/// Parsed query output, grouped into the batches the index builder consumes.
pub struct QueryResult {
    pub updates: Vec<PackageUpdate>,
    pub outcome: QueryOutcome,
    /// Packages that produced at least one target.
    pub packages_seen: usize,
}

/// Queries a set of packages and groups the results.
///
/// `requested` packages that produce no targets still get an (empty) update, so
/// that a package emptied by an edit has its rows tombstoned rather than left
/// stale.
pub fn query_packages(
    bazel: &Bazel,
    requested: &[String],
    opts: &ConvertOptions,
    files: &FileSource,
) -> Result<QueryResult, BazelError> {
    let mut grouped: BTreeMap<String, Vec<ParsedTarget>> = BTreeMap::new();
    for p in requested {
        grouped.entry(p.clone()).or_default();
    }
    // What may come back is what was asked for: an external package key in
    // the request is a repository this index holds, and conversion has to
    // keep its rows rather than drop them as it does every other repository's.
    let opts = with_repos(opts, repos_of(requested));
    let outcome = bazel.query_packages(requested, group_into(&mut grouped, &opts))?;
    Ok(finish(grouped, outcome, files, Some(requested)))
}

/// The batch callback every stream reader uses: decode the batch, file each
/// target under its package.
fn group_into<'a>(
    grouped: &'a mut BTreeMap<String, Vec<ParsedTarget>>,
    opts: &'a ConvertOptions,
) -> impl FnMut(&[u8], &[(usize, usize)]) -> Result<(), cambium_proto::ReadError> + 'a {
    move |slab, ranges| {
        for t in decode_batch(slab, ranges, opts)? {
            grouped.entry(t.package.clone()).or_default().push(t);
        }
        Ok(())
    }
}

/// Queries the whole repository for a cold build (spec 6.9).
pub fn query_universe(
    bazel: &Bazel,
    opts: &ConvertOptions,
    files: &FileSource,
    repos: &[String],
) -> Result<QueryResult, BazelError> {
    let mut grouped: BTreeMap<String, Vec<ParsedTarget>> = BTreeMap::new();
    // The universe was widened by exactly these repositories, so exactly
    // these are the ones whose targets get a package key.
    let opts = with_repos(opts, repos.to_vec());
    let outcome = bazel.query_universe(repos, group_into(&mut grouped, &opts))?;
    Ok(finish(grouped, outcome, files, None))
}

/// The external repositories named by a package request.
fn repos_of(packages: &[String]) -> Vec<String> {
    let mut v: Vec<String> = packages
        .iter()
        .filter_map(|p| cambium_index::label::key_repo(p))
        .map(|r| r.to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// `opts` with its indexed repositories replaced.
fn with_repos(opts: &ConvertOptions, repos: Vec<String>) -> ConvertOptions {
    ConvertOptions {
        indexed_repos: repos,
        ..opts.clone()
    }
}

/// Decodes one batch of frames.
///
/// Decoding is the expensive half of ingestion and each message is
/// independent, so it fans out — but per batch, not per stream or per
/// message. The reader hands over a slab of a few thousand frames at a time,
/// which is what bounds memory while Bazel is still writing: the whole stream
/// of a large repository never has to be resident at once. Inside a batch,
/// one message is too little work to be worth a rayon task on its own, so the
/// batch is the unit and `par_iter` splits it. `collect` preserves order,
/// which keeps id assignment, and therefore the serialised index,
/// deterministic.
fn decode_batch(
    slab: &[u8],
    ranges: &[(usize, usize)],
    opts: &ConvertOptions,
) -> Result<Vec<ParsedTarget>, cambium_proto::ReadError> {
    let decoded: Result<Vec<Option<ParsedTarget>>, cambium_proto::ReadError> = ranges
        .par_iter()
        .map(|&range| Ok(convert(&cambium_proto::decode_frame(slab, range)?, opts)))
        .collect();
    Ok(decoded?.into_iter().flatten().collect())
}

fn finish(
    grouped: BTreeMap<String, Vec<ParsedTarget>>,
    outcome: QueryOutcome,
    files: &FileSource,
    restrict_to: Option<&[String]>,
) -> QueryResult {
    let allowed: Option<std::collections::HashSet<&str>> =
        restrict_to.map(|r| r.iter().map(|s| s.as_str()).collect());
    let mut packages_seen = 0;
    let mut updates = Vec::with_capacity(grouped.len());
    for (package, mut targets) in grouped {
        // A `//pkg:*` query only returns that package's targets, but be strict:
        // a stray target from elsewhere would silently steal ownership rows.
        if let Some(a) = &allowed {
            if !a.contains(package.as_str()) {
                continue;
            }
        }
        if !targets.is_empty() {
            packages_seen += 1;
        }
        // Bazel is asked for --order_output=no; sorting here is what makes id
        // assignment, and therefore the serialised index, deterministic.
        targets.sort_by(|a, b| a.row.label.cmp(&b.row.label));
        targets.dedup_by(|a, b| a.row.label == b.row.label);
        updates.push(PackageUpdate {
            build_hash: files.build_hash(&package),
            // Reserved. The `.bzl` load closure lives in its own section of
            // the index (see `cambium_index::loads`) rather than as a
            // per-package hash: the engine needs "which packages load this
            // file", which a hash cannot answer.
            bzl_hash: [0; 32],
            package,
            targets: targets.into_iter().map(|t| t.row).collect(),
        });
    }
    QueryResult {
        updates,
        outcome,
        packages_seen,
    }
}

/// Decodes a whole streamed_proto buffer. Used by tests and by
/// `index verify`-style tooling that has bytes rather than a process.
pub fn parse_stream(
    bytes: &[u8],
    opts: &ConvertOptions,
    files: &FileSource,
) -> Result<Vec<PackageUpdate>, cambium_proto::ReadError> {
    let mut grouped: BTreeMap<String, Vec<ParsedTarget>> = BTreeMap::new();
    cambium_proto::read_frames(
        bytes,
        cambium_proto::DEFAULT_BATCH,
        group_into(&mut grouped, opts),
    )?;
    let outcome = QueryOutcome {
        exit_code: Some(0),
        success: true,
        stderr_tail: String::new(),
        targets_read: 0,
        load_errors: cambium_bazel::LoadErrors::default(),
        deleted_packages: Vec::new(),
    };
    Ok(finish(grouped, outcome, files, None).updates)
}

/// Empty `FileSource` for callers that do not care about BUILD hashes.
pub fn no_files() -> FileSource {
    FileSource::worktree(Path::new("/nonexistent"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cambium_bazel::{Bazel, CannedRunner};
    use cambium_proto::blaze_query::{target::Discriminator, Rule, Target};

    use super::*;
    use crate::diag::Code;
    use crate::error::EngineError;

    fn rule(name: &str) -> Target {
        Target {
            r#type: Discriminator::Rule as i32,
            rule: Some(Rule {
                name: name.to_string(),
                rule_class: "cc_library".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A querier over a Bazel that answers every query with these bytes,
    /// this stderr and this exit code.
    fn querier(code: i32, targets: &[Target], stderr: &str) -> BazelQuerier {
        let bazel =
            Bazel::at(std::env::temp_dir(), Some("bazel")).with_runner(Arc::new(CannedRunner {
                code,
                stdout: cambium_proto::encode_stream(targets),
                stderr: stderr.to_string(),
            }));
        BazelQuerier {
            bazel,
            opts: ConvertOptions {
                test_rule_suffixes: vec!["_test".into()],
                exclude_tags: vec!["manual".into()],
                indexed_repos: Vec::new(),
            },
            files: no_files(),
            diags: crate::diag::Diagnostics::new(),
        }
    }

    fn codes(q: &BazelQuerier) -> Vec<Code> {
        q.diags.drain().into_iter().map(|d| d.code).collect()
    }

    const LOAD_ERROR: &str = "ERROR: error loading package 'b': no such file\n";
    const DECLARED_DELETED: &str = "ERROR: Skipping '//b:*': no such package 'b': Package is \
                                    considered deleted due to --deleted_packages\n";
    /// A load error about a package nobody asked for.
    const ELSEWHERE: &str = "ERROR: error loading package 'c': no such file\n";

    /// A querier whose `FileSource` reads a tree with a `BUILD` file in each
    /// of `packages`. The directory is returned with it: dropping it deletes
    /// the tree.
    fn in_tree(
        code: i32,
        targets: &[Target],
        stderr: &str,
        packages: &[&str],
    ) -> (tempfile::TempDir, BazelQuerier) {
        let tree = tempfile::tempdir().unwrap();
        for p in packages {
            std::fs::create_dir_all(tree.path().join(p)).unwrap();
            std::fs::write(
                tree.path().join(p).join("BUILD"),
                format!("cc_library(name = \"{p}\")\n"),
            )
            .unwrap();
        }
        let mut q = querier(code, targets, stderr);
        q.files = FileSource::worktree(tree.path());
        (tree, q)
    }

    fn carried(rows: &Rows) -> Vec<&str> {
        rows.unloaded.iter().map(|u| u.package.as_str()).collect()
    }

    fn applied(rows: &Rows) -> Vec<&str> {
        rows.updates.iter().map(|u| u.package.as_str()).collect()
    }

    /// Exit 3 under `--keep_going` where the package that did not come back
    /// has no `BUILD` file either: that is a deletion, its empty update
    /// tombstones its rows, and the answer says it is partial.
    #[test]
    fn a_package_with_no_build_file_is_tombstoned_and_reported() {
        let q = querier(3, &[rule("//a:x")], LOAD_ERROR);
        let rows = q.query(&["a".into(), "b".into()]).unwrap();
        let by_package: Vec<(&str, usize)> = rows
            .updates
            .iter()
            .map(|u| (u.package.as_str(), u.targets.len()))
            .collect();
        assert_eq!(by_package, vec![("a", 1), ("b", 0)]);
        let partial = rows.partial.expect("a load error makes the answer partial");
        assert_eq!(partial.count, 1);
        assert_eq!(partial.packages, vec!["b".to_string()]);
        assert_eq!(codes(&q), vec![Code::W005PackagesTombstoned]);
    }

    /// The same exit 3, but the package that did not come back still has a
    /// `BUILD` file on disk. That is a load failure, not a deletion, and
    /// tombstoning it would clear every forward row its targets carried --
    /// which no later step could repair, because nothing re-queries a package
    /// whose `BUILD` did not change. Its rows are carried forward instead and
    /// the run says so (issue #23: whole xla packages went missing from every
    /// later step of a 300-step walk this way).
    #[test]
    fn a_package_that_failed_to_load_keeps_the_rows_the_index_has() {
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir(tree.path().join("b")).unwrap();
        std::fs::write(tree.path().join("b/BUILD"), "cc_library(name = \"y\")\n").unwrap();
        let mut q = querier(3, &[rule("//a:x")], LOAD_ERROR);
        q.files = FileSource::worktree(tree.path());

        let rows = q.query(&["a".into(), "b".into()]).unwrap();
        let by_package: Vec<&str> = rows.updates.iter().map(|u| u.package.as_str()).collect();
        assert_eq!(by_package, vec!["a"], "b keeps whatever the index has");
        assert_eq!(carried(&rows), vec!["b"]);
        assert!(rows.partial.is_some());
        assert_eq!(codes(&q), vec![Code::W028PackageLoadFailed]);
    }

    /// Exit 3 whose only complaint is a package the invocation declared
    /// deleted: tombstoned like a load failure, but the answer is complete.
    #[test]
    fn a_declared_deleted_package_is_tombstoned_but_not_partial() {
        let q = querier(3, &[rule("//a:x")], DECLARED_DELETED);
        let rows = q.query(&["a".into(), "b".into()]).unwrap();
        assert_eq!(rows.updates.len(), 2);
        assert!(
            rows.partial.is_none(),
            "declared deleted is not a load failure"
        );
        assert_eq!(
            codes(&q),
            vec![Code::W005PackagesTombstoned],
            "its consumers are still reported as affected, and that is said"
        );
    }

    /// Exit 3 with every requested package loaded — the errors were about
    /// something else — is a complete answer with nothing to warn about.
    #[test]
    fn exit_3_with_every_package_loaded_warns_about_nothing() {
        let q = querier(3, &[rule("//a:x"), rule("//b:y")], ELSEWHERE);
        let rows = q.query(&["a".into(), "b".into()]).unwrap();
        assert!(
            rows.partial.is_some(),
            "the load error still travels with the rows"
        );
        assert!(codes(&q).is_empty());
    }

    /// A package bazel *named in a load error* that still came back with some
    /// targets did not load: `--keep_going` returns the targets created before
    /// the error and marks the package in error. Applying that update would
    /// tombstone every target the error swallowed, which is the same
    /// permanent clear as the empty case and the commoner one in practice (a
    /// BUILD file or a macro that raises partway down). The truncated update
    /// is dropped and the rows stand.
    #[test]
    fn a_package_that_loaded_only_partly_keeps_the_rows_the_index_has() {
        let (_tree, q) = in_tree(3, &[rule("//a:x"), rule("//b:y")], LOAD_ERROR, &["a", "b"]);
        let rows = q.query(&["a".into(), "b".into()]).unwrap();
        assert_eq!(applied(&rows), vec!["a"]);
        assert_eq!(
            carried(&rows),
            vec!["b"],
            "half a package is not a smaller package"
        );
        assert_eq!(codes(&q), vec![Code::W028PackageLoadFailed]);
    }

    /// A package this configuration declares deleted does not exist here,
    /// however many other packages failed to load in the same batch. Envoy's
    /// `.bazelrc` lists `--deleted_packages` whose `BUILD` files are still on
    /// disk, so "has a BUILD file" cannot be the test: carrying such a package
    /// forward keeps the index reporting targets no build can resolve, and its
    /// consumers never go direct via `DeletedDep`.
    #[test]
    fn a_declared_deleted_package_is_tombstoned_even_beside_a_load_error() {
        let stderr = format!("{DECLARED_DELETED}ERROR: error loading package 'c': no such file\n");
        let (_tree, q) = in_tree(3, &[rule("//a:x")], &stderr, &["a", "b", "c"]);
        let rows = q.query(&["a".into(), "b".into(), "c".into()]).unwrap();
        assert_eq!(
            applied(&rows),
            vec!["a", "b"],
            "b was declared deleted, so its empty update tombstones its rows"
        );
        assert_eq!(carried(&rows), vec!["c"], "only c failed to load");
        assert!(
            rows.partial.is_some(),
            "c's load error still makes the answer partial"
        );
        assert_eq!(
            codes(&q),
            vec![Code::W028PackageLoadFailed, Code::W005PackagesTombstoned]
        );
    }

    /// A package that loaded and simply has no targets any more was emptied by
    /// an edit, and its rows are stale, not missing. Nothing in the query
    /// names it, so it is tombstoned as it always was -- otherwise the index
    /// keeps a package the last edit removed, and the answer depends on
    /// whether some unrelated package failed to load in the same batch.
    #[test]
    fn a_package_emptied_by_an_edit_is_still_tombstoned() {
        let (_tree, q) = in_tree(3, &[rule("//a:x")], ELSEWHERE, &["a", "b"]);
        let rows = q.query(&["a".into(), "b".into()]).unwrap();
        assert_eq!(applied(&rows), vec!["a", "b"]);
        assert!(carried(&rows).is_empty());
        assert_eq!(codes(&q), vec![Code::W005PackagesTombstoned]);
    }

    /// Past `MAX_ERROR_PACKAGES` names bazel's stderr is counted but not
    /// recorded, so the names decide nothing and the shape has to: no targets
    /// and a `BUILD` file on disk is read as a load failure. Stale rows beat
    /// rows an error cleared for good.
    #[test]
    fn a_batch_with_more_errors_than_names_falls_back_to_the_shape() {
        let mut stderr = String::new();
        for i in 0..cambium_bazel::MAX_ERROR_PACKAGES + 1 {
            stderr.push_str(&format!("ERROR: error loading package 'z{i}': boom\n"));
        }
        let (_tree, q) = in_tree(3, &[rule("//a:x")], &stderr, &["a", "b"]);
        let rows = q.query(&["a".into(), "b".into()]).unwrap();
        assert_eq!(applied(&rows), vec!["a"]);
        assert_eq!(carried(&rows), vec!["b"]);
    }

    /// Any other failure while a requested package is missing is not a
    /// partial answer: the run must not guess.
    #[test]
    fn a_hard_failure_is_an_error_with_bazels_exit_code() {
        let q = querier(1, &[], "ERROR: something else entirely\n");
        let err = q.query(&["a".into()]).unwrap_err();
        assert!(
            matches!(err, EngineError::BazelQuery { code: Some(1), ref stderr, .. }
                if stderr.contains("something else")),
            "{err}"
        );
        assert!(codes(&q).is_empty());
    }

    fn querier_printing(code: i32, stdout: &str) -> BazelQuerier {
        let mut q = querier(code, &[], "");
        q.bazel =
            Bazel::at(std::env::temp_dir(), Some("bazel")).with_runner(Arc::new(CannedRunner {
                code,
                stdout: stdout.as_bytes().to_vec(),
                stderr: String::new(),
            }));
        q
    }

    /// Unreadable output from a command that succeeded loses a real mapping,
    /// and says so with a code; a command that failed is WORKSPACE mode, where
    /// identity is the answer and there is nothing to act on.
    #[test]
    fn an_unreadable_repo_mapping_is_a_diagnostic_and_a_failed_one_is_not() {
        let q = querier_printing(0, "Starting local Bazel server...\n");
        assert!(q.repo_mapping().is_identity());
        assert_eq!(codes(&q), vec![Code::W022RepoMappingUnparsed]);

        let q = querier_printing(0, "{\"rules_cc\": ");
        assert!(q.repo_mapping().is_identity());
        assert_eq!(codes(&q), vec![Code::W022RepoMappingUnparsed]);

        let q = querier_printing(0, "{\"\": \"\", \"rules_cc\": \"rules_cc+\"}");
        assert!(!q.repo_mapping().is_identity());
        assert!(codes(&q).is_empty());

        let q = querier_printing(2, "");
        assert!(q.repo_mapping().is_identity());
        assert!(codes(&q).is_empty());
    }

    #[test]
    fn a_universe_that_exits_3_is_a_partial_index_and_says_so() {
        let q = querier(3, &[rule("//a:x")], LOAD_ERROR);
        let rows = q.universe().unwrap();
        assert_eq!(rows.updates.len(), 1);
        assert_eq!(rows.partial.as_ref().map(|e| e.count), Some(1));
        assert_eq!(codes(&q), vec![Code::W006PartialQuery]);

        let q = querier(0, &[rule("//a:x")], "");
        let rows = q.universe().unwrap();
        assert!(rows.partial.is_none());
        assert!(codes(&q).is_empty());

        let q = querier(1, &[], "ERROR: bazel did not finish\n");
        assert!(matches!(
            q.universe().unwrap_err(),
            EngineError::BazelQuery { code: Some(1), .. }
        ));
    }
}
