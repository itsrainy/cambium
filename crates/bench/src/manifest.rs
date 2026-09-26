//! `subject_repos.toml`: the real subjects, and the commit each is pinned at.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// The manifest file name at the cambium repository root.
pub const MANIFEST_FILE: &str = "subject_repos.toml";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default, rename = "repo")]
    pub repos: Vec<RepoEntry>,
}

/// One real subject: an open-source Bazel repository, pinned.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    pub id: String,
    pub url: String,
    /// Baseline commit. Bumped deliberately, in a PR that regenerates the
    /// repo's `docs/RESULTS.md` section.
    pub pin: String,
    /// The history window: how many first-parent commits before `pin` a
    /// workload may use.
    #[serde(default = "default_history_window")]
    pub history_window: usize,
    /// Bazel binary; `bazelisk` honours `.bazelversion`.
    #[serde(default = "default_bazel")]
    pub bazel: String,
    /// `USE_BAZEL_VERSION` for repositories that need one and do not say so
    /// themselves: no `.bazelversion`, so bazelisk would pick the latest
    /// release, which for an unmigrated WORKSPACE repo means nothing loads.
    /// `--bazel-version` on the command line overrides it. Like that flag, it
    /// suffixes the result file names and the output base, so a run under a
    /// pinned Bazel cannot be confused with, or served from, another one's.
    ///
    /// It is not a version *matrix*: every entry runs under one Bazel, its
    /// own. That cambium reads 7.x, 8.x and 9.x graphs alike is CI's
    /// `bazel-version-matrix` job over the synthetic subjects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bazel_version: Option<String>,
    #[serde(default)]
    pub extra_query_flags: Vec<String>,
    /// Extra Bazel *startup* options, before the command word, on top of the
    /// `--output_base` and `--bazelrc` the suite always passes. The reason
    /// this exists is `--noworkspace_rc`: a repository's own `.bazelrc` is
    /// written for the Bazel of its day, and running the history 300 commits walks back
    /// into flags a pinned Bazel no longer recognises, which fails the query
    /// outright rather than degrading. Use it only where the repo's rc is the
    /// obstacle, and note what it costs: `--deleted_packages` and friends
    /// live in those files and do change the graph.
    #[serde(default)]
    pub extra_startup_flags: Vec<String>,
    /// Repo-specific additions to the generated `cambium.toml`'s invalidators.
    /// Entries with glob characters become globs, the rest exact paths.
    #[serde(default)]
    pub invalidators: Vec<String>,
    /// Which packages this subject is expected to fail to load, so that the
    /// pin's index may be published despite `bazel query //...` finishing with
    /// errors. The engine refuses a partial index by default, since a run
    /// spliced onto one silently under-reports for the missing packages; a
    /// subject names the failures that are a known, constant absence on the
    /// benchmark host (apollo's `@local_config_cuda`) rather than a broken
    /// checkout, and any other failure still stops the publish. The report
    /// still says the index is partial.
    ///
    /// `true` accepts every failure and is what let an xla index short
    /// seventeen packages reach the store on 2026-09-18; prefer the list.
    #[serde(default)]
    pub publish_partial: cambium_engine::config::PartialPolicy,
    #[serde(default)]
    pub skip: bool,
    #[serde(default)]
    pub notes: String,
}

fn default_history_window() -> usize {
    300
}

fn default_bazel() -> String {
    "bazelisk".to_string()
}

