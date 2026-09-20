//! The external-repository stage: from a diff to a set of dirty repositories,
//! and from those to dirty packages and direct targets.
//!
//! # Why a stage of its own
//!
//! cambium's graph stops at the main repository's edge. `@envoy_api//envoy:pkg`
//! is a node with no out-edges, because nothing ever queried that repository.
//! So the two ways an edit can reach a target through an external repository
//! were both, until this stage, whole-world invalidations:
//!
//! - the repository's *contents* moved, because `MODULE.bazel` moved (a
//!   `bazel_dep` version, an extension tag, an override), or
//! - the repository is *built from* a main-repo file — a `local_path_override`
//!   directory, or a file a repository rule reads by label — and that file
//!   moved.
//!
//! [`cambium_index::externals`] answers "which repositories moved". This
//! module answers "and who depends on them", from the graph the index already
//! holds.
//!
//! # Soundness
//!
//! Claim: if a main-repo target's build can change because external
//! repository `@r` changed, then this stage marks that target, or a target it
//! depends on, dirty or direct.
//!
//! There are exactly two ways `@r` can reach a main-repo target:
//!
//! 1. **Through a label.** Some main-repo rule names `@r//...` in its
//!    attributes. Bazel resolves that to an edge, the index records the
//!    external label as a leaf node, and the node's reverse row is precisely
//!    the set of rules that name it. Seeding those leaf nodes into the direct
//!    set and running the ordinary reverse walk therefore reaches every target
//!    that depends on `@r`, at any depth — the same answer
//!    `rdeps(//..., @r//...)` gives.
//! 2. **Through a `load()`.** A main-repo package loads a `.bzl` from `@r`,
//!    directly or transitively. The static load closure (format 4) records
//!    every external label a `.bzl` load walk stopped at, so the packages
//!    involved are one lookup away; they are marked dirty and re-queried like
//!    any other dirty package, and the splice then compares rule hashes as
//!    usual.
//!
//! # Why (2) does not always fire
//!
//! A repository built from a main-repo directory is the common case, and it is
//! the expensive one: on envoy every package's load closure reaches
//! `@envoy_api//bazel:api_build_system.bzl`, and `@envoy_api` is `api/`. Taken
//! at face value, rule (2) says an edit to one `.proto` under `api/envoy/`
//! dirties all twelve hundred packages — past `max_dirty_packages`, so a cold
//! rebuild, on nearly half of envoy's history.
//!
//! But rule (2) exists because a `.bzl` *in the repository* may have changed,
//! and `api/envoy/config/v3/x.proto` is `envoy/config/v3/x.proto` inside
//! `@envoy_api`, which is not a file anything loads. So when a repository is
//! dirty because a main-repo **input path** moved, and the index carries that
//! repository's path mapping ([`cambium_index::externals::PathMap`]), each
//! changed path is translated into the repository and rule (2) applies only if
//! one of the results is a `.bzl` the closure actually loads from there.
//!
//! Three things keep that sound. A path the mapping does not cover translates
//! to nothing, and an untranslatable path keeps rule (2) — unless the mapping
//! is *exhaustive* ([`cambium_index::externals::PathMap::exhaustive`]): a
//! repository whose whole content is one main-repo directory, because a
//! `local_path_override` or a `local_repository` put it there, cannot hold a
//! `.bzl` that lives outside that directory, so an uncovered path there is not
//! a `.bzl` it exports. A repository with no derivable mapping keeps rule (2).
//! And a repository dirtied by the `MODULE.bazel` diff — a version bump, a
//! changed tag — keeps rule (2) unconditionally, because *any* file in it may
//! have moved. Rule (1) is untouched in every case.
//!
//! The same question has a second static answer, for repositories with no
//! mapping at all. The index's input map errs wide: every main-repo label in a
//! repository rule's `.bzl` is an input of the repositories it defines, and
//! that includes labels the rule merely *writes* — envoy's `bazel/repo.bzl`
//! generates a BUILD file whose `genrule` lists `@envoy//changelogs` in
//! `tools`, so every commit that adds a changelog entry dirtied `@envoy_repo`,
//! whose `compiler.bzl` is in eleven hundred package closures. But a file only
//! a generated target names is a build-time dependency of that target; the
//! rule never reads it, so nothing it writes — no `.bzl` — can have moved. The
//! index records those patterns as target-only
//! ([`cambium_index::externals::RepoInputs::target_only`]) and the stage
//! applies rule (1) alone for them, which is exactly the edge Bazel would
//! follow. A path a rule both resolves and writes counts as resolved.
//!
//! # A repository built around a rule target, and the fixpoint that finds it
//!
//! `@envoy//changelogs` in that generated `genrule` is not a file: it is the
//! rule target `//changelogs:changelogs`, a `filegroup` over
//! `glob(["current/*.rst"])`. `@envoy_repo`'s targets genuinely depend on it,
//! so a new `changelogs/current/x.rst` does move them — but *which* files that
//! filegroup globs is not knowable from the label. The index used to guess
//! "everything under `changelogs/`", which on a real repository, applied to
//! visibility specs and `config_setting` keys as well, claimed 91% of the
//! packages (issue #8).
//!
//! The exact answer is in the graph, which has the filegroup's edges to its
//! sources. So the index records the label itself
//! ([`cambium_index::externals::Pattern::Label`]), the engine resolves it to a
//! node once per run, and after the reverse walk has produced the affected set
//! it asks which of those nodes the walk reached. Each repository named that
//! way is seeded by **rule (1) alone** — its leaf nodes into the direct set —
//! and the walk runs again, which can reach further such labels, so it is a
//! fixpoint, bounded by the number of repositories.
//!
//! Rule (2) is never right here, for the same reason target-only inputs exist:
//! a repository rule runs during loading, before anything is built, so it
//! cannot read a rule target's *output*. A rule target it names can only be a
//! dependency of what the rule *writes*, and nothing it wrote — no `.bzl` it
//! exports — can have moved.
//!
//! There is no third way. A repository cannot affect a package's rules without
//! appearing in that package's `load()` closure or in some rule's attributes —
//! except through toolchain resolution, which has no edge to follow and which
//! this stage therefore refuses to localise: any change to
//! `register_toolchains`, to `register_execution_platforms`, or to a module on
//! [`cambium_index::externals::TOOLCHAIN_MODULES`] stays global. And, under a
//! Bazel that autoloads, except through autoload: a bare `py_binary` is
//! `@rules_python`'s rule through a load no BUILD file writes, and the loading
//! layer drops the `PRECOMPUTED:autoload_symbols` singleton that records it,
//! so a move of a module on [`cambium_index::externals::AUTOLOAD_PROVIDERS`]
//! stays global too, under its own diagnostic. [`autoload_at`] decides which
//! Bazel autoloads: 8 at its default, a 7.4 whose `.bazelrc` sets
//! `--incompatible_autoload_externally`, and a version nothing states, which
//! counts as 8.
//!
//! # What stays global
//!
//! - A legacy `WORKSPACE` edit the statement diff cannot classify. The file is
//!   imperative — repositories are declared by arbitrary macro code, order
//!   matters, and `native.existing_rules()` makes a later statement's meaning
//!   depend on an earlier one — so [`cambium_index::workspace`] reads only the
//!   shapes whose effect is confined to one declaration and escalates for
//!   everything else, reordering included.
//! - A `MODULE.bazel.lock` whose JSON the lock parser does not recognise: an
//!   unknown key that changed, a format version it was not written against, a
//!   version that changed between the two sides, a registry-level file that
//!   moved. Everything else in that file is diffed structurally by
//!   [`cambium_index::lockfile`], which names the repositories a re-pinned
//!   registry or a re-evaluated extension moved. The file stays a default
//!   invalidator all the same, so that the blunt answer is still there when
//!   `externals.enabled` is off; `handled` exempts it when the stage read it.
//! - A `MODULE.bazel` statement the parser does not recognise, on either side
//!   of the diff.
//! - Toolchain registration, as above. Reported under the toolchain code
//!   (`CAMBI-W023`, the one a moved `toolchain()` rule raises) rather than
//!   the stage's own: the gap is the graph's, not the stage's.
//! - An autoload provider moving under a Bazel that autoloads, as above:
//!   `CAMBI-W024` rather than the stage's `CAMBI-W014`, because the stage
//!   accounted for the change exactly and the answer is still everything.
//! - A repository-rule input that no label names. See the soundness gap
//!   documented on [`cambium_index::externals`].

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};

