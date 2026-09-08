//! Commit-change set selection (spec §5): deterministic from the pin and a seed.

use std::collections::BTreeSet;

use anyhow::Result;
use cambium_diff::Repo;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

/// The packages of an index's busiest targets, sorted and deduplicated.
///
/// An edit anywhere in one of these reaches the largest share of the graph,
/// which is what the stress change sets are for: `in_degree.top` is the
/// targets most depended on, and a change set that touches their package is
/// the worst case a CI job actually meets.
pub fn hub_packages(index: cambium_index::IndexRef<'_>) -> Vec<String> {
    let stats = cambium_engine::IndexQuery::new(index).stats(0);
    let mut pkgs: Vec<String> = stats
        .in_degree
        .top
        .iter()
        .map(|h| package_of(&h.label))
        .collect();
    pkgs.sort();
    pkgs.dedup();
    pkgs
}

/// `//src/main/java/foo:bar` -> `src/main/java/foo`.
pub fn package_of(label: &str) -> String {
    let l = label.strip_prefix("//").unwrap_or(label);
    match l.split_once(':') {
        Some((pkg, _)) => pkg.to_string(),
        None => l.to_string(),
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSetKind {
    /// `(parent, commit)` for first-parent commits with no PR reference: the
    /// same shape as a squash-merged PR, but without the marker.
    Adjacent,
    /// One pull request's worth of change (METHODOLOGY.md, "Change sets"):
    /// for a merge commit, `merge-base(parents)..commit`; for a first-parent
    /// commit whose message carries a PR reference, `parent..commit`.
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

/// Why a stress change set was chosen. Only [`ChangeSetKind::Stress`] carries
/// one; the other kinds are selected by their *shape*, not by what they touch.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum StressCategory {
    /// The commit touches a `.bzl` file.
    Bzl,
    /// It adds or deletes a `BUILD` file: the two cases the hash-diffing tools
    /// cannot see the whole of, since at one end the target does not exist.
    BuildAddDelete,
    /// It touches a package holding one of the index's highest in-degree
    /// targets, so the reverse closure is as large as this subject gets.
    Hub,
    /// It touches a path the manifest names as an invalidator, which is the
    /// only way to make cambium answer `//...` on purpose.
    Invalidator,
}

impl StressCategory {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bzl => "bzl",
            Self::BuildAddDelete => "build_add_delete",
            Self::Hub => "hub",
            Self::Invalidator => "invalidator",
        }
    }
}

impl std::fmt::Display for StressCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeSet {
    pub base: String,
    pub head: String,
    pub kind: ChangeSetKind,
    /// Why a stress change set was chosen; `None` for every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<StressCategory>,
    /// The pull request number, when a PR-shaped change set's commit message
    /// named one, so the report can link it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
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
        crate::util::read_result(path)
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

/// Finds a pull-request reference in a commit subject line (METHODOLOGY.md,
/// "Change sets"). Four forms: `Merge pull request #1234`, GitHub's default
/// merge-commit subject; `PR #1234` anywhere; `(#1234)` at the end, as
/// GitHub's squash merge writes it; and a trailing
/// `(https://github.com/<owner>/<repo>/pull/1234)`, as bazel's import tooling
/// writes it. Deliberately narrow: an issue reference like `Closes #1234` or
/// a trailing `.../issues/1234` URL is not a PR marker and must not match.
pub fn pr_reference(subject: &str) -> Option<u64> {
    fn leading_digits(s: &str) -> Option<u64> {
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            None
        } else {
            digits.parse().ok()
        }
    }
    fn all_digits(s: &str) -> Option<u64> {
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
            s.parse().ok()
        } else {
            None
        }
    }

    if let Some(rest) = subject.strip_prefix("Merge pull request #") {
        if let Some(n) = leading_digits(rest) {
            return Some(n);
        }
    }
    if let Some(idx) = subject.find("PR #") {
        if let Some(n) = leading_digits(&subject[idx + "PR #".len()..]) {
            return Some(n);
        }
    }
    // The last parenthesised group of the subject: `(#N)` or a PR URL.
    let trimmed = subject.trim_end();
    let inner = trimmed
        .strip_suffix(')')
        .and_then(|s| s.rfind('(').map(|open| &s[open + 1..]));
    if let Some(inner) = inner {
        if let Some(n) = inner.strip_prefix('#').and_then(all_digits) {
            return Some(n);
        }
        if let Some(rest) = inner.strip_prefix("https://github.com/") {
            // `<owner>/<repo>/pull/<N>`: exactly four segments, no more.
            let parts: Vec<&str> = rest.split('/').collect();
            if let [owner, repo, "pull", n] = parts.as_slice() {
                if !owner.is_empty() && !repo.is_empty() {
                    if let Some(n) = all_digits(n) {
                        return Some(n);
                    }
                }
            }
        }
    }
    None
}

