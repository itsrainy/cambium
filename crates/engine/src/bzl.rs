//! What the engine still reads out of the static `.bzl` load closure.
//!
//! Which packages a changed `.bzl` dirties is no longer answered here. That
//! was this module's reason for existing: a stored closure keyed by package,
//! plus a witness that re-derived one hop of the load graph from the head
//! checkout whenever the closure had no node for a changed file. Both are
//! gone. The index carries Bazel's own loading-phase graph
//! (`cambium_index::sky`), whose `BZL_LOAD` nodes hang off the `FILE` node of
//! the Starlark file and load each other, so a changed `.bzl` reaches the
//! packages behind it through the same reverse walk every other changed path
//! takes. There is nothing left for a witness to be incomplete about: the
//! edges are Bazel's, recorded when Bazel loaded the repository.
//!
//! What survives is the closure the *external-repository* stage reads, which
//! is a different question over the same files: which repositories a
//! workspace-level `.bzl` can define, which external labels a `.bzl` or a
//! BUILD file loads directly, and what `MODULE.bazel` feeds each repository.
//! That stage is unchanged, so its closure is rebuilt after every splice
//! exactly as before.

use std::collections::BTreeSet;

use crate::query::PackageQuerier;

/// What a run knows about the in-tree external repositories: which `path =`
/// claims Bazel confirmed, and which of those the universe query was actually
/// widened by.
///
/// Two facts and not one. A cold build widens by every claim it confirms, but
/// an index written before Phase 5 stage 2 holds confirmations and none of
/// the packages, and a run splicing onto it must not write a flag saying
/// otherwise.
#[derive(Clone, Copy)]
pub(crate) struct InTreeRepos<'a> {
    pub confirmed: &'a BTreeSet<(String, String)>,
    pub indexed: &'a [String],
}

/// Recomputes the static `.bzl` load closure for the packages a run touched.
///
/// `prior` is the base index's closure, carried through the splice: every
/// package that was not re-queried keeps the row it had, because nothing that
/// could have moved it is in this diff. What does get recomputed:
///
/// - every changed `.bzl` is invalidated, so it is re-read rather than
///   answered from the memoised parse (`changed_bzl` is the diff's `.bzl`
///   paths, which the dirty stage collects on its way past);
/// - every re-queried package has its BUILD file re-scanned and its closure
///   rebuilt (a package is re-queried precisely when its BUILD file, or the
///   files it owns, moved — which is when its loads can have changed);
/// - a deleted package's closure goes with it;
/// - the workspace closure is rebuilt, since a `.bzl` can move into or out of
///   it without `MODULE.bazel` changing (`MODULE.bazel` itself is an
///   invalidator, so that case never reaches here).
///
/// Returns `None` when the row source cannot read the tree, which leaves the
/// index without a closure and makes the next `.bzl` change global.
pub(crate) fn rescan_closure(
    querier: &dyn PackageQuerier,
    prior: Option<cambium_index::BzlClosure>,
    packages: &[String],
    changed_bzl: &BTreeSet<String>,
    deleted: &BTreeSet<String>,
    boundaries: &[String],
    repos: InTreeRepos<'_>,
) -> Option<cambium_index::BzlClosure> {
    if packages.is_empty() && changed_bzl.is_empty() && deleted.is_empty() {
        return prior;
    }
    let src = querier.load_source()?;
    // With the main repository's own apparent names: `@envoy//...` in a
    // repository rule's `.bzl` is a main-repo input under either name source.
    let mapping = main_alias_mapping(querier.repo_mapping(), src);
    let mut c = prior.unwrap_or_default();
    for p in changed_bzl {
        c.graph.invalidate(p);
    }
    for p in deleted {
        c.packages.remove(p);
        c.package_external.remove(p);
    }
    for p in packages {
        // A package key is a path for the main repository, but an in-tree
        // external repository's key is `@repo//pkg` and its BUILD file is on
        // disk under the directory the repository is. Scan the directory and
        // file the closure under the key: the closure graph is keyed by path
        // throughout, so a relative `load(":x.bzl")` in one of those BUILD
        // files resolves to the file that really backs it.
        let dir = package_dir(p, repos.confirmed);
        let pc = c.graph.package_closure(&dir, src, &mapping);
        c.packages.insert(p.clone(), pc.bzl);
        if pc.external.is_empty() {
            c.package_external.remove(p);
        } else {
            c.package_external.insert(p.clone(), pc.external);
        }
    }
    c.workspace = c.graph.workspace_closure(src, &mapping);
    // The workspace closure just moved, so what the main repository feeds its
    // external repositories may have moved with it. Which world Bazel reads
    // `MODULE.bazel` in is decided for the tree `src` reads — the head — not
    // taken from the externals stage, whose answer is about the base: the
    // two trees can disagree, and this closure describes the head.
    let mode = cambium_index::Mode::detect(src, &mapping);
    let facts = cambium_index::read_module_facts_in(src, mode);
    c.inputs =
        cambium_index::build_repo_inputs(src, &mapping, &c.graph, &c.workspace, &facts, boundaries);
    // Bazel's own answer for what each nested repository is, in the
    // WORKSPACE world: `query //external:*`. It goes before the confirmation
    // loop because it is also where a WORKSPACE-mode claim's *candidate*
    // comes from. The probe that widened the universe read the claim out of
    // `MODULE.bazel` (`crate::repos`, which reads it in either mode, since
    // envoy's pre-migration trees carry one), but `build_repo_inputs` reads
    // the tree in the mode Bazel does and so records no candidate from a
    // `MODULE.bazel` Bazel is not reading. Without one, `confirm_dir` below
    // had nothing to land on: the index held every `@envoy_api` package and
    // read `confirmed false, indexed false` for the repository, at every
    // envoy base before the bzlmod migration.
    map_nested_repositories(querier, mode, boundaries, &mut c.inputs);
    // A repository rule's `path =` claim is only a claim until Bazel's own
    // package list for the repository says it holds (`crate::repos`). The
    // check runs once, at cold build, before the universe query; an
    // incremental run carries the answer forward from the index it splices
    // onto, since a confirmed claim is a fact about the lineage and the
    // declaration that made it is an invalidator.
    for (r, dir) in repos.confirmed {
        if c.inputs.confirm_dir(r, dir) {
            // Whether the universe query was actually widened by it is a
            // separate fact, and the caller's: an index built before Phase 5
            // stage 2 can hold confirmed claims and none of their packages,
            // and a flag claiming otherwise would send a later refresh
            // looking for rows that were never written.
            let indexed = repos.indexed.iter().any(|i| i == r) && c.inputs.mark_indexed(r);
            tracing::debug!(
                repo = %r,
                %dir,
                indexed,
                "a confirmed repository claim maps its directory"
            );
        } else {
            tracing::debug!(
                repo = %r,
                %dir,
                "a carried-forward confirmation names a directory this closure no longer claims"
            );
        }
    }
    report_repo_settings(querier, src, &mapping, &facts);
    Some(c)
}