use cambium_diff::Change;
use cambium_index::externals::{
    bazelrc_autoload_externally, diff_module, is_autoload_provider, scan_module, Autoload, Mode,
    ModuleChange, ModuleFacts, MODULE_SEGMENT_SUFFIX,
};
use cambium_index::label::{join_path, Label, Repo};
use cambium_index::lockfile::diff_lock;
use cambium_index::workspace::{diff_workspace, WorkspaceChange};
use cambium_index::IndexRef;

use crate::reason::ExternalsCause;

/// Workspace files whose own diff this stage interprets, rather than letting
/// them fire an invalidator rule.
pub const MODULE_FILE: &str = "MODULE.bazel";
pub const MODULE_LOCK: &str = "MODULE.bazel.lock";
pub const LEGACY_WORKSPACE_FILES: &[&str] = &["WORKSPACE", "WORKSPACE.bazel", "WORKSPACE.bzlmod"];

/// Reads a workspace file at one side of the diff.
///
/// A trait so the engine tests can drive the stage with two strings and no
/// git: the real implementation reads `git show <rev>:MODULE.bazel`.
pub trait WorkspaceText {
    fn text_at(&self, rev: &str, path: &str) -> Option<String>;
}

/// The `git show`-backed implementation.
pub struct GitText<'a>(pub &'a cambium_diff::Repo);

impl WorkspaceText for GitText<'_> {
    fn text_at(&self, rev: &str, path: &str) -> Option<String> {
        self.0
            .read_at(rev, path)
            .and_then(|b| String::from_utf8(b).ok())
    }
}

/// Two fixed texts, for tests.
pub struct FixedText(pub BTreeMap<(String, String), String>);

impl WorkspaceText for FixedText {
    fn text_at(&self, rev: &str, path: &str) -> Option<String> {
        self.0.get(&(rev.to_string(), path.to_string())).cloned()
    }
}

/// What the stage concluded.
#[derive(Clone, Debug, Default)]
pub struct Externals {
    /// Invalidate everything, for this reason. Its `Display` is the wording
    /// the run reports.
    pub cause: Option<ExternalsCause>,
    /// Which world Bazel reads the base tree in, when the stage had to
    /// decide. Only a diff that touches `MODULE.bazel` or its lock asks —
    /// an edit to either is inert under WORKSPACE mode — and deciding reads
    /// three files of the base tree, so a diff that touches neither never
    /// pays for it.
    pub mode: Option<Mode>,
    /// Apparent names of the repositories that moved.
    pub repos: BTreeSet<String>,
    /// The subset of [`Externals::repos`] whose *contents* may have moved in a
    /// way a `load()` would see, and for which the package closures are
    /// therefore re-queried. See "Why (2) does not always fire".
    pub load_repos: BTreeSet<String>,
    /// Workspace files the stage accounted for, and which must therefore not
    /// also fire an invalidator rule.
    pub handled: Vec<String>,
    /// `MODULE.bazel.lock` moved and the stage *read its diff*: the
    /// repositories it names are the ones whose resolved state moved. Only
    /// the lock-without-`MODULE.bazel` route sets this. When `MODULE.bazel`
    /// moved too, the lock is in [`Externals::handled`] because the module
    /// diff subsumes it for invalidator purposes, and nobody read the lock;
    /// a caller that needs the lock's own answer (`Run::registry_change`)
    /// must read this, not `handled`.
    pub lock_read: bool,
    /// How many repositories were named by a repository-rule input edit.
    pub by_input: usize,
    /// How many were named by the `MODULE.bazel` diff.
    pub by_module: usize,
    /// How many were named by a changed workspace-closure `.bzl`.
    pub by_bzl: usize,
    /// How many input-dirtied repositories were spared rule (2) — by the
    /// path mapping, or because the only thing naming the path is a target
    /// the repository rule writes: their label consumers are dirty, their
    /// loaders are not.
    pub label_only: usize,
    /// For each repository that moved, the changed files that moved it: an
    /// input path the repository is built from, a workspace-closure `.bzl`
    /// that defines it, `MODULE.bazel`, its lock, or `WORKSPACE`. Kept for the
    /// diagnostic a run prints when its dirty set overflows
    /// `max_dirty_packages`: the repository set alone is hundreds of names and
    /// says nothing about *why*, and a route that named repositories without
    /// saying which file did so left that line empty.
    pub dirtied_by: BTreeMap<String, Vec<String>>,
    /// The subset of [`Externals::repos`] dirtied only through a path that a
    /// target the repository rule *writes* names. Rule (1) alone applies to
    /// them, and nothing the rule wrote can have moved, its `BUILD` files
    /// included: a toolchain declared in one of these is where it was.
    pub target_only: BTreeSet<String>,
}

impl Externals {
    /// Gives up on localising the diff: everything is affected, for `cause`.
    pub fn escalate(&mut self, cause: ExternalsCause) {
        self.cause = Some(cause);
    }

    /// Takes the escalation, if there was one, leaving the stage clear.
    pub fn take_cause(&mut self) -> Option<ExternalsCause> {
        self.cause.take()
    }

    /// Record that `file` moved every repository in `repos`.
    pub fn name<'a>(&mut self, file: &str, repos: impl IntoIterator<Item = &'a String>) {
        for r in repos {
            let by = self.dirtied_by.entry(r.clone()).or_default();
            if !by.iter().any(|f| f == file) {
                by.push(file.to_string());
            }
        }
    }
}

