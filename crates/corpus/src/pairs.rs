//! Commit-pair selection (spec §5): deterministic from the pin and a seed.

use std::collections::BTreeSet;

use anyhow::Result;
use cambium_diff::Repo;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PairKind {
    /// `(parent, commit)` for every first-parent commit in the replay range.
    Adjacent,
    /// `(merge-base, merge)` for every merge commit in the range: what CI asks.
    Pr,
    /// Commits touching `.bzl` files, adding or deleting BUILD files, touching
    /// a hub target's package, or touching an invalidator path.
    Stress,
}

impl PairKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "adjacent" => Some(Self::Adjacent),
            "pr" => Some(Self::Pr),
            "stress" => Some(Self::Stress),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Adjacent => "adjacent",
            Self::Pr => "pr",
            Self::Stress => "stress",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pair {
    pub base: String,
    pub head: String,
    pub kind: PairKind,
    /// Stress category: `bzl`, `build_add_delete`, `hub`, `invalidator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

impl Pair {
    pub fn key(&self) -> String {
        crate::state::State::pair_key(&self.base, &self.head)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairSet {
    pub schema: u32,
    pub repo: String,
    pub pin: String,
    pub seed: u64,
    pub pairs: Vec<Pair>,
}

impl PairSet {
    pub fn write(&self, path: &std::path::Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn read(path: &std::path::Path) -> Result<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }
}

#[derive(Clone, Debug)]
pub struct PairOptions {
    pub repo_id: String,
    pub pin: String,
    pub replay_range: usize,
    pub seed: u64,
    /// Stress pairs per category.
    pub cap: usize,
    /// Packages of the hub targets, from `index stats`' top-in-degree list.
    pub hub_packages: Vec<String>,
    /// Invalidator paths and globs from the repo's effective `cambium.toml`.
    pub invalidator_paths: Vec<String>,
    pub invalidator_globs: Vec<String>,
}

/// The first-parent commits of the replay range, oldest first, ending at the
/// pin. `range` commits before the pin means `range + 1` commits.
pub fn range_commits(repo: &Repo, pin: &str, range: usize) -> Result<Vec<String>> {
    Ok(repo.first_parent_log(pin, range + 1)?)
}

pub fn adjacent(repo: &Repo, pin: &str, range: usize) -> Result<Vec<Pair>> {
    let commits = range_commits(repo, pin, range)?;
    Ok(commits
        .windows(2)
        .map(|w| Pair {
            base: w[0].clone(),
            head: w[1].clone(),
            kind: PairKind::Adjacent,
            category: None,
        })
        .collect())
}

pub fn pr_shaped(repo: &Repo, pin: &str, range: usize) -> Result<Vec<Pair>> {
    let commits = range_commits(repo, pin, range)?;
    let mut out = Vec::new();
    for c in &commits {
        let parents = repo.parents(c)?;
        if parents.len() < 2 {
            continue;
        }
        let base = repo.merge_base(&parents[0], &parents[1])?;
        if base.is_empty() || base == *c {
            continue;
        }
        out.push(Pair {
            base,
            head: c.clone(),
            kind: PairKind::Pr,
            category: None,
        });
    }
    Ok(out)
}

/// Turns a `cambium.toml` glob into a git pathspec.
fn glob_pathspec(g: &str) -> String {
    format!(":(glob){g}")
}

/// First-parent commits in the range whose diff touches `pathspecs`, newest
/// first, with `extra` git-log flags (`--diff-filter=AD`, say).
fn commits_touching(
    repo: &Repo,
    oldest: &str,
    pin: &str,
    extra: &[&str],
    pathspecs: &[String],
) -> Result<Vec<String>> {
    if pathspecs.is_empty() {
        return Ok(Vec::new());
    }
    let range = format!("{oldest}..{pin}");
    let mut args: Vec<&str> = vec!["log", "--first-parent", "--format=%H", &range];
    args.extend(extra);
    args.push("--");
    let specs: Vec<&str> = pathspecs.iter().map(String::as_str).collect();
    args.extend(specs);
    let out = repo.git(&args)?;
    Ok(out.lines().map(|s| s.trim().to_string()).collect())
}

/// Picks at most `cap` of `candidates`, deterministically from `seed`, keeping
/// the chosen ones in their original (newest-first) order.
fn sample(candidates: Vec<String>, cap: usize, seed: u64, salt: u64) -> Vec<String> {
    if candidates.len() <= cap {
        return candidates;
    }
    let mut rng = ChaCha8Rng::seed_from_u64(seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut idx: Vec<usize> = (0..candidates.len()).collect();
    idx.shuffle(&mut rng);
    let mut chosen: Vec<usize> = idx.into_iter().take(cap).collect();
    chosen.sort_unstable();
    chosen.into_iter().map(|i| candidates[i].clone()).collect()
}

pub fn stress(repo: &Repo, o: &PairOptions) -> Result<Vec<Pair>> {
    let commits = range_commits(repo, &o.pin, o.replay_range)?;
    let Some(oldest) = commits.first() else {
        return Ok(Vec::new());
    };
    let categories: Vec<(&str, Vec<&str>, Vec<String>)> = vec![
        (
            "bzl",
            vec![],
            vec![glob_pathspec("**/*.bzl"), glob_pathspec("*.bzl")],
        ),
        (
            "build_add_delete",
            vec!["--diff-filter=AD"],
            vec![
                glob_pathspec("**/BUILD"),
                glob_pathspec("**/BUILD.bazel"),
                "BUILD".into(),
                "BUILD.bazel".into(),
            ],
        ),
        (
            "hub",
            vec![],
            o.hub_packages
                .iter()
                .map(|p| {
                    if p.is_empty() {
                        glob_pathspec("*")
                    } else {
                        glob_pathspec(&format!("{p}/*"))
                    }
                })
                .collect(),
        ),
        (
            "invalidator",
            vec![],
            o.invalidator_paths
                .iter()
                .cloned()
                .chain(o.invalidator_globs.iter().map(|g| glob_pathspec(g)))
                .collect(),
        ),
    ];
    let mut out = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (i, (name, extra, specs)) in categories.into_iter().enumerate() {
        let found = commits_touching(repo, oldest, &o.pin, &extra, &specs)?;
        // A commit already chosen under an earlier category is not repeated.
        let fresh: Vec<String> = found.into_iter().filter(|c| !seen.contains(c)).collect();
        for c in sample(fresh, o.cap, o.seed, i as u64 + 1) {
            let parents = repo.parents(&c)?;
            let Some(parent) = parents.first() else {
                continue;
            };
            seen.insert(c.clone());
            out.push(Pair {
                base: parent.clone(),
                head: c,
                kind: PairKind::Stress,
                category: Some(name.to_string()),
            });
        }
    }
    Ok(out)
}

/// Every requested kind, in kind order, as one set.
pub fn select(repo: &Repo, kinds: &[PairKind], o: &PairOptions) -> Result<PairSet> {
    let mut pairs = Vec::new();
    for k in kinds {
        pairs.extend(match k {
            PairKind::Adjacent => adjacent(repo, &o.pin, o.replay_range)?,
            PairKind::Pr => pr_shaped(repo, &o.pin, o.replay_range)?,
            PairKind::Stress => stress(repo, o)?,
        });
    }
    Ok(PairSet {
        schema: cambium_engine::report::SCHEMA_VERSION,
        repo: o.repo_id.clone(),
        pin: o.pin.clone(),
        seed: o.seed,
        pairs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_is_deterministic_and_order_preserving() {
        let c: Vec<String> = (0..50).map(|i| format!("c{i}")).collect();
        let a = sample(c.clone(), 5, 7, 1);
        let b = sample(c.clone(), 5, 7, 1);
        assert_eq!(a, b);
        assert_eq!(a.len(), 5);
        let pos: Vec<usize> = a.iter().map(|s| s[1..].parse().unwrap()).collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{pos:?}");
        assert_ne!(
            sample(c.clone(), 5, 8, 1),
            a,
            "a different seed picks differently"
        );
        assert_eq!(sample(c[..3].to_vec(), 5, 7, 1), c[..3].to_vec());
    }

    #[test]
    fn kinds_parse() {
        assert_eq!(PairKind::parse("pr"), Some(PairKind::Pr));
        assert_eq!(PairKind::parse("nope"), None);
        assert_eq!(PairKind::Stress.name(), "stress");
    }
}
