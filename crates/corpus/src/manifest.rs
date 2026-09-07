//! `corpus.toml`: which repositories, at which commits.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The manifest file name at the cambium repository root.
pub const MANIFEST_FILE: &str = "corpus.toml";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default, rename = "repo")]
    pub repos: Vec<RepoEntry>,
}

/// One corpus repository, pinned.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    pub id: String,
    pub url: String,
    /// Share another entry's bare mirror instead of cloning `url` again.
    /// The named entry must have the same `url` and no `mirror` of its own;
    /// this entry still gets its own worktree, store and output base. Used by
    /// the version-matrix entries, which are the same repository at an older
    /// pin and would otherwise cost a second full clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
    /// Baseline commit. Bumped deliberately, in a PR that regenerates the
    /// repo's `docs/CORPUS.md` section.
    pub pin: String,
    /// First-parent commits before `pin` to replay.
    #[serde(default = "default_replay_range")]
    pub replay_range: usize,
    /// Bazel binary; `bazelisk` honours `.bazelversion`.
    #[serde(default = "default_bazel")]
    pub bazel: String,
    /// `USE_BAZEL_VERSION` for repositories that need one and do not say so
    /// themselves: no `.bazelversion`, so bazelisk would pick the latest
    /// release, which for an unmigrated WORKSPACE repo means nothing loads.
    /// `--bazel-version` on the command line overrides it. Like that flag, it
    /// suffixes the result file names, so the numbers say which Bazel produced
    /// them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bazel_version: Option<String>,
    #[serde(default)]
    pub extra_query_flags: Vec<String>,
    /// Extra Bazel *startup* options, before the command word, on top of the
    /// `--output_base` and `--bazelrc` the suite always passes. The reason
    /// this exists is `--noworkspace_rc`: a repository's own `.bazelrc` is
    /// written for the Bazel of its day, and replaying 300 commits walks back
    /// into flags a pinned Bazel no longer recognises, which fails the query
    /// outright rather than degrading. Use it only where the repo's rc is the
    /// obstacle, and note what it costs — `--deleted_packages` and friends
    /// live in those files and do change the graph.
    #[serde(default)]
    pub extra_startup_flags: Vec<String>,
    /// Repo-specific additions to the generated `cambium.toml`'s invalidators.
    /// Entries with glob characters become globs, the rest exact paths.
    #[serde(default)]
    pub invalidators: Vec<String>,
    /// Informational only: which Bazel majors this repository is exercised
    /// under. No code reads it. The version matrix is expressed as separate
    /// manifest entries (`bazel-7`, `bazel-8`, ...) pinned to commits whose
    /// own `.bazelversion` is the major under test, because a HEAD pin does
    /// not load under an older major at all.
    #[serde(default)]
    pub bazel_versions: Vec<String>,
    #[serde(default)]
    pub skip: bool,
    #[serde(default)]
    pub notes: String,
}

fn default_replay_range() -> usize {
    300
}

fn default_bazel() -> String {
    "bazelisk".to_string()
}

