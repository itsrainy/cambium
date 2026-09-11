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

/// The machine-local override, beside `cambium.toml` and gitignored by
/// convention.
///
/// `cambium.toml` says what the graph is and is hashed into every index; this
/// file says what this machine is, and may therefore only set keys that are
/// already outside that hash ([`LOCAL_ONLY_KEYS`]). Anything else would mean
/// two checkouts of one commit producing indexes that claim to be
/// interchangeable and are not, which is precisely what the hash exists to
/// prevent — so a key that is not on the list is an error naming the file it
/// belongs in.
pub const LOCAL_CONFIG_FILE: &str = "cambium.local.toml";

/// Every key `cambium.local.toml` may set. Each one is excluded from
/// [`Config::hash`]; the test below holds that open.
pub const LOCAL_ONLY_KEYS: &[&str] = &[
    "bazel.binary",
    "bazel.startup_flags",
    "baseline.cache_dir",
    "baseline.compress_indexes",
    "baseline.publish_partial",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub bazel: BazelConfig,
    pub git: GitConfig,
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

/// Git, as `cambi` reads it. Not part of the config hash: it says which
/// revisions a run compares by default, not what the graph is — the same
/// reason `cache_dir` is left out.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct GitConfig {
    /// The branch `cambi affected` measures from when it is given no range:
    /// the merge base of this branch with `HEAD`.
    pub default_branch: String,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            default_branch: "main".to_string(),
        }
    }
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
    /// Write published and cached indexes as a zstd frame. On by default: an
    /// index compresses about 4-6x at level 3 for a few tens of milliseconds,
    /// and a reader takes either form, so this only exists for a store that
    /// wants the bytes verbatim. Not part of the config hash, for the same
    /// reason as `cache_dir`: it says how the file is stored, not what the
    /// graph is.
    #[serde(default = "yes")]
    pub compress_indexes: bool,
}

fn yes() -> bool {
    true
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
                // Both of these are diffed by the externals stage when it is
                // enabled, which exempts them; the entries are what answers
                // for them when it is off.
                "MODULE.bazel.lock",
                "WORKSPACE",
                "WORKSPACE.bazel",
                "WORKSPACE.bzlmod",
                "WORKSPACE.bzlmod.lock",
                ".bazelrc",
                ".bazelversion",
                // `repo()` here sets defaults — visibility, package metadata,
                // features — for every package in the repository, and nothing
                // in the graph carries an edge to it.
                //
                // The root `BUILD` file is deliberately *not* on this list. A
                // `package()` call applies to its own package and no other;
                // repository-wide defaults live in `REPO.bazel`. Everything
                // else a root BUILD file does — `exports_files`, visibility,
                // a `package_group` — is an ordinary target in the root
                // package, which a change to that file dirties like any other
                // BUILD file, and whose consumers the graph already knows.
                "REPO.bazel",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            // `third_party/**/*.bzl` used to be here. It is gone because a
            // changed `.bzl` is now handled exhaustively without it: a file in
            // a package's load closure dirties its consumers, a file in the
            // workspace closure moves the repositories it defines (or goes
            // global when they cannot be named), and a file the closure does
            // not know is checked against every `load()` in the head tree.
            // The glob only ever pre-empted one of those answers with a wider
            // one.
            globs: ["tools/toolchains/**", "platforms/**"]
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
            compress_indexes: true,
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

/// Where an effective configuration value came from.
///
/// One per leaf key, which is what makes `cambi config explain` answerable:
/// "the flag", "the local file", "the checked-in file", or "nobody set it".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// Nothing set it; this is the built-in value.
    Default,
    /// `cambium.toml` (or whatever `--config` named).
    File { path: PathBuf },
    /// `cambium.local.toml`.
    LocalFile { path: PathBuf },
    /// A command-line flag, by the spelling a caller types.
    Flag { name: String },
    /// An environment variable, by name.
    Env { name: String },
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Origin::Default => f.write_str("default"),
            Origin::File { path } | Origin::LocalFile { path } => write!(f, "{}", path.display()),
            Origin::Flag { name } => write!(f, "{name}"),
            Origin::Env { name } => write!(f, "${name}"),
        }
    }
}

