//! Commit-change set selection (spec §5): deterministic from the pin and a seed.

use std::collections::BTreeSet;

use anyhow::Result;
use cambium_diff::Repo;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSetKind {
    /// `(parent, commit)` for every first-parent commit in the history range.
    Adjacent,
    /// `(merge-base, merge)` for every merge commit in the range: what CI asks.
    Pr,
    /// Commits touching `.bzl` files, adding or deleting BUILD files, touching
    /// a hub target's package, or touching an invalidator path.
    Stress,
}

impl ChangeSetKind {
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
pub struct ChangeSet {
    pub base: String,
    pub head: String,
    pub kind: ChangeSetKind,
    /// Stress category: `bzl`, `build_add_delete`, `hub`, `invalidator`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

impl ChangeSet {
    pub fn key(&self) -> String {
        crate::state::State::change_set_key(&self.base, &self.head)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeSetSet {
    pub schema: u32,
    pub repo: String,
    pub pin: String,
    pub seed: u64,
    /// The history window the selection drew from — the size is part of the
    /// result, not of a mode, so a set chosen from the last 50 commits and one
    /// chosen from the manifest's 300 say so themselves.
    #[serde(default)]
    pub history_window: usize,
    /// The `--change-sets N` this selection was capped at, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<usize>,
    pub change_sets: Vec<ChangeSet>,
}

impl ChangeSetSet {
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
pub struct ChangeSetOptions {
    pub repo_id: String,
    pub pin: String,
    pub history_window: usize,
    pub seed: u64,
    /// Stress change sets per category.
    pub cap: usize,
    /// Bounds the whole selection, across every kind; see [`cap_total`].
    /// `None` selects everything the window offers.
    pub total_cap: Option<usize>,
    /// Packages of the hub targets, from `index stats`' top-in-degree list.
    pub hub_packages: Vec<String>,
    /// Invalidator paths and globs from the repo's effective `cambium.toml`.
    pub invalidator_paths: Vec<String>,
    pub invalidator_globs: Vec<String>,
}

/// The first-parent commits of the history range, oldest first, ending at the
/// pin. `range` commits before the pin means `range + 1` commits.
pub fn range_commits(repo: &Repo, pin: &str, range: usize) -> Result<Vec<String>> {
    Ok(repo.first_parent_log(pin, range + 1)?)
}

pub fn adjacent(repo: &Repo, pin: &str, range: usize) -> Result<Vec<ChangeSet>> {
    let commits = range_commits(repo, pin, range)?;
    Ok(commits
        .windows(2)
        .map(|w| ChangeSet {
            base: w[0].clone(),
            head: w[1].clone(),
            kind: ChangeSetKind::Adjacent,
            category: None,
        })
        .collect())
}

pub fn pr_shaped(repo: &Repo, pin: &str, range: usize) -> Result<Vec<ChangeSet>> {
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
        out.push(ChangeSet {
            base,
            head: c.clone(),
            kind: ChangeSetKind::Pr,
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

pub fn stress(repo: &Repo, o: &ChangeSetOptions) -> Result<Vec<ChangeSet>> {
    let commits = range_commits(repo, &o.pin, o.history_window)?;
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
            out.push(ChangeSet {
                base: parent.clone(),
                head: c,
                kind: ChangeSetKind::Stress,
                category: Some(name.to_string()),
            });
        }
    }
    Ok(out)
}

/// Keeps at most `cap` of `change_sets`, round-robin over the kinds, in the input
/// order.
///
/// Taking a plain prefix would be simpler and wrong: [`select`] emits every
/// adjacent change set before the first `pr` or `stress` one, so a 50-change set prefix of
/// a 300-commit selection is 50 adjacent change sets and no stress at all — exactly
/// the change sets least likely to catch anything. Round-robin keeps each kind's
/// share while leaving what it keeps a *prefix of that kind*, so a quick set
/// is a subset of the full set at the same pin and seed, and the choice is a
/// pure function of the input order.
pub fn cap_total(change_sets: &[ChangeSet], cap: usize) -> Vec<ChangeSet> {
    if change_sets.len() <= cap {
        return change_sets.to_vec();
    }
    let kinds: BTreeSet<ChangeSetKind> = change_sets.iter().map(|p| p.kind).collect();
    let mut taken: BTreeSet<usize> = BTreeSet::new();
    let mut round = 0;
    while taken.len() < cap {
        let before = taken.len();
        for kind in &kinds {
            let Some((i, _)) = change_sets
                .iter()
                .enumerate()
                .filter(|(_, p)| p.kind == *kind)
                .nth(round)
            else {
                continue;
            };
            taken.insert(i);
            if taken.len() == cap {
                break;
            }
        }
        // Every kind is exhausted: there is nothing left to take, whatever the
        // cap says.
        if taken.len() == before {
            break;
        }
        round += 1;
    }
    taken.into_iter().map(|i| change_sets[i].clone()).collect()
}

/// Every requested kind, in kind order, as one set.
pub fn select(repo: &Repo, kinds: &[ChangeSetKind], o: &ChangeSetOptions) -> Result<ChangeSetSet> {
    let range = o.history_window;
    let mut change_sets = Vec::new();
    for k in kinds {
        change_sets.extend(match k {
            ChangeSetKind::Adjacent => adjacent(repo, &o.pin, range)?,
            ChangeSetKind::Pr => pr_shaped(repo, &o.pin, range)?,
            ChangeSetKind::Stress => stress(repo, o)?,
        });
    }
    if let Some(cap) = o.total_cap {
        change_sets = cap_total(&change_sets, cap);
    }
    Ok(ChangeSetSet {
        schema: crate::RESULT_SCHEMA,
        repo: o.repo_id.clone(),
        pin: o.pin.clone(),
        seed: o.seed,
        history_window: range,
        cap: o.total_cap,
        change_sets,
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

    fn change_set(kind: ChangeSetKind, n: usize) -> ChangeSet {
        ChangeSet {
            base: format!("b{n}"),
            head: format!("h{n}"),
            kind,
            category: None,
        }
    }

    /// 100 adjacent, then 20 pr, then 10 stress: [`select`]'s own order.
    fn subject_shaped() -> Vec<ChangeSet> {
        let mut v: Vec<ChangeSet> = (0..100)
            .map(|n| change_set(ChangeSetKind::Adjacent, n))
            .collect();
        v.extend((100..120).map(|n| change_set(ChangeSetKind::Pr, n)));
        v.extend((120..130).map(|n| change_set(ChangeSetKind::Stress, n)));
        v
    }

    #[test]
    fn the_total_cap_is_deterministic_and_keeps_every_kind() {
        let all = subject_shaped();
        let a = cap_total(&all, 50);
        assert_eq!(a, cap_total(&all, 50), "selection must be a pure function");
        assert_eq!(a.len(), 50);
        for kind in [
            ChangeSetKind::Adjacent,
            ChangeSetKind::Pr,
            ChangeSetKind::Stress,
        ] {
            let kept: Vec<&ChangeSet> = a.iter().filter(|p| p.kind == kind).collect();
            assert!(!kept.is_empty(), "{kind:?} was dropped entirely");
            // What it keeps of a kind is that kind's prefix, so the quick set
            // is a subset of the full one at the same pin and seed.
            let full: Vec<&ChangeSet> = all.iter().filter(|p| p.kind == kind).collect();
            assert_eq!(kept, full[..kept.len()].to_vec(), "{kind:?}");
        }
        // And the whole thing is a subset, in the full set's own order.
        assert!(a.iter().all(|p| all.contains(p)));
        let pos: Vec<usize> = a
            .iter()
            .map(|p| all.iter().position(|q| q == p).unwrap())
            .collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{pos:?}");
    }

    #[test]
    fn a_cap_at_or_above_the_count_keeps_everything() {
        let all = subject_shaped();
        assert_eq!(cap_total(&all, all.len()), all);
        assert_eq!(cap_total(&all, all.len() + 5), all);
        // More cap than one kind can supply: the others take up the slack.
        let only = vec![
            change_set(ChangeSetKind::Pr, 0),
            change_set(ChangeSetKind::Pr, 1),
        ];
        assert_eq!(cap_total(&only, 10), only);
    }

    #[test]
    fn kinds_parse() {
        assert_eq!(ChangeSetKind::parse("pr"), Some(ChangeSetKind::Pr));
        assert_eq!(ChangeSetKind::parse("nope"), None);
        assert_eq!(ChangeSetKind::Stress.name(), "stress");
    }
}
