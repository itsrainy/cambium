//! Checking what the tree claims an external repository is against Bazel.
//!
//! A repository rule with a literal `path = "third_party/tsl"` is claiming
//! that the repository *is* that directory of this tree. The claim is worth a
//! lot — it turns every change under the directory from a global invalidation
//! into an input edit of one repository — and the reader that makes it cannot
//! check itself: the rule is arbitrary Starlark, free to symlink somewhere
//! else, to read the attribute as a prefix, or to ignore it. So the claim is
//! recorded as a candidate ([`cambium_index::DirCandidate`]) and confirmed
//! here, against Bazel's own package list for the repository.
//!
//! Two conditions, both named in the Phase 5 plan:
//!
//! - **C1**: the repository loaded at all. At least one package came back and
//!   none of its packages failed to load. A repository that did not fetch
//!   tells us nothing, and its empty package list would pass C2 for an empty
//!   directory. Failing it is `CAMBI-W025`.
//! - **C2**: the package list equals the BUILD-bearing directories under the
//!   claimed directory ([`cambium_index::build_dirs_under`]). This is the
//!   condition a rule that reinterpreted its `path` cannot pass. Failing it
//!   is `CAMBI-W026`, and the boundary keeps the global answer it has today.
//!
//! Plus the cap: a repository with more packages than `[externals]
//! index_max_packages` is left alone, because the width Stage B buys from
//! indexing it is not worth the query. That is `CAMBI-W025` too, with the
//! count in the message.
//!
//! The probe runs at cold build, before the universe query. It must not run
//! between that query and the loading-layer dump: see `full_build_with`.

use std::collections::BTreeSet;

use crate::query::PackageQuerier;

/// The repositories whose candidate `Dir` claim Bazel confirmed.
///
/// An empty answer is the behaviour before Phase 5: no claim is promoted, no
/// boundary is mapped by one, and a change under it is global. Every way this
/// can fail — no Bazel, no tree, a probe that did not run, a claim Bazel
/// disagrees with — fails to that same side.
pub(crate) fn confirm_dir_claims(
    q: &dyn PackageQuerier,
    cfg: &crate::config::Externals,
    boundaries: &[cambium_index::Boundary],
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if !cfg.enabled || boundaries.is_empty() {
        return out;
    }
    let (Some(src), Some(root)) = (q.load_source(), q.workspace()) else {
        return out;
    };
    let mapping = crate::bzl::main_alias_mapping(q.repo_mapping(), src);
    let dirs = crate::bzl::dirs(boundaries);
    let candidates = cambium_index::dir_candidates(src, &mapping, &dirs);
    if candidates.is_empty() {
        return out;
    }
    let names: Vec<String> = candidates.keys().cloned().collect();
    let probes = q.probe_repos(&names);
    for (repo, c) in &candidates {
        let Some(p) = probes.iter().find(|p| &p.repo == repo) else {
            tracing::debug!(repo, dir = %c.dir, "no probe answered for a repository claim");
            continue;
        };
        // C1.
        if p.packages.is_empty() || p.load_error {
            emit(
                q,
                crate::diag::Code::W025RepoNotIndexed,
                format!(
                    "`bazel query '@{repo}//...'` did not list a package cambium can trust \
                     ({} listed{}), so `{}` keeps the wide answer",
                    p.packages.len(),
                    if p.load_error {
                        ", and a package of it failed to load"
                    } else {
                        ""
                    },
                    c.dir,
                ),
                format!(
                    "fix the fetch, or leave it: a repository cambium cannot list is treated \
                     as though the tree said nothing about it (claimed at {})",
                    c.evidence
                ),
            );
            continue;
        }
        // The cap.
        if p.packages.len() as u32 > cfg.index_max_packages {
            emit(
                q,
                crate::diag::Code::W025RepoNotIndexed,
                format!(
                    "`@{repo}` has {} packages, over `[externals] index_max_packages` ({})",
                    p.packages.len(),
                    cfg.index_max_packages
                ),
                "raise `index_max_packages` to route changes under this repository's directory \
                 to it, or leave it and keep the wide answer"
                    .to_string(),
            );
            continue;
        }
        // C2.
        let tree = cambium_index::build_dirs_under(root, &c.dir);
        let said: BTreeSet<String> = p.packages.iter().cloned().collect();
        if tree != said {
            let missing = said.difference(&tree).next().cloned();
            let extra = tree.difference(&said).next().cloned();
            emit(
                q,
                crate::diag::Code::W026RepoOriginUnconfirmed,
                format!(
                    "`{}` claims to be `@{repo}` ({}), but Bazel's {} packages are not that \
                     directory's {}{}{}",
                    c.dir,
                    c.evidence,
                    said.len(),
                    tree.len(),
                    missing
                        .map(|p| format!("; Bazel has `@{repo}//{p}`, the tree does not"))
                        .unwrap_or_default(),
                    extra
                        .map(|p| format!("; the tree has `{}{p}`, Bazel does not", c.dir))
                        .unwrap_or_default(),
                ),
                "the claim is not used: a change under the directory stays global until the \
                 rule and Bazel agree about what the repository is"
                    .to_string(),
            );
            continue;
        }
        tracing::debug!(repo, dir = %c.dir, packages = said.len(), "a repository claim is confirmed");
        out.insert(repo.clone());
    }
    out
}

fn emit(q: &dyn PackageQuerier, code: crate::diag::Code, message: String, help: String) {
    tracing::debug!(code = code.as_str(), %message, "a repository claim was not confirmed");
    if let Some(d) = q.diagnostics() {
        d.emit(crate::diag::Diagnostic::warning(code, message).with_help(help));
    }
}
