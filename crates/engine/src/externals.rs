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
//! There is no third way. A repository cannot affect a package's rules without
//! appearing in that package's `load()` closure or in some rule's attributes —
//! except through toolchain resolution, which has no edge to follow and which
//! this stage therefore refuses to localise: any change to
//! `register_toolchains`, to `register_execution_platforms`, or to a module on
//! [`cambium_index::externals::TOOLCHAIN_MODULES`] stays global.
//!
//! # What stays global
//!
//! - Legacy `WORKSPACE` / `WORKSPACE.bazel` / `WORKSPACE.bzlmod` edits.
//!   `WORKSPACE` is imperative: repositories are declared by arbitrary macro
//!   code, order matters, and `native.existing_rules()` makes a later
//!   statement's meaning depend on an earlier one. A static diff of it would
//!   be a guess.
//! - A `MODULE.bazel.lock` that moves without `MODULE.bazel` moving. The lock
//!   records resolved registry contents; a change with no module edit means
//!   the *registry* moved under the same declarations, which can re-point any
//!   repository. The stage leaves that file to the invalidator rules, where
//!   it is a default entry, rather than escalating over the top of a
//!   `cambium.toml` that has removed it.
//! - A `MODULE.bazel` statement the parser does not recognise, on either side
//!   of the diff.
//! - Toolchain registration, as above.
//! - A repository-rule input that no label names. See the soundness gap
//!   documented on [`cambium_index::externals`].

use std::collections::{BTreeMap, BTreeSet};

use cambium_diff::Change;
use cambium_index::externals::{diff_module, scan_module, ModuleChange, ModuleFacts};
use cambium_index::IndexRef;

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
    /// Invalidate everything, for this reason.
    pub global: Option<String>,
    /// Apparent names of the repositories that moved.
    pub repos: BTreeSet<String>,
    /// Workspace files the stage accounted for, and which must therefore not
    /// also fire an invalidator rule.
    pub handled: Vec<String>,
    /// How many repositories were named by a repository-rule input edit.
    pub by_input: usize,
    /// How many were named by the `MODULE.bazel` diff.
    pub by_module: usize,
    /// How many were named by a changed workspace-closure `.bzl`.
    pub by_bzl: usize,
}

/// Runs the stage over a diff.
///
/// `base` is the base index, which carries the input map; `texts` reads
/// `MODULE.bazel` at either revision.
pub fn analyse(
    base: IndexRef<'_>,
    changes: &[Change],
    base_rev: &str,
    head_rev: &str,
    texts: &dyn WorkspaceText,
) -> Externals {
    let mut out = Externals::default();

    let mut module_changed = false;
    let mut lock_changed = false;
    for c in changes {
        if LEGACY_WORKSPACE_FILES.contains(&c.path.as_str()) {
            out.global = Some(format!(
                "`{}` changed; WORKSPACE is imperative (macro-declared repositories, \
                 order-dependent, `native.existing_rules()`), so its diff cannot be mapped to \
                 repositories",
                c.path
            ));
            return out;
        }
        match c.path.as_str() {
            MODULE_FILE => module_changed = true,
            MODULE_LOCK => lock_changed = true,
            _ => {}
        }
    }

    if module_changed {
        let base_facts = facts_at(texts, base_rev);
        let head_facts = facts_at(texts, head_rev);
        match diff_module(&base_facts, &head_facts) {
            ModuleChange::Global(reason) => {
                out.global = Some(reason);
                return out;
            }
            ModuleChange::Repos(repos) => {
                out.by_module = repos.len();
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
    // A lockfile that moved *without* `MODULE.bazel` records a registry that
    // moved under unchanged declarations, which can re-point any repository.
    // The stage deliberately does not handle that case: it leaves the file
    // alone, so the invalidator rules decide — and `MODULE.bazel.lock` is a
    // default invalidator, so the default answer is still global. A repository
    // that has removed it from its `cambium.toml` has said the file does not
    // matter, and this stage is not the place to overrule that.
    let _ = lock_changed;

    // Repository-rule inputs: main-repo files an external repository is built
    // from.
    let mut by_input: BTreeSet<String> = BTreeSet::new();
    for c in changes {
        for r in base.repos_for_path(&c.path) {
            by_input.insert(base.repo_name(r).to_string());
        }
    }
    out.by_input = by_input.len();
    out.repos.extend(by_input);

    out
}

fn facts_at(texts: &dyn WorkspaceText, rev: &str) -> ModuleFacts {
    match texts.text_at(rev, MODULE_FILE) {
        Some(t) => scan_module(&t),
        // A revision whose `MODULE.bazel` cannot be read is not the same as
        // one with no dependencies: an empty side would diff as "every
        // `bazel_dep` was added". The parser records that as unparseable so
        // the caller escalates.
        None => ModuleFacts {
            unparsed: vec![format!("MODULE.bazel is unreadable at {rev}")],
            ..Default::default()
        },
    }
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

/// The repository part of a label: `@@rules_cc+//x:y` -> `rules_cc+`.
fn label_repo(label: &str) -> Option<&str> {
    let rest = label
        .strip_prefix("@@")
        .or_else(|| label.strip_prefix('@'))?;
    let (repo, _) = rest.split_once("//")?;
    (!repo.is_empty()).then_some(repo)
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
}

/// Resolves dirty repositories to graph work.
pub fn reach(base: IndexRef<'_>, repos: &BTreeSet<String>) -> RepoReach {
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
    let matches = |token: &str| {
        repos
            .iter()
            .any(|a| repo_token_matches(token, a, canon.get(a.as_str()).copied().unwrap_or("")))
    };

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
        if label_repo(label).is_some_and(matches) {
            out.nodes.push(id);
        }
    }
    out.nodes.sort_unstable();

    // (2) loads. The static closure recorded the external labels every `.bzl`
    // load walk stopped at; a package whose closure contains one of them can
    // have its rules change when that repository does.
    if base.bzl_closure {
        let hot: Vec<u32> = (0..base.bzl_ext_labels.len() as u32)
            .filter(|&e| label_repo(base.bzl_ext_labels.get(e)).is_some_and(matches))
            .collect();
        if !hot.is_empty() {
            for id in 0..base.num_bzl() {
                if !base.bzl_ext.row(id).iter().any(|e| hot.contains(e)) {
                    continue;
                }
                for &p in base.bzl_consumers(id) {
                    if !base.package_tombstoned(p) {
                        out.packages.insert(base.package_name(p).to_string());
                    }
                }
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
    fn reads_the_repository_out_of_a_label() {
        assert_eq!(label_repo("@@rules_cc+//x:y"), Some("rules_cc+"));
        assert_eq!(label_repo("@r//x:y"), Some("r"));
        assert_eq!(label_repo("//x:y"), None);
        assert_eq!(label_repo("@//x:y"), None);
    }
}
