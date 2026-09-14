//! The two tiers: what a walk checks at each step, and how often.
//!
//! One workload, two budgets. The correctness tier is what every engine
//! change pays for: the walk with no peers, the live `rdeps` oracle on a
//! sample of the steps. The full tier is the box run: the oracle on every
//! step, the peers on a category-stratified sample, the periodic rebuild
//! oracle for drift. Every flag below is an override of the tier's default,
//! recorded in the result beside the tier's name so a number says which
//! budget produced it.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Offline replay's live companion: cambium and the oracle, sampled.
    Correctness,
    /// Everything: the oracle everywhere, the peers on a sample.
    Full,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Self::Correctness => "correctness",
            Self::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "correctness" => Some(Self::Correctness),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// The tier's defaults, before any flag.
    pub fn defaults(self) -> Schedule {
        match self {
            Self::Correctness => Schedule {
                tier: self,
                peers: false,
                oracle_every_step: false,
                oracle_sample: 20,
                peer_sample: None,
                per_tag: 2,
                rebuild_every: None,
            },
            Self::Full => Schedule {
                tier: self,
                peers: true,
                oracle_every_step: true,
                oracle_sample: 0,
                peer_sample: Some(40),
                per_tag: 5,
                rebuild_every: None,
            },
        }
    }

    /// The tier's defaults with the flags applied.
    pub fn schedule(self, o: &Overrides) -> Schedule {
        let mut s = self.defaults();
        if let Some(n) = o.oracle_sample {
            // Asking for a sample is asking not to run it everywhere; `0`
            // is the way to say "every step" on the correctness tier.
            s.oracle_every_step = n == 0;
            s.oracle_sample = n;
        }
        if let Some(n) = o.peer_sample {
            s.peer_sample = Some(n);
        }
        if let Some(n) = o.per_tag {
            s.per_tag = n;
        }
        if o.rebuild_every.is_some() {
            s.rebuild_every = o.rebuild_every.filter(|n| *n > 0);
        }
        if let Some(p) = o.peers {
            s.peers = p;
        }
        s
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The flags that override a tier's defaults. `None` is "the tier's own".
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    /// `--oracle-sample N`: the live oracle on N sampled steps; 0 is every
    /// step.
    pub oracle_sample: Option<usize>,
    /// `--peer-sample N`: the peers on N sampled steps.
    pub peer_sample: Option<usize>,
    /// `--per-tag N`: the sampler's minimum per tag.
    pub per_tag: Option<usize>,
    /// `--rebuild-every N`: the rebuild oracle every N steps; 0 is never.
    pub rebuild_every: Option<usize>,
    /// Whether any peer runs at all; `--peers none` turns them off on the
    /// full tier, naming peers turns them on elsewhere.
    pub peers: Option<bool>,
}

/// What a walk does at each step, resolved from a tier and its overrides.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Schedule {
    pub tier: Tier,
    /// Run the peers on the peer sample.
    pub peers: bool,
    /// Run the `rdeps` oracle on every step.
    pub oracle_every_step: bool,
    /// Otherwise, on this many sampled steps.
    pub oracle_sample: usize,
    /// How many steps the peers run on; `None` is every step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_sample: Option<usize>,
    /// The sampler's per-tag minimum for both samples.
    pub per_tag: usize,
    /// Cold-build and compare every this many steps, on top of the rebuild
    /// oracle at the pin. `None` is only at the pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebuild_every: Option<usize>,
}

impl Schedule {
    /// Whether step `i` (1-based, counting from the first step after the cold
    /// build) gets a mid-walk rebuild oracle.
    pub fn rebuilds_at(&self, i: usize) -> bool {
        matches!(self.rebuild_every, Some(n) if n > 0 && i % n == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_parse_and_have_the_documented_defaults() {
        assert_eq!(Tier::parse("full"), Some(Tier::Full));
        assert_eq!(Tier::parse("correctness"), Some(Tier::Correctness));
        assert_eq!(Tier::parse("quick"), None);
        let c = Tier::Correctness.defaults();
        assert!(!c.peers && !c.oracle_every_step && c.oracle_sample > 0);
        let f = Tier::Full.defaults();
        assert!(f.peers && f.oracle_every_step && f.peer_sample.is_some());
        assert_eq!(f.rebuild_every, None);
    }

    #[test]
    fn a_sample_flag_turns_the_every_step_oracle_off_and_zero_turns_it_on() {
        let s = Tier::Full.schedule(&Overrides {
            oracle_sample: Some(10),
            ..Default::default()
        });
        assert!(!s.oracle_every_step);
        assert_eq!(s.oracle_sample, 10);
        let s = Tier::Correctness.schedule(&Overrides {
            oracle_sample: Some(0),
            ..Default::default()
        });
        assert!(s.oracle_every_step);
    }

    #[test]
    fn the_rebuild_cadence_counts_from_the_first_step() {
        let s = Tier::Full.schedule(&Overrides {
            rebuild_every: Some(3),
            ..Default::default()
        });
        assert!(!s.rebuilds_at(1) && !s.rebuilds_at(2) && s.rebuilds_at(3) && s.rebuilds_at(6));
        let never = Tier::Full.schedule(&Overrides {
            rebuild_every: Some(0),
            ..Default::default()
        });
        assert_eq!(never.rebuild_every, None);
        assert!(!never.rebuilds_at(3));
    }
}
