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
    PartialIndex { commit: String, packages_in_error: u32 },
}

impl EngineError {
    /// `map_err` adapter that records which path an io error was about:
    /// `std::fs::read(&p).map_err(EngineError::io_at(&p))?`.
    pub fn io_at(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> EngineError {
        let path = path.into();
        move |source| EngineError::IoAt { path, source }
    }

    /// Exit code for this failure (spec 10).
    pub fn exit_code(&self) -> i32 {
        match self {
            EngineError::Bazel(_) | EngineError::BazelQuery { .. } => 3,
            EngineError::BaselineUnavailable { .. } => 4,
            _ => 1,
        }
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;
