//! How much of a repository one corpus run covers.
//!
//! The suite exists to produce the published numbers, and those come from the
//! full profile: the manifest's whole replay range, every selected pair, every
//! producer on every pair. That is an EC2 job measured in hours, which makes it
//! useless as a development loop — a change to the invalidator rules cannot
//! wait a working day to be shown wrong.
//!
//! The quick profile is the same code over a deterministically chosen slice:
//! the last [`QUICK_REPLAY_RANGE`] commits, at most [`QUICK_PAIR_CAP`] pairs,
//! and Tango — the slowest producer by an order of magnitude — on only the
//! first [`QUICK_TANGO_PAIRS`] of them. Nothing about the selection depends on
//! the clock or a random source, so two quick runs of the same pin compare
//! like for like, and a quick run's pairs are a subset of the full run's.
//!
//! The two profiles write to different directories ([`Profile::results_dir`]),
//! because a laptop's quick evidence must never be mistaken for, or overwrite,
//! the published numbers.

use std::path::{Path, PathBuf};

/// The environment variable `--profile` falls back to. Read by the CLI flag
/// itself, so a shell that exports it gets quick runs without repeating the
/// flag on every subcommand.
pub const PROFILE_ENV: &str = "CAMBIUM_CORPUS_PROFILE";

/// First-parent commits a quick replay covers, ending at the pin.
pub const QUICK_REPLAY_RANGE: usize = 50;

/// Pairs a quick comparison runs, across all kinds.
pub const QUICK_PAIR_CAP: usize = 50;

/// Pairs a *full* comparison runs by default — the `--cap` a published run
/// gets when nobody passes one.
///
/// It is a cap on the tail of an EC2 job, not a statement about the pair set:
/// the selection is deterministic, so raising it only ever adds pairs to the
/// end of the same list. Forty rather than twenty because the timing rows now
/// cover globally invalidated pairs too (see
/// `cambium_compare::threeway::summarise`), and a sample that has to carry
/// both an incremental and an all-pairs distribution needs the room.
pub const FULL_PAIR_CAP: usize = 40;

/// Pairs a quick comparison runs Tango on: the first this many, in the pair
/// file's own order. bazel-diff and cambium still run on all of them.
pub const QUICK_TANGO_PAIRS: usize = 10;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Profile {
    /// The published numbers: the manifest's range, every pair, every producer.
    #[default]
    Full,
    /// Local evidence: a bounded slice that finishes on a laptop.
    Quick,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" | "" => Some(Self::Full),
            "quick" => Some(Self::Quick),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Quick => "quick",
        }
    }

    pub fn is_quick(self) -> bool {
        self == Self::Quick
    }

    /// `corpus-results/<id>` for full, `corpus-results/<id>/quick` for quick.
    pub fn results_dir(self, root: &Path, id: &str) -> PathBuf {
        self.within(&root.join(id))
    }

    /// This profile's corner of a repository's results directory.
    pub fn within(self, repo_results: &Path) -> PathBuf {
        match self {
            Self::Full => repo_results.to_path_buf(),
            Self::Quick => repo_results.join(self.name()),
        }
    }

    /// The replay range this profile runs, given the manifest's.
    ///
    /// Never larger than the manifest's: a quick run is a slice of the full
    /// one, so the same commits are replayed under both and the store lineage
    /// they publish is shared.
    pub fn replay_range(self, manifest: usize) -> usize {
        match self {
            Self::Full => manifest,
            Self::Quick => manifest.min(QUICK_REPLAY_RANGE),
        }
    }

    /// The cap on how many pairs are compared, or `None` for "the caller's".
    pub fn pair_cap(self) -> Option<usize> {
        match self {
            Self::Full => None,
            Self::Quick => Some(QUICK_PAIR_CAP),
        }
    }

    /// How many pairs Tango runs on, or `None` for all of them.
    pub fn tango_pairs(self) -> Option<usize> {
        match self {
            Self::Full => None,
            Self::Quick => Some(QUICK_TANGO_PAIRS),
        }
    }
}

impl std::fmt::Display for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_parse_and_round_trip() {
        assert_eq!(Profile::parse("quick"), Some(Profile::Quick));
        assert_eq!(Profile::parse(" FULL "), Some(Profile::Full));
        assert_eq!(Profile::parse(""), Some(Profile::Full));
        assert_eq!(Profile::parse("quik"), None);
        assert_eq!(Profile::parse(Profile::Quick.name()), Some(Profile::Quick));
        assert_eq!(Profile::default(), Profile::Full);
    }

    #[test]
    fn quick_results_live_under_their_own_directory() {
        let root = Path::new("/r");
        assert_eq!(Profile::Full.results_dir(root, "envoy"), root.join("envoy"));
        assert_eq!(
            Profile::Quick.results_dir(root, "envoy"),
            root.join("envoy").join("quick")
        );
        assert_ne!(
            Profile::Quick.results_dir(root, "envoy"),
            Profile::Full.results_dir(root, "envoy"),
            "a quick run must not overwrite the published files"
        );
    }

    #[test]
    fn quick_bounds_the_range_but_never_grows_it() {
        assert_eq!(Profile::Full.replay_range(300), 300);
        assert_eq!(Profile::Quick.replay_range(300), QUICK_REPLAY_RANGE);
        assert_eq!(Profile::Quick.replay_range(10), 10);
        assert_eq!(Profile::Quick.pair_cap(), Some(QUICK_PAIR_CAP));
        assert_eq!(Profile::Quick.tango_pairs(), Some(QUICK_TANGO_PAIRS));
        assert_eq!(Profile::Full.pair_cap(), None);
        assert_eq!(Profile::Full.tango_pairs(), None);
    }

    #[test]
    fn a_full_run_compares_forty_pairs_by_default() {
        assert_eq!(FULL_PAIR_CAP, 40);
        // The quick profile's own cap is larger, and `do_compare` takes the
        // max of the two, so a quick run is never shaved down by the full
        // default.
        assert_eq!(FULL_PAIR_CAP.max(QUICK_PAIR_CAP), QUICK_PAIR_CAP);
    }
}
