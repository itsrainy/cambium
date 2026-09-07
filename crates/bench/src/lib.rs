//! The cambium benchmark: what it runs on, what it runs, and what checks it.
//!
//! The vocabulary is [`docs/METHODOLOGY.md`](../../../docs/METHODOLOGY.md) and
//! this crate is its implementation. Four nouns, and every module is one of
//! them:
//!
//! - **Subject** — a Bazel repository a benchmark points at ([`subject`]),
//!   either a real one from `subject_repos.toml` ([`manifest`]) or a synthetic
//!   one emitted from a preset. Mirrors, worktrees, stores and the generated
//!   per-subject `cambium.toml` live in [`checkout`], under the subjects cache
//!   directory ([`checkout::SubjectsDir`]).
//! - **Tool** — anything that answers "which targets could this change
//!   affect": cambium itself, bazel-diff and Tango ([`tools`]).
//! - **Oracle** — an independent check named by what it proves
//!   ([`oracles`]): `rdeps` (did a tool miss a target?), `rebuild` (is the
//!   incremental updater exact?) and `load` (is the static `.bzl` load closure
//!   complete?).
//! - **Workload** — what the tools are run on. [`history`] applies a subject's
//!   last N first-parent commits one at a time to an index built at the start
//!   of the window; [`changes`] selects `(base, head)` change sets shaped like
//!   the question a CI job asks, and [`compare`] runs every tool and both
//!   incremental oracles over them. [`classify`] files each disagreement,
//!   [`report`] renders `docs/RESULTS.md`, and [`state`] is what makes a
//!   killed run resumable.
//!
//! Underneath those sit the engine micro-benchmarks the workloads do not
//! cover: [`stages`] times each in-memory stage over a generated graph at
//! sizes no real subject reaches, [`runner`] drives them and the sweeps, and
//! [`legacy`] is the like-for-like bazel-diff stage that puts both tools on
//! one synthetic graph with Bazel replaced by a shim that replays a recorded query.

pub mod changes;
pub mod checkout;
pub mod classify;
pub mod compare;
pub mod harness;
pub mod history;
pub mod legacy;
pub mod manifest;
pub mod oracles;
pub mod report;
pub mod runner;
pub mod stages;
pub mod state;
pub mod subject;
pub mod tools;

pub use changes::{ChangeSet, ChangeSetKind, ChangeSetSet};
pub use checkout::{Checkout, SubjectsDir};
pub use classify::{classify, BazelFacts, Class, Facts, Side};
pub use compare::{
    compare_change_set, compare_change_set_with, compare_change_sets, ChangeSetResult,
    ChangesOptions, ChangesResult, ChangesSummary, ToolComparison,
};
pub use harness::{BenchReport, Machine, StageResult};
pub use history::{history, CommitRecord, Driver, HistoryOptions, HistoryResult};
pub use manifest::{Manifest, RepoEntry};
pub use oracles::load::{LoadOracleResult, LoadOracleSummary};
pub use oracles::rdeps::{RdepsOracle, RdepsResult};
pub use report::{
    render_bench_section, render_subject_section, render_top_table, top_row, update_doc,
    BenchLegacy, SubjectResults, TopRow,
};
pub use stages::{StageContext, IN_MEMORY_STAGES};
pub use state::State;
pub use subject::{Subject, Synthetic};
pub use tools::bazeldiff::{BazelDiff, Implementation};
pub use tools::cambium::Cambium;
pub use tools::tango::Tango;
pub use tools::{normalise_label, BaseWorktree, RunContext, Tool, ToolRun};

/// The version stamped into every result file this crate writes.
///
/// It is the benchmark's own contract, not the engine's: the two used to share
/// `cambium_engine::report::SCHEMA_VERSION`, which meant a benchmark field could
/// not be renamed without claiming the `cambi affected` JSON had changed.
/// Bumped to 6 by the rename to the methodology's vocabulary — `pairs` became
/// `change_sets`, `producers` became `tools`, and the files moved from
/// `corpus-results/<id>/` to `bench-results/<subject>/<workload>/` — so a
/// result file written before it is not readable here.
pub const RESULT_SCHEMA: u32 = 6;

/// How many first-parent commits a history workload covers when nobody says.
///
/// `None` here means "the subject's own window", which is what the manifest's
/// `history_window` is for; the CLI's `--commits N` overrides it.
pub const DEFAULT_CHANGE_SETS: usize = 40;

/// The sizes `--quick` sets: a bounded, deterministic slice that finishes on a
/// laptop. It is sugar for three flags, not a mode — the results land in the
/// same tree as any other run, and the sizes that produced them are inside the
/// result file.
pub const QUICK_COMMITS: usize = 50;
pub const QUICK_CHANGE_SETS: usize = 50;
pub const QUICK_TANGO_CHANGE_SETS: usize = 10;

/// `bench-results/<subject>/<workload>/`.
pub fn workload_dir(root: &std::path::Path, subject: &str, workload: &str) -> std::path::PathBuf {
    root.join(subject).join(workload)
}

/// `bench-results/<subject>/` — the per-subject directory that holds
/// `state.json` and the cold indexes every workload shares.
pub fn subject_dir(root: &std::path::Path, subject: &str) -> std::path::PathBuf {
    root.join(subject)
}
