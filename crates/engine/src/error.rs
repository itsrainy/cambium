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
    #[error("bazel query for {what} failed (exit {code:?})\n--- last stderr ---\n{stderr}")]
    BazelQuery {
        what: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error(transparent)]
    Index(#[from] cambium_index::IndexError),
    #[error(transparent)]
    Classify(#[from] cambium_classify::ClassifyError),
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
    #[error("no baseline index available for {base} and --no-full-build was given")]
    BaselineUnavailable { base: String },
    #[error(
        "{path} was built with a different cambium.toml; splicing onto it could \
         disagree with a full build. Run `cambi baseline build` to mint a new lineage."
    )]
    ConfigHashMismatch { path: PathBuf },
    #[error("no baseline store is configured; set [baseline].store in cambium.toml")]
    NoStore,
    #[error(
        "refusing to publish the index for {commit}: bazel could not load every package \
         ({packages_in_error} error lines), and an index missing packages would silently \
         under-report for every run spliced onto it. Pass --allow-partial, or set \
         baseline.publish_partial = true, to publish it anyway."
    )]
    PartialIndex {
        commit: String,
        packages_in_error: u32,
    },
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

        let (code, help): (Code, Option<&str>) = match self {
            EngineError::Config(_) => (
                Code::E002Config,
                Some("fix the file the message names, or point --config at another one"),
            ),
            EngineError::Git(_) => (
                Code::E003Git,
                Some("check that the revisions passed to --base and --head exist here"),
            ),
            EngineError::Bazel(_) => (
                Code::E004Bazel,
                Some("run from inside the Bazel workspace or pass -C, and check --bazel-binary"),
            ),
            EngineError::BazelQuery { .. } => (Code::E005BazelQuery, None),
            EngineError::BaselineUnavailable { .. } => (
                Code::E006BaselineUnavailable,
                Some("run `cambi baseline build --publish`, or drop --no-full-build"),
            ),
            EngineError::ConfigHashMismatch { .. } => (
                Code::E007ConfigHashMismatch,
                Some("mint a new lineage: `cambi baseline build --publish`"),
            ),
            EngineError::NoStore => (
                Code::E008NoStore,
                Some("set [baseline].store in cambium.toml"),
            ),
            EngineError::PartialIndex { .. } => (
                Code::E009PartialIndex,
                Some(
                    "fix the load errors, or pass --allow-partial (or set \
                     baseline.publish_partial = true) to publish it anyway",
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
            EngineError::Store(StoreError::Unsupported(_)) => (
                Code::E012StoreUnsupported,
                Some("use a store backend that supports it, or skip the operation"),
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

pub type Result<T> = std::result::Result<T, EngineError>;
