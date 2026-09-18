//! Every list of Bazel and Starlark names this crate reasons from, in one
//! place, each with the question it answers.
//!
//! The readers in [`crate::externals`], [`crate::workspace`] and the
//! attribution pass each used to carry their own vocabulary, and the lists
//! overlap in ways that are easy to get wrong when one of them moves: a
//! statement `MODULE.bazel` may contain harmlessly had better be one the
//! attribution pass also knows declares nothing. The test at the bottom pins
//! those relationships.

/// Undotted builtins that cannot create a repository.
///
/// Answers: *may a call to this name, which the closure does not define,
/// declare a repository under a name nothing here can see?* No, for these.
/// Every other unresolved call might be a macro from another repository —
/// `envoy_dependencies()` calling into `@envoy_api`'s helpers — so a literal
/// in its scope keeps the wide answer.
pub(crate) const PURE_BUILTINS: &[&str] = &[
    "Label",
    // `MODULE.bazel`'s own vocabulary. `scan_module` reads these
    // structurally, and none of them declares a repository the analysis
    // cannot already see through the facts it is seeded with.
    "archive_override",
    "bazel_dep",
    "include",
    "inject_repo",
    "local_path_override",
    "module",
    "multiple_version_override",
    "override_repo",
    "register_execution_platforms",
    "register_toolchains",
    "single_version_override",
    "use_extension",
    "use_repo",
    "use_repo_rule",
    "workspace",
    "abs",
    "all",
    "any",
    "aspect",
    "bool",
    "depset",
    "dict",
    "dir",
    "enumerate",
    "fail",
    "float",
    "getattr",
    "git_override",
    "glob",
    "hasattr",
    "hash",
    "int",
    "len",
    "list",
    "max",
    "min",
    "module_extension",
    "print",
    "provider",
    "range",
    "repr",
    "repository_rule",
    "reversed",
    "rule",
    "select",
    "sorted",
    "str",
    "struct",
    "tag_class",
    "tuple",
    "type",
    "zip",
];

/// Starlark keywords.
///
/// Answers: *is the `(` after this name a call?* No: `return (a or b)`,
/// `if (x)` and `not (y)` open an expression, and reading one as a call to a
/// symbol the closure cannot resolve made every helper that brackets a
/// condition unattributable.
pub(crate) const KEYWORDS: &[&str] = &[
    "and", "assert", "break", "continue", "def", "del", "elif", "else", "for", "if", "in", "is",
    "lambda", "load", "not", "or", "pass", "return", "while",
];

/// Calls whose literal `name =` is not the name of a repository the main
/// repository builds; see [`names_a_repository`].
///
/// `workspace()` and `module()` name the main repository itself;
/// `bazel_dep()` names one the registry provides, whose contents no main-repo
/// file feeds; `register_toolchains` and `include` take a `name` that is a
/// label. None belongs in the universe a fallback ranges over.
pub(crate) const NAMES_NO_REPOSITORY: &[&str] = &[
    "workspace",
    "module",
    "bazel_dep",
    "register_toolchains",
    "include",
];

/// Whether a call's `name =` is the name of a repository the main repository
/// builds: everything but [`NAMES_NO_REPOSITORY`] and the `*_override` family,
/// whose `module_name` is a registry module's.
pub(crate) fn names_a_repository(head: &str) -> bool {
    !NAMES_NO_REPOSITORY.contains(&head) && !head.ends_with("_override")
}

/// Statements a `MODULE.bazel` may contain that the module parser
/// deliberately ignores rather than recording as unrecognised.
///
/// Answers: *does this top-level statement, which is none of the shapes
/// `scan_module` extracts facts from, mean the file is beyond the parser?*
/// No: `load` and `include` are edges the load graph already has, `print`
/// and `fail` declare nothing, and `module` is read on its own.
pub(crate) const HARMLESS: &[&str] = &["load", "print", "fail", "module", "include"];

/// `WORKSPACE` statements that declare something under a `name =` but whose
/// effect is not confined to a repository of that name.
///
/// Answers: *is a changed call with this callee just the repository it
/// names?* No: `bind` rewrites `//external:` labels, which is a
/// whole-workspace aliasing table, and `workspace()` renames the main
/// repository itself.
pub(crate) const NOT_JUST_A_REPOSITORY: &[&str] = &["bind", "workspace"];

