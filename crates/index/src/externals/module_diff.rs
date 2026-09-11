//! What changed between two revisions of a `MODULE.bazel`.

use std::collections::BTreeSet;

use crate::util::diff_maps;

use crate::vocabulary::is_toolchain_module;

use super::module_facts::{apparent_of, ModuleChange, ModuleFacts};

/// What changed between two revisions of a `MODULE.bazel`.
///
/// Every rule here answers the same question: can a reverse walk over the
/// main-repo graph find the targets this change moves? If it can, the change
/// names repositories; if it cannot, it is global and says why.
pub fn diff_module(base: &ModuleFacts, head: &ModuleFacts) -> ModuleChange {
    // A statement the parser does not know is global only when it is what
    // changed. One that reads the same at both revisions (a call to a macro
    // the file loaded, say) moves nothing on its own; the `.bzl` behind it
    // has its own route.
    if base.unparsed != head.unparsed {
        let added = head.unparsed.iter().find(|s| !base.unparsed.contains(s));
        let removed = base.unparsed.iter().find(|s| !head.unparsed.contains(s));
        return ModuleChange::Global(match added.or(removed) {
            Some(what) => {
                format!("MODULE.bazel contains a statement cambium cannot parse: `{what}`")
            }
            // The two lists differ but every statement is on both sides: one
            // was repeated, or they were reordered. Text the parser cannot
            // read moved, which is the same global answer with no single
            // statement to name.
            None => "MODULE.bazel's unparsed statements were repeated or reordered".into(),
        });
    }
    if base.module != head.module {
        return ModuleChange::Global(
            "the root `module()` declaration changed (its version reaches every repository that \
             reads it)"
                .into(),
        );
    }
    if base.registrations != head.registrations {
        return ModuleChange::Global(
            "`register_toolchains` / `register_execution_platforms` changed; toolchain resolution \
             has no edge in the query graph, so no reverse walk can find what it moves"
                .into(),
        );
    }

    let mut repos: BTreeSet<String> = BTreeSet::new();

    // Dependencies: a version bump changes exactly one repository's contents.
    for (n, b, h) in diff_maps(&base.deps, &head.deps) {
        let module = h.or(b).map(|d| d.module.clone()).unwrap_or_default();
        if is_toolchain_module(&module) {
            return ModuleChange::Global(format!(
                "`bazel_dep({module})` changed, and {module} registers toolchains: toolchain \
                 resolution has no edge in the query graph"
            ));
        }
        repos.insert(n.to_string());
    }

    // Overrides: same, keyed by module name rather than apparent name.
    for (m, _, _) in diff_maps(&base.overrides, &head.overrides) {
        if is_toolchain_module(m) {
            return ModuleChange::Global(format!(
                "the override for `{m}` changed, and {m} registers toolchains"
            ));
        }
        // The apparent name is the `repo_name` of the matching bazel_dep, and
        // the module name itself when there is none.
        repos.insert(apparent_of(head, m).unwrap_or_else(|| m.to_string()));
    }

    // Extensions: a changed tag, a changed `use_repo`, or a changed
    // `override_repo` / `inject_repo` moves the repositories on both sides of
    // the change.
    for (_, b, h) in diff_maps(&base.extensions, &head.extensions) {
        for e in [b, h].into_iter().flatten() {
            repos.extend(e.repos.iter().cloned());
        }
    }

    for (_, b, h) in diff_maps(&base.repo_rules, &head.repo_rules) {
        for r in [b, h].into_iter().flatten() {
            repos.extend(r.repos.iter().cloned());
        }
    }

    ModuleChange::Repos(repos)
}