impl Manifest {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let m: Manifest =
            toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
        m.validate()?;
        Ok(m)
    }

    /// The manifest next to `start`, or in any parent directory.
    pub fn discover(start: &Path) -> anyhow::Result<(PathBuf, Self)> {
        let mut dir = start.to_path_buf();
        loop {
            let candidate = dir.join(MANIFEST_FILE);
            if candidate.is_file() {
                return Ok((candidate.clone(), Self::load(&candidate)?));
            }
            if !dir.pop() {
                anyhow::bail!(
                    "no {MANIFEST_FILE} found above {}; pass --manifest",
                    start.display()
                );
            }
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for r in &self.repos {
            anyhow::ensure!(!r.id.is_empty(), "a subject entry has no id");
            anyhow::ensure!(
                r.id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "subject id {:?} must be [A-Za-z0-9_-]",
                r.id
            );
            anyhow::ensure!(seen.insert(r.id.clone()), "duplicate subject id {}", r.id);
            anyhow::ensure!(!r.url.is_empty(), "subject {} has no url", r.id);
            anyhow::ensure!(
                r.pin.len() == 40 && r.pin.chars().all(|c| c.is_ascii_hexdigit()),
                "subject {}: pin must be a full 40-hex commit sha, got {:?}",
                r.id,
                r.pin
            );
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> anyhow::Result<&RepoEntry> {
        self.repos.iter().find(|r| r.id == id).ok_or_else(|| {
            anyhow::anyhow!(
                "no subject named {id}; known: {}",
                self.repos
                    .iter()
                    .map(|r| r.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }
}

impl RepoEntry {
    /// Splits the manifest's invalidator entries into the exact paths and the
    /// globs of a `cambium.toml`.
    pub fn invalidator_rules(&self) -> (Vec<String>, Vec<String>) {
        let mut paths = Vec::new();
        let mut globs = Vec::new();
        for i in &self.invalidators {
            if i.contains(['*', '?', '[', '{']) {
                globs.push(i.clone());
            } else {
                paths.push(i.clone());
            }
        }
        (paths, globs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The io or toml error stays the source, and the rendered chain reads
    /// the way the flattened message did.
    #[test]
    fn a_manifest_that_cannot_be_read_or_parsed_says_which_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subject_repos.toml");
        let err = Manifest::load(&path).unwrap_err();
        assert!(err.downcast_ref::<std::io::Error>().is_some(), "{err:?}");
        assert!(
            format!("{err:#}").starts_with(&format!("reading {}: ", path.display())),
            "{err:#}"
        );
        std::fs::write(&path, "[[repo]\n").unwrap();
        let err = Manifest::load(&path).unwrap_err();
        assert!(
            format!("{err:#}").starts_with(&format!("parsing {}: ", path.display())),
            "{err:#}"
        );
    }

    #[test]
    fn parses_a_full_entry() {
        let m: Manifest = toml::from_str(
            r#"
[[repo]]
id = "envoy"
url = "https://github.com/envoyproxy/envoy.git"
pin = "7ba51266cc60cd560c37ea9cb3e2adf70676b6cf"
history_window = 300
bazel = "bazelisk"
extra_query_flags = []
invalidators = ["bazel/repositories.bzl", "api/bazel/**"]
skip = false
notes = "external fetch ~6 GB; needs --repository_cache"
"#,
        )
        .unwrap();
        m.validate().unwrap();
        let e = m.get("envoy").unwrap();
        assert_eq!(e.history_window, 300);
        assert_eq!(
            e.invalidator_rules(),
            (
                vec!["bazel/repositories.bzl".to_string()],
                vec!["api/bazel/**".to_string()]
            )
        );
        assert!(m.get("nope").is_err());
    }

    #[test]
    fn rejects_short_pins_and_duplicates() {
        let short: Manifest =
            toml::from_str("[[repo]]\nid = \"a\"\nurl = \"u\"\npin = \"abc\"\n").unwrap();
        assert!(short.validate().is_err());
        let dup: Manifest = toml::from_str(&format!(
            "[[repo]]\nid = \"a\"\nurl = \"u\"\npin = \"{0}\"\n[[repo]]\nid = \"a\"\nurl = \"u\"\npin = \"{0}\"\n",
            "0".repeat(40)
        ))
        .unwrap();
        assert!(dup.validate().is_err());
    }

    #[test]
    fn the_checked_in_manifest_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../subject_repos.toml");
        let m = Manifest::load(&path).unwrap();
        assert!(m.repos.iter().any(|r| r.id == "bazel"));
        for r in &m.repos {
            assert!(!r.url.is_empty(), "{}", r.id);
        }
        // One entry per repository, each under its own Bazel. Major coverage
        // is CI's synthetic version matrix, not a second pin of the same repo.
        let urls: std::collections::BTreeSet<&str> =
            m.repos.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(urls.len(), m.repos.len(), "a repository is listed twice");
    }
}
