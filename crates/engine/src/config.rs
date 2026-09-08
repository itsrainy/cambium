//! `cambium.toml` (spec 9).
//!
//! The config is hashed into every index it produces. An index whose
//! `config_hash` differs from the current config cannot be spliced onto,
//! because the invalidator rules or query flags that produced it may have
//! changed — the incremental result would silently disagree with a full build.

use std::path::{Path, PathBuf};

use cambium_classify::Rules;
use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "cambium.toml";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub bazel: BazelConfig,
    pub externals: Externals,
    pub invalidators: Invalidators,
    pub bazelrc: BazelrcConfig,
    pub ignore: Ignore,
    pub output: Output,
    pub baseline: Baseline,
    pub limits: Limits,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct BazelConfig {
    /// Binary to run. Empty means: `$BAZEL_REAL`, then `$BAZEL`, then
    /// `bazelisk` if on PATH, then `bazel`.
    pub binary: String,
    pub extra_query_flags: Vec<String>,
    /// Startup options placed before the command word (`--output_base=...`,
    /// `--bazelrc=...`). They choose *which server* answers, not what the
    /// graph is, so they are excluded from the config hash: a benchmark run on a
    /// different machine, with a different output base, still shares the
    /// lineage.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub startup_flags: Vec<String>,
}

/// Extends the built-in allowlist of `.bazelrc` flags known not to affect
/// loading or analysis (see [`crate::bazelrc`]).
///
/// A repository knows things about its own flags that cambium cannot: a
/// `--experimental_` flag it has audited, a wrapper flag only its own tooling
/// reads. Entries are flag names, dashes optional, and may end in `*`. This is
/// part of the config hash, because it decides what a run is allowed to skip.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct BazelrcConfig {
    pub allowlist_extra: Vec<String>,
}

/// External-repository invalidation (`cambium_index::externals`).
///
/// On by default. Turning it off restores the older, blunter behaviour:
/// `MODULE.bazel` and every workspace-level `.bzl` are invalidators, and a
/// file that only matters through a repository rule is invisible unless it is
/// listed as one. That is the right setting for a repository whose repository
/// rules read main-repo files by computed path rather than by label, since no
/// static scan can see those.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Externals {
    pub enabled: bool,
}