/// What the command line and the environment said, with the spelling to
/// attribute each value to. Highest precedence of all the layers.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub bazel_binary: Option<(String, Origin)>,
    pub cache_dir: Option<(PathBuf, Origin)>,
}

/// The result of [`Config::load_layered`]: the effective config, and the
/// provenance of every leaf value in it.
#[derive(Clone, Debug)]
pub struct Layered {
    pub config: Config,
    /// Leaf key (`baseline.retention_days`) to where its value came from.
    pub origins: std::collections::BTreeMap<String, Origin>,
    /// The `cambium.toml` that was read, whether or not it exists.
    pub path: PathBuf,
    pub local_path: PathBuf,
    /// The keys `cambium.local.toml` set, in key order. Empty when there is no
    /// local file.
    pub local_keys: Vec<String>,
    pub diagnostics: Vec<crate::diag::Diagnostic>,
}

impl Layered {
    pub fn local_exists(&self) -> bool {
        self.local_path.exists()
    }
}

/// `cambium.local.toml`, in the shape the allowed keys make.
///
/// Its own type rather than a partial [`Config`]: the point of the file is
/// that it cannot express anything else, and a struct that holds only these
/// five fields is the cheapest way to say so in the type system. The key list
/// is still checked by name, because a typo inside an allowed section
/// (`bazel.binry`) has to name the key a caller wrote.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Local {
    bazel: LocalBazel,
    baseline: LocalBaseline,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LocalBazel {
    binary: Option<String>,
    startup_flags: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LocalBaseline {
    cache_dir: Option<PathBuf>,
    compress_indexes: Option<bool>,
    publish_partial: Option<bool>,
}

impl Local {
    fn apply(self, cfg: &mut Config) {
        if let Some(v) = self.bazel.binary {
            cfg.bazel.binary = v;
        }
        if let Some(v) = self.bazel.startup_flags {
            cfg.bazel.startup_flags = v;
        }
        if let Some(v) = self.baseline.cache_dir {
            cfg.baseline.cache_dir = Some(v);
        }
        if let Some(v) = self.baseline.compress_indexes {
            cfg.baseline.compress_indexes = v;
        }
        if let Some(v) = self.baseline.publish_partial {
            cfg.baseline.publish_partial = v;
        }
    }
}

/// The dotted leaf keys a TOML file sets, or nothing at all when it is absent.
///
/// Absent is not an error here: both files are optional, and a run with
/// neither is a run on the defaults.
fn leaf_keys_of(path: &Path) -> Result<Vec<String>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let value: toml::Value = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(leaf_keys(&value))
}

/// Every dotted path in a TOML value that holds something other than a table.
///
/// An array is a leaf: `invalidators.paths` is one value a caller sets or does
/// not, and attributing its elements separately would say nothing useful.
pub fn leaf_keys(value: &toml::Value) -> Vec<String> {
    fn walk(value: &toml::Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            toml::Value::Table(t) => {
                for (k, v) in t {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(v, &path, out);
                }
            }
            _ => out.push(prefix.to_string()),
        }
    }
    let mut out = Vec::new();
    walk(value, "", &mut out);
    out.sort();
    out
}

/// One effective value, as `cambi config explain` prints it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExplainedValue {
    pub key: String,
    /// The value, in TOML's own spelling (`["a", "b"]`, `"bazelisk"`, `500`).
    pub value: String,
    pub origin: Origin,
    /// What the origin reads as on one line: a path, a flag, `$VAR`, or
    /// `default`.
    pub origin_label: String,
}

impl Layered {
    /// Every effective leaf value with its origin, in key order.
    pub fn explain(&self) -> Vec<ExplainedValue> {
        let value = self.config.to_toml_value();
        self.origins
            .iter()
            .map(|(key, origin)| ExplainedValue {
                key: key.clone(),
                value: lookup(&value, key)
                    .map(toml_scalar)
                    .unwrap_or_else(|| "-".into()),
                origin: origin.clone(),
                origin_label: origin.to_string(),
            })
            .collect()
    }
}

