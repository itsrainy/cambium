//! Which steps get the expensive checks: a seeded, prefix-stable, stratified
//! sample over tagged steps.
//!
//! Two populations are drawn this way. In the correctness tier the live
//! `rdeps` oracle runs on a sample of the steps; in the full tier the peers
//! run on a sample. Both want the same thing: every tag represented, the
//! choice a pure function of the seed and the steps, and a bigger total a
//! superset of a smaller one at the same seed and per-tag minimum, so a run
//! that raised `--steps` or its sample did not throw earlier steps away.
//! (Raising the per-tag minimum against a binding total can trade one step
//! for another, since the minima are filled before the cap.)
//!
//! The rank of a step is a hash of the seed and its head sha, so which steps
//! are chosen does not depend on how many steps the window holds: `--steps
//! 40` and `--steps 300` at the same pin agree on every step both contain.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::steps::{Step, Tag};

/// How a tag fared in the sample.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Coverage {
    /// Steps in the walk carrying the tag.
    pub present: usize,
    /// Steps in the sample carrying it.
    pub sampled: usize,
    /// The rule's minimum. A `sampled` below it is a deficit the report
    /// shows; it means the window did not hold enough of that tag, never
    /// that the sampler skipped them.
    pub wanted: usize,
}

impl Coverage {
    pub fn deficit(&self) -> usize {
        self.wanted.saturating_sub(self.sampled)
    }
}

/// The sample: which step indices, and how each tag is covered.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sample {
    pub seed: u64,
    /// The per-tag minimum the sample was drawn with.
    pub per_tag: usize,
    /// The cap on the whole sample, when there was one. The per-tag minima
    /// are always honoured; the cap bounds what is filled in after them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// Indices into the step list, sorted.
    pub chosen: BTreeSet<usize>,
    pub coverage: BTreeMap<Tag, Coverage>,
}

impl Sample {
    pub fn contains(&self, step: usize) -> bool {
        self.chosen.contains(&step)
    }

    pub fn len(&self) -> usize {
        self.chosen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chosen.is_empty()
    }

    /// Every step, for the tier that runs the check everywhere.
    pub fn all(steps: &[Step]) -> Self {
        let mut coverage = BTreeMap::new();
        for t in Tag::ALL {
            let present = steps.iter().filter(|s| s.has(t)).count();
            coverage.insert(
                t,
                Coverage {
                    present,
                    sampled: present,
                    wanted: 0,
                },
            );
        }
        Self {
            seed: 0,
            per_tag: 0,
            total: None,
            chosen: (0..steps.len()).collect(),
            coverage,
        }
    }
}

/// FNV-1a over the seed and the sha: a stable rank, independent of the
/// standard library's hasher and of the step's position in the window.
fn rank(seed: u64, head: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in seed.to_le_bytes().iter().chain(head.as_bytes()) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Draws the sample: at least `per_tag` steps of every tag the window
/// holds, then the lowest-ranked remaining steps up to `total`.
///
/// Pure in `(steps, seed, per_tag, total)`. Raising `total` at a fixed
/// per-tag minimum only adds steps; so does raising `per_tag` when no total
/// cap binds.
pub fn sample(steps: &[Step], seed: u64, per_tag: usize, total: Option<usize>) -> Sample {
    let mut order: Vec<usize> = (0..steps.len()).collect();
    order.sort_by_key(|&i| (rank(seed, &steps[i].head), i));

    let mut chosen: BTreeSet<usize> = BTreeSet::new();
    for t in Tag::ALL {
        for &i in order.iter().filter(|&&i| steps[i].has(t)).take(per_tag) {
            chosen.insert(i);
        }
    }
    if let Some(total) = total {
        for &i in &order {
            if chosen.len() >= total {
                break;
            }
            chosen.insert(i);
        }
    } else {
        chosen.extend(order.iter().copied());
    }

    let mut coverage = BTreeMap::new();
    for t in Tag::ALL {
        coverage.insert(
            t,
            Coverage {
                present: steps.iter().filter(|s| s.has(t)).count(),
                sampled: chosen.iter().filter(|&&i| steps[i].has(t)).count(),
                wanted: per_tag,
            },
        );
    }
    Sample {
        seed,
        per_tag,
        total,
        chosen,
        coverage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(n: usize, tags: &[Tag]) -> Step {
        let mut tags = tags.to_vec();
        tags.sort();
        Step {
            parent: format!("p{n}"),
            head: format!("h{n}"),
            tags,
        }
    }

    /// 100 adjacent, ten of them bzl, three merges, one invalidator.
    fn window() -> Vec<Step> {
        (0..100)
            .map(|n| {
                let mut tags = vec![if n % 33 == 32 {
                    Tag::Merge
                } else {
                    Tag::Adjacent
                }];
                if n % 10 == 0 {
                    tags.push(Tag::Bzl);
                }
                if n == 57 {
                    tags.push(Tag::Invalidator);
                }
                step(n, &tags)
            })
            .collect()
    }

    #[test]
    fn the_sample_is_a_pure_function_and_covers_every_tag() {
        let w = window();
        let a = sample(&w, 7, 2, Some(20));
        assert_eq!(a, sample(&w, 7, 2, Some(20)));
        assert_eq!(a.len(), 20);
        assert!(a.coverage[&Tag::Bzl].sampled >= 2);
        assert!(a.coverage[&Tag::Merge].sampled >= 2);
        assert_eq!(a.coverage[&Tag::Invalidator].sampled, 1);
        assert_eq!(a.coverage[&Tag::Invalidator].present, 1);
        // One invalidator step exists and two were wanted: a deficit of one,
        // recorded rather than hidden.
        assert_eq!(a.coverage[&Tag::Invalidator].deficit(), 1);
        assert_eq!(a.coverage[&Tag::Hub].present, 0);
        assert_eq!(a.coverage[&Tag::Hub].deficit(), 2);
        assert_ne!(
            sample(&w, 8, 2, Some(20)).chosen,
            a.chosen,
            "the seed matters"
        );
    }

    #[test]
    fn a_bigger_total_is_a_superset_at_the_same_seed_and_per_tag() {
        let w = window();
        let small = sample(&w, 3, 2, Some(10));
        let big = sample(&w, 3, 2, Some(40));
        assert!(small.chosen.is_subset(&big.chosen));
        // Raising per_tag with no binding cap only adds steps.
        let base = sample(&w, 3, 2, None);
        let more = sample(&w, 3, 4, None);
        assert!(base.chosen.is_subset(&more.chosen));
        // No cap is every step.
        assert_eq!(base.len(), w.len());
    }

    /// The rank is by sha, so a step keeps its place when the window grows:
    /// with no per-tag minima, whatever the long window chose from the short
    /// window's range, the short window chose too.
    #[test]
    fn the_choice_does_not_depend_on_the_window_size() {
        let w = window();
        let short = sample(&w[60..], 5, 0, Some(8));
        let long = sample(&w, 5, 0, Some(8));
        let short_heads: BTreeSet<&str> = short
            .chosen
            .iter()
            .map(|&i| w[60 + i].head.as_str())
            .collect();
        for &i in &long.chosen {
            if i >= 60 {
                assert!(
                    short_heads.contains(w[i].head.as_str()),
                    "{} moved",
                    w[i].head
                );
            }
        }
    }

    #[test]
    fn every_step_is_coverage_without_a_wanted_minimum() {
        let w = window();
        let all = Sample::all(&w);
        assert_eq!(all.len(), w.len());
        assert!(all.coverage.values().all(|c| c.deficit() == 0));
        assert!(all.contains(57));
    }
}
