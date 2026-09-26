//! Engine errors, mapped to the CLI's documented exit codes (spec 10).

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Git(#[from] cambium_diff::GitError),
    #[error(transparent)]
    Bazel(#[from] cambium_bazel::BazelError),
    #[error("bazel query for {what} failed ({})\n--- last stderr ---\n{stderr}", exit_text(*.code))]
    BazelQuery {
        what: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error(transparent)]
    Index(#[from] cambium_index::IndexError),
    #[error(transparent)]
    Classify(#[from] cambium_classify::ClassifyError),
    /// An index query named something the index does not have.
    #[error(transparent)]
    Query(#[from] crate::inspect::QueryError),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Proto(#[from] cambium_proto::ReadError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("io error on {path}: {source}")]
    IoAt {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "no index is available for {base}, in the store or within the ancestor walk, and \
         --no-full-build was given"
    )]
    BaselineUnavailable { base: String },
    #[error(
        "{path} was built with a different cambium.toml; splicing onto it could \
         disagree with a full build. Run `cambi index build` to mint a new lineage."
    )]
    ConfigHashMismatch { path: PathBuf },
    #[error("no index store is configured")]
    NoStore,
    #[error(
        "refusing to publish the index for {commit}: bazel could not load every package \
         ({packages_in_error} error lines), and an index missing packages would silently \
         under-report for every run spliced onto it. Name the packages whose absence is \
         expected here in baseline.publish_partial, or pass --allow-partial for this run."
    )]
    PartialIndex {
        commit: String,
        packages_in_error: u32,
    },
    /// `baseline.publish_partial` names the packages whose absence is
    /// expected, and something else failed to load. The expected list is the
    /// whole point: a subject that cannot load its Windows toolchains on
    /// Linux still must not publish an index short a repository that failed
    /// to fetch.
    #[error(
        "refusing to publish the index for {commit}: bazel could not load {} package(s) \
         that baseline.publish_partial does not name ({}), so this index is short more \
         than the absences this subject expects",
        .packages.len(),
        .packages.join(", ")
    )]
    PartialIndexUnexpected {
        commit: String,
        packages: Vec<String>,
    },
    /// More packages failed than bazel's stderr recorded names for, so the
    /// failures cannot be checked against the expected list at all.
    #[error(
        "refusing to publish the index for {commit}: {count} packages failed to load but \
         only {named} were named in bazel's stderr, so baseline.publish_partial's list \
         cannot be checked against them"
    )]
    PartialIndexUnverifiable {
        commit: String,
        count: u32,
        named: usize,
    },
    /// `index update` was asked to bring an index forward across a range
    /// that invalidates everything, and told not to rebuild instead.
    #[error("the range invalidates everything ({reason}); the index cannot be brought forward")]
    RangeInvalidated { reason: String },
}

