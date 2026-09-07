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
    /// graph is, so they are excluded from the config hash: a corpus run on a
    /// different machine, with a different output base, still shares the
    /// lineage.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub startup_flags: Vec<String>,
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
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Diffs larger than this fall back to global invalidation (spec 11).
    pub max_changed_files: usize,
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
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_changed_files: 5_000,
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
            Ok(s) => toml::from_str(&s).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            }),
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
        assert_eq!(c.limits.max_changed_files, 5_000);
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
