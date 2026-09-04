//! Engine errors, mapped to the CLI's documented exit codes (spec 10).

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("no Bazel workspace found above {0}")]
    NoWorkspace(PathBuf),
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
    #[error("no baseline index available for {base} and --no-full-build was given")]
    BaselineUnavailable { base: String },
    #[error(
        "{path} was built with a different cambium.toml; splicing onto it could \
         disagree with a full build. Run `cambi baseline build` to mint a new lineage."
    )]
    ConfigHashMismatch { path: PathBuf },
    #[error("no baseline store is configured; set [baseline].store in cambium.toml")]
    NoStore,
}

impl EngineError {
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
