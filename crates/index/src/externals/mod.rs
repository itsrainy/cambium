//! External repositories: what the main repository feeds them, and what a
//! `MODULE.bazel` edit changes about them.
//!
//! # The problem
//!
//! cambium indexes the main repository. Every label into an external
//! repository is a leaf: `@envoy_api//envoy/config/core/v3:pkg` is a node with
//! no outgoing edges, because nothing ever queried that repository. So a
//! change that reaches a main-repo target *through* an external repository is
//! invisible to the graph, and the only sound answer without this module is
//! to invalidate everything:
//!
//! - envoy lists `api/` in `.bazelignore` and re-exposes it as the module
//!   `envoy_api` via `local_path_override`. An edit to one `.proto` there owns
//!   no main-repo package at all, and yet `//tools/proto_format:proto_sync`
//!   really does depend on it.
//! - envoy's `@envoy_repo` repository rule reads `.github/config.yml` at load
//!   time and writes the build-image digests into the `exec_properties` of the
//!   RBE platform rules. Four main-repo rule hashes move when that YAML moves,
//!   and no query names the edge.
//! - a `bazel_dep` version bump changes the contents of exactly one external
//!   repository, but `MODULE.bazel` as an invalidator changes everything.
//!
//! Both shapes are the same question with a repository in the middle: *which
//! external repositories did this edit change, and who depends on them?* This
//! module answers the first half statically; [`crate::data::IndexRef`] stores
//! the answer, and the engine answers the second half from the graph it
//! already has.
//!
//! # What is computed, and when
//!
//! At index time, from the text of `MODULE.bazel` / `WORKSPACE` and of the
//! `.bzl` files in their load closure (which [`crate::loads`] already walks):
//!
//! - **[`RepoInputs::patterns`]** — main-repo path patterns to the set of
//!   external repositories built from them. Sources: the `path` of a
//!   `local_path_override`, every main-repo label in a module extension's tag
//!   calls or a `use_repo_rule` instantiation, and every main-repo label
//!   appearing anywhere in a workspace-closure `.bzl`.
//! - **[`RepoInputs::bzl_repos`]** — which repositories each workspace-closure
//!   `.bzl` can define. A change to `bazel/repo.bzl` then dirties `envoy_repo`
//!   rather than the world.
//! - **[`ModuleFacts`]** — the structured content of `MODULE.bazel`, so that a
//!   run can diff base against head and name the repositories that moved.
//!
//! # Attribution, and why it is conservative in the right direction
//!
//! A repository rule's implementation lives in a `.bzl`; the *instance* is
//! named where the rule is called — `use_repo(ext, "envoy_repo")` in
//! `MODULE.bazel`, the `name =` of a `use_repo_rule` call, or a `name =` in
//! Starlark itself. The question this module answers is which of those
//! instances a given main-repo path feeds.
//!
//! Answering it per *file* is not good enough. envoy's `bazel/extensions.bzl`
//! declares five module extensions and loads `bazel/repo.bzl`, so a file-level
//! reading makes `repo.bzl`'s
//! `attr.label(default = "@envoy//:.github/config.yml")` an input of all six
//! repositories — `envoy_repo` among them, whose generated `compiler.bzl` is
//! in eleven hundred package load closures. Every commit that touched that
//! YAML then re-queried the whole repository. So attribution is per
//! *occurrence*: the analysis in [`crate::attribution`] reads the closure's
//! Starlark and asks, of each string literal, which repositories the code it
//! sits in can create.
//!
//! Four rules, innermost first:
//!
//! 1. the nearest enclosing call with a literal `name =` — `pip_parse(name =
//!    "base_pip3", requirements_lock = "@envoy//tools/base:requirements.txt")`
//!    feeds `base_pip3` and nothing else;
//! 2. otherwise, a literal inside an `X = repository_rule(...)` or
//!    `E = module_extension(...)` declaration — an `attrs =` default the rule
//!    itself resolves — feeds the instances of `X`, or the `use_repo` names of
//!    `E`;
//! 3. otherwise, the enclosing `def`: a rule or extension implementation feeds
//!    that rule's instances, a forwarding wrapper (envoy's
//!    `external_http_archive(name, **kwargs)`) feeds the names its callers
//!    pass, and any other macro feeds what it and the macros it calls
//!    instantiate;
//! 4. otherwise — a literal at module level — the file's own set, below.
//!
//! A `def` that instantiates nothing and implements no rule (apollo's
//! `_find_cuda_config(repository_ctx)`, which only reads a file) has no answer
//! of its own; it takes the union of what its call sites are attributed to, so
//! a helper called from a repository rule's implementation belongs to that
//! rule's repositories. A helper nothing in the closure calls keeps the
//! fallback.
//!
//! The instantiation sets are two fixpoints over the closure's call graph, so
//! a name that reaches a repository rule through three macros still lands on
//! the right repository.
//!
//! Over-attribution is safe (more repositories dirty, more targets reported,
//! never fewer) and under-attribution is not, and every step above resolves an
//! unknown to the wide answer rather than to a guess:
//!
//! - a `name =` the analysis cannot evaluate — string arithmetic, a loop
//!   variable — makes its whole scope fall back, and makes the callable it
//!   names instantiable under any name;
//! - a call to a symbol the closure does not define and that is not one of
//!   Starlark's own builtins may be a macro from another repository declaring
//!   repositories under names nothing here can see, so it too makes its scope
//!   fall back;
//! - a rule whose instantiation is never seen, or a macro nothing calls, has
//!   an empty answer, which is read as "nothing learned" and falls back;
//! - a syntactic shape the reader cannot structure is simply not recorded, and
//!   the file keeps its old answer.
//!
//! The fallback itself is a set, not the world: the repositories the main
//! repository defines, plus every literal `name =` written anywhere in the
//! closure. On a WORKSPACE-only repository the first half is empty and the
//! second is the whole answer, which is what makes a `bazel/repositories.bzl`
//! edit a named set rather than a global invalidation.
//!
//! # What a change to one workspace `.bzl` moves
//!
//! [`RepoInputs::bzl_repos`] answers that, and the answer is *not* the seed
//! propagated down `load()` edges. apollo's `tools/workspace.bzl` loads a
//! hundred `third_party/*/init.bzl` files and calls each one's `init()`;
//! propagating its answer down would make every one of those files responsible
//! for everything apollo declares, which is a cold rebuild for every
//! third-party bump.
//!
//! A `.bzl` reaches a repository in exactly two ways: through its own
//! definitions, which the rules above already attribute, or through a symbol
//! another file imported from it — and the effect of *that* is bounded by
//! where the symbol is used. So a file's set is what it defines itself, plus,
//! for every `load()` of it, the attribution of each use of the symbols
//! imported. Calling an imported function does exactly what that function
//! does (`pcl()` in the root's macro is answered by `init()`'s own
//! repositories, not by everything the root declares); any other use — a
//! constant read into an expression — is attributed like any other
//! occurrence, and falls back to the using file's own set when the occurrence
//! says nothing. Re-exports are covered by attributing a use to every file the
//! symbol resolved through. The whole thing is a fixpoint, so transitivity
//! comes free, and a file whose set comes out empty keeps the fallback.
//!
//! # The soundness gap, stated plainly
//!
//! A repository rule can read a main-repo file whose path never appears as a
//! label: `repository_ctx.read(repository_ctx.workspace_root.get_child("x"))`,
//! or a path assembled by string arithmetic. No static scan sees that, and
//! neither does Bazel's own query. Such a path must still be listed as an
//! invalidator in `cambium.toml`. Everything reachable through a label —
//! which is the documented, and overwhelmingly the common, way for a
//! repository rule to name a file — is covered.
//!
//! # Layout
//!
//! - `pattern` — [`Pattern`], [`PathMap`], [`RepoInputs`]: the tables, in the
//!   mutable form the builder flattens and the reader restores.
//! - `module_facts` — [`ModuleFacts`] and its parts, [`ModuleChange`], and the
//!   toolchain-module list.
//! - `module_parser` — [`scan_module`], the `MODULE.bazel` statement reader,
//!   on top of [`crate::starlark`].
//! - `module_diff` — [`diff_module`].
//! - `path_maps` — [`scan_bzl_path_maps`]: `local_repository` and `symlink`
//!   calls as [`PathMap`]s.
//! - `inputs` — [`main_repo_patterns`] and [`build_repo_inputs`], which ties
//!   the module facts, the path maps and the attribution pass together.
//! - `bazelrc` — [`bzlmod_live`] and [`bazelrc_bzlmod`]: which world Bazel
//!   reads in a tree that carries both.