/// First-parent commits with a single parent and no PR reference in their
/// message: the same shape as a squash-merged PR, but without the marker
/// that would make them [`pr_shaped`] instead.
pub fn adjacent(repo: &Repo, pin: &str, range: usize) -> Result<Vec<ChangeSet>> {
    let commits = range_commits(repo, pin, range)?;
    let mut out = Vec::new();
    for w in commits.windows(2) {
        if repo.parents(&w[1])?.len() != 1 {
            continue;
        }
        if pr_reference(&repo.subject(&w[1])?).is_some() {
            continue;
        }
        out.push(ChangeSet {
            base: w[0].clone(),
            head: w[1].clone(),
            kind: ChangeSetKind::Adjacent,
            category: None,
            pr_number: None,
        });
    }
    Ok(out)
}

/// One pull request's worth of change, per commit in the history range: a
/// merge commit's `merge-base(parents)..commit`, or a first-parent single-
/// parent commit whose message carries a PR reference, `parent..commit`. A
/// commit is [`adjacent`] or PR-shaped, never both.
pub fn pr_shaped(repo: &Repo, pin: &str, range: usize) -> Result<Vec<ChangeSet>> {
    let commits = range_commits(repo, pin, range)?;
    let mut out = Vec::new();
    for (i, c) in commits.iter().enumerate() {
        let parents = repo.parents(c)?;
        if parents.len() >= 2 {
            let base = repo.merge_base(&parents[0], &parents[1])?;
            if base.is_empty() || base == *c {
                continue;
            }
            let pr_number = pr_reference(&repo.subject(c)?);
            out.push(ChangeSet {
                base,
                head: c.clone(),
                kind: ChangeSetKind::Pr,
                category: None,
                pr_number,
            });
        } else if parents.len() == 1 && i > 0 {
            if let Some(pr_number) = pr_reference(&repo.subject(c)?) {
                out.push(ChangeSet {
                    base: commits[i - 1].clone(),
                    head: c.clone(),
                    kind: ChangeSetKind::Pr,
                    category: None,
                    pr_number: Some(pr_number),
                });
            }
        }
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
    let categories: Vec<(StressCategory, Vec<&str>, Vec<String>)> = vec![
        (
            StressCategory::Bzl,
            vec![],
            vec![glob_pathspec("**/*.bzl"), glob_pathspec("*.bzl")],
        ),
        (
            StressCategory::BuildAddDelete,
            vec!["--diff-filter=AD"],
            vec![
                glob_pathspec("**/BUILD"),
                glob_pathspec("**/BUILD.bazel"),
                "BUILD".into(),
                "BUILD.bazel".into(),
            ],
        ),
        (
            StressCategory::Hub,
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
            StressCategory::Invalidator,
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
                category: Some(name),
                pr_number: None,
            });
        }
    }
    Ok(out)
}