impl Manifest {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let m: Manifest =
            toml::from_str(&s).map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
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
            anyhow::ensure!(!r.id.is_empty(), "a corpus entry has no id");
            anyhow::ensure!(
                r.id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "corpus id {:?} must be [A-Za-z0-9_-]",
                r.id
            );
            anyhow::ensure!(seen.insert(r.id.clone()), "duplicate corpus id {}", r.id);
            anyhow::ensure!(!r.url.is_empty(), "corpus {} has no url", r.id);
            anyhow::ensure!(
                r.pin.len() == 40 && r.pin.chars().all(|c| c.is_ascii_hexdigit()),
                "corpus {}: pin must be a full 40-hex commit sha, got {:?}",
                r.id,
                r.pin
            );
        }
        for r in &self.repos {
            let Some(parent_id) = r.mirror.as_deref() else {
                continue;
            };
            let parent = self
                .repos
                .iter()
                .find(|p| p.id == parent_id)
                .ok_or_else(|| {
                    anyhow::anyhow!("corpus {}: mirror = {parent_id:?} names no entry", r.id)
                })?;
            anyhow::ensure!(
                parent.id != r.id,
                "corpus {}: mirror names the entry itself",
                r.id
            );
            anyhow::ensure!(
                parent.mirror.is_none(),
                "corpus {}: mirror = {parent_id:?}, which itself borrows a mirror",
                r.id
            );
            anyhow::ensure!(
                parent.url == r.url,
                "corpus {}: mirror = {parent_id:?} but their urls differ ({} vs {})",
                r.id,
                r.url,
                parent.url
            );
        }
        Ok(())
    }

    /// The id of the bare mirror `entry` uses: its own, or the one it borrows.
    pub fn mirror_id(entry: &RepoEntry) -> &str {
        entry.mirror.as_deref().unwrap_or(&entry.id)
    }

    pub fn get(&self, id: &str) -> anyhow::Result<&RepoEntry> {
        self.repos.iter().find(|r| r.id == id).ok_or_else(|| {
            anyhow::anyhow!(
                "no corpus repo named {id}; known: {}",
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

    #[test]
    fn parses_the_spec_example() {
        let m: Manifest = toml::from_str(
            r#"
[[repo]]
id = "envoy"
url = "https://github.com/envoyproxy/envoy.git"
pin = "7ba51266cc60cd560c37ea9cb3e2adf70676b6cf"
replay_range = 300
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
        assert_eq!(e.replay_range, 300);
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
    fn a_shared_mirror_must_name_a_same_url_root_entry() {
        let base = |extra: &str| {
            format!(
                "[[repo]]\nid = \"a\"\nurl = \"u\"\npin = \"{p}\"\n{extra}",
                p = "0".repeat(40)
            )
        };
        let ok: Manifest = toml::from_str(&base(&format!(
            "[[repo]]\nid = \"a-7\"\nurl = \"u\"\nmirror = \"a\"\npin = \"{}\"\n",
            "1".repeat(40)
        )))
        .unwrap();
        ok.validate().unwrap();
        assert_eq!(Manifest::mirror_id(ok.get("a-7").unwrap()), "a");
        assert_eq!(Manifest::mirror_id(ok.get("a").unwrap()), "a");

        for bad in [
            // names no entry
            format!(
                "[[repo]]\nid = \"a-7\"\nurl = \"u\"\nmirror = \"nope\"\npin = \"{}\"\n",
                "1".repeat(40)
            ),
            // different url
            format!(
                "[[repo]]\nid = \"a-7\"\nurl = \"v\"\nmirror = \"a\"\npin = \"{}\"\n",
                "1".repeat(40)
            ),
            // chained through another borrower
            format!(
                "[[repo]]\nid = \"a-7\"\nurl = \"u\"\nmirror = \"a\"\npin = \"{p}\"\n\
                 [[repo]]\nid = \"a-8\"\nurl = \"u\"\nmirror = \"a-7\"\npin = \"{p}\"\n",
                p = "1".repeat(40)
            ),
        ] {
            let m: Manifest = toml::from_str(&base(&bad)).unwrap();
            assert!(m.validate().is_err(), "should have been rejected: {bad}");
        }
    }

    #[test]
    fn the_checked_in_manifest_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus.toml");
        let m = Manifest::load(&path).unwrap();
        assert!(m.repos.iter().any(|r| r.id == "bazel"));
        for r in &m.repos {
            assert!(!r.url.is_empty(), "{}", r.id);
        }
        // The version-matrix entries share their parent's mirror rather than
        // cloning the same repository a second time.
        for (id, parent) in [
            ("bazel-7", "bazel"),
            ("bazel-8", "bazel"),
            ("envoy-7", "envoy"),
        ] {
            let e = m.get(id).unwrap();
            assert_eq!(e.mirror.as_deref(), Some(parent), "{id}");
            assert_eq!(e.url, m.get(parent).unwrap().url, "{id}");
            assert_eq!(
                e.invalidators,
                m.get(parent).unwrap().invalidators,
                "{id} should carry its parent's invalidators"
            );
            assert_ne!(e.pin, m.get(parent).unwrap().pin, "{id}");
        }
    }
}
