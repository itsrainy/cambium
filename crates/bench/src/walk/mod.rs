//! The walk: one first-parent traversal, two tiers, the oracle as the only
//! ground truth.
//!
//! A walk cold-builds an index at the start of a first-parent window and then
//! takes one step per commit forward to the pin, each step `(parent, head)`
//! with the index carried from the step before — the shape a CI job's
//! incremental run has. Every step asks the same three questions of cambium
//! and of every peer alike: did it miss anything (recall against the rdeps
//! oracle), how much wider than the truth was it (tightness against the
//! oracle), and what did it cost (the per-step wall CI pays). The oracle is
//! ground truth for cambium too; the peers survive only as the explanation
//! layer, never as the yardstick.
//!
//! Two tiers ([`tier`]) share this one workload. The correctness tier — no
//! peers, the oracle on a sample — is what every engine change pays for,
//! beside the offline [`crate::replay`]. The full tier — the oracle on every
//! step, the peers on a stratified sample, the periodic rebuild oracle for
//! drift — is the box run.
//!
//! The pieces: [`steps`](mod@steps) turns the window into tagged `(parent, head)` pairs;
//! [`sampling`] draws the seeded, prefix-stable, stratified sample the
//! expensive checks run on; [`tier`] resolves a tier and its flags into a
//! schedule; [`model`] is what a walk is (every type in the result JSON) and
//! the pure arithmetic that rolls it up. [`driver`] runs it: the cold build,
//! the carried index, and the checks the schedule calls for at each step.

pub mod cambi;
pub mod driver;
pub mod live;
pub mod model;
pub mod rebuild_rows;
pub mod report;
pub mod sampling;
pub mod steps;
pub mod tier;

pub use cambi::{
    cold_build, tree_paths, CambiDriver, ColdBuild, Driver, EngineDriver, RowSource, Update,
};
pub use driver::{score, walk, walk_only, AnsweredStep, LiveChecks, NoLiveChecks, WalkOptions};
pub use live::BazelChecks;
pub use model::{StepRecord, WalkResult, WalkSummary};
pub use rebuild_rows::{RebuildRows, Row, RowClass, RowCounts};
pub use sampling::{sample, Coverage, Sample};
pub use steps::{steps, tag_steps, Probes, Step, Tag};
pub use tier::{Overrides, Schedule, Tier};
