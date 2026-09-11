//! What changed between two revisions of a `MODULE.bazel`.

use std::collections::BTreeSet;

use super::module_facts::{apparent_of, is_toolchain_module, ModuleChange, ModuleFacts};

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
        let what = head
            .unparsed
            .iter()
            .find(|s| !base.unparsed.contains(s))
            .or_else(|| base.unparsed.iter().find(|s| !head.unparsed.contains(s)))
            .unwrap();
        return ModuleChange::Global(format!(
            "MODULE.bazel contains a statement cambium cannot parse: `{what}`"
        ));
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
    let names: BTreeSet<&String> = base.deps.keys().chain(head.deps.keys()).collect();
    for n in names {
        let (b, h) = (base.deps.get(n), head.deps.get(n));
        if b == h {
            continue;
        }
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
    let mods: BTreeSet<&String> = base.overrides.keys().chain(head.overrides.keys()).collect();
    for m in mods {
        if base.overrides.get(m) == head.overrides.get(m) {
            continue;
        }
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
    let keys: BTreeSet<&String> = base
        .extensions
        .keys()
        .chain(head.extensions.keys())
        .collect();
    for k in keys {
        let (b, h) = (base.extensions.get(k), head.extensions.get(k));
        if b == h {
            continue;
        }
        for e in [b, h].into_iter().flatten() {
            repos.extend(e.repos.iter().cloned());
        }
    }

    let keys: BTreeSet<&String> = base
        .repo_rules
        .keys()
        .chain(head.repo_rules.keys())
        .collect();
    for k in keys {
        let (b, h) = (base.repo_rules.get(k), head.repo_rules.get(k));
        if b == h {
            continue;
        }
        for r in [b, h].into_iter().flatten() {
            repos.extend(r.repos.iter().cloned());
        }
    }

    ModuleChange::Repos(repos)
}