/// Runs the stage over a diff.
///
/// `base` is the base index, which carries the input map; `texts` reads
/// `MODULE.bazel` at either revision. `mode` decides which world Bazel reads
/// the base tree in — [`mode_at`] for a real run — and is asked at most
/// once, and only when the diff touches `MODULE.bazel` or its lock.
pub fn analyse(
    base: IndexRef<'_>,
    changes: &[impl Borrow<Change>],
    base_rev: &str,
    head_rev: &str,
    texts: &dyn WorkspaceText,
    mode: impl FnOnce() -> Mode,
) -> Externals {
    let mut out = Externals::default();

    let mut module_changed = false;
    let mut lock_changed = false;
    let mut workspaces: Vec<&str> = Vec::new();
    let mut segments: Vec<&str> = Vec::new();
    for c in changes {
        let c = c.borrow();
        if LEGACY_WORKSPACE_FILES.contains(&c.path.as_str()) {
            workspaces.push(c.path.as_str());
            continue;
        }
        match c.path.as_str() {
            MODULE_FILE => module_changed = true,
            MODULE_LOCK => lock_changed = true,
            p if p.ends_with(MODULE_SEGMENT_SUFFIX) => segments.push(p),
            _ => {}
        }
    }
    // An `include()`d segment of `MODULE.bazel` (Bazel 7.2 and later) is
    // textually part of it, so a change there is a change to the module
    // file. Which files those are is only known from the root's include
    // list, read at either revision; the suffix Bazel requires of them is
    // the cheap test that decides whether to read the root at all. A file
    // with the suffix that nothing includes is an ordinary file. The two
    // sides are read once and kept, since the module diff below wants the
    // same pair.
    let mut facts: Option<(ModuleFacts, ModuleFacts)> = None;
    if !segments.is_empty() {
        let (b, h) =
            facts.get_or_insert_with(|| (facts_at(texts, base_rev), facts_at(texts, head_rev)));
        let included: Vec<&str> = segments
            .iter()
            .copied()
            .filter(|s| b.includes.iter().any(|i| i == s) || h.includes.iter().any(|i| i == s))
            .collect();
        if !included.is_empty() {
            tracing::debug!(
                segments = ?included,
                "a segment MODULE.bazel includes changed; diffing the module file with it"
            );
            module_changed = true;
            out.handled.extend(included.iter().map(|s| s.to_string()));
        }
    }
    // A tree Bazel reads in WORKSPACE mode has an inert `MODULE.bazel`: an
    // edit to it, or to its lock, moves nothing. A flip of the mode itself
    // is a bazelrc change, which the flags stage makes global.
    if module_changed || lock_changed {
        let mode = *out.mode.insert(mode());
        if !mode.bzlmod_live() {
            tracing::info!(
                "MODULE.bazel changed, but the base tree runs Bazel with bzlmod off: the edit is inert"
            );
            if module_changed {
                out.handled.push(MODULE_FILE.to_string());
            }
            if lock_changed {
                out.handled.push(MODULE_LOCK.to_string());
            }
            module_changed = false;
            lock_changed = false;
        }
    }
    // Whether the Bazel reading the tree autoloads native rules from provider
    // modules, asked only when a diff could name one.
    let autoload = if module_changed || lock_changed || !workspaces.is_empty() {
        autoload_at(base, texts, base_rev)
    } else {
        Autoload::Off
    };

    // A legacy `WORKSPACE`, diffed statement by statement. The file is
    // imperative, so most of what it can say is still global; what it *can*
    // say is which declaration moved. See [`cambium_index::workspace`].
    for path in workspaces {
        let (b, h) = (texts.text_at(base_rev, path), texts.text_at(head_rev, path));
        let (Some(b), Some(h)) = (b, h) else {
            out.escalate(ExternalsCause::WorkspaceAddedOrRemoved {
                path: path.to_string(),
            });
            return out;
        };
        match diff_workspace(&b, &h) {
            WorkspaceChange::Registration(why) => {
                out.escalate(ExternalsCause::WorkspaceRegistration {
                    path: path.to_string(),
                    why,
                });
                return out;
            }
            WorkspaceChange::Global(why) => {
                out.escalate(ExternalsCause::Workspace {
                    path: path.to_string(),
                    why,
                });
                return out;
            }
            WorkspaceChange::Moved { repos, bzl_paths } => {
                // A Bazel that autoloads autoloads from a `WORKSPACE`
                // repository under the same names, `com_google_protobuf` for
                // protobuf.
                if autoload == Autoload::On {
                    if let Some(r) = repos.iter().find(|r| is_autoload_provider(r)) {
                        out.escalate(ExternalsCause::AutoloadProvider {
                            module: r.clone(),
                            why: format!(
                                "`{path}` moved `{r}`, and the Bazel here autoloads native \
                                 rules from {r}: a BUILD file uses them through a load it never \
                                 writes, so no edge names their consumers"
                            ),
                        });
                        return out;
                    }
                }
                for p in &bzl_paths {
                    // The repositories a workspace-closure `.bzl` can define
                    // are exactly what the index recorded for a changed `.bzl`
                    // of its own; a file it cannot attribute is global there
                    // and here.
                    let named = base
                        .bzl_id(p)
                        .and_then(|id| repos_of_workspace_bzl(base, id));
                    let Some(named) = named else {
                        out.escalate(ExternalsCause::WorkspaceBzlUnattributed {
                            path: path.to_string(),
                            bzl: p.clone(),
                        });
                        return out;
                    };
                    out.by_bzl += named.len();
                    out.name(p, named.iter());
                    out.repos.extend(named.iter().cloned());
                    out.load_repos.extend(named);
                }
                out.by_module += repos.len();
                out.name(path, repos.iter());
                // A declaration that moved replaces its repository wholesale,
                // `.bzl` files included: rule (2) applies.
                out.load_repos.extend(repos.iter().cloned());
                out.repos.extend(repos);
                out.handled.push(path.to_string());
            }
        }
    }

    if module_changed {
        let (base_facts, head_facts) =
            facts.unwrap_or_else(|| (facts_at(texts, base_rev), facts_at(texts, head_rev)));
        match diff_module(&base_facts, &head_facts, autoload) {
            ModuleChange::Registration(why) => {
                out.escalate(ExternalsCause::ModuleRegistration { why });
                return out;
            }
            ModuleChange::Global(why) => {
                out.escalate(ExternalsCause::Module { why });
                return out;
            }
            ModuleChange::Autoload { module, why } => {
                out.escalate(ExternalsCause::AutoloadProvider { module, why });
                return out;
            }
            ModuleChange::Repos(repos) => {
                out.by_module = repos.len();
                out.name(MODULE_FILE, repos.iter());
                // A version bump replaces the repository wholesale, so every
                // file in it, `.bzl` included, may have moved.
                out.load_repos.extend(repos.iter().cloned());
                out.repos.extend(repos);
                out.handled.push(MODULE_FILE.to_string());
                // The lockfile is a function of the module file plus the
                // registry; when the module file moved, the lock moving with
                // it says nothing further.
                if lock_changed {
                    out.handled.push(MODULE_LOCK.to_string());
                }
            }
        }
    }
    // A lockfile that moved *without* `MODULE.bazel` is the resolved state
    // moving under unchanged declarations: a re-pinned registry, a
    // re-evaluated extension, a lock a newer Bazel rewrote. Its JSON says
    // which repositories that moved; anything unrecognised in it escalates.
    if lock_changed && !module_changed {
        let facts = facts_at(texts, head_rev);
        let base_lock = texts.text_at(base_rev, MODULE_LOCK);
        let head_lock = texts.text_at(head_rev, MODULE_LOCK);
        let change = match (base_lock, head_lock) {
            (Some(b), Some(h)) => diff_lock(&b, &h, &facts, autoload),
            // One side has no lockfile at all: it was added or deleted, and
            // every repository in it is new or gone.
            _ => {
                out.escalate(ExternalsCause::LockAddedOrRemoved);
                return out;
            }
        };
        match change {
            // A lockfile carries no registration; the second arm is for the
            // enum's sake.
            ModuleChange::Global(why) | ModuleChange::Registration(why) => {
                out.escalate(ExternalsCause::Lock { why });
                return out;
            }
            ModuleChange::Autoload { module, why } => {
                out.escalate(ExternalsCause::AutoloadProvider { module, why });
                return out;
            }
            ModuleChange::Repos(repos) => {
                out.by_module += repos.len();
                out.name(MODULE_LOCK, repos.iter());
                // The repository's *contents* moved, so a `.bzl` inside it may
                // have moved with them: rule (2) applies, as it does for a
                // `MODULE.bazel` version bump.
                out.load_repos.extend(repos.iter().cloned());
                out.repos.extend(repos);
                out.handled.push(MODULE_LOCK.to_string());
                out.lock_read = true;
            }
        }
    }

    // Repository-rule inputs: main-repo files an external repository is built
    // from. Keyed by repo id, because deciding whether rule (2) applies needs
    // that repository's path mapping and the paths that dirtied it. A path
    // that only a *target* the rule writes names (its generated BUILD file
    // lists `@envoy//changelogs` in `tools`) is kept apart: it moves the
    // repository's targets and nothing the rule itself reads.
    let mut by_input: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
    let mut by_target: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
    for c in changes {
        let c = c.borrow();
        for (r, feeds_rule) in base.repos_for_path_kinds(&c.path) {
            let map = if feeds_rule {
                &mut by_input
            } else {
                &mut by_target
            };
            map.entry(r).or_default().push(c.path.as_str());
        }
    }
    out.by_input = by_input
        .keys()
        .chain(by_target.keys())
        .collect::<BTreeSet<_>>()
        .len();
    for (repo, paths) in by_input {
        dirty_by_input(base, &mut out, repo, &paths);
    }
    for (repo, paths) in by_target {
        let name = base.repo_name(repo).to_string();
        for p in &paths {
            out.name(p, std::iter::once(&name));
        }
        if out.repos.insert(name.clone()) {
            out.label_only += 1;
            out.target_only.insert(name.clone());
            tracing::debug!(
                repo = %name,
                ?paths,
                "a target the repository rule writes names this path; no .bzl in the repository can have moved"
            );
        }
    }

    out
}