/// The value at a dotted key, if the serialisation has one.
fn lookup<'a>(value: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let mut cur = value;
    for part in key.split('.') {
        cur = cur.as_table()?.get(part)?;
    }
    Some(cur)
}

/// A TOML value on one line, as TOML would write it.
fn toml_scalar(value: &toml::Value) -> String {
    // Through a one-key document rather than `Value::to_string`, which writes
    // a table as an inline table and a scalar as a bare `{ v = 42 }`.
    let doc: toml::map::Map<String, toml::Value> =
        [("v".to_string(), value.clone())].into_iter().collect();
    match toml::to_string(&doc) {
        Ok(s) => s
            .trim()
            .strip_prefix("v = ")
            .unwrap_or(s.trim())
            .trim()
            .to_string(),
        Err(_) => String::from("-"),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "{path} sets {key}, which belongs in cambium.toml: it is part of the config hash, \
         so it says what the graph is rather than what this machine is"
    )]
    LocalOverrideKey { path: PathBuf, key: String },
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

    /// The same findings as [`Config::warn_about_removed_keys`], as
    /// diagnostics.
    ///
    /// The warning is emitted while the file is being read, before there is an
    /// engine to report into, so the actionable half is recomputed from the
    /// config once the sink exists. The two must say the same thing.
    pub fn removed_key_diagnostics(&self) -> Vec<crate::diag::Diagnostic> {
        let mut out = Vec::new();
        if let Some(n) = self.limits.max_changed_files {
            out.push(
                crate::diag::Diagnostic::warning(
                    crate::diag::Code::W002RemovedConfigKey,
                    format!(
                        "limits.max_changed_files = {n} was removed and is ignored: the limit \
                         is now limits.max_dirty_packages, which bounds the re-query rather \
                         than the diff"
                    ),
                )
                .with_help(
                    "delete limits.max_changed_files, and set limits.max_dirty_packages instead",
                ),
            );
        }
        out
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
    ///
    /// Only what decides the *content* of an index reaches the hash. The
    /// whole `[baseline]` section is left out: the store is where an index
    /// lives, `max_ancestor_walk` and `retention_days` are how far a run
    /// looks for one and how long it keeps it, and none of them changes a
    /// byte of what is built — bumping either used to invalidate every
    /// published index. `[limits]` is a cost decision (splice or re-query
    /// `//...`), not an accuracy one, and is left out for the same reason.
    /// So is `[git]`, which only says which revisions a bare `cambi affected`
    /// compares.
    pub fn hash(&self) -> [u8; 32] {
        let mut graph_relevant = self.clone();
        // Which binary answers, like which server answers, is a fact about a
        // machine: one developer's `/opt/homebrew/bin/bazelisk` and CI's
        // `tools/bazel` wrap the same release and produce the same graph. It is
        // also one of the keys `cambium.local.toml` may set, and everything on
        // that list has to be outside this hash or a machine-local override
        // could silently mint a lineage.
        graph_relevant.bazel.binary.clear();
        graph_relevant.bazel.startup_flags.clear();
        graph_relevant.baseline = Baseline::default();
        graph_relevant.git = GitConfig::default();
        graph_relevant.limits = Limits::default();
        let mut h = blake3::Hasher::new();
        h.update(b"cambium-rule-hash-v");
        h.update(&cambium_proto::RULE_HASH_VERSION.to_le_bytes());
        h.update(b"\n");
        h.update(b"cambium-index-content-v");
        h.update(&cambium_index::CONTENT_REVISION.to_le_bytes());
        h.update(b"\n");
        h.update(graph_relevant.to_toml().as_bytes());
        *h.finalize().as_bytes()
    }

    /// Loads the whole stack — defaults, `cambium.toml`, `cambium.local.toml`,
    /// then the flags and environment variables in `over` — and records where
    /// every effective value came from.
    ///
    /// This is what [`Engine::open`](crate::Engine::open) uses, so the local
    /// file reaches real runs and not only `cambi config explain`.
    pub fn load_layered(
        root: &Path,
        config_path: Option<&Path>,
        over: &Overrides,
    ) -> Result<Layered, ConfigError> {
        let path = config_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.join(CONFIG_FILE));
        let mut cfg = Self::load_from(&path)?;
        let file_keys = leaf_keys_of(&path)?;

        // Beside cambium.toml, wherever --config pointed: a caller who moved
        // the contract moved its machine-local companion with it.
        let local_path = path.parent().unwrap_or(root).join(LOCAL_CONFIG_FILE);
        let mut diagnostics = cfg.removed_key_diagnostics();
        let local_keys = leaf_keys_of(&local_path)?;
        let mut local_applied = Vec::new();
        if !local_keys.is_empty() {
            let text = std::fs::read_to_string(&local_path).map_err(|source| ConfigError::Io {
                path: local_path.clone(),
                source,
            })?;
            // By name, and before the file is deserialised: `Local` rejects
            // unknown fields too, but as a parse error that does not name the
            // rule being broken.
            for key in &local_keys {
                if !LOCAL_ONLY_KEYS.contains(&key.as_str()) {
                    return Err(ConfigError::LocalOverrideKey {
                        path: local_path.clone(),
                        key: key.clone(),
                    });
                }
                local_applied.push(key.clone());
            }
            let local: Local = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: local_path.clone(),
                source,
            })?;
            local.apply(&mut cfg);
        }

        let mut origins: Vec<(String, Origin)> = Vec::new();
        if let Some((binary, origin)) = &over.bazel_binary {
            cfg.bazel.binary = binary.clone();
            origins.push(("bazel.binary".into(), origin.clone()));
        }
        if let Some((dir, origin)) = &over.cache_dir {
            cfg.baseline.cache_dir = Some(dir.clone());
            origins.push(("baseline.cache_dir".into(), origin.clone()));
        }

        let overridden: Vec<String> = origins.iter().map(|(k, _)| k.clone()).collect();
        let mut attributed = std::collections::BTreeMap::new();
        for (k, o) in origins {
            attributed.insert(k, o);
        }
        for key in leaf_keys(&cfg.to_toml_value()) {
            if overridden.contains(&key) {
                continue;
            }
            let origin = if local_applied.contains(&key) {
                Origin::LocalFile {
                    path: local_path.clone(),
                }
            } else if file_keys.contains(&key) {
                Origin::File { path: path.clone() }
            } else {
                Origin::Default
            };
            attributed.insert(key, origin);
        }

        Ok(Layered {
            config: cfg,
            origins: attributed,
            path,
            local_path,
            local_keys,
            diagnostics: std::mem::take(&mut diagnostics),
        })
    }

    /// The canonical serialisation as a TOML value, for walking its leaves.
    pub fn to_toml_value(&self) -> toml::Value {
        toml::Value::try_from(self).expect("config is serialisable")
    }

    pub fn rules(&self) -> Rules {
        Rules {
            invalidator_paths: self.invalidators.paths.clone(),
            invalidator_globs: self.invalidators.globs.clone(),
            ignore_globs: self.ignore.globs.clone(),
        }
    }

    pub fn convert_options(&self) -> cambium_bazel::ConvertOptions {
        cambium_bazel::ConvertOptions {
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

    /// Where an index lives, how far back a run looks for one, how long it
    /// is kept, and when a run stops splicing: none of it changes a byte of
    /// the index, so none of it may retire every published one.
    #[test]
    fn baseline_and_limits_do_not_reach_the_hash() {
        let a = Config::default();
        let mut b = Config::default();
        b.baseline.max_ancestor_walk = 50;
        b.baseline.retention_days = 90;
        b.baseline.store = StoreConfig::Dir {
            path: PathBuf::from("/srv/indexes"),
        };
        b.limits.max_dirty_packages = 5000;
        assert_eq!(a.hash(), b.hash());
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
            h.update(b"cambium-index-content-v");
            h.update(&3u32.to_le_bytes());
            h.update(b"\n");
            h.update(Config::default().to_toml().as_bytes());
            *h.finalize().as_bytes()
        };
        assert_eq!(
            Config::default().hash(),
            expected,
            "RULE_HASH_VERSION or CONTENT_REVISION moved: old indexes are now a \
             different lineage, which is the point — update this test"
        );
    }

    /// A published index is trusted on the strength of its `config_hash`
    /// alone, so a build whose *derivation* changed has to change that hash
    /// too: nothing else distinguishes an index a fixed cambium wrote from one
    /// a buggy cambium wrote at the same commit with the same configuration.
    /// That was the envoy change-set step, which kept fetching pre-fix bases
    /// out of the store and cold-rebuilding every one of them long after the
    /// splice bug was fixed.
    #[test]
    fn the_index_content_revision_is_part_of_the_config_hash() {
        let mut without = blake3::Hasher::new();
        without.update(b"cambium-rule-hash-v");
        without.update(&cambium_proto::RULE_HASH_VERSION.to_le_bytes());
        without.update(b"\n");
        without.update(Config::default().to_toml().as_bytes());
        assert_ne!(
            Config::default().hash(),
            *without.finalize().as_bytes(),
            "an index built before a derivation fix must not be spliceable"
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

    /// The whole point of the local file: every key it may set is already
    /// outside the config hash, so a machine-local override can never make two
    /// indexes claim to be interchangeable when they are not.
    #[test]
    fn every_locally_overridable_key_is_outside_the_config_hash() {
        let base = Config::default().hash();
        let mut variants: Vec<(&str, Config)> = Vec::new();
        let mut c = Config::default();
        c.bazel.binary = "/opt/bazelisk".into();
        variants.push(("bazel.binary", c));
        let mut c = Config::default();
        c.bazel.startup_flags = vec!["--output_base=/tmp/x".into()];
        variants.push(("bazel.startup_flags", c));
        let mut c = Config::default();
        c.baseline.cache_dir = Some(PathBuf::from("/tmp/cache"));
        variants.push(("baseline.cache_dir", c));
        let mut c = Config::default();
        c.baseline.compress_indexes = false;
        variants.push(("baseline.compress_indexes", c));
        let mut c = Config::default();
        c.baseline.publish_partial = true;
        variants.push(("baseline.publish_partial", c));

        for (key, cfg) in &variants {
            assert_eq!(
                cfg.hash(),
                base,
                "{key} is in cambium.local.toml's allowlist but reaches the config hash"
            );
        }
        let mut covered: Vec<&str> = variants.iter().map(|(k, _)| *k).collect();
        covered.sort();
        let mut allowed = LOCAL_ONLY_KEYS.to_vec();
        allowed.sort();
        assert_eq!(
            covered, allowed,
            "the allowlist and this test must name the same keys"
        );
    }

    fn layered(dir: &Path, main: &str, local: Option<&str>) -> Result<Layered, ConfigError> {
        std::fs::write(dir.join(CONFIG_FILE), main).unwrap();
        if let Some(l) = local {
            std::fs::write(dir.join(LOCAL_CONFIG_FILE), l).unwrap();
        }
        Config::load_layered(dir, None, &Overrides::default())
    }

    #[test]
    fn the_local_file_wins_over_the_checked_in_one_and_is_attributed() {
        let d = tempfile::tempdir().unwrap();
        let l = layered(
            d.path(),
            "[bazel]\nbinary = \"bazel\"\n",
            Some("[bazel]\nbinary = \"bazelisk\"\n[baseline]\npublish_partial = true\n"),
        )
        .unwrap();
        assert_eq!(l.config.bazel.binary, "bazelisk");
        assert!(l.config.baseline.publish_partial);
        assert_eq!(
            l.origins.get("bazel.binary"),
            Some(&Origin::LocalFile {
                path: d.path().join(LOCAL_CONFIG_FILE)
            })
        );
        assert_eq!(
            l.local_keys,
            vec!["baseline.publish_partial", "bazel.binary"],
            "leaf keys are reported in key order"
        );
    }

    #[test]
    fn a_flag_outranks_both_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(CONFIG_FILE), "[bazel]\nbinary = \"bazel\"\n").unwrap();
        std::fs::write(
            d.path().join(LOCAL_CONFIG_FILE),
            "[bazel]\nbinary = \"bazelisk\"\n",
        )
        .unwrap();
        let over = Overrides {
            bazel_binary: Some((
                "/opt/bazel".into(),
                Origin::Flag {
                    name: "--bazel-binary".into(),
                },
            )),
            cache_dir: Some((
                PathBuf::from("/tmp/c"),
                Origin::Env {
                    name: "CAMBIUM_CACHE_DIR".into(),
                },
            )),
        };
        let l = Config::load_layered(d.path(), None, &over).unwrap();
        assert_eq!(l.config.bazel.binary, "/opt/bazel");
        assert_eq!(l.config.baseline.cache_dir, Some(PathBuf::from("/tmp/c")));
        assert_eq!(
            l.origins.get("bazel.binary").map(Origin::to_string),
            Some("--bazel-binary".to_string())
        );
        assert_eq!(
            l.origins.get("baseline.cache_dir").map(Origin::to_string),
            Some("$CAMBIUM_CACHE_DIR".to_string())
        );
    }

    /// A key that decides what the graph is does not belong in a file one
    /// machine has and the next does not, and saying so quietly is how an
    /// index that disagrees with a full build gets published.
    #[test]
    fn a_key_outside_the_allowlist_is_refused_by_name() {
        let d = tempfile::tempdir().unwrap();
        let e = layered(
            d.path(),
            "",
            Some("[invalidators]\npaths = [\"MODULE.bazel\"]\n"),
        )
        .unwrap_err();
        let ConfigError::LocalOverrideKey { key, path } = &e else {
            panic!("wrong error: {e}");
        };
        assert_eq!(key, "invalidators.paths");
        assert_eq!(path, &d.path().join(LOCAL_CONFIG_FILE));
        assert!(e.to_string().contains("belongs in cambium.toml"), "{e}");
    }

    #[test]
    fn unset_keys_are_attributed_to_the_defaults_and_set_ones_to_the_file() {
        let d = tempfile::tempdir().unwrap();
        let l = layered(d.path(), "[limits]\nmax_dirty_packages = 42\n", None).unwrap();
        assert_eq!(
            l.origins.get("limits.max_dirty_packages"),
            Some(&Origin::File {
                path: d.path().join(CONFIG_FILE)
            })
        );
        assert_eq!(
            l.origins.get("baseline.retention_days"),
            Some(&Origin::Default)
        );
        let explained = l.explain();
        let row = explained
            .iter()
            .find(|v| v.key == "limits.max_dirty_packages")
            .expect("every leaf key is explained");
        assert_eq!(row.value, "42");
        assert_eq!(
            row.origin_label,
            d.path().join(CONFIG_FILE).display().to_string()
        );
        let arrays = explained
            .iter()
            .find(|v| v.key == "invalidators.globs")
            .expect("an array is one leaf");
        assert!(arrays.value.starts_with('['), "{}", arrays.value);
        assert_eq!(arrays.origin, Origin::Default);
    }

    #[test]
    fn no_files_at_all_is_every_key_on_its_default() {
        let d = tempfile::tempdir().unwrap();
        let l = Config::load_layered(d.path(), None, &Overrides::default()).unwrap();
        assert_eq!(l.config, Config::default());
        assert!(!l.origins.is_empty());
        assert!(
            l.origins.values().all(|o| *o == Origin::Default),
            "{:?}",
            l.origins
        );
        assert!(!l.local_exists());
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