impl Default for Externals {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Invalidators {
    pub paths: Vec<String>,
    pub globs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Ignore {
    /// Globs whose changes are assumed irrelevant to the build graph. The
    /// assumption is checked: a changed path some rule consumes as a source
    /// marks its consumers direct however these globs read, because the graph
    /// knows better than the configuration does.
    pub globs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Output {
    pub exclude_tags: Vec<String>,
    pub test_rule_suffixes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Baseline {
    pub store: StoreConfig,
    pub max_ancestor_walk: usize,
    pub retention_days: u64,
    /// Where fetched indexes, per-revision worktrees and publish scratch
    /// files live. Defaults to `<tmp>/cambium-cache`; `--cache-dir` on the
    /// command line overrides it. Not part of the config hash: it says where a
    /// run keeps its files, not what the graph is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    /// Allow publishing an index whose `bazel query` finished with load errors
    /// (exit 3 under `--keep_going`). Off by default: a partial index answers
    /// correctly about the packages it has and knows nothing about the rest,
    /// and once it is in the store every later run splices onto it. Not part
    /// of the config hash, for the same reason as `cache_dir`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub publish_partial: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// How many packages a run may re-query incrementally before it gives up
    /// on splicing and re-indexes the whole repository at head.
    ///
    /// This bounds the thing that actually costs time. Its predecessor,
    /// `max_changed_files`, bounded the diff instead, which is a proxy that
    /// misfires in both directions: an `apollo` commit touching 10146 files
    /// went global on file count alone though it dirtied a handful of
    /// packages, while a fifty-file commit that rewrites fifty BUILD files
    /// sailed under it. A cold rebuild is also not a global invalidation —
    /// it re-indexes at head and still answers exactly — so exceeding this
    /// limit costs time, never precision.
    ///
    /// Zero disables the limit: every diff is spliced, however large.
    pub max_dirty_packages: usize,
    /// Removed. Present only so that a `cambium.toml` written for an older
    /// cambium still parses; the value is warned about at load and ignored.
    /// Never serialised, so it reaches neither `config show` nor the hash.
    #[serde(default, skip_serializing)]
    pub max_changed_files: Option<usize>,
}

/// Where baseline indexes live. `command` templates get `{name}` and `{path}`
/// substituted, which is enough to drive gsutil, aws s3, or curl.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[derive(Default)]
pub enum StoreConfig {
    /// No baseline store configured; every run must build or be handed an index.
    #[default]
    None,
    Dir {
        path: PathBuf,
    },
    Http {
        /// Base URL; the index name is appended.
        url: String,
        #[serde(default)]
        headers: Vec<String>,
    },
    Command {
        fetch_cmd: Vec<String>,
        push_cmd: Vec<String>,
        #[serde(default)]
        list_cmd: Vec<String>,
        /// The exit status `fetch_cmd` uses for "no such object". When set,
        /// any other non-zero exit is a store error rather than a miss; when
        /// unset, every non-zero exit is read as a miss (and logged with its
        /// stderr when there is any). `aws s3 cp` and `gsutil cp` both exit 1
        /// for a missing key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_found_exit: Option<i32>,
    },
}

impl Default for Invalidators {
    fn default() -> Self {
        Self {
            paths: [
                "MODULE.bazel",
                "MODULE.bazel.lock",
                "WORKSPACE",
                "WORKSPACE.bazel",
                "WORKSPACE.bzlmod",
                "WORKSPACE.bzlmod.lock",
                ".bazelrc",
                ".bazelversion",
                // The root BUILD file can carry repo-wide package() settings.
                "BUILD",
                "BUILD.bazel",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            globs: [
                "tools/toolchains/**",
                "platforms/**",
                "third_party/**/*.bzl",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        }
    }
}

impl Default for Ignore {
    fn default() -> Self {
        Self {
            globs: ["**/*.md", "docs/**", ".github/**"]
                .into_iter()
                .map(String::from)
                .collect(),
        }
    }
}

impl Default for Output {
    fn default() -> Self {
        Self {
            exclude_tags: vec!["manual".to_string()],
            test_rule_suffixes: vec!["_test".to_string()],
        }
    }
}

impl Default for Baseline {
    fn default() -> Self {
        Self {
            store: StoreConfig::None,
            max_ancestor_walk: 20,
            retention_days: 30,
            cache_dir: None,
            publish_partial: false,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // Where an incremental re-query of N packages costs about what a
            // cold `//...` re-index costs, measured on the history workloads:
            // the crossover is ~450 packages on `bazel`, ~620 on `apollo`,
            // ~210 on `sourcegraph` and beyond the whole repository on
            // `envoy` (whose cold build is 90 s). 500 sits in the middle of
            // that spread and below the package count of every
            // repository, so it fires only on a genuinely repository-wide
            // diff.
            max_dirty_packages: 500,
            max_changed_files: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

impl Config {
    /// Loads `cambium.toml` from `root`, or returns defaults if absent.
    pub fn load(root: &Path) -> Result<Self, ConfigError> {
        Self::load_from(&root.join(CONFIG_FILE))
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let cfg: Self = toml::from_str(&s).map_err(|source| ConfigError::Parse {
                    path: path.to_path_buf(),
                    source,
                })?;
                cfg.warn_about_removed_keys(path);
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Says so, once, for every key that a newer cambium no longer honours.
    ///
    /// Refusing the file instead would be the stricter reading of
    /// `deny_unknown_fields`, but a removed limit is not a typo: the repository
    /// that set it did so deliberately, and failing every `cambi` invocation
    /// until someone edits the file punishes the wrong person.
    fn warn_about_removed_keys(&self, path: &Path) {
        if let Some(n) = self.limits.max_changed_files {
            tracing::warn!(
                config = %path.display(),
                value = n,
                "limits.max_changed_files was removed and is ignored; the limit is now \
                 limits.max_dirty_packages, which bounds the re-query rather than the diff"
            );
        }
    }

    /// Canonical serialisation, used for hashing and for `config show`.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("config is serialisable")
    }

    /// Hash of the *normalised* config, so that comments and key order do not
    /// invalidate a lineage but a changed invalidator does.
    ///
    /// [`cambium_proto::RULE_HASH_VERSION`] is mixed in as well. It is not
    /// configuration, but it decides what a target's rule hash means, and the
    /// index carries this value as its `config_hash`: an index whose
    /// `config_hash` does not match is refused for splicing
    /// ([`crate::EngineError::ConfigHashMismatch`]) and the run mints a new
    /// lineage. That is exactly the guarantee a canonicalisation change needs —
    /// rows hashed one way are never compared against rows hashed another —
    /// and it is cheaper than a format break, which would make old indexes
    /// unreadable rather than merely unspliceable.
    pub fn hash(&self) -> [u8; 32] {
        let mut graph_relevant = self.clone();
        graph_relevant.bazel.startup_flags.clear();
        graph_relevant.baseline.cache_dir = None;
        graph_relevant.baseline.publish_partial = false;
        let mut h = blake3::Hasher::new();
        h.update(b"cambium-rule-hash-v");
        h.update(&cambium_proto::RULE_HASH_VERSION.to_le_bytes());
        h.update(b"\n");
        h.update(graph_relevant.to_toml().as_bytes());
        *h.finalize().as_bytes()
    }

    pub fn rules(&self) -> Rules {
        Rules {
            invalidator_paths: self.invalidators.paths.clone(),
            invalidator_globs: self.invalidators.globs.clone(),
            ignore_globs: self.ignore.globs.clone(),
        }
    }

    pub fn convert_options(&self) -> cambium_proto::ConvertOptions {
        cambium_proto::ConvertOptions {
            test_rule_suffixes: self.output.test_rule_suffixes.clone(),
            exclude_tags: self.output.exclude_tags.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let c = Config::default();
        let s = c.to_toml();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn hash_ignores_formatting_but_not_content() {
        let a = Config::default();
        let mut b = Config::default();
        assert_eq!(a.hash(), b.hash());
        b.invalidators.globs.push("extra/**".into());
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn startup_flags_do_not_reach_the_hash_or_the_default_serialisation() {
        let a = Config::default();
        let mut b = Config::default();
        b.bazel.startup_flags.push("--output_base=/tmp/x".into());
        assert_eq!(
            a.hash(),
            b.hash(),
            "startup flags pick a server, not a graph"
        );
        assert!(!a.to_toml().contains("startup_flags"));
        let back: Config = toml::from_str(&b.to_toml()).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn the_rule_hash_version_is_part_of_the_config_hash() {
        // Not a config field, but it decides what a rule hash means, so an
        // index built under a different canonicalisation must not be
        // spliceable. Pinned by value so bumping the version is a deliberate
        // act with a visible test to update.
        let expected = {
            let mut h = blake3::Hasher::new();
            h.update(b"cambium-rule-hash-v");
            h.update(&4u32.to_le_bytes());
            h.update(b"\n");
            h.update(Config::default().to_toml().as_bytes());
            *h.finalize().as_bytes()
        };
        assert_eq!(
            Config::default().hash(),
            expected,
            "RULE_HASH_VERSION moved: old indexes are now a different lineage, \
             which is the point — update this test"
        );
    }

    #[test]
    fn parses_the_spec_example() {
        let toml_src = r#"
[bazel]
binary = "bazelisk"
extra_query_flags = []

[invalidators]
paths = ["MODULE.bazel", ".bazelrc"]
globs = ["tools/toolchains/**"]

[ignore]
globs = ["**/*.md"]

[output]
exclude_tags = ["manual"]
test_rule_suffixes = ["_test"]

[baseline]
store = { kind = "dir", path = "/var/cache/cambium" }
max_ancestor_walk = 20
retention_days = 30
"#;
        let c: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(c.bazel.binary, "bazelisk");
        assert_eq!(
            c.baseline.store,
            StoreConfig::Dir {
                path: PathBuf::from("/var/cache/cambium")
            }
        );
        // Unset sections keep their defaults.
        assert_eq!(c.limits.max_dirty_packages, 500);
    }

    /// A `cambium.toml` written for an older cambium must still load. The
    /// value is parsed, warned about and ignored — and it must not reach the
    /// hash, or upgrading cambium would orphan every published index of a
    /// repository that had set it.
    #[test]
    fn the_removed_max_changed_files_key_is_accepted_and_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(&path, "[limits]\nmax_changed_files = 5000\n").unwrap();
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.limits.max_changed_files, Some(5_000));
        assert_eq!(
            c.limits.max_dirty_packages, 500,
            "the live limit is unchanged"
        );
        assert_eq!(
            c.hash(),
            Config::default().hash(),
            "a dead key must not mint a new lineage"
        );
        assert!(!c.to_toml().contains("max_changed_files"));
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(dir.path()).unwrap(), Config::default());
    }

    #[test]
    fn unknown_keys_are_an_error() {
        let e = toml::from_str::<Config>("[bazel]\nbinry = \"x\"\n").unwrap_err();
        assert!(e.to_string().contains("binry"), "{e}");
    }
}
