//! The steps of a walk and what each one is: `(parent, head)` pairs along
//! the first-parent line to the pin, each tagged by its shape and by what it
//! touches.
//!
//! One workload, one selection rule: every first-parent commit in the window
//! is a step, and the tags say which steps are the interesting ones. A merge
//! step is the PR-shaped pair the old `pr` kind selected; a step touching a
//! `.bzl`, adding or deleting a BUILD file, touching a hub package or an
//! invalidator path is what the old `stress` categories picked out. Nothing
//! is selected *by* its tag any more -- the walk carries the index through
//! every step -- but the sampler ([`super::sampling`]) stratifies on them,
//! and the report's coverage table is per tag.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use cambium_diff::Repo;
use serde::{Deserialize, Serialize};

/// What a step is: its shape (exactly one of `merge`, `adjacent`) and what
/// it touches (any number of the rest).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Tag {
    /// The commit touches a `.bzl` file: the static load closure is under
    /// test.
    Bzl,
    /// It adds or deletes a `BUILD` file: the two cases a hash-diffing tool
    /// cannot see the whole of, since at one end the target does not exist.
    BuildAddDelete,
    /// It touches a package holding one of the index's highest in-degree
    /// targets, so the reverse closure is as large as this subject gets.
    Hub,
    /// It touches a path the configuration names as an invalidator, the one
    /// way to make cambium answer `//...` on purpose.
    Invalidator,
    /// A merge commit: one pull request's worth of change, the shape a CI job
    /// asks about.
    Merge,
    /// A single-parent commit: the same shape as a squash-merged PR.
    Adjacent,
}

impl Tag {
    pub const ALL: [Tag; 6] = [
        Tag::Bzl,
        Tag::BuildAddDelete,
        Tag::Hub,
        Tag::Invalidator,
        Tag::Merge,
        Tag::Adjacent,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Bzl => "bzl",
            Self::BuildAddDelete => "build_add_delete",
            Self::Hub => "hub",
            Self::Invalidator => "invalidator",
            Self::Merge => "merge",
            Self::Adjacent => "adjacent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name() == s)
    }

    /// The tags that say what a step touches, as opposed to its shape.
    pub fn is_content(self) -> bool {
        !matches!(self, Self::Merge | Self::Adjacent)
    }
}

impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One step of the walk: the index at `parent` is brought forward to `head`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Step {
    pub parent: String,
    pub head: String,
    /// Sorted, deduplicated; exactly one of `merge` and `adjacent`.
    pub tags: Vec<Tag>,
}

impl Step {
    pub fn has(&self, tag: Tag) -> bool {
        self.tags.contains(&tag)
    }

    pub fn key(&self) -> String {
        crate::state::State::change_set_key(&self.parent, &self.head)
    }
}

/// The first-parent commits of the history range, oldest first, ending at the
/// pin. `range` commits before the pin means `range + 1` commits.
pub fn range_commits(repo: &Repo, pin: &str, range: usize) -> Result<Vec<String>> {
    Ok(repo.first_parent_log(pin, range + 1)?)
}

/// `//src/main/java/foo:bar` -> `src/main/java/foo`.
pub fn package_of(label: &str) -> String {
    let l = label.strip_prefix("//").unwrap_or(label);
    match l.split_once(':') {
        Some((pkg, _)) => pkg.to_string(),
        None => l.to_string(),
    }
}

/// The packages of an index's busiest targets, sorted and deduplicated.
///
/// An edit anywhere in one of these reaches the largest share of the graph:
/// `in_degree.top` is the targets most depended on, and a step that touches
/// their package is the worst case a CI job actually meets.
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

/// What the tagger probes the history with: the subject's hub packages and
/// its invalidator rules. The `.bzl` and BUILD probes need nothing from the
/// subject.
#[derive(Clone, Debug, Default)]
pub struct Probes {
    /// Packages of the hub targets, from `index stats`' top-in-degree list.
    pub hub_packages: Vec<String>,
    /// Invalidator paths and globs from the subject's effective `cambium.toml`.
    pub invalidator_paths: Vec<String>,
    pub invalidator_globs: Vec<String>,
}