/// A dirty repository that a `register_toolchains` /
/// `register_execution_platforms` pattern at head offers rules from, and the
/// cause to escalate with.
///
/// The rule-class check reads its rows off `//...`, and a `toolchain()` or
/// `platform()` declared in an external repository is not among them: a
/// `local_path_override` directory, a `local_repository`, a repository built
/// from a main-repo file, a version bump. Every route that dirties such a
/// repository localises to its label consumers and loaders, and toolchain
/// resolution reaches neither, so a registered repository moving is the
/// declaration moving, seen from outside the graph. A repository dirtied only
/// through a target the rule writes is spared: nothing the rule wrote, its
/// `BUILD` files included, can have moved.
///
/// The patterns are read at head, from `MODULE.bazel` (with its included
/// segments) and from whichever legacy `WORKSPACE` file the tree has, and
/// only when there is a dirty repository to hold them against. The root
/// module's own name is the main repository and names nothing here.
///
/// Both worlds' files are read without asking which one Bazel is in. Which
/// is live depends on the Bazel version as well as the flag (Bazel 7 with
/// bzlmod on still reads `WORKSPACE`, unless a `WORKSPACE.bzlmod` stands in
/// for it), and a registration in a file Bazel no longer reads can only make
/// this answer wider. A pattern in a `.bzl` a macro registers from is
/// invisible here, as it is to the statement diff: the macro moving is what
/// escalates there.
pub fn registered_repository_moved(
    ext: &Externals,
    texts: &dyn WorkspaceText,
    head_rev: &str,
) -> Option<ExternalsCause> {
    if ext.cause.is_some() {
        return None;
    }
    let dirty: Vec<&str> = ext
        .repos
        .iter()
        .filter(|r| !ext.target_only.contains(*r))
        .map(String::as_str)
        .collect();
    if dirty.is_empty() {
        return None;
    }
    let find = |registered: &BTreeSet<String>| -> Option<String> {
        dirty
            .iter()
            .copied()
            .find(|r| registered.contains(*r))
            .map(str::to_string)
    };
    // `MODULE.bazel` first: in the world Bazel reads today it is the file
    // that registers, and the one a reader of the diagnostic will open. A
    // tree with no module file at all skips it and pays nothing.
    if texts.text_at(head_rev, MODULE_FILE).is_some() {
        let facts = facts_at(texts, head_rev);
        let mut repos = facts.registration_repos;
        repos.remove(&facts.module_name);
        if let Some(repo) = find(&repos) {
            return Some(ExternalsCause::RegisteredRepository {
                repo,
                file: MODULE_FILE.to_string(),
            });
        }
    }
    for path in LEGACY_WORKSPACE_FILES {
        let Some(t) = texts.text_at(head_rev, path) else {
            continue;
        };
        if let Some(repo) = find(&cambium_index::workspace::registration_repos(&t)) {
            return Some(ExternalsCause::RegisteredRepository {
                repo,
                file: path.to_string(),
            });
        }
    }
    None
}

