//! The index-side data: what a repository is built from, and how main-repo
//! paths land inside it.
//!
//! [`Pattern`], [`PathMap`] and [`RepoInputs`] are the mutable form of the
//! externals tables [`crate::IndexBuilder`] flattens and [`crate::IndexRef`]
//! reads back; the encodings here are the on-disk ones and must not move
//! without [`crate::FORMAT_VERSION`].

use std::collections::{BTreeMap, BTreeSet};

/// A main-repo input of an external repository: a path pattern the repository
/// is built from, or a *rule label* whose own inputs the graph knows.
///
/// # How the three kinds share one stored column
///
/// The table on disk is a string per pattern plus the two boolean columns
/// `repo_input_prefix` and `repo_input_target_only`, and this enum needs no
/// third column: a repo-relative path never begins with `//`, so a stored
/// string that does *is* a label and nothing else can be. Both booleans keep
/// their meaning — a label is never a prefix, and it is `target_only`
/// exactly when the path patterns derived beside it are. So adding
/// [`Pattern::Label`] moves what the tables *mean* without moving the layout:
/// [`crate::FORMAT_VERSION`] stands and [`crate::CONTENT_REVISION`] is what
/// retires the indexes written before it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pattern {
    /// Exactly this repo-relative path.
    Exact(String),
    /// This directory and everything under it. Stored with its trailing `/`.
    Prefix(String),
    /// A main-repo *target* the repository names, in canonical `//pkg:name`
    /// form. It matches no path; the engine resolves it to a node and asks
    /// the graph whether the change reached it. See
    /// [`crate::externals::main_repo_patterns`].
    Label(String),
}

impl Pattern {
    /// Whether a changed main-repo *path* hits this pattern. A label pattern
    /// matches no path: it is answered from the graph, not from the diff.
    pub fn matches(&self, path: &str) -> bool {
        match self {
            Pattern::Exact(p) => path == p,
            Pattern::Prefix(d) => path.starts_with(d.as_str()),
            Pattern::Label(_) => false,
        }
    }

    /// The stored form: a prefix keeps its trailing `/`, which is what
    /// distinguishes it from an exact path, and a label keeps its leading
    /// `//`, which is what distinguishes it from either.
    pub fn as_str(&self) -> &str {
        match self {
            Pattern::Exact(p) => p,
            Pattern::Prefix(d) => d,
            Pattern::Label(l) => l,
        }
    }

    pub fn is_prefix(&self) -> bool {
        matches!(self, Pattern::Prefix(_))
    }

    pub fn is_label(&self) -> bool {
        matches!(self, Pattern::Label(_))
    }

    /// Reads back a stored pattern: a leading `//` means a label, and
    /// otherwise the boolean says whether it is a prefix.
    pub fn from_stored(s: &str, prefix: bool) -> Self {
        if s.starts_with("//") {
            Pattern::Label(s.to_string())
        } else if prefix {
            Pattern::Prefix(s.to_string())
        } else {
            Pattern::Exact(s.to_string())
        }
    }
}

/// The separator between the two halves of a stored [`PathMap`]; no path
/// contains it.
pub(crate) const SEP: char = '\u{0}';

/// A statically derived rewrite from a main-repo path to a path inside an
/// external repository.
///
/// This is what lets an input-path edit stop short of re-querying every
/// package that loads a `.bzl` from the repository. `@envoy_api` is built from
/// `api/`, so `api/envoy/config/v3/x.proto` is `envoy/config/v3/x.proto` inside
/// the repository — which is not `bazel/api_build_system.bzl`, the one file
/// every envoy package's load closure reaches into that repository for. Rule
/// (a) of the engine's external stage can then be skipped for that edit.
///
/// A trailing `/` on [`PathMap::from`] means a directory rewrite; without one
/// the entry maps exactly one file. A directory rewrite's [`PathMap::to`] is
/// either empty (the repository root) or itself ends in `/`.
///
/// Being *wrong* here is a soundness bug — it would hide a `.bzl` edit — so
/// every shape the parser is not certain of is simply not recorded, and a
/// repository with no recorded mapping keeps the old whole-closure behaviour.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PathMap {
    /// Main-repo path, or directory prefix with its trailing `/`.
    pub from: String,
    /// The corresponding repository-relative path or directory prefix.
    pub to: String,
    /// True when this mapping accounts for *everything* in the repository: a
    /// `local_path_override` directory, or a `local_repository` / a
    /// `new_local_repository` whose `path` becomes the repository root. Such a
    /// repository cannot contain a `.bzl` that lives outside the directory, so
    /// a changed path the mapping does not cover is not a `.bzl` it exports —
    /// which is what lets the engine skip rule (2) for it. A partial mapping,
    /// such as one `symlink` out of several, is never exhaustive.
    pub exhaustive: bool,
}

