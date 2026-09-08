//! Running Bazel queries and turning the results into index rows.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cambium_bazel::{convert, ConvertOptions, ParsedTarget};
use cambium_bazel::{Bazel, BazelError, QueryOutcome};
use cambium_index::PackageUpdate;
use cambium_proto::blaze_query::Target;
use prost::Message;
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
}

impl PackageQuerier for BazelQuerier {
    fn query(&self, packages: &[String]) -> Result<Rows, crate::error::EngineError> {
        let q = query_packages(&self.bazel, packages, &self.opts, &self.files)?;
        // Exit 3 is `--keep_going` finishing with errors: every package that
        // could load is in the output, and the ones that could not are the
        // ones a cold build would also drop (the universe query runs with
        // `--keep_going` too, so they are absent from a fresh index). Their
        // empty updates tombstone their rows, and consumers become direct via
        // `DeletedDep`. Envoy's `.bazelrc` lists `--deleted_packages` that still
        // have BUILD files on disk; that is where this shows up in practice.
        //
        // The load errors travel with the rows so the index built from them
        // is flagged partial, exactly as a cold build's would be: a package
        // that failed to load because a registry was unreachable is missing
        // from the index, not deleted from the repository, and a `baseline
        // update` that published the result without saying so would leave
        // every later run silently short of it.
        if q.outcome.exit_code == Some(3) {
            if q.packages_seen < packages.len() {
                tracing::warn!(
                    requested = packages.len(),
                    loaded = q.packages_seen,
                    stderr = %q.outcome.stderr_tail,
                    "some dirty packages failed to load under --keep_going; tombstoning them, as a cold build would"
                );
            }
            return Ok(Rows {
                updates: q.updates,
                partial: Some(q.outcome.load_errors),
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

    fn universe(&self) -> Result<Rows, crate::error::EngineError> {
        let q = query_universe(&self.bazel, &self.opts, &self.files)?;
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
                what: "//...".into(),
                code: q.outcome.exit_code,
                stderr: q.outcome.stderr_tail,
            });
        }
        let partial = (q.outcome.exit_code == Some(3)).then(|| {
            tracing::warn!(
                packages_in_error = q.outcome.load_errors.count,
                loaded = q.updates.len(),
                first = ?q.outcome.load_errors.packages,
                "bazel query //... finished with errors under --keep_going; the index will be PARTIAL"
            );
            q.outcome.load_errors.clone()
        });
        Ok(Rows {
            updates: q.updates,
            partial,
        })
    }

    fn version(&self) -> String {
        self.bazel.version()
    }

    fn load_source(&self) -> Option<&dyn cambium_index::FileText> {
        Some(&self.files)
    }

    fn repo_mapping(&self) -> cambium_index::RepoMapping {
        match self.bazel.repo_mapping_dump() {
            Some(dump) => cambium_index::RepoMapping::from_dump(&dump),
            None => cambium_index::RepoMapping::identity(),
        }
    }

    fn workspace(&self) -> Option<&Path> {
        Some(self.bazel.workspace())
    }
}

/// A query's answer: every package Bazel could load, and — when it could not
/// load them all — what it said about the ones it dropped.
pub struct Rows {
    pub updates: Vec<PackageUpdate>,
    /// `Some` when the query exited 3 under `--keep_going`.
    pub partial: Option<cambium_bazel::LoadErrors>,
}

impl From<Vec<PackageUpdate>> for Rows {
    fn from(updates: Vec<PackageUpdate>) -> Self {
        Self {
            updates,
            partial: None,
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
    let outcome = bazel.query_packages(requested, |slab, ranges| {
        for t in decode_batch(slab, ranges, opts)? {
            grouped.entry(t.package.clone()).or_default().push(t);
        }
        Ok(())
    })?;
    Ok(finish(grouped, outcome, files, Some(requested)))
}

/// Queries the whole repository for a cold build (spec 6.9).
pub fn query_universe(
    bazel: &Bazel,
    opts: &ConvertOptions,
    files: &FileSource,
) -> Result<QueryResult, BazelError> {
    let mut grouped: BTreeMap<String, Vec<ParsedTarget>> = BTreeMap::new();
    let outcome = bazel.query_universe(|slab, ranges| {
        for t in decode_batch(slab, ranges, opts)? {
            grouped.entry(t.package.clone()).or_default().push(t);
        }
        Ok(())
    })?;
    Ok(finish(grouped, outcome, files, None))
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
        .map(|&(s, e)| {
            let t = Target::decode(&slab[s..e])?;
            Ok(convert(&t, opts))
        })
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
    cambium_proto::read_frames(bytes, cambium_proto::DEFAULT_BATCH, |slab, ranges| {
        for t in decode_batch(slab, ranges, opts)? {
            grouped.entry(t.package.clone()).or_default().push(t);
        }
        Ok(())
    })?;
    let outcome = QueryOutcome {
        exit_code: Some(0),
        success: true,
        stderr_tail: String::new(),
        targets_read: 0,
        load_errors: cambium_bazel::LoadErrors::default(),
    };
    Ok(finish(grouped, outcome, files, None).updates)
}

/// Empty `FileSource` for callers that do not care about BUILD hashes.
pub fn no_files() -> FileSource {
    FileSource::worktree(Path::new("/nonexistent"))
}