/// Keeps at most `cap` of `change_sets`, PR-shaped first.
///
/// Taking a plain prefix would be simpler and wrong: [`select`] can emit
/// every adjacent change set before the first `pr` or `stress` one, so a
/// 50-change-set prefix of a 300-commit selection could be 50 adjacent change
/// sets and no PR-shaped ones at all — the opposite of the methodology's
/// priority. PR-shaped is the primary kind, so it fills the cap first, up to
/// all of it; stress gets at most a quarter of the cap, since it exists to
/// hit paths a quiet history might not, not to dominate the run; adjacent
/// takes whatever is left. What each kind keeps is a *prefix of that kind's
/// own change sets*, so a quick set is a subset of the full set at the same
/// pin and seed, and the choice is a pure function of `change_sets` and `cap`.
pub fn cap_total(change_sets: &[ChangeSet], cap: usize) -> Vec<ChangeSet> {
    if change_sets.len() <= cap {
        return change_sets.to_vec();
    }
    let indices_of = |kind: ChangeSetKind| -> Vec<usize> {
        change_sets
            .iter()
            .enumerate()
            .filter(|(_, p)| p.kind == kind)
            .map(|(i, _)| i)
            .collect()
    };
    let mut taken: BTreeSet<usize> = BTreeSet::new();

    let pr = indices_of(ChangeSetKind::Pr);
    taken.extend(pr.into_iter().take(cap - taken.len()));

    let stress_cap = (cap - taken.len()).min(cap / 4);
    let stress = indices_of(ChangeSetKind::Stress);
    taken.extend(stress.into_iter().take(stress_cap));

    let adjacent = indices_of(ChangeSetKind::Adjacent);
    taken.extend(adjacent.into_iter().take(cap - taken.len()));

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
    #[test]
    fn labels_reduce_to_their_package() {
        use super::package_of;
        assert_eq!(package_of("//src/main/java/foo:bar"), "src/main/java/foo");
        assert_eq!(package_of("//:root"), "");
        assert_eq!(package_of("//a/b"), "a/b");
        assert_eq!(package_of("@repo//a:b"), "@repo//a");
    }

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
            pr_number: None,
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
    fn pr_reference_matches_squash_and_merge_markers() {
        // GitHub's squash-merge default: the PR number in parens at the end.
        assert_eq!(
            pr_reference("index: account for label costs (#1234)"),
            Some(1234)
        );
        // Trailing whitespace after the marker must not break the match.
        assert_eq!(pr_reference("fix the thing (#42) "), Some(42));
        // A parenthesised suffix that isn't `(#N)` must not match.
        assert_eq!(pr_reference("clean up (WIP)"), None);
        // `PR #N` anywhere in the subject.
        assert_eq!(pr_reference("Backport fix, PR #987, to 1.2"), Some(987));
        // GitHub's default merge-commit subject.
        assert_eq!(
            pr_reference("Merge pull request #55 from user/branch"),
            Some(55)
        );
        // An issue reference is not a PR marker.
        assert_eq!(pr_reference("Closes #1234"), None);
        assert_eq!(pr_reference("fixes issue #1234, no PR here"), None);
        // A bare subject with no marker at all.
        assert_eq!(pr_reference("bump the version"), None);
    }

    #[test]
    fn pr_reference_matches_a_trailing_pull_request_url() {
        // bazel's import tooling writes the full URL in parens at the end.
        assert_eq!(
            pr_reference(
                "Remove the deprecated flag (https://github.com/bazelbuild/bazel/pull/27015)"
            ),
            Some(27015)
        );
        assert_eq!(
            pr_reference("fix (https://github.com/o/r/pull/7)  "),
            Some(7)
        );
        // A trailing issue URL is not a PR marker.
        assert_eq!(
            pr_reference("fix (https://github.com/o/r/issues/1234)"),
            None
        );
        // Nor is a PR URL that isn't the trailing parenthesised group.
        assert_eq!(
            pr_reference("see https://github.com/o/r/pull/1234 for context"),
            None
        );
        // Nor a malformed one.
        assert_eq!(pr_reference("fix (https://github.com/o/r/pull/x)"), None);
        assert_eq!(
            pr_reference("fix (https://github.com/o/r/pull/1/files)"),
            None
        );
    }

    #[test]
    fn cap_fills_pr_shaped_first_then_a_quarter_stress_then_adjacent() {
        let all = subject_shaped(); // 100 adjacent, 20 pr, 10 stress
        let a = cap_total(&all, 50);
        let count = |kind: ChangeSetKind| a.iter().filter(|p| p.kind == kind).count();
        // All 20 pr change sets fit under the cap, so all are kept.
        assert_eq!(count(ChangeSetKind::Pr), 20);
        // Stress is capped at a quarter of 50 = 12, and only 10 exist.
        assert_eq!(count(ChangeSetKind::Stress), 10);
        // Adjacent takes whatever cap remains: 50 - 20 - 10 = 20.
        assert_eq!(count(ChangeSetKind::Adjacent), 20);

        // With more stress than a quarter of the cap, stress is held to it.
        let mut many = all.clone();
        many.extend((130..160).map(|n| change_set(ChangeSetKind::Stress, n)));
        let b = cap_total(&many, 40);
        let count = |kind: ChangeSetKind| b.iter().filter(|p| p.kind == kind).count();
        assert_eq!(count(ChangeSetKind::Pr), 20);
        assert_eq!(count(ChangeSetKind::Stress), 10);
        assert_eq!(count(ChangeSetKind::Adjacent), 10);
    }

    #[test]
    fn cap_smaller_than_pr_count_takes_only_pr_shaped() {
        let all = subject_shaped();
        let a = cap_total(&all, 10);
        assert_eq!(a.len(), 10);
        assert!(a.iter().all(|p| p.kind == ChangeSetKind::Pr));
        // And it is that kind's own prefix.
        let full: Vec<&ChangeSet> = all.iter().filter(|p| p.kind == ChangeSetKind::Pr).collect();
        assert_eq!(a.iter().collect::<Vec<_>>(), full[..10].to_vec());
    }

    #[test]
    fn kinds_parse() {
        assert_eq!(ChangeSetKind::parse("pr"), Some(ChangeSetKind::Pr));
        assert_eq!(ChangeSetKind::parse("nope"), None);
        assert_eq!(ChangeSetKind::Stress.name(), "stress");
    }
}