/// Whether a callee registers toolchains.
///
/// Answers: *can a reverse walk over the graph find what a change to this
/// call moves?* No: toolchain resolution has no edge in the query graph, the
/// same reason `register_toolchains` in a `MODULE.bazel` is global. In
/// `WORKSPACE` the registration is usually wrapped in a rules-provided macro,
/// and the naming convention is universal enough to match on:
/// `go_register_toolchains`, `oci_register_toolchains`,
/// `python_register_toolchains`. A macro that registers toolchains under some
/// *other* name is the documented gap.
pub(crate) fn registers_toolchains(callee: &str) -> bool {
    callee == "register_toolchains"
        || callee == "register_execution_platforms"
        || callee.ends_with("_register_toolchains")
        || callee.ends_with("_register_execution_platforms")
}

/// Repository rules whose `path` attribute is a main-repo directory that
/// becomes the repository's root.
///
/// Answers: *is the whole of this repository one main-repo directory?* Yes,
/// which is what makes its path map exhaustive at index time, and what makes
/// a `WORKSPACE` diff that moved the `path` global (the recorded map is
/// stale).
pub(crate) fn is_local_repo_rule(rule: &str) -> bool {
    matches!(rule, "local_repository" | "new_local_repository")
}

/// Names a `repository_ctx.symlink` source argument may contain and still be
/// read as a literal path.
///
/// Answers: *is `ctx.symlink(ctx.path(Label("//api:BUILD")), "BUILD")` a
/// mapping the path-map reader can follow?* Yes: the wrappers around the
/// literal are these receivers and `Label`/`path`. A source that computes its
/// path — `ctx.path(ctx.attr.root).dirname.get_child(d)`, the shape envoy's
/// `bazel/api_binding.bzl` used before `local_path_override` — is not, and
/// is deliberately left unmapped: a mapping guessed wrong would *hide* a
/// `.bzl` edit, where no mapping merely keeps the wider answer.
pub(crate) const PATHY_NAMES: &[&str] = &[
    "Label",
    "ctx",
    "path",
    "rctx",
    "repo_ctx",
    "repository_ctx",
    "repository_context",
];

/// Modules whose version decides how *every* configured target is built, not
/// what any particular target depends on.
///
/// Answers: *can a `bazel_dep` bump of this module be localised to one
/// repository?* No. A toolchain is resolved, not depended on: `bazel query`
/// shows no edge from a `cc_library` to the C++ toolchain that will compile
/// it, so no reverse walk can find the targets a toolchain change moves. A
/// version bump of one of these therefore has to invalidate everything, and
/// the list is explicit rather than heuristic so that each entry can carry
/// its reason.
///
/// - `rules_cc`, `rules_java`, `rules_python`, `rules_go`, `rules_rust`,
///   `rules_shell`, `rules_nodejs` — each registers the toolchains for its
///   language from its own `MODULE.bazel`.
/// - `platforms`, `bazel_tools` — the constraint values and the host
///   toolchains every resolution is expressed in.
/// - `toolchains_llvm`, `apple_support`, `rules_apple`, `rules_foreign_cc` —
///   C++ toolchains and execution platforms.
/// - `bazel_skylib` is deliberately *not* here: it registers no toolchains,
///   it is a macro library, and its consumers are visible through `load()`.
pub const TOOLCHAIN_MODULES: &[&str] = &[
    "apple_support",
    "bazel_tools",
    "platforms",
    "rules_apple",
    "rules_cc",
    "rules_foreign_cc",
    "rules_go",
    "rules_java",
    "rules_nodejs",
    "rules_python",
    "rules_rust",
    "rules_shell",
    "toolchains_llvm",
];

pub fn is_toolchain_module(name: &str) -> bool {
    TOOLCHAIN_MODULES.contains(&name)
}

/// The rule classes that declare what toolchain resolution chooses between.
///
/// Answers: *can a reverse walk over the graph find what a rule of this class
/// moves when it appears, changes or vanishes?* No. A `toolchain()` is
/// resolved, not depended on: the `cc_library` it will compile has no edge to
/// it, and neither does the `register_toolchains` call that offers it. So one
/// of these moving, in any package, can change what every target builds with,
/// and the only sound answer is everything. The three are native rules with no
/// implementation of their own, which is what makes the rule hash a complete
/// witness: a macro that declares one moves its proto, and there is no `.bzl`
/// behind the rule itself to move without the proto moving.
pub const TOOLCHAIN_RULE_CLASSES: &[&str] = &["platform", "toolchain", "toolchain_type"];