/// Says once, per closure rebuild, that a `.bazelrc` line makes a target of
/// this tree an input of an external repository's targets.
///
/// The input itself is recorded by
/// [`cambium_index::build_repo_inputs`]; this is the actionable half. The
/// answer it buys is wide by the whole repository, because the edge the flag
/// creates is between a main-repo target and whichever of the repository's
/// targets read the setting, and nothing in the tree says which those are.
fn report_repo_settings(
    querier: &dyn PackageQuerier,
    src: &dyn cambium_index::FileText,
    mapping: &cambium_index::RepoMapping,
    facts: &cambium_index::ModuleFacts,
) {
    let mapping = mapping.clone().with_main_alias(&facts.module_name);
    for s in cambium_index::repo_setting_inputs(src, &mapping) {
        tracing::debug!(
            repo = %s.repo, flag = %s.flag, value = %s.value,
            "a bazelrc line sets an external repository's build setting to a target of this tree"
        );
        let Some(diags) = querier.diagnostics() else {
            continue;
        };
        diags.emit(
            crate::diag::Diagnostic::warning(
                crate::diag::Code::W027BuildSettingCrossesRepository,
                format!(
                    "`.bazelrc` sets `{}` to `{}`, so a change to that target moves every \
                     target of `@{}`",
                    s.flag, s.value, s.repo
                ),
            )
            .with_help(format!(
                "expected on a tree that configures its external repositories this way; the \
                 answer is wide by `@{}` until the flag's readers are known",
                s.repo
            )),
        );
    }
}