/// Marks repository `repo` dirty because main-repo `paths` it is built from
/// moved: rule (1) always, and rule (2) only if one of those paths, translated
/// into the repository, could be a `.bzl` a main-repo package loads from there.
///
/// The one place an *input path* dirties a repository. The routing of a change
/// inside a nested repository (`crate::run`) comes here too, so that a
/// `third_party/xla` edit is seeded exactly the way a `local_path_override`
/// directory's edit already was.
pub fn dirty_by_input(base: IndexRef<'_>, out: &mut Externals, repo: u32, paths: &[&str]) {
    let name = base.repo_name(repo).to_string();
    for p in paths {
        out.name(p, std::iter::once(&name));
    }
    // A path the rule reads can move anything it writes.
    out.target_only.remove(&name);
    if out.load_repos.contains(&name) || input_can_be_a_loaded_bzl(base, repo, paths) {
        out.load_repos.insert(name.clone());
    } else {
        out.label_only += 1;
        tracing::debug!(
            repo = %name,
            ?paths,
            "an input-path edit cannot be one of the .bzl files loaded from this repository"
        );
    }
    out.repos.insert(name);
}

/// The repository whose whole content is the main-repo directory `dir`, if the
/// index derived such a mapping for one. `dir` has no trailing slash.
///
/// This is how a nested repository's directory becomes a repository name: the
/// exhaustive path map a `local_path_override`, a `local_repository`, or the
/// `//external:*` scan recorded is exactly the claim "this repository *is*
/// that directory", read back the other way round.
pub fn repo_rooted_at(base: IndexRef<'_>, dir: &str) -> Option<u32> {
    let want = format!("{dir}/");
    (0..base.num_repos()).find(|&r| {
        base.repo_path_maps(r)
            .iter()
            .any(|m| m.exhaustive && m.from == want)
    })
}

/// Whether any of `paths`, translated into repository `repo`, could be one of
/// the `.bzl` files a main-repo load closure reaches into it for.
///
/// `true` — rule (2) applies — whenever the question cannot be answered: no
/// derivable mapping, no static load closure to compare against, or a changed
/// path the mapping does not cover.
fn input_can_be_a_loaded_bzl(base: IndexRef<'_>, repo: u32, paths: &[&str]) -> bool {
    if !base.bzl_closure {
        return true;
    }
    let maps = base.repo_path_maps(repo);
    if maps.is_empty() {
        return true;
    }
    let apparent = base.repo_name(repo);
    let canonical = base.repo_canonical.get(repo);
    let loaded: BTreeSet<String> = (0..base.bzl_ext_labels.len() as u32)
        .filter_map(|e| repo_relative_path(base.bzl_ext_labels.get(e), apparent, canonical))
        .collect();
    // A repository whose mapping is exhaustive — its whole content is one
    // main-repo directory — cannot hold a `.bzl` that lives outside it, so a
    // changed path the mapping does not cover is not a `.bzl` it exports. A
    // partial mapping says nothing about what it does not cover, and an
    // uncovered path there keeps rule (2).
    let exhaustive = maps.iter().any(|m| m.exhaustive);
    paths.iter().any(|p| {
        let mut covered = false;
        for m in &maps {
            let Some(inside) = m.translate(p) else {
                continue;
            };
            covered = true;
            if loaded.contains(inside.as_str()) {
                return true;
            }
        }
        !covered && !exhaustive
    })
}

/// `@@envoy_api+//bazel:api_build_system.bzl` -> `bazel/api_build_system.bzl`,
/// but only for labels in the repository named.
fn repo_relative_path(label: &str, apparent: &str, canonical: &str) -> Option<String> {
    let l = Label::parse(label)?;
    // The repository is matched by the shape of its token against the two
    // names the index recorded, not resolved through a mapping: these labels
    // are the ones Bazel wrote into query output, and which spelling of a
    // repository they carry depends on the Bazel that ran. See
    // [`repo_token_matches`].
    let token = match l.repo() {
        Repo::Main => return None,
        Repo::Apparent(t) | Repo::Canonical(t) => t,
    };
    if !repo_token_matches(token, apparent, canonical) {
        return None;
    }
    // `pkg:sub/file.bzl` is `pkg/sub/file.bzl`, and the root package's
    // `:file.bzl` is just `file.bzl`. A label with no `:` names no file.
    Some(join_path(l.package(), l.explicit_name()?))
}

/// Whether the Bazel reading the base tree autoloads native rules from the
/// provider modules.
///
/// The version decides ([`Autoload::from_major`]): Bazel 8 autoloads at its
/// default, 7 and 9 do not, and a version nothing states is read as 8, the
/// side that over-reports. Where that Bazel reads the flag
/// ([`Autoload::reads_flag`]: 7, 8, or an unknown version, but not 9, where
/// autoloads are disabled and the option is a no-op),
/// `--incompatible_autoload_externally` on an unconditional command line of
/// `.bazelrc` at `rev` says otherwise instead ([`Autoload::from_flag`]):
/// empty is off on an 8 tree, a value naming a provider is on for a 7.4 tree
/// opting in early.
///
/// The version is `.bazelversion` at `rev` where that file states one, and
/// the index header otherwise: a tree with no `.bazelversion`, one whose
/// content is a channel name (`latest`) rather than a number, or a run
/// reading a synthetic index. The tree's own file is asked first because the
/// header is written once, at the cold build ([`crate::engine`]), and copied
/// forward verbatim on every update ([`crate::run`]); a lineage cold-built
/// under Bazel 7 carries "bazel 7.4.1" in its header for as long as it
/// lives, including after the tree has moved to Bazel 8, and deciding from
/// it there would localise a provider bump the running Bazel autoloads.
/// Where the header is the only source it is `bazel --version` of the Bazel
/// that built the index (bazelisk's resolution of `.bazelversion`, where
/// that is how Bazel runs).
///
/// The rc files are read as Bazel reads them: the root `.bazelrc` with its
/// `import` / `try-import` targets inlined where they appear, so a tree that
/// opts in from an imported file is seen ([`autoload_flag_at`]).
pub fn autoload_at(base: IndexRef<'_>, texts: &dyn WorkspaceText, rev: &str) -> Autoload {
    let header = base.bazel_version.trim_start_matches("bazel").trim();
    let (major, source) = match texts
        .text_at(rev, ".bazelversion")
        .and_then(|t| crate::probe::leading_major(&t))
    {
        Some(m) => (Some(m), ".bazelversion"),
        None => (crate::probe::leading_major(header), "index header"),
    };
    let flag = Autoload::reads_flag(major)
        .then(|| autoload_flag_at(texts, rev))
        .flatten();
    let autoload = match &flag {
        Some(value) => Autoload::from_flag(value),
        None => Autoload::from_major(major),
    };
    tracing::debug!(
        ?major,
        source,
        flag = ?flag,
        ?autoload,
        "whether the Bazel reading the tree autoloads native rules"
    );
    autoload
}

