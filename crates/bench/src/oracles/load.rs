//! The `.bzl` load-closure oracle.
//!
//! cambium works out which packages a changed `.bzl` reaches by scanning
//! `load()` statements at index time (`cambium_index::loads`). That is a
//! reimplementation of something Bazel already knows, so on every change set whose
//! change set contains a `.bzl` the suite asks Bazel the same question —
//! `rbuildfiles(<paths>)`, the call the engine used to make — and requires
//! cambium's dirty set to be a **superset** of the packages Bazel names.
//!
//! One-sided on purpose, for the same reason the `rdeps` oracle is: extra
//! packages on cambium's side are over-approximation, which costs a query;
//! a package Bazel names that cambium did not dirty is a missed rule
//! implementation change, which is the bug class the design does not tolerate.
//!
//! # When `rbuildfiles` is not answering the same question
//!
//! `rbuildfiles` is not a load-closure query. It is a reverse walk over
//! Skyframe's *file* nodes, and under bzlmod every package's evaluation
//! depends on the main repository's repository mapping, which depends on the
//! module extensions, which depend on the `.bzl` files that declare
//! repositories. So a change to a workspace-closure `.bzl` — envoy's
//! `bazel/repository_locations.bzl`, `bazel/repositories_extra.bzl`,
//! `api/bazel/repositories.bzl` — makes `rbuildfiles` name **every package in
//! the repository**, whether or not anything loads the file.
//!
//! The second published run is the evidence. On all five envoy change sets
//! that touched such a file, Bazel named 1211 packages and the packages
//! cambium had not dirtied were exactly the complement of its dirty set —
//! 1099 + 112, 1102 + 109, 1104 + 110, 1101 + 111, 1099 + 112, all summing to
//! the same 1211 no matter which `.bzl` moved. Among them is
//! `bazel/external/c-ares/include/config_linux`, whose whole `BUILD` file is
//! `exports_files(["ares_config.h"])`: no `load()` at all, so no load closure
//! can reach it and no static closure could have been expected to.
//!
//! Requiring containment there would be requiring cambium to answer "the whole
//! repository" for every repository-declaring `.bzl` edit, which is the answer
//! the external-repository stage exists to avoid, and which the `rdeps` oracle
//! — the check on the *affected set*, the thing that has to be right — already
//! covers. So the oracle looks for that signature before it fails: a package
//! Bazel named, that cambium did not dirty, whose `BUILD` file contains no
//! `load()` statement at all. One such witness proves the answer came through
//! repository evaluation rather than through load edges, and the whole answer
//! is recorded ([`LoadOracleResult::repository_evaluation`]) rather than
//! failed. Without a witness — every named package does load something — the
//! containment check stands exactly as before.
//!
//! Exit 3 (Sky Query finished with errors under `--keep_going`) is accepted as
//! a partial answer and recorded, exactly as the `rdeps` oracle accepts a
//! partial batch: a shorter Bazel answer can only weaken the containment
//! check, never invert it. That is also the failure mode this whole change
//! exists to remove — on the real subjects, `rbuildfiles` exited 3 on 5 of 150
//! `bazel/bazel` commits, and the engine had to answer `//...` each time.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context as _, Result};
use cambium_bazel::Bazel;
use serde::{Deserialize, Serialize};

/// What Bazel said about the `.bzl` files in one change set's change set.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadOracleResult {
    /// The changed `.bzl` paths the query asked about.
    pub checked: Vec<String>,
    /// Sky Query finished with errors, so Bazel's answer is a lower bound.
    #[serde(default)]
    pub partial: bool,
    /// Main-repository packages `rbuildfiles` named.
    pub bazel_packages: BTreeSet<String>,
    /// Labels naming a loader outside any main-repo package — a module
    /// extension's `.bzl`, a `WORKSPACE` load. cambium answers those with a
    /// global invalidation, and a global change set never reaches this oracle, so
    /// they are recorded rather than compared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_level: Vec<String>,
    /// `bazel_packages - cambium's dirty set`: unsoundness. Empty means pass.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub missing_packages: BTreeSet<String>,
    /// Packages in [`LoadOracleResult::missing_packages`] whose `BUILD` file
    /// has no `load()` in it at all, so no load closure could have reached
    /// them: proof that this `rbuildfiles` answer came through Bazel's
    /// repository evaluation and is not a load closure. Non-empty means the
    /// containment check does not apply; see the module docs.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub repository_evaluation: BTreeSet<String>,
}

