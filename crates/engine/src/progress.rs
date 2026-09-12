//! What `affected` says while it runs.
//!
//! A run over a real repository spends its time in three places — finding an
//! index for the base, bringing one forward to the base, and querying the
//! packages the diff dirtied — and on a large graph each of those is seconds
//! to minutes. This is the vocabulary the run speaks about them, in the same
//! shape [`crate::profile::Progress`] uses for the profile walk: one enum,
//! nothing in it prints, and a caller that wants nothing told passes a
//! callback that drops it. The CLI renders it through `cambium_ui`.
//!
//! The splice phase reports the replay as a single step rather than one per
//! commit, because that is what the engine does: the dirty set over
//! `ancestor..base` is the union of the range and re-querying a package once
//! per commit would buy nothing. The row carries the same
//! [`crate::profile::CommitStep`] shape a profile row does, so the two render
//! through one layout.

use crate::profile::CommitStep;

/// A stage of an `affected` run, as a renderer hears it.
#[derive(Clone, Debug)]
pub enum Progress {
    /// The search for an index has started, at the base itself.
    IndexLookup { base: String },
    /// Nothing at the base, so the first-parent ancestors are being walked.
    /// `ancestors` is `baseline.max_ancestor_walk`, the most that will be
    /// checked.
    SearchStart { base: String, ancestors: usize },
    /// One more ancestor has been asked about and had nothing. A tick, not a
    /// milestone: a renderer with a live line counts these off on it, and one
    /// without drops them.
    AncestorChecked {
        checked: usize,
        of: usize,
        sha: String,
    },
    /// An index was found, at the base itself (`behind` 0) or at
    /// `base~behind`.
    IndexFound {
        sha: String,
        behind: u32,
        /// The index's lineage, as hex.
        lineage: String,
        /// How old the index is, in seconds, from the provenance it carries.
        age_secs: Option<u64>,
        elapsed_ms: u64,
    },
    /// No index within the ancestor walk, so one is being built cold.
    IndexAbsent { searched: usize, elapsed_ms: u64 },
    /// The cold build at `commit` is running.
    ColdBuild { commit: String },
    /// It finished, with the size of the graph it found.
    ColdBuilt { targets: u32, elapsed_ms: u64 },
    /// The index found at `from` is being replayed forward to `to`.
    SpliceStart {
        from: String,
        to: String,
        commits: usize,
    },
    /// The replay is over: one row, in the profile's shape.
    SpliceStep { step: Box<CommitStep> },
    /// And what it left behind.
    SpliceDone {
        at: String,
        rows_rewritten: usize,
        elapsed_ms: u64,
    },
    /// No replay was needed, and why.
    SpliceSkipped { reason: String },
    /// The diff, the query and the reverse walk are running.
    QueryStart,
    /// They are over.
    QueryDone {
        changed_files: usize,
        packages: usize,
        affected: usize,
        tests: usize,
        elapsed_ms: u64,
    },
    /// The range invalidated everything, so there is no answer to narrate.
    Global { reason: String },
}

/// What a caller that wants nothing told passes.
pub fn quiet(_: Progress) {}