/// Records which external repository each nested repository's directory is,
/// where the workspace files do not say.
///
/// A directory `//...` stops at is a repository built entirely from that
/// directory, which is exactly the claim
/// [`cambium_index::externals::PathMap::exhaustive_dir`] records, and the
/// engine reads it back to route a change inside the directory to the
/// repository. `local_path_override` and a literal `local_repository` already
/// produce one; a repository rule that computes its own path does not, and
/// tensorflow declares `third_party/xla` with one. In the WORKSPACE world
/// Bazel will print the attributes it ran that rule with, so the cold build
/// asks it.
///
/// Bzlmod has no `//external:*` to query and needs none: an override's path is
/// in `MODULE.bazel`, which is read statically. A query that fails leaves the
/// boundaries unmapped, and a change inside one is then global rather than
/// wrong.
fn map_nested_repositories(
    querier: &dyn PackageQuerier,
    mode: cambium_index::Mode,
    boundaries: &[String],
    inputs: &mut cambium_index::externals::RepoInputs,
) {
    if boundaries.is_empty() || mode.bzlmod_live() {
        return;
    }
    let Some(text) = querier.external_rules() else {
        return;
    };
    for (repo, ext) in cambium_index::externals::external_dir_repos(&text, boundaries) {
        if let Some(dir) = &ext.dir {
            tracing::debug!(repo = %repo, dir = %dir, "a nested repository is an external repository");
            inputs.path_maps.entry(repo.clone()).or_default().insert(
                cambium_index::externals::PathMap::exhaustive_dir(format!("{dir}/")),
            );
            // The claim as a candidate too, so that the probe's confirmation
            // (`RepoInputs::confirm_dir`) has a row to land on. Bazel printed
            // the directory itself, which is the strongest evidence a
            // candidate can carry; a static reader's candidate for the same
            // repository, if there is one, stands.
            inputs
                .dir_candidates
                .entry(repo.clone())
                .or_insert(cambium_index::externals::DirCandidate {
                    dir: format!("{dir}/"),
                    evidence: EVIDENCE.to_string(),
                });
            cambium_index::externals::RepoOrigin::claim(
                &mut inputs.origins,
                &repo,
                cambium_index::externals::RepoOrigin {
                    kind: cambium_index::externals::OriginKind::Dir,
                    dir: format!("{dir}/"),
                    evidence: EVIDENCE.to_string(),
                    ..Default::default()
                },
            );
        }
        // A `build_file` names a file of this tree, so Bazel's own answer
        // identifies the repository as an overlay even where no workspace
        // file this build can read declares it. The path map is minted by the
        // static reader when it can see the declaration; here the origin is
        // what the answer is for, and the evidence says where it came from.
        if let Some(bf) = &ext.build_file {
            cambium_index::externals::RepoOrigin::claim(
                &mut inputs.origins,
                &repo,
                cambium_index::externals::RepoOrigin {
                    kind: cambium_index::externals::OriginKind::Overlay,
                    dir: String::new(),
                    evidence: EVIDENCE.to_string(),
                    ..Default::default()
                },
            );
            tracing::debug!(repo = %repo, build_file = %bf, "an external repository overlays a tree BUILD file");
        }
        // Without an id in the repository table the mapping has nowhere to be
        // written, and the routing would find nothing.
        inputs.repos.insert(repo);
    }
}

/// Where a claim read out of `bazel query '//external:*'` came from.
const EVIDENCE: &str = "//external:*";

/// Just the directories of a boundary list, which is what the parsers below
/// and the `//external:*` matcher take.
pub fn dirs(boundaries: &[cambium_index::Boundary]) -> Vec<String> {
    boundaries.iter().map(|b| b.dir.clone()).collect()
}

/// The querier's mapping plus the main repository's own apparent name, taken
/// from `module(name = ...)` and, in WORKSPACE mode, `workspace(name = ...)`.
pub fn main_alias_mapping(
    m: cambium_index::RepoMapping,
    src: &dyn cambium_index::FileText,
) -> cambium_index::RepoMapping {
    let mut m = m.with_main_alias(&cambium_index::externals::read_module_facts(src).module_name);
    for name in ["WORKSPACE", "WORKSPACE.bazel"] {
        if let Some(n) = src
            .text(name)
            .as_deref()
            .and_then(cambium_index::loads::workspace_name)
        {
            m = m.with_main_alias(&n);
        }
    }
    m
}

/// The directory a package key's BUILD file is in.
///
/// The key itself for a main-repo package. For an in-tree external one,
/// the confirmed directory the repository is, plus the package inside it:
/// `@tsl//tsl/platform` under `third_party/tsl/` is
/// `third_party/tsl/tsl/platform`. A key whose repository has no confirmed
/// directory here has no directory to scan, and falls back to the package
/// part alone rather than inventing one.
pub(crate) fn package_dir(pkg: &str, confirmed: &BTreeSet<(String, String)>) -> String {
    let Some(repo) = cambium_index::label::key_repo(pkg) else {
        return pkg.to_string();
    };
    let inner = cambium_index::label::key_package(pkg);
    match confirmed.iter().find(|(r, _)| r == repo) {
        Some((_, dir)) => cambium_index::label::join_path(dir.trim_end_matches('/'), inner),
        None => inner.to_string(),
    }
}
