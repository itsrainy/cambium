//! Three-way comparison: cambium against another affected-targets tool and
//! against Bazel itself.
//!
//! The synthetic oracles prove the engine over graphs it generated, and
//! [`cambium_corpus`] proves the updater over real histories. What neither
//! answers is the question a person evaluating cambium actually asks: *does it
//! agree with the tool I use today, and when it does not, who is right?* This
//! crate answers it, per commit pair:
//!
//! - [`producer`]: the pluggable "other tool" — [`bazeldiff`] today, Uber's
//!   Tango or anything else tomorrow — plus the second git worktree such a
//!   tool needs to see the base revision.
//! - [`oracle`]: a seeded sample of `bazel query rdeps(...)` calls, the only
//!   ground truth in the picture. cambium must be a superset of it.
//! - [`bzloracle`]: the same idea for `.bzl` changes — `rbuildfiles` against
//!   the static load closure the engine now computes itself.
//! - [`classify`]: why a disagreement happened, in the spec's seven classes.
//!   Two of them fail the run; the rest are expected and counted.
//! - [`threeway`]: the driver, its JSON, and the precision/recall arithmetic.
//! - [`report`]: the markdown that lands in `docs/CORPUS.md`.
//!
//! [`bazeldiff`] and [`tango`] are the two producers that ship.

pub mod bazeldiff;
pub mod bzloracle;
pub mod classify;
pub mod oracle;
pub mod producer;
pub mod report;
pub mod tango;
pub mod threeway;

pub use bazeldiff::{BazelDiff, Implementation};
pub use bzloracle::{BzlOracleResult, BzlOracleSummary};
pub use classify::{classify, BazelFacts, Class, Facts, Side};
pub use oracle::{OracleResult, RdepsOracle};
pub use producer::{normalise_label, BaseWorktree, Producer, ProducerRun, RunContext};
pub use report::{
    render_bench_section, render_repo_section, render_top_table, top_row, update_doc, BenchLegacy,
    RepoResults, TopRow,
};
pub use tango::Tango;
pub use threeway::{
    compare_pair, compare_pair_with, compare_pairs, CompareOptions, CompareResult, CompareSummary,
    PairComparison, ProducerComparison,
};