mod bazelrc;
mod inputs;
mod module_diff;
mod module_facts;
mod module_parser;
mod path_maps;
mod pattern;

pub use bazelrc::{bazelrc_bzlmod, bzlmod_live};
pub use inputs::{build_repo_inputs, main_repo_patterns};
pub use module_diff::diff_module;
pub use module_facts::{
    apparent_of, is_toolchain_module, Dep, Extension, ModuleChange, ModuleFacts, RepoRule,
    TOOLCHAIN_MODULES,
};
pub use module_parser::scan_module;
pub use path_maps::{scan_bzl_path_maps, BzlPathMaps};
pub use pattern::{PathMap, Pattern, RepoInputs};

use crate::loads::{FileText, RepoMapping};

/// [`read_module_facts`], or empty facts when [`bzlmod_live`] says Bazel does
/// not read the file: what the externals tables must be derived from.
pub fn read_live_module_facts(src: &dyn FileText, m: &RepoMapping) -> ModuleFacts {
    if bzlmod_live(src, m) {
        read_module_facts(src)
    } else {
        ModuleFacts::default()
    }
}

/// Reads `MODULE.bazel` out of a checkout. Absent (a WORKSPACE-only
/// repository) yields empty facts, which diff to no change. Reads the file
/// whether or not Bazel does; the tables want [`read_live_module_facts`].
pub fn read_module_facts(src: &dyn FileText) -> ModuleFacts {
    match src.text("MODULE.bazel") {
        Some(t) => scan_module(&t),
        None => ModuleFacts::default(),
    }
}