pub fn is_toolchain_rule_class(class: &str) -> bool {
    TOOLCHAIN_RULE_CLASSES.contains(&class)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_and_sorted(list: &[&str]) -> bool {
        list.windows(2).all(|w| w[0] < w[1])
    }

    fn unique(list: &[&str]) -> bool {
        let mut v = list.to_vec();
        v.sort_unstable();
        v.dedup();
        v.len() == list.len()
    }

    /// The three native declaration classes, and nothing that merely
    /// implements a toolchain (`cc_toolchain`, `cc_toolchain_config`, ...):
    /// those are reached through ordinary edges and have no place here.
    #[test]
    fn the_toolchain_rule_classes_are_the_three_native_declarations() {
        assert!(unique_and_sorted(TOOLCHAIN_RULE_CLASSES));
        for c in ["platform", "toolchain", "toolchain_type"] {
            assert!(is_toolchain_rule_class(c), "{c}");
        }
        for c in [
            "cc_toolchain",
            "cc_toolchain_config",
            "constraint_value",
            "filegroup",
        ] {
            assert!(!is_toolchain_rule_class(c), "{c}");
        }
    }

    /// The relationships between the lists that the readers rely on, stated
    /// once so that editing one list cannot silently unbalance another.
    #[test]
    fn the_lists_agree_with_each_other() {
        // A statement the module parser waves through must be one the
        // attribution pass also knows declares nothing: a Starlark builtin,
        // or the `load` keyword.
        for h in HARMLESS {
            assert!(
                PURE_BUILTINS.contains(h) || KEYWORDS.contains(h),
                "HARMLESS `{h}` is neither a pure builtin nor a keyword"
            );
        }
        // A call whose `name =` names no repository is `MODULE.bazel` or
        // `WORKSPACE` vocabulary, which the attribution pass must already
        // treat as declaring nothing.
        for n in NAMES_NO_REPOSITORY {
            assert!(
                PURE_BUILTINS.contains(n),
                "`{n}` names no repository but is not a builtin"
            );
            assert!(!names_a_repository(n));
        }
        assert!(!names_a_repository("git_override"));
        assert!(names_a_repository("http_archive"));
        // The two names the WORKSPACE diff refuses to localise: `workspace`
        // is the builtin that names the main repository; `bind` is a native
        // WORKSPACE rule, not a Starlark builtin, and so is deliberately
        // absent from PURE_BUILTINS — read as a repository declaration it
        // widens, never narrows.
        assert!(PURE_BUILTINS.contains(&"workspace"));
        assert!(!PURE_BUILTINS.contains(&"bind"));
        assert_eq!(NOT_JUST_A_REPOSITORY, &["bind", "workspace"]);
        // The toolchain registrations are builtins on both readings.
        for r in ["register_toolchains", "register_execution_platforms"] {
            assert!(registers_toolchains(r));
            assert!(PURE_BUILTINS.contains(&r));
        }
        assert!(registers_toolchains("go_register_toolchains"));
        assert!(!registers_toolchains("register_toolchains_helper"));
        // A local repository rule declares a repository; it must not be a
        // pure builtin, or its `name =` would be lost from the universe.
        for r in ["local_repository", "new_local_repository"] {
            assert!(is_local_repo_rule(r));
            assert!(!PURE_BUILTINS.contains(&r));
        }
        // `Label` is the one wrapper a symlink source may use that is also a
        // builtin; the rest of PATHY_NAMES are receivers, never called.
        assert!(PATHY_NAMES.contains(&"Label") && PURE_BUILTINS.contains(&"Label"));
        for n in PATHY_NAMES.iter().filter(|n| **n != "Label") {
            assert!(
                !PURE_BUILTINS.contains(n),
                "`{n}` is a receiver, not a builtin"
            );
        }
        // Keywords and builtins are disjoint, and every list is a set.
        assert!(KEYWORDS.iter().all(|k| !PURE_BUILTINS.contains(k)));
        assert!(unique_and_sorted(KEYWORDS));
        assert!(unique_and_sorted(TOOLCHAIN_MODULES));
        assert!(unique(PATHY_NAMES));
        assert!(unique(PURE_BUILTINS), "PURE_BUILTINS repeats a name");
        // The one module the toolchain list documents leaving out.
        assert!(!is_toolchain_module("bazel_skylib"));
        assert!(is_toolchain_module("rules_cc"));
    }
}