/// Turns a `cambium.toml` glob into a git pathspec.
fn glob_pathspec(g: &str) -> String {
    format!(":(glob){g}")
}

/// First-parent commits in `oldest..pin` whose diff touches `pathspecs`,
/// with `extra` git-log flags (`--diff-filter=AD`, say).
fn commits_touching(
    repo: &Repo,
    oldest: &str,
    pin: &str,
    extra: &[&str],
    pathspecs: &[String],
) -> Result<BTreeSet<String>> {
    if pathspecs.is_empty() {
        return Ok(BTreeSet::new());
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

/// The content probes, each as the git-log flags and pathspecs that find
/// the commits it names.
fn content_probes(p: &Probes) -> Vec<(Tag, Vec<&'static str>, Vec<String>)> {
    vec![
        (
            Tag::Bzl,
            vec![],
            vec![glob_pathspec("**/*.bzl"), glob_pathspec("*.bzl")],
        ),
        (
            Tag::BuildAddDelete,
            vec!["--diff-filter=AD"],
            vec![
                glob_pathspec("**/BUILD"),
                glob_pathspec("**/BUILD.bazel"),
                "BUILD".into(),
                "BUILD.bazel".into(),
            ],
        ),
        (
            Tag::Hub,
            vec![],
            p.hub_packages
                .iter()
                .map(|pkg| {
                    if pkg.is_empty() {
                        glob_pathspec("*")
                    } else {
                        glob_pathspec(&format!("{pkg}/*"))
                    }
                })
                .collect(),
        ),
        (
            Tag::Invalidator,
            vec![],
            p.invalidator_paths
                .iter()
                .cloned()
                .chain(p.invalidator_globs.iter().map(|g| glob_pathspec(g)))
                .collect(),
        ),
    ]
}

/// Every step of the walk over `commits` (oldest first, the pin last),
/// tagged. `commits[0]` is where the cold build happens and is no step.
pub fn tag_steps(repo: &Repo, commits: &[String], probes: &Probes) -> Result<Vec<Step>> {
    let Some(oldest) = commits.first() else {
        return Ok(Vec::new());
    };
    let pin = commits.last().expect("a non-empty range has a last commit");
    let mut touching: BTreeMap<Tag, BTreeSet<String>> = BTreeMap::new();
    for (tag, extra, specs) in content_probes(probes) {
        touching.insert(tag, commits_touching(repo, oldest, pin, &extra, &specs)?);
    }
    let mut out = Vec::with_capacity(commits.len().saturating_sub(1));
    for w in commits.windows(2) {
        let (parent, head) = (&w[0], &w[1]);
        let mut tags: Vec<Tag> = touching
            .iter()
            .filter(|(_, set)| set.contains(head))
            .map(|(tag, _)| *tag)
            .collect();
        tags.push(if repo.parents(head)?.len() >= 2 {
            Tag::Merge
        } else {
            Tag::Adjacent
        });
        tags.sort();
        tags.dedup();
        out.push(Step {
            parent: parent.clone(),
            head: head.clone(),
            tags,
        });
    }
    Ok(out)
}

/// Every step of the walk to `pin` over the last `range` first-parent
/// commits, tagged. The commits themselves come back too: the first is
/// where the cold build happens.
pub fn steps(
    repo: &Repo,
    pin: &str,
    range: usize,
    probes: &Probes,
) -> Result<(Vec<String>, Vec<Step>)> {
    let commits = range_commits(repo, pin, range)?;
    let steps = tag_steps(repo, &commits, probes)?;
    Ok((commits, steps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_reduce_to_their_package() {
        assert_eq!(package_of("//src/main/java/foo:bar"), "src/main/java/foo");
        assert_eq!(package_of("//:root"), "");
        assert_eq!(package_of("//a/b"), "a/b");
        assert_eq!(package_of("@repo//a:b"), "@repo//a");
    }

    #[test]
    fn tags_round_trip_by_name() {
        for t in Tag::ALL {
            assert_eq!(Tag::parse(t.name()), Some(t));
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(json, format!("\"{}\"", t.name()));
        }
        assert_eq!(Tag::parse("nope"), None);
        assert!(Tag::Bzl.is_content() && !Tag::Merge.is_content());
    }
}