/// The `--incompatible_autoload_externally` the rc files Bazel reads for this
/// workspace state at `rev`, or `None` when none of them states it.
///
/// `import` and `try-import` are followed in place, as Bazel reads them: the
/// imported file's lines take effect where the import line sits, so the last
/// unconditional statement anywhere in the closure wins. A tree opting into
/// autoload early on Bazel 7.4 routinely keeps the flag in a shared file
/// (`tools/common.bazelrc`) that the root `.bazelrc` imports, and reading
/// the root file alone would call that tree "not autoloading" and localise a
/// provider bump the running Bazel autoloads from.
///
/// Targets are `%workspace%`-relative or repository-relative, as in
/// [`crate::bazelrc::reachable`]; an absolute path is outside the tree as
/// `texts` reads it and is not followed, and neither is a file that does not
/// exist at `rev` (which is what `try-import` is for). A cycle stops at the
/// file already read.
fn autoload_flag_at(texts: &dyn WorkspaceText, rev: &str) -> Option<String> {
    fn walk(
        texts: &dyn WorkspaceText,
        rev: &str,
        path: &str,
        seen: &mut BTreeSet<String>,
        out: &mut Option<String>,
    ) {
        if !seen.insert(path.to_string()) {
            return;
        }
        let Some(text) = texts.text_at(rev, path) else {
            return;
        };
        for line in text.lines() {
            let mut tokens = line.split_whitespace();
            let Some(command) = tokens.next() else {
                continue;
            };
            if command == "import" || command == "try-import" {
                if let Some(target) = tokens.next().and_then(rc_relative) {
                    walk(texts, rev, &target, seen, out);
                }
                continue;
            }
            if let Some(value) = bazelrc_autoload_externally(line) {
                *out = Some(value);
            }
        }
    }
    let mut out = None;
    walk(texts, rev, ".bazelrc", &mut BTreeSet::new(), &mut out);
    out
}

/// An `import` target as a repository-relative path, or `None` when it names
/// a file the tree does not hold.
fn rc_relative(target: &str) -> Option<String> {
    let rel = target.strip_prefix("%workspace%/").unwrap_or(target);
    if rel.starts_with('/') {
        return None;
    }
    let rel = rel.trim_start_matches("./");
    (!rel.is_empty()).then(|| rel.to_string())
}

/// [`Mode::detect`] for the base side of the diff: the tree as `texts` reads
/// it at `rev`, and the mapping the base index carries. What a run hands
/// [`analyse`], to be decided once and only if the diff asks.
pub fn mode_at(base: IndexRef<'_>, texts: &dyn WorkspaceText, rev: &str) -> Mode {
    struct At<'a>(&'a dyn WorkspaceText, &'a str);
    impl cambium_index::FileText for At<'_> {
        fn text(&self, path: &str) -> Option<String> {
            self.0.text_at(self.1, path)
        }
    }
    let m = cambium_index::RepoMapping::from_canonical(
        (0..base.num_repos()).map(|r| (base.repo_name(r), base.repo_canonical.get(r))),
    );
    Mode::detect(&At(texts, rev), &m)
}

/// `MODULE.bazel` at `rev`, with every segment it `include()`s merged in.
///
/// A segment is read at the same revision and absorbed as if its text stood
/// at the `include()`; a segment's own includes are followed too, each path
/// once. One that cannot be read is recorded as unparseable, for the same
/// reason an unreadable root is: an empty side would diff as everything in
/// it having been added or removed.
fn facts_at(texts: &dyn WorkspaceText, rev: &str) -> ModuleFacts {
    let Some(t) = texts.text_at(rev, MODULE_FILE) else {
        // A revision whose `MODULE.bazel` cannot be read is not the same as
        // one with no dependencies: an empty side would diff as "every
        // `bazel_dep` was added". The parser records that as unparseable so
        // the caller escalates.
        return ModuleFacts {
            unparsed: vec![format!("MODULE.bazel is unreadable at {rev}")],
            ..Default::default()
        };
    };
    let mut facts = scan_module(&t);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut next = 0;
    while next < facts.includes.len() {
        let path = facts.includes[next].clone();
        next += 1;
        if !seen.insert(path.clone()) {
            continue;
        }
        match texts.text_at(rev, &path) {
            Some(t) => facts.absorb(scan_module(&t)),
            None => facts.unparsed.push(format!(
                "`{path}`, which MODULE.bazel includes, is unreadable at {rev}"
            )),
        }
    }
    facts
}

/// The repositories a changed workspace-closure `.bzl` can define, or `None`
/// when the index cannot attribute it and the change must go global.
pub fn repos_of_workspace_bzl(base: IndexRef<'_>, bzl_id: u32) -> Option<Vec<String>> {
    let row = base.bzl_repos_of(bzl_id);
    if row.is_empty() {
        return None;
    }
    Some(row.iter().map(|&r| base.repo_name(r).to_string()).collect())
}

// ------------------------------------------------- repositories to targets

/// The repository part of a label, as written: `@@rules_cc+//x:y` ->
/// `rules_cc+`. `None` for the main repository however it is spelled, since
/// the callers match the token by shape ([`repo_token_matches`]) rather than
/// resolving it.
fn label_repo(label: &str) -> Option<&str> {
    match Label::parse(label)?.repo() {
        Repo::Apparent(r) | Repo::Canonical(r) if !r.is_empty() => Some(r),
        _ => None,
    }
}

/// Whether a repository token from a label denotes the repository whose
/// apparent name is `apparent`.
///
/// Bazel writes canonical names in query output, and their shape is a moving
/// target: `rules_cc+` under 8 and 9, `rules_cc~` under 7, and for a repository
/// a module extension generated, `rules_python++python+python_3_12` — where the
/// apparent name is the *last* segment, not the first. All three shapes are
/// matched, plus an exact hit on the canonical name the index recorded.
/// Matching too widely costs precision, never soundness.
pub fn repo_token_matches(token: &str, apparent: &str, canonical: &str) -> bool {
    if apparent.is_empty() {
        return false;
    }
    if token == apparent || (!canonical.is_empty() && token == canonical) {
        return true;
    }
    if let Some(rest) = token.strip_prefix(apparent) {
        if rest.starts_with('+') || rest.starts_with('~') {
            return true;
        }
    }
    token
        .rsplit(['+', '~'])
        .next()
        .is_some_and(|last| last == apparent)
}

/// Everything a set of dirty repositories reaches in the base index.
#[derive(Default, Debug)]
pub struct RepoReach {
    /// External leaf nodes to seed the direct set with.
    pub nodes: Vec<u32>,
    /// Packages whose `.bzl` closure loads from one of the repositories.
    pub packages: BTreeSet<String>,
    /// How many of [`RepoReach::packages`] each repository accounted for.
    /// Packages counted once per repository that reaches them, so the values
    /// sum to at least `packages.len()`. Kept so that a run which overflows
    /// `max_dirty_packages` can say which repositories cost it the most.
    pub packages_by_repo: BTreeMap<String, usize>,
}