/// The class a non-empty [`LoadOracleResult::missing_packages`] is counted
/// under, and reported in the class table.
pub const MISSING_CLASS: &str = "cambium_missing_bzl_closure";

impl LoadOracleResult {
    pub fn ok(&self) -> bool {
        self.missing_packages.is_empty() || !self.repository_evaluation.is_empty()
    }
}

/// Whether `package`'s `BUILD` file, in the head worktree, contains no
/// `load()` at all.
///
/// The witness for "this answer is not a load closure". Read from the tree and
/// not from the index on purpose: the index's own load data is the thing under
/// test, so excusing it with its own answer would be circular. A package whose
/// `BUILD` file cannot be read is not a witness — the safe direction, since a
/// missing witness only keeps the containment check on.
pub fn build_file_loads_nothing(workspace: &Path, package: &str) -> bool {
    let dir = workspace.join(package);
    let Some(text) = ["BUILD.bazel", "BUILD"]
        .iter()
        .find_map(|n| std::fs::read_to_string(dir.join(n)).ok())
    else {
        return false;
    };
    !text.lines().any(|l| l.trim_start().starts_with("load("))
}

/// The `.bzl` paths in a change set, in the form `rbuildfiles` wants
/// (workspace-relative).
pub fn changed_bzl<'a>(changed: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut v: Vec<String> = changed
        .into_iter()
        .filter(|p| cambium_classify::is_bzl_file(p))
        .cloned()
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Runs the oracle for one change set. `None` when the change set has no `.bzl` in
/// it, which is most change sets.
///
/// `bazel` must be the handle for the **head** workspace: `rbuildfiles` is a
/// question about the tree the changed file now lives in.
pub fn run<'a>(
    bazel: &Bazel,
    changed: impl IntoIterator<Item = &'a String>,
    cambium_dirty: &BTreeSet<String>,
) -> Result<Option<LoadOracleResult>> {
    let paths = changed_bzl(changed);
    if paths.is_empty() {
        return Ok(None);
    }
    let (r, partial) = bazel
        .rbuildfiles(&paths)
        .with_context(|| format!("rbuildfiles oracle over {} .bzl file(s)", paths.len()))?;
    let bazel_packages: BTreeSet<String> = r.packages.into_iter().collect();
    let missing_packages: BTreeSet<String> =
        bazel_packages.difference(cambium_dirty).cloned().collect();
    let repository_evaluation: BTreeSet<String> = missing_packages
        .iter()
        .filter(|p| build_file_loads_nothing(bazel.workspace(), p))
        .cloned()
        .collect();
    Ok(Some(LoadOracleResult {
        checked: paths,
        partial,
        bazel_packages,
        workspace_level: r.workspace_level,
        missing_packages,
        repository_evaluation,
    }))
}

/// Roll-up over a run's change sets, for the summary document and the report table.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadOracleSummary {
    /// ChangeSets whose change set contained a `.bzl`.
    pub change_sets: usize,
    /// `.bzl` files asked about, over those change sets.
    pub checked: usize,
    /// ChangeSets where Bazel's answer was partial (exit 3).
    pub partial: usize,
    /// Packages Bazel named that cambium did not dirty, on the change sets
    /// where the answer was a load closure. Must be zero.
    pub missing_packages: usize,
    /// ChangeSets whose `rbuildfiles` answer named a package that loads
    /// nothing, so it came through repository evaluation and the containment
    /// check did not apply. Reported, not failed.
    #[serde(default)]
    pub repository_evaluation: usize,
}

