//! Running cambium against repositories nobody generated.
//!
//! The synthetic oracles prove the engine over graphs with the *shape* of a
//! monorepo. This crate points it at real ones — real macros, real `select`
//! patterns, real `exports_files`, real broken packages — and at their real
//! commit histories:
//!
//! - [`manifest`]: `corpus.toml`, the pinned list of repositories.
//! - [`checkout`]: bare mirrors and worktrees under `$CAMBIUM_CORPUS_DIR`, the
//!   generated per-repo `cambium.toml`, and the shared Bazel caches.
//! - [`pairs`]: deterministic `(base, head)` commit pairs — adjacent, PR-shaped
//!   and stress — that the three-way comparison consumes.
//! - [`replay`]: the updater replay over a first-parent range, ending with the
//!   incremental-equals-full oracle on real input.
//! - [`state`]: the resumable per-repo state file.
//!
//! Nothing here builds or tests a corpus repository; `bazel query` loading is
//! the only Bazel work involved, and the replay takes its row source through
//! the same [`cambium_engine::PackageQuerier`] trait the engine uses, so the
//! whole replay can be exercised over a synthetic git history with no Bazel.

pub mod checkout;
pub mod manifest;
pub mod oracle;
pub mod pairs;
pub mod replay;
pub mod state;

pub use checkout::{Checkout, CorpusDir};
pub use manifest::{Manifest, RepoEntry};
pub use pairs::{Pair, PairKind, PairSet};
pub use replay::{replay, CommitRecord, Driver, ReplayOptions, ReplayResult};
pub use state::State;

/// Where a corpus run writes its results: `corpus-results/<id>/`.
pub fn results_dir(root: &std::path::Path, id: &str) -> std::path::PathBuf {
    root.join(id)
}