impl EngineError {
    /// `map_err` adapter that records which path an io error was about:
    /// `std::fs::read(&p).map_err(EngineError::io_at(&p))?`.
    pub fn io_at(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> EngineError {
        let path = path.into();
        move |source| EngineError::IoAt { path, source }
    }

    /// Exit code for this failure (spec 10).
    ///
    /// 0 through 4 are what callers have always branched on and never change
    /// meaning. 5 is "the store could not be reached", which is worth telling
    /// apart from "the store has no index for this commit": a CI job can retry
    /// the first and build for the second. `NotConfigured` is not that — it is
    /// a configuration error, and stays 1.
    pub fn exit_code(&self) -> i32 {
        match self {
            EngineError::Bazel(_) | EngineError::BazelQuery { .. } => 3,
            EngineError::BaselineUnavailable { .. } => 4,
            EngineError::Store(
                crate::store::StoreError::Backend { .. } | crate::store::StoreError::Io { .. },
            ) => 5,
            _ => 1,
        }
    }

    /// This failure as a diagnostic, so an error renders and serialises like
    /// every warning does.
    ///
    /// The message is the error's own `Display`; the code is what a caller
    /// branches on, and the `help:` line is there whenever there is an action
    /// to name.
    pub fn diagnostic(&self) -> crate::diag::Diagnostic {
        use crate::diag::{Code, Diagnostic};
        use crate::store::StoreError;

        // A Bazel that could not be started at all is the fact `doctor`
        // reports as `CAMBI-E015`, and it gets the same code and help here:
        // the workspace is fine, the binary is what is missing.
        if let EngineError::Bazel(cambium_bazel::BazelError::Spawn { source, .. }) = self {
            let help = match source.kind() {
                std::io::ErrorKind::NotFound => {
                    "install Bazel or bazelisk, or name the binary with --bazel-binary, \
                     [bazel].binary in cambium.toml, or bazel.binary in cambium.local.toml"
                }
                _ => {
                    "check that the binary can be run by this user, or name another with \
                     --bazel-binary, [bazel].binary in cambium.toml, or bazel.binary in \
                     cambium.local.toml"
                }
            };
            return Diagnostic::error(Code::E015BazelNotFound, self.to_string()).with_help(help);
        }
        let (code, help): (Code, Option<&str>) = match self {
            EngineError::Config(crate::config::ConfigError::LocalOverrideKey { .. }) => (
                Code::E014LocalOverrideKey,
                Some(
                    "move the key into cambium.toml; cambium.local.toml may only set \
                     bazel.binary, bazel.startup_flags, baseline.cache_dir, \
                     baseline.compress_indexes and baseline.publish_partial",
                ),
            ),
            EngineError::Config(_) => (
                Code::E002Config,
                Some("fix the file the message names, or point --config at another one"),
            ),
            EngineError::Git(_) => (
                Code::E003Git,
                Some(
                    "check that both revisions of the range exist in this clone; a shallow \
                     clone may need more history (`git fetch --deepen=N`)",
                ),
            ),
            EngineError::Bazel(_) => (
                Code::E004Bazel,
                Some("run from inside the Bazel workspace or pass -C, and check --bazel-binary"),
            ),
            EngineError::BazelQuery { .. } => (Code::E005BazelQuery, None),
            EngineError::BaselineUnavailable { .. } => (
                Code::E006BaselineUnavailable,
                Some("run `cambi index build --publish`, or drop --no-full-build"),
            ),
            EngineError::ConfigHashMismatch { .. } => (
                Code::E007ConfigHashMismatch,
                Some("mint a new lineage: `cambi index build --publish`"),
            ),
            EngineError::NoStore => (
                Code::E008NoStore,
                Some("set [baseline].store in cambium.toml"),
            ),
            EngineError::PartialIndexUnexpected { .. } => (
                Code::E009PartialIndex,
                Some(
                    "fix the load errors, or add the packages to \
                     baseline.publish_partial if their absence is expected here",
                ),
            ),
            EngineError::PartialIndexUnverifiable { .. } => (
                Code::E009PartialIndex,
                Some(
                    "fix the load errors: past 32 error lines bazel stops naming \
                     packages, so an expected-package list cannot clear this index",
                ),
            ),
            EngineError::PartialIndex { .. } => (
                Code::E009PartialIndex,
                Some(
                    "fix the load errors, or name the expected absences in \
                     baseline.publish_partial; --allow-partial overrides for one run",
                ),
            ),
            EngineError::Store(StoreError::NotConfigured) => (
                Code::E008NoStore,
                Some("set [baseline].store in cambium.toml"),
            ),
            EngineError::Store(StoreError::Backend { .. }) => (
                Code::E010StoreBackend,
                Some("check that [baseline].store is reachable and its credentials are current"),
            ),
            EngineError::Store(StoreError::Io { .. }) => (
                Code::E011StoreIo,
                Some("check the path and the permissions on [baseline].store"),
            ),
            EngineError::Store(StoreError::Unsupported(op)) => {
                (Code::E012StoreUnsupported, Some(unsupported_help(op)))
            }
            EngineError::Query(_) => (
                Code::E017IndexQuery,
                Some(
                    "check the spelling against the index: `cambi inspect targets <package>` \
                     lists what a package holds",
                ),
            ),
            EngineError::RangeInvalidated { .. } => (
                Code::E018RangeInvalidated,
                Some("drop --no-rebuild-on-global to mint a fresh lineage instead"),
            ),
            // The transparent wrappers: io, protobuf, the index format, the
            // classifier. One code, because there is no action to name that
            // the message does not already carry.
            EngineError::Index(_)
            | EngineError::Classify(_)
            | EngineError::Proto(_)
            | EngineError::Io(_)
            | EngineError::IoAt { .. } => (Code::E001Internal, None),
        };
        let d = Diagnostic::error(code, self.to_string());
        match help {
            Some(h) => d.with_help(h),
            None => d,
        }
    }
}

/// How a child process ended, for a message: `exit 1`, or `killed by a signal` when
/// there is no exit code to print. Never Rust's `Some(1)`/`None`.
pub(crate) fn exit_text(code: Option<i32>) -> String {
    match code {
        Some(c) => format!("exit {c}"),
        None => "killed by a signal".to_string(),
    }
}

/// The `help:` line of `CAMBI-E012`, by the operation the backend lacks.
/// `docs/DIAGNOSTICS.md`'s row says the same for each.
fn unsupported_help(op: &str) -> &'static str {
    match op {
        "gc" => {
            "garbage collection runs on a `dir` store only; give an S3 or GCS prefix a \
             lifecycle rule instead (docs/STORE.md, \"Garbage collection\")"
        }
        op if op.starts_with("list") => {
            "listing needs a store that can enumerate: a `dir` store, or a `command` store \
             with `list_cmd` set"
        }
        "fetch" => "set `fetch_cmd` in [baseline].store",
        "publish" => "set `push_cmd` in [baseline].store",
        _ => "use a store backend that supports it, or skip the operation",
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;