impl PathMap {
    /// The repository-relative path a main-repo path lands at, if this entry
    /// covers it at all.
    pub fn translate(&self, path: &str) -> Option<String> {
        match self.from.strip_suffix('/') {
            Some(_) => path
                .strip_prefix(self.from.as_str())
                .map(|rest| format!("{}{rest}", self.to)),
            None => (path == self.from).then(|| self.to.clone()),
        }
    }

    /// A whole-directory mapping that accounts for the repository.
    pub fn exhaustive_dir(from: String) -> PathMap {
        PathMap {
            from,
            to: String::new(),
            exhaustive: true,
        }
    }

    /// The stored form: `from` and `to` joined by a NUL, with a third
    /// NUL-separated field — `!` — appended when the mapping is exhaustive.
    /// Two fields is what every index written before that field existed
    /// contains, and it reads back as non-exhaustive, which is the wide
    /// answer.
    pub fn encode(&self) -> String {
        let mut s = self.from.clone();
        s.push(SEP);
        s.push_str(&self.to);
        if self.exhaustive {
            s.push(SEP);
            s.push('!');
        }
        s
    }

    /// Reads back [`PathMap::encode`], in either flavour.
    pub fn decode(s: &str) -> Option<PathMap> {
        let (from, rest) = s.split_once(SEP)?;
        let (to, exhaustive) = match rest.split_once(SEP) {
            Some((to, mark)) => (to, mark == "!"),
            None => (rest, false),
        };
        Some(PathMap {
            from: from.to_string(),
            to: to.to_string(),
            exhaustive,
        })
    }
}

/// The main repository's contribution to external repositories, in the
/// mutable form a build works with. [`crate::IndexBuilder`] flattens it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoInputs {
    /// Main-repo path pattern -> the repositories built from it.
    pub patterns: BTreeMap<Pattern, BTreeSet<String>>,
    /// Workspace-closure `.bzl` path -> the repositories it can define.
    pub bzl_repos: BTreeMap<String, BTreeSet<String>>,
    /// Repository -> how main-repo paths map into it, where that could be
    /// derived statically. A repository with no entry has no known mapping.
    pub path_maps: BTreeMap<String, BTreeSet<PathMap>>,
    /// The subset of [`RepoInputs::patterns`] whose every occurrence was a
    /// label *embedded in text the repository rule writes* — the `srcs` and
    /// `tools` of a BUILD file it generates — rather than a label the rule
    /// itself resolves (`attr.label(default = ...)`, an extension tag, a
    /// `local_path_override`). A file such a label names is a dependency of
    /// the repository's *targets*; it cannot move a `.bzl` file inside the
    /// repository, so the engine applies rule (1) alone for it. An embedded
    /// label that is itself a `.bzl` is excluded — a generated `.bzl` may
    /// `load()` it. See `inputs::string_literals`.
    pub target_only: BTreeSet<Pattern>,
    /// Every repository the main repository itself defines (extensions and
    /// repo rules whose `.bzl` is a main-repo label), plus every module the
    /// main repository overrides. The fallback attribution set.
    pub repos: BTreeSet<String>,
    /// Apparent name -> canonical name, captured so that a run can match a
    /// dirty repository against the canonical `@@name+//...` labels Bazel
    /// emits without re-running `bazel mod dump_repo_mapping`.
    pub canonical: BTreeMap<String, String>,
}

impl RepoInputs {
    /// The repositories a changed main-repo path feeds.
    pub fn repos_for(&self, path: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for (pat, repos) in &self.patterns {
            if pat.matches(path) {
                out.extend(repos.iter().cloned());
            }
        }
        out
    }

    /// The repositories a main-repo *target* is an input of; `label` is
    /// canonical `//pkg:name`. See [`Pattern::Label`].
    pub fn repos_for_label(&self, label: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for (pat, repos) in &self.patterns {
            if matches!(pat, Pattern::Label(l) if l == label) {
                out.extend(repos.iter().cloned());
            }
        }
        out
    }
}