/// Resolves dirty repositories to graph work.
///
/// `repos` are all the dirty repositories, which is what rule (1) needs.
/// `load_repos` is the subset whose `.bzl` files may have moved, which is what
/// rule (2) needs; see "Why (2) does not always fire".
pub fn reach(
    base: IndexRef<'_>,
    repos: &BTreeSet<String>,
    load_repos: &BTreeSet<String>,
) -> RepoReach {
    let mut out = RepoReach::default();
    if repos.is_empty() {
        return out;
    }
    // Apparent name -> canonical name as the index recorded it, so a repo the
    // index never heard of still matches by shape.
    let mut canon: BTreeMap<&str, &str> = BTreeMap::new();
    for r in 0..base.num_repos() {
        canon.insert(base.repo_name(r), base.repo_canonical.get(r));
    }
    let matching = |set: &BTreeSet<String>, token: &str| {
        set.iter()
            .any(|a| repo_token_matches(token, a, canon.get(a.as_str()).copied().unwrap_or("")))
    };
    let matches = |token: &str| matching(repos, token);

    // (0) the repository's own packages, when this index holds them.
    //
    // Phase 5 stage 2 puts an in-tree external repository's packages in the
    // index under `@repo//pkg` keys. A repository that moved re-queries those
    // the same way a main-repo package change re-queries its own package:
    // without this, `@tsl` would be dirty, its consumers would be re-queried,
    // and its own rules would stand at whatever the index last recorded.
    //
    // A repository whose packages are *not* in the index has none of these
    // keys and this loop finds nothing, which is the behaviour before the
    // universe was widened.
    for p in 0..base.num_packages() {
        if base.package_tombstoned(p) {
            continue;
        }
        let name = base.package_name(p);
        let Some(token) = cambium_index::label::key_repo(name) else {
            continue;
        };
        if matches(token) {
            out.packages.insert(name.to_string());
            *out.packages_by_repo.entry(token.to_string()).or_default() += 1;
        }
    }

    // (1) labels. Every external label starts with `@`, and `label_sorted` is
    // in label order, so they are one contiguous run at the end of it: found
    // by binary search rather than by touching every label in the index.
    let start = base
        .label_sorted
        .partition_point(|&id| base.label(id) < "@");
    for &id in &base.label_sorted[start..] {
        let label = base.label(id);
        if !label.starts_with('@') {
            break;
        }
        if base.is_tombstone(id) {
            continue;
        }
        if !label_repo(label).is_some_and(matches) {
            continue;
        }
        // Phase 5 stage 3. A label whose package rule (0) re-queries is not
        // seeded here: its rule comes back from Bazel and the splice decides
        // whether it moved, which is exactly how a main-repo package is
        // answered. Seeding it as well is the "declaration moved, every leaf
        // dirty" answer, and on envoy it is 65226 extra labels across 40
        // change sets.
        //
        // The test is the package, not the repository, so a label of a dirty
        // repository whose package this index does *not* hold -- one that
        // failed to load, or one outside the widened universe -- is still
        // seeded and still answered wide. The changed files under the
        // repository's own directory reach these rules through their real
        // edges, by the labels `run::external_labels` resolves them to.
        if cambium_index::label::Label::parse(label)
            .map(|l| cambium_index::label::external_package_key(l.repo_name(), l.package()))
            .is_some_and(|k| out.packages.contains(&k))
        {
            continue;
        }
        out.nodes.push(id);
    }
    out.nodes.sort_unstable();

    // (2) loads. The static closure recorded the external labels every `.bzl`
    // load walk stopped at; a package whose closure contains one of them can
    // have its rules change when that repository does. The labels a BUILD
    // file loads directly sit on the package itself (`pkg_ext`), since no
    // `.bzl` row stands between it and the repository (issue #4).
    if base.bzl_closure && !load_repos.is_empty() {
        // Each hot external-label entry is charged to the repository whose
        // name it carries, so the per-repository counts fall out of the same
        // pass rather than costing a walk per repository.
        let mut hot: Vec<u32> = Vec::new();
        let mut charged: BTreeMap<u32, &str> = BTreeMap::new();
        for e in 0..base.bzl_ext_labels.len() as u32 {
            let Some(token) = label_repo(base.bzl_ext_labels.get(e)) else {
                continue;
            };
            let Some(owner) = load_repos.iter().find(|a| {
                repo_token_matches(token, a, canon.get(a.as_str()).copied().unwrap_or(""))
            }) else {
                continue;
            };
            hot.push(e);
            charged.insert(e, owner.as_str());
        }
        if !hot.is_empty() {
            // Distinct packages per repository: a package whose closure
            // reaches the repository through several `.bzl` rows, or whose
            // rows carry several of its labels, is one package. Counting the
            // (package, load) pairs instead once printed a contributor at
            // nine times the repository's package count.
            let mut by_repo: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
            let mut charge = |out: &mut RepoReach, row: &[u32], pkg: &str| {
                let mut hit = false;
                for e in row {
                    if let Some(owner) = charged.get(e) {
                        hit = true;
                        by_repo.entry(owner).or_default().insert(pkg.to_string());
                    }
                }
                if hit {
                    out.packages.insert(pkg.to_string());
                }
            };
            for id in 0..base.num_bzl() {
                if !base.bzl_ext.row(id).iter().any(|e| hot.contains(e)) {
                    continue;
                }
                let row: Vec<u32> = base.bzl_ext.row(id).to_vec();
                for &p in base.bzl_consumers(id) {
                    if !base.package_tombstoned(p) {
                        charge(&mut out, &row, base.package_name(p));
                    }
                }
            }
            for p in 0..base.pkg_ext.len() as u32 {
                if base.package_tombstoned(p) {
                    continue;
                }
                let row: Vec<u32> = base.pkg_ext.row(p).to_vec();
                if row.iter().any(|e| hot.contains(e)) {
                    charge(&mut out, &row, base.package_name(p));
                }
            }
            for (repo, pkgs) in by_repo {
                // Added, not assigned: (0) may already have charged this
                // repository for its own packages.
                *out.packages_by_repo.entry(repo.to_string()).or_default() += pkgs.len();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_every_canonical_spelling() {
        assert!(repo_token_matches("rules_cc", "rules_cc", ""));
        assert!(repo_token_matches("rules_cc+", "rules_cc", "rules_cc+"));
        assert!(repo_token_matches("rules_cc~", "rules_cc", ""));
        assert!(repo_token_matches(
            "rules_python++python+python_3_12",
            "python_3_12",
            ""
        ));
        assert!(!repo_token_matches("rules_ccc", "rules_cc", ""));
        assert!(!repo_token_matches("rules_cc", "", ""));
    }

    #[test]
    fn a_load_label_becomes_a_repository_relative_path() {
        assert_eq!(
            repo_relative_path("@@envoy_api+//bazel:api_build_system.bzl", "envoy_api", ""),
            Some("bazel/api_build_system.bzl".to_string())
        );
        assert_eq!(
            repo_relative_path("@mod//:defs.bzl", "mod", "mod+"),
            Some("defs.bzl".to_string())
        );
        assert_eq!(
            repo_relative_path("@mod//a:b/c.bzl", "mod", ""),
            Some("a/b/c.bzl".to_string())
        );
        // Another repository, and a main-repo label.
        assert_eq!(repo_relative_path("@@other+//a:b.bzl", "mod", ""), None);
        assert_eq!(repo_relative_path("//a:b.bzl", "mod", ""), None);
    }

    /// `.bazelversion` wins over the index header (which is written once and
    /// goes stale when the tree moves), and the `.bazelrc` flag over both
    /// where the Bazel reads it; only Bazel 8 autoloads at its default, and
    /// nothing at all is read as 8.
    #[test]
    fn the_version_and_the_bazelrc_flag_decide_autoload_and_unknown_means_on() {
        use cambium_index::IndexBuilder;
        let tree = |v: Option<&str>, rc: Option<&str>| {
            let mut m = BTreeMap::new();
            if let Some(v) = v {
                m.insert(
                    ("base".to_string(), ".bazelversion".to_string()),
                    v.to_string(),
                );
            }
            if let Some(rc) = rc {
                m.insert(("base".to_string(), ".bazelrc".to_string()), rc.to_string());
            }
            FixedText(m)
        };
        let at = |header: &str, v: Option<&str>, rc: Option<&str>| {
            let mut b = IndexBuilder::new(cambium_index::new_lineage());
            b.set_bazel_version(header.to_string());
            let d = b.finish("c0");
            autoload_at(d.view(), &tree(v, rc), "base")
        };
        assert_eq!(at("bazel 7.4.1", None, None), Autoload::Off);
        assert_eq!(at("bazel 8.0.0rc2", None, None), Autoload::On);
        assert_eq!(at("bazel 9.2.0", None, None), Autoload::Off);
        assert_eq!(
            at("bazel 8.4.2", Some("7.4.1"), None),
            Autoload::Off,
            "the tree's own `.bazelversion` wins"
        );
        // The case the precedence exists for: a lineage cold-built under 7
        // keeps "bazel 7.4.1" in its header for ever, and the tree has since
        // moved to 8, which autoloads.
        assert_eq!(
            at("bazel 7.4.1", Some("8.4.2"), None),
            Autoload::On,
            "a header that went stale does not decide"
        );
        assert_eq!(at("", Some("7.6.1"), None), Autoload::Off);
        assert_eq!(at("synth", Some("8.4.2"), None), Autoload::On);
        assert_eq!(at("", Some("9.1.1"), None), Autoload::Off);
        assert_eq!(at("", Some("latest"), None), Autoload::On);
        assert_eq!(at("", None, None), Autoload::On);
        // Where Bazel reads the flag it decides either way: empty is off on
        // 8, a value naming a provider is on for a 7.4 tree opting in early,
        // and a value that only removes symbols autoloads nothing.
        let rc =
            |v: &str| format!("build --jobs=8\ncommon --incompatible_autoload_externally={v}\n");
        assert_eq!(at("bazel 8.4.2", None, Some(&rc(""))), Autoload::Off);
        assert_eq!(
            at(
                "bazel 7.4.1",
                None,
                Some(&rc("+@rules_python,@com_google_protobuf"))
            ),
            Autoload::On
        );
        assert_eq!(
            at("bazel 8.4.2", None, Some(&rc("-@rules_python"))),
            Autoload::Off
        );
        // Bazel 9 does not read it: autoloads are disabled there and the
        // option is a no-op, so a line left over from an 8 tree changes
        // nothing.
        assert_eq!(
            at("bazel 9.2.0", None, Some(&rc("@rules_shell"))),
            Autoload::Off
        );
        assert_eq!(
            at("", Some("9.0.0"), Some(&rc("+@rules_python"))),
            Autoload::Off
        );
        // A `--config` line says nothing, and the version stands.
        assert_eq!(
            at(
                "bazel 8.4.2",
                None,
                Some("build:migrate --incompatible_autoload_externally=\n")
            ),
            Autoload::On
        );
    }

    /// The flag counts wherever the rc closure states it, which on a tree
    /// that opted into autoload early is routinely an imported file.
    #[test]
    fn the_autoload_flag_is_read_through_import_lines() {
        use cambium_index::IndexBuilder;
        let at = |header: &str, files: &[(&str, &str)]| {
            let mut m = BTreeMap::new();
            for (path, text) in files {
                m.insert(("base".to_string(), path.to_string()), text.to_string());
            }
            let mut b = IndexBuilder::new(cambium_index::new_lineage());
            b.set_bazel_version(header.to_string());
            let d = b.finish("c0");
            autoload_at(d.view(), &FixedText(m), "base")
        };
        // A 7.4 tree whose only autoload statement sits in an imported file
        // autoloads, and reading the root file alone would say it does not.
        assert_eq!(
            at(
                "bazel 7.4.1",
                &[
                    (".bazelversion", "7.4.1\n"),
                    (".bazelrc", "import %workspace%/tools/common.bazelrc\n"),
                    (
                        "tools/common.bazelrc",
                        "common --incompatible_autoload_externally=+@rules_python,@com_google_protobuf\n"
                    ),
                ]
            ),
            Autoload::On
        );
        // `try-import`, a repository-relative target, and an 8 tree turning
        // autoload off from an imported file.
        assert_eq!(
            at(
                "bazel 8.4.2",
                &[
                    (".bazelversion", "8.4.2\n"),
                    (".bazelrc", "try-import tools/flags.bazelrc\n"),
                    (
                        "tools/flags.bazelrc",
                        "common --incompatible_autoload_externally=\n"
                    ),
                ]
            ),
            Autoload::Off
        );
        // The import takes effect where it sits: a line after it wins.
        assert_eq!(
            at(
                "bazel 8.4.2",
                &[
                    (".bazelversion", "8.4.2\n"),
                    (
                        ".bazelrc",
                        "import %workspace%/tools/off.bazelrc\ncommon --incompatible_autoload_externally=@rules_shell\n"
                    ),
                    ("tools/off.bazelrc", "common --incompatible_autoload_externally=\n"),
                ]
            ),
            Autoload::On
        );
        // An import nothing can read (a `try-import` of a file that is not
        // there, an absolute path outside the tree) says nothing, and a cycle
        // terminates.
        assert_eq!(
            at(
                "bazel 7.4.1",
                &[
                    (".bazelversion", "7.4.1\n"),
                    (
                        ".bazelrc",
                        "try-import %workspace%/missing.bazelrc\nimport /etc/bazel.rc\nimport %workspace%/a.bazelrc\n"
                    ),
                    ("a.bazelrc", "import %workspace%/.bazelrc\n"),
                ]
            ),
            Autoload::Off
        );
    }

    #[test]
    fn reads_the_repository_out_of_a_label() {
        assert_eq!(label_repo("@@rules_cc+//x:y"), Some("rules_cc+"));
        assert_eq!(label_repo("@r//x:y"), Some("r"));
        assert_eq!(label_repo("//x:y"), None);
        assert_eq!(label_repo("@//x:y"), None);
    }
}