impl LoadOracleSummary {
    pub fn add(&mut self, r: &LoadOracleResult) {
        self.change_sets += 1;
        self.checked += r.checked.len();
        self.partial += usize::from(r.partial);
        if r.repository_evaluation.is_empty() {
            self.missing_packages += r.missing_packages.len();
        } else {
            self.repository_evaluation += 1;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.change_sets == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_bzl_paths_are_asked_about() {
        let changed = vec![
            "a/BUILD".to_string(),
            "bazel/defs.bzl".into(),
            "src/x.cc".into(),
            "bazel/defs.bzl".into(),
        ];
        assert_eq!(changed_bzl(&changed), vec!["bazel/defs.bzl"]);
        assert!(changed_bzl(&["a/BUILD".to_string()]).is_empty());
    }

    #[test]
    fn a_superset_passes_and_a_gap_fails() {
        let mut r = LoadOracleResult {
            checked: vec!["bazel/defs.bzl".into()],
            bazel_packages: set(&["app", "lib"]),
            ..Default::default()
        };
        r.missing_packages = r
            .bazel_packages
            .difference(&set(&["app", "lib", "extra"]))
            .cloned()
            .collect();
        assert!(r.ok(), "over-approximation is not a failure");

        r.missing_packages = r
            .bazel_packages
            .difference(&set(&["app"]))
            .cloned()
            .collect();
        assert!(!r.ok());
        assert_eq!(r.missing_packages, set(&["lib"]));
    }

    /// envoy's `bazel/external/c-ares/include/config_linux/BUILD`, verbatim,
    /// against one that does load something.
    #[test]
    fn a_build_file_with_no_load_is_the_witness() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let write = |rel: &str, text: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        write(
            "bazel/external/c-ares/include/config_linux/BUILD",
            "exports_files([\"ares_config.h\"])\n",
        );
        write(
            "bazel/external/BUILD",
            "load(\"@rules_cc//cc:cc_library.bzl\", \"cc_library\")\n\ncc_library(name = \"x\")\n",
        );
        write("tools/BUILD.bazel", "  load(\"//x:y.bzl\", \"z\")\n");
        assert!(build_file_loads_nothing(
            root,
            "bazel/external/c-ares/include/config_linux"
        ));
        assert!(!build_file_loads_nothing(root, "bazel/external"));
        assert!(
            !build_file_loads_nothing(root, "tools"),
            "BUILD.bazel counts, and an indented load() is still a load()"
        );
        assert!(
            !build_file_loads_nothing(root, "no/such/package"),
            "a BUILD file that cannot be read is not a witness"
        );
    }

    #[test]
    fn a_gap_that_names_a_package_loading_nothing_is_not_a_load_closure() {
        // The five envoy change sets: Bazel names the whole repository because
        // a repository-declaring `.bzl` moved, and the packages cambium left
        // out include ones with no `load()` at all.
        let r = LoadOracleResult {
            checked: vec!["bazel/repository_locations.bzl".into()],
            bazel_packages: set(&[
                "bazel/external/c-ares/include/config_linux",
                "source/common",
            ]),
            missing_packages: set(&["bazel/external/c-ares/include/config_linux"]),
            repository_evaluation: set(&["bazel/external/c-ares/include/config_linux"]),
            ..Default::default()
        };
        assert!(
            r.ok(),
            "a package whose BUILD file loads nothing cannot be reached by a load closure, \
             so this answer is not one"
        );

        // Same gap, no witness: every package Bazel named does load something,
        // so the containment check stands.
        let r = LoadOracleResult {
            repository_evaluation: BTreeSet::new(),
            ..r
        };
        assert!(!r.ok());
    }

    #[test]
    fn the_summary_counts_change_sets_files_and_gaps() {
        let mut s = LoadOracleSummary::default();
        assert!(s.is_empty());
        s.add(&LoadOracleResult {
            checked: vec!["a.bzl".into(), "b.bzl".into()],
            partial: true,
            ..Default::default()
        });
        s.add(&LoadOracleResult {
            checked: vec!["c.bzl".into()],
            missing_packages: set(&["lib"]),
            ..Default::default()
        });
        // A repository-evaluation answer is counted apart: its gap is not a
        // gap in the load closure, so it must not reach `missing_packages`.
        s.add(&LoadOracleResult {
            checked: vec!["bazel/repository_locations.bzl".into()],
            missing_packages: set(&["p1", "p2"]),
            repository_evaluation: set(&["p1"]),
            ..Default::default()
        });
        assert_eq!(s.repository_evaluation, 1);
        assert_eq!(s.change_sets, 3);
        assert_eq!(s.checked, 4);
        assert_eq!(s.partial, 1);
        assert_eq!(s.missing_packages, 1);
    }
}
