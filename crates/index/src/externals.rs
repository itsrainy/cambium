//! External repositories: what the main repository feeds them, and what a
//! `MODULE.bazel` edit changes about them.
//!
//! # The problem
//!
//! cambium indexes the main repository. Every label into an external
//! repository is a leaf: `@envoy_api//envoy/config/core/v3:pkg` is a node with
//! no outgoing edges, because nothing ever queried that repository. So a
//! change that reaches a main-repo target *through* an external repository is
//! invisible to the graph, and until this module the only sound answer was to
//! invalidate everything:
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
//!   repository, but `MODULE.bazel` was an invalidator, so it changed
//!   everything.
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
//! 4. otherwise — a literal at module level — the file's old answer: the
//!    repositories whose extension or repo-rule `.bzl` reaches this file
//!    through `load()`, and failing that every repository in the universe.
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
//! second is the whole answer, which is what turns issue #6 — every
//! `bazel/repositories.bzl` edit going global — into a named set.
//!
//! [`RepoInputs::bzl_repos`], the answer when a workspace `.bzl` *itself*
//! changes, stays the union of the load-graph propagation and what the file
//! can define. It is not narrowed below the propagation: a `.bzl` may export a
//! constant that some other file's rule reads, and only the `load()` edge sees
//! that.
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

use std::collections::{BTreeMap, BTreeSet};

use crate::attribution::{arg_spans, top_level_spans, Attribution};
use crate::loads::{lex, unescape, FileText, Lexed, LoadGraph, RepoMapping, Tok};

/// A main-repo path pattern that feeds an external repository.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pattern {
    /// Exactly this repo-relative path.
    Exact(String),
    /// This directory and everything under it. Stored with its trailing `/`.
    Prefix(String),
}

impl Pattern {
    pub fn matches(&self, path: &str) -> bool {
        match self {
            Pattern::Exact(p) => path == p,
            Pattern::Prefix(d) => path.starts_with(d.as_str()),
        }
    }

    /// The stored form: a prefix keeps its trailing `/`, which is also what
    /// distinguishes the two kinds on disk.
    pub fn as_str(&self) -> &str {
        match self {
            Pattern::Exact(p) => p,
            Pattern::Prefix(d) => d,
        }
    }

    pub fn is_prefix(&self) -> bool {
        matches!(self, Pattern::Prefix(_))
    }

    /// Reads back a stored pattern; a trailing `/` means a prefix.
    pub fn from_stored(s: &str, prefix: bool) -> Self {
        if prefix {
            Pattern::Prefix(s.to_string())
        } else {
            Pattern::Exact(s.to_string())
        }
    }
}

/// The separator between the two halves of a stored [`PathMap`]; no path
/// contains it.
const SEP: char = '\u{0}';

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
    /// `load()` it. See [`string_literals`].
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
}

// ------------------------------------------------------------ MODULE.bazel

/// One `bazel_dep`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dep {
    pub module: String,
    pub version: String,
    /// The apparent name the main repository sees it under.
    pub repo_name: String,
}

/// One `use_extension` plus the tags and `use_repo` calls against it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extension {
    /// The `.bzl` label the extension is defined in, as written.
    pub bzl: String,
    pub name: String,
    /// Apparent repository names this extension is asked for, from `use_repo`.
    pub repos: BTreeSet<String>,
    /// Canonical rendering of each tag call, for diffing.
    pub tags: Vec<String>,
    /// Main-repo labels named in the tag calls.
    pub labels: Vec<String>,
}

/// One `use_repo_rule` plus its instantiations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoRule {
    pub bzl: String,
    pub rule: String,
    /// The `name =` of each instantiation.
    pub repos: BTreeSet<String>,
    pub calls: Vec<String>,
    pub labels: Vec<String>,
}

/// The structured content of a `MODULE.bazel`.
///
/// Only what a diff needs: everything else is deliberately dropped, and
/// anything the parser did not recognise lands in [`ModuleFacts::unparsed`],
/// which forces a global invalidation rather than a guess.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleFacts {
    /// `module(name = ..., version = ...)`, rendered.
    pub module: String,
    /// The root module's own name, which is also an apparent name for the
    /// main repository.
    pub module_name: String,
    /// By apparent repository name.
    pub deps: BTreeMap<String, Dep>,
    /// Module name -> rendered override call.
    pub overrides: BTreeMap<String, String>,
    /// Module name -> the main-repo directory a `local_path_override` points
    /// at, with its trailing `/`.
    pub local_paths: BTreeMap<String, String>,
    /// Repository name -> the main-repo directory a `local_repository` or
    /// `new_local_repository` instantiation points at, with its trailing `/`.
    /// A `use_repo_rule` shape; the `.bzl` scanner finds the WORKSPACE one.
    pub local_repo_paths: BTreeMap<String, String>,
    /// Keyed by `bzl\0name`.
    pub extensions: BTreeMap<String, Extension>,
    /// Keyed by `bzl\0rule`.
    pub repo_rules: BTreeMap<String, RepoRule>,
    /// `register_toolchains` / `register_execution_platforms` arguments, in
    /// order. Toolchain resolution has no edge in the graph, so any movement
    /// here is global.
    pub registrations: Vec<String>,
    /// Top-level statements the parser did not recognise, rendered. A run that
    /// sees one of these on either side of a diff goes global.
    pub unparsed: Vec<String>,
}

/// Modules whose version decides how *every* configured target is built, not
/// what any particular target depends on.
///
/// A toolchain is resolved, not depended on: `bazel query` shows no edge from
/// a `cc_library` to the C++ toolchain that will compile it, so no reverse
/// walk can find the targets a toolchain change moves. A version bump of one
/// of these therefore has to invalidate everything, and the list is explicit
/// rather than heuristic so that each entry can carry its reason.
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

/// What a `MODULE.bazel` edit means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleChange {
    /// Nothing a run can localise: invalidate everything, for this reason.
    Global(String),
    /// Exactly these apparent repository names moved.
    Repos(BTreeSet<String>),
}

// ------------------------------------------------------------- the parser

/// A call's arguments, flattened enough for the questions asked of them.
#[derive(Debug, Default)]
struct Args {
    positional: Vec<String>,
    /// Keyword arguments in source order; the value is every string literal
    /// inside it, so a list or a dict argument yields all of its entries.
    kw: Vec<(String, Vec<String>)>,
}

impl Args {
    fn first(&self, key: &str) -> Option<&str> {
        self.kw
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.first())
            .map(String::as_str)
    }

    /// Every string literal in the call, keyword and positional alike.
    fn strings(&self) -> impl Iterator<Item = &String> {
        self.positional
            .iter()
            .chain(self.kw.iter().flat_map(|(_, v)| v.iter()))
    }

    /// A stable rendering, for comparing two revisions of the same call.
    fn render(&self, head: &str) -> String {
        let mut s = String::from(head);
        s.push('(');
        for p in &self.positional {
            s.push_str(p);
            s.push(',');
        }
        for (k, v) in &self.kw {
            s.push_str(k);
            s.push('=');
            s.push_str(&v.join("|"));
            s.push(',');
        }
        s.push(')');
        s
    }
}

/// A top-level statement, as a token slice plus the shape the parser found.
struct Stmt<'a, 't> {
    toks: &'t [Lexed<'a>],
}

/// Splits a token stream into top-level statements: every token that starts a
/// line at bracket depth zero begins one.
fn statements<'a, 't>(toks: &'t [Lexed<'a>]) -> Vec<Stmt<'a, 't>> {
    top_level_spans(toks)
        .into_iter()
        .map(|r| Stmt { toks: &toks[r] })
        .collect()
}

/// Arguments of the call whose `(` is at `open`, which must be `Tok::Open`.
/// The arguments of the call whose `(` is at `open`, with each argument
/// flattened to the string literals inside it.
///
/// Argument splitting is [`arg_spans`]'s job — it is the same walk, and having
/// two of them meant two chances to disagree about what starts a keyword
/// argument. This adds only the flattening: a list or dict argument yields
/// every literal in it, at any nesting depth, which is what the callers ask
/// of `Args`.
fn parse_args(text: &str, toks: &[Lexed<'_>], open: usize) -> Args {
    let mut out = Args::default();
    for (key, span) in arg_spans(toks, open) {
        let vals = toks[span].iter().filter_map(|t| match t.tok {
            Tok::Str(s, e) => Some(unescape(&text[s..e])),
            _ => None,
        });
        match key {
            Some(k) => out.kw.push((k, vals.collect())),
            None => out.positional.extend(vals),
        }
    }
    out
}

/// Statements a `MODULE.bazel` may contain that this parser deliberately
/// ignores rather than treating as unrecognised.
const HARMLESS: &[&str] = &["load", "print", "fail", "module", "include"];

/// Reads a `MODULE.bazel`'s structure.
///
/// The parser recognises the fixed grammar `MODULE.bazel` actually has —
/// bzlmod forbids control flow and function definitions there — and records
/// anything else in [`ModuleFacts::unparsed`] so the engine can escalate
/// rather than silently under-approximate.
pub fn scan_module(text: &str) -> ModuleFacts {
    let toks = lex(text);
    let mut f = ModuleFacts::default();
    // Variable name -> the extension / repo-rule key it was bound to.
    let mut ext_vars: BTreeMap<String, String> = BTreeMap::new();
    let mut rule_vars: BTreeMap<String, String> = BTreeMap::new();

    for st in statements(&toks) {
        let t = st.toks;
        // `x = f(...)`
        let (target, head) = match (t.first().map(|x| x.tok), t.get(1).map(|x| x.tok)) {
            (Some(Tok::Name(v)), Some(Tok::Punct(b'='))) => (Some(v), 2usize),
            _ => (None, 0usize),
        };
        let Some(Tok::Name(name)) = t.get(head).map(|x| x.tok) else {
            // A list comprehension of tag calls — `[maven.artifact(...) for a
            // in ARTIFACTS]`, which `bazel/bazel` writes twice. The loop body
            // cannot be evaluated statically, but it does not have to be: what
            // the diff needs is a rendering that moves when the statement's
            // text moves, and the extension it belongs to.
            if let Some(key) = comprehension_target(t, &ext_vars) {
                let e = f.extensions.entry(key).or_default();
                e.tags.push(render_limited(text, t, usize::MAX));
                e.labels.extend(statement_strings(text, t));
                continue;
            }
            // A bare string (a docstring) or a comment-only line.
            if t.iter()
                .any(|x| matches!(x.tok, Tok::Name(_) | Tok::Open | Tok::Close))
            {
                f.unparsed.push(render_stmt(text, t));
            }
            continue;
        };
        // `x.tag(...)`
        let (dotted, call_at) = match (
            t.get(head + 1).map(|x| x.tok),
            t.get(head + 2).map(|x| x.tok),
        ) {
            (Some(Tok::Punct(b'.')), Some(Tok::Name(m))) => (Some(m), head + 3),
            _ => (None, head + 1),
        };
        if !matches!(t.get(call_at).map(|x| x.tok), Some(Tok::Open)) {
            if !HARMLESS.contains(&name) {
                f.unparsed.push(render_stmt(text, t));
            }
            continue;
        }
        let args = parse_args(text, t, call_at);

        if let Some(method) = dotted {
            // A tag call on an extension proxy.
            let Some(key) = ext_vars.get(name) else {
                f.unparsed.push(render_stmt(text, t));
                continue;
            };
            let e = f.extensions.entry(key.clone()).or_default();
            e.tags.push(args.render(method));
            e.labels.extend(args.strings().cloned());
            continue;
        }

        match (target, name) {
            (Some(v), "use_extension") => {
                let bzl = args.positional.first().cloned().unwrap_or_default();
                let ext = args.positional.get(1).cloned().unwrap_or_default();
                let key = format!("{bzl}\u{0}{ext}");
                let e = f.extensions.entry(key.clone()).or_default();
                e.bzl = bzl;
                e.name = ext;
                ext_vars.insert(v.to_string(), key);
            }
            (Some(v), "use_repo_rule") => {
                let bzl = args.positional.first().cloned().unwrap_or_default();
                let rule = args.positional.get(1).cloned().unwrap_or_default();
                let key = format!("{bzl}\u{0}{rule}");
                let r = f.repo_rules.entry(key.clone()).or_default();
                r.bzl = bzl;
                r.rule = rule;
                rule_vars.insert(v.to_string(), key);
            }
            (None, "use_repo") => {
                // `use_repo(proxy, "a", apparent = "b")`: both the positional
                // names and the *values* of the keyword aliases are repository
                // names; the keyword is the apparent name the main repository
                // sees, and both are worth recording.
                let Some(Tok::Name(var)) = t.get(call_at + 1).map(|x| x.tok) else {
                    f.unparsed.push(render_stmt(text, t));
                    continue;
                };
                let Some(key) = ext_vars.get(var) else {
                    f.unparsed.push(render_stmt(text, t));
                    continue;
                };
                let e = f.extensions.entry(key.clone()).or_default();
                for (k, v) in &args.kw {
                    e.repos.insert(k.clone());
                    e.repos.extend(v.iter().cloned());
                }
                e.repos.extend(args.positional.iter().cloned());
            }
            (None, "bazel_dep") => {
                let module = args.first("name").unwrap_or_default().to_string();
                let version = args.first("version").unwrap_or_default().to_string();
                let repo_name = args
                    .first("repo_name")
                    .map(String::from)
                    .unwrap_or_else(|| module.clone());
                f.deps.insert(
                    repo_name.clone(),
                    Dep {
                        module,
                        version,
                        repo_name,
                    },
                );
            }
            (None, "module") => {
                f.module = args.render("module");
                f.module_name = args.first("name").unwrap_or_default().to_string();
            }
            (None, "register_toolchains") | (None, "register_execution_platforms") => {
                f.registrations.push(args.render(name));
            }
            (None, n) if n.ends_with("_override") => {
                let m = args.first("module_name").unwrap_or_default().to_string();
                f.overrides.insert(m.clone(), args.render(n));
                if n == "local_path_override" {
                    if let Some(d) = args.first("path").and_then(local_dir) {
                        f.local_paths.insert(m, d);
                    }
                }
            }
            (None, n) if HARMLESS.contains(&n) => {}
            (Some(_), n) if rule_vars.contains_key(n) => {
                // Assigning a repo-rule proxy to another name; follow it.
                let key = rule_vars[n].clone();
                if let Some(v) = target {
                    rule_vars.insert(v.to_string(), key);
                }
            }
            (None, n) if rule_vars.contains_key(n) => {
                let key = rule_vars[n].clone();
                let rule = key.split(SEP).nth(1).unwrap_or("").to_string();
                let r = f.repo_rules.entry(key).or_default();
                r.calls.push(args.render(n));
                if let Some(name) = args.first("name") {
                    r.repos.insert(name.to_string());
                    // `local_repository(name = "x", path = "dir")` is a
                    // directory of the main repository under another name:
                    // exactly the mapping the engine needs to decide whether
                    // an edit under `dir` could be a `.bzl` the repository
                    // exports.
                    if is_local_repo_rule(&rule) {
                        if let Some(d) = args.first("path").and_then(local_dir) {
                            f.local_repo_paths.insert(name.to_string(), d);
                        }
                    }
                }
                r.labels.extend(args.strings().cloned());
            }
            _ => f.unparsed.push(render_stmt(text, t)),
        }
    }
    f
}

/// The extension a bracketed statement's tag calls belong to, when every
/// `x.tag(` in it names the same known extension proxy.
fn comprehension_target(t: &[Lexed<'_>], ext_vars: &BTreeMap<String, String>) -> Option<String> {
    let mut found: Option<String> = None;
    for w in t.windows(4) {
        let (Tok::Name(v), Tok::Punct(b'.'), Tok::Name(_), Tok::Open) =
            (w[0].tok, w[1].tok, w[2].tok, w[3].tok)
        else {
            continue;
        };
        // Other calls in the same statement — `coord.split(":")` in
        // `bazel/bazel`'s nested comprehension — are not tag calls.
        let Some(key) = ext_vars.get(v) else {
            continue;
        };
        if found.get_or_insert_with(|| key.clone()) != key {
            return None;
        }
    }
    found
}

/// Every string literal in a statement.
fn statement_strings(text: &str, t: &[Lexed<'_>]) -> Vec<String> {
    t.iter()
        .filter_map(|x| match x.tok {
            Tok::Str(a, b) => Some(unescape(&text[a..b])),
            _ => None,
        })
        .collect()
}

/// A textual rendering of a statement. `limit` truncates it for a
/// human-facing message; a rendering that is *compared* must not be
/// truncated, or a change past the cut would be invisible.
fn render_limited(text: &str, t: &[Lexed<'_>], limit: usize) -> String {
    let mut s = String::new();
    for x in t.iter().take(limit) {
        match x.tok {
            Tok::Name(n) => s.push_str(n),
            Tok::Str(a, b) => {
                s.push('"');
                s.push_str(&text[a..b]);
                s.push('"');
            }
            Tok::Open => s.push('('),
            Tok::Close => s.push(')'),
            Tok::Punct(p) => s.push(p as char),
            Tok::Other => s.push('?'),
        }
    }
    s
}

fn render_stmt(text: &str, t: &[Lexed<'_>]) -> String {
    render_limited(text, t, 24)
}

// ------------------------------------------------------- the path mapping

/// Repository rules whose `path` attribute is a main-repo directory that
/// becomes the repository's root.
fn is_local_repo_rule(rule: &str) -> bool {
    matches!(rule, "local_repository" | "new_local_repository")
}

/// A `path =` attribute as a main-repo directory prefix, when it is one.
///
/// A path outside the repository is not a main-repo input: `..` cannot be
/// matched against a diff, and neither can an absolute path.
fn local_dir(p: &str) -> Option<String> {
    let p = p.trim_end_matches('/');
    let p = p.strip_prefix("./").unwrap_or(p);
    (!p.is_empty() && !p.starts_with("..") && !p.starts_with('/')).then(|| format!("{p}/"))
}

/// The path mappings a workspace-closure `.bzl` declares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BzlPathMaps {
    /// `local_repository(name = ..., path = ...)`: the call names its own
    /// repository, so no attribution is needed.
    pub named: Vec<(String, PathMap)>,
    /// `repository_ctx.symlink(<main-repo path or label>, "<dest>")`: the call
    /// says nothing about which repository it builds, so these are attributed
    /// from the position of the call, which is inside the implementation of
    /// the rule that makes the mapping true. The token index of the call's
    /// `(` comes back alongside so the caller can ask for that attribution.
    pub symlinks: Vec<(usize, PathMap)>,
}

/// Names a `symlink` source argument may contain and still be a literal.
///
/// `ctx.symlink(ctx.path(Label("//api:BUILD")), "BUILD")` is a path this
/// parser can follow. `ctx.symlink(ctx.path(ctx.attr.root).dirname.get_child(d), d)`
/// — the shape envoy's `bazel/api_binding.bzl` used before it moved to
/// `local_path_override` — is not, and is deliberately left unmapped: a
/// mapping this module guesses wrong would *hide* a `.bzl` edit, where no
/// mapping at all merely keeps the old, wider answer.
const PATHY_NAMES: &[&str] = &[
    "Label",
    "ctx",
    "path",
    "rctx",
    "repo_ctx",
    "repository_ctx",
    "repository_context",
];

/// Reads the path mappings out of one workspace-closure `.bzl`.
pub fn scan_bzl_path_maps(text: &str, m: &RepoMapping) -> BzlPathMaps {
    let toks = lex(text);
    let mut out = BzlPathMaps::default();
    for i in 0..toks.len() {
        let Tok::Name(name) = toks[i].tok else {
            continue;
        };
        if !matches!(toks.get(i + 1).map(|t| t.tok), Some(Tok::Open)) {
            continue;
        }
        if is_local_repo_rule(name) {
            let args = parse_args(text, &toks, i + 1);
            if let (Some(n), Some(d)) = (args.first("name"), args.first("path").and_then(local_dir))
            {
                out.named.push((n.to_string(), PathMap::exhaustive_dir(d)));
            }
            continue;
        }
        // `x.symlink(...)`, on any receiver: only a `repository_ctx` has one.
        if name != "symlink"
            || !matches!(
                toks.get(i.wrapping_sub(1)).map(|t| t.tok),
                Some(Tok::Punct(b'.'))
            )
        {
            continue;
        }
        let spans = arg_spans(&toks, i + 1);
        let pick = |kw: &str, pos: usize| -> Option<std::ops::Range<usize>> {
            spans
                .iter()
                .find(|(k, _)| k.as_deref() == Some(kw))
                .or_else(|| spans.iter().filter(|(k, _)| k.is_none()).nth(pos))
                .map(|(_, r)| r.clone())
        };
        let (Some(src), Some(dst)) = (pick("target", 0), pick("link_name", 1)) else {
            continue;
        };
        let (Some(src), Some(dst)) = (
            only_literal(text, &toks, src, PATHY_NAMES),
            only_literal(text, &toks, dst, &[]),
        ) else {
            continue;
        };
        if let Some(pm) = symlink_map(&src, &dst, m) {
            out.symlinks.push((i + 1, pm));
        }
    }
    out
}

/// The single string literal of an argument that is *nothing but* that
/// literal, optionally wrapped in calls whose names are all in `allowed`.
fn only_literal(
    text: &str,
    toks: &[Lexed<'_>],
    span: std::ops::Range<usize>,
    allowed: &[&str],
) -> Option<String> {
    let mut lit = None;
    for t in &toks[span] {
        match t.tok {
            Tok::Str(a, b) => {
                if lit.is_some() {
                    return None;
                }
                lit = Some(unescape(&text[a..b]));
            }
            Tok::Name(n) if allowed.contains(&n) => {}
            Tok::Open | Tok::Close => {}
            Tok::Punct(b'.') => {}
            _ => return None,
        }
    }
    lit
}

/// One `symlink(source, destination)` as a path mapping, when both ends are
/// unambiguous.
///
/// A source that ends in `/` is a directory and rewrites everything under it;
/// anything else is taken to be exactly one file, which is the conservative
/// reading — a directory misread as a file simply fails to translate the paths
/// beneath it, and a path that does not translate keeps the wide answer.
/// A label with no target (`//api`) is rejected for the same reason: it names
/// the target `//api:api`, not the directory, and guessing otherwise would
/// rewrite paths that do not belong to it.
fn symlink_map(src: &str, dst: &str, m: &RepoMapping) -> Option<PathMap> {
    if dst.starts_with('/') || dst.split('/').any(|s| s == "..") {
        return None;
    }
    let dst = dst.trim_start_matches("./").trim_end_matches('/');
    let from = if src.contains("//") {
        let (pkg, name) = main_repo_target(src, m)?;
        if pkg.is_empty() {
            name
        } else {
            format!("{pkg}/{name}")
        }
    } else if src.starts_with('/') || src.split('/').any(|s| s == ".." || s == ".") {
        return None;
    } else {
        src.to_string()
    };
    if from.is_empty() {
        return None;
    }
    match from.strip_suffix('/') {
        // A directory: the destination is a directory too, and an empty one
        // means the repository root.
        Some(_) => Some(PathMap {
            from,
            to: if dst.is_empty() {
                String::new()
            } else {
                format!("{dst}/")
            },
            exhaustive: false,
        }),
        None => (!dst.is_empty()).then(|| PathMap {
            from,
            to: dst.to_string(),
            exhaustive: false,
        }),
    }
}

/// `//pkg:name` in the main repository, split. `None` for any other label.
fn main_repo_target(label: &str, m: &RepoMapping) -> Option<(String, String)> {
    let rest = match label.strip_prefix("@@") {
        Some(r) => r.strip_prefix("//")?,
        None => match label.strip_prefix('@') {
            Some(r) => {
                let (repo, rest) = r.split_once("//")?;
                if !m.is_main(repo) {
                    return None;
                }
                rest
            }
            None => label.strip_prefix("//")?,
        },
    };
    let (pkg, name) = rest.split_once(':')?;
    (!name.is_empty() && !pkg.starts_with('/')).then(|| (pkg.to_string(), name.to_string()))
}

// ---------------------------------------------------------------- the diff

/// What changed between two revisions of a `MODULE.bazel`.
///
/// Every rule here answers the same question: can a reverse walk over the
/// main-repo graph find the targets this change moves? If it can, the change
/// names repositories; if it cannot, it is global and says why.
pub fn diff_module(base: &ModuleFacts, head: &ModuleFacts) -> ModuleChange {
    if !base.unparsed.is_empty() || !head.unparsed.is_empty() {
        let what = head.unparsed.first().or(base.unparsed.first()).unwrap();
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

    // Extensions: a changed tag or a changed `use_repo` moves the repositories
    // on both sides of the change.
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

fn apparent_of(f: &ModuleFacts, module: &str) -> Option<String> {
    f.deps
        .values()
        .find(|d| d.module == module)
        .map(|d| d.repo_name.clone())
}

// ------------------------------------------------------ building the inputs

/// True if a label string names something in the main repository, and what
/// path patterns it points at.
///
/// `//pkg:file.txt` is one file. `//pkg` (no target) is a whole package, so it
/// is a prefix. `//pkg:name` where the name carries no extension is most
/// likely a *rule*, whose own sources are not visible from here, so it is a
/// prefix too — the alternative is to miss every file that rule globs.
pub fn main_repo_patterns(label: &str, m: &RepoMapping) -> Vec<Pattern> {
    let rest = match label.strip_prefix("@@") {
        Some(r) => match r.split_once("//") {
            Some(("", rest)) => rest,
            _ => return Vec::new(),
        },
        None => match label.strip_prefix('@') {
            Some(r) => match r.split_once("//") {
                Some((repo, rest)) if m.is_main(repo) => rest,
                _ => return Vec::new(),
            },
            None => match label.strip_prefix("//") {
                Some(rest) => rest,
                None => return Vec::new(),
            },
        },
    };
    // `rest` is now `pkg:name` or `pkg`, without the leading `//`.
    if rest.contains(|c: char| c.is_whitespace()) {
        return Vec::new();
    }
    let (pkg, name) = match rest.split_once(':') {
        Some((p, n)) => (p, Some(n)),
        None => (rest, None),
    };
    let dir = if pkg.is_empty() {
        String::new()
    } else {
        format!("{pkg}/")
    };
    match name {
        None => {
            if pkg.is_empty() {
                // `//` alone would claim the whole repository; that is what a
                // global invalidation is for, not a pattern.
                Vec::new()
            } else {
                vec![Pattern::Prefix(dir)]
            }
        }
        Some("") => Vec::new(),
        Some(n) => {
            let path = format!("{dir}{n}");
            let mut v = vec![Pattern::Exact(path)];
            // A name with no extension is a rule, not a file.
            if !n.contains('.') && !pkg.is_empty() {
                v.push(Pattern::Prefix(dir));
            }
            v
        }
    }
}

/// Computes the repository-input map for a checkout.
///
/// `graph` is the load graph the closure walk already built, so the `.bzl`
/// files here are read from the same memoised parse; `workspace` is that
/// walk's workspace closure.
pub fn build_repo_inputs(
    src: &dyn FileText,
    m: &RepoMapping,
    graph: &LoadGraph,
    workspace: &[u32],
    facts: &ModuleFacts,
) -> RepoInputs {
    let mut out = RepoInputs::default();
    // `@envoy//...` inside envoy is a main-repo label; see
    // [`RepoMapping::with_main_alias`].
    let m = &m.clone().with_main_alias(&facts.module_name);

    // Seed: which `.bzl` each extension / repo rule is defined in, and the
    // repositories that come out of it.
    let mut seeds: Vec<(String, BTreeSet<String>)> = Vec::new();
    for e in facts.extensions.values() {
        if let Some(p) = main_bzl_path(&e.bzl, m) {
            seeds.push((p, e.repos.clone()));
        }
        for l in &e.labels {
            for pat in main_repo_patterns(l, m) {
                out.patterns
                    .entry(pat)
                    .or_default()
                    .extend(e.repos.iter().cloned());
            }
        }
        out.repos.extend(e.repos.iter().cloned());
    }
    for r in facts.repo_rules.values() {
        if let Some(p) = main_bzl_path(&r.bzl, m) {
            seeds.push((p, r.repos.clone()));
        }
        for l in &r.labels {
            for pat in main_repo_patterns(l, m) {
                out.patterns
                    .entry(pat)
                    .or_default()
                    .extend(r.repos.iter().cloned());
            }
        }
        out.repos.extend(r.repos.iter().cloned());
    }
    // `local_path_override(module_name = "envoy_api", path = "api")`: the whole
    // directory is that module's source, and its root is that directory.
    for (module, path) in &facts.local_paths {
        let repo = apparent_of(facts, module).unwrap_or_else(|| module.clone());
        out.repos.insert(repo.clone());
        out.patterns
            .entry(Pattern::Prefix(path.clone()))
            .or_default()
            .insert(repo.clone());
        out.path_maps
            .entry(repo)
            .or_default()
            .insert(PathMap::exhaustive_dir(path.clone()));
    }
    // The same shape written as a repository rule in `MODULE.bazel`.
    for (repo, path) in &facts.local_repo_paths {
        if !out.repos.contains(repo) {
            continue;
        }
        out.patterns
            .entry(Pattern::Prefix(path.clone()))
            .or_default()
            .insert(repo.clone());
        out.path_maps
            .entry(repo.clone())
            .or_default()
            .insert(PathMap::exhaustive_dir(path.clone()));
    }
    for d in facts.deps.values() {
        out.canonical
            .insert(d.repo_name.clone(), m.canonical(&d.repo_name).to_string());
    }

    // Propagate down the load graph: a helper a repository rule's `.bzl`
    // loads is part of that rule's implementation.
    let mut owner: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    for (path, repos) in &seeds {
        let Some(id) = graph.id_of(path) else {
            continue;
        };
        let mut stack = vec![id];
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        while let Some(i) = stack.pop() {
            if !seen.insert(i) {
                continue;
            }
            owner.entry(i).or_default().extend(repos.iter().cloned());
            stack.extend_from_slice(graph.loads(i));
        }
    }

    // Per-occurrence attribution. The analysis reads every file of the
    // closure, the workspace files included: a `local_repository(name = ...)`
    // written in `WORKSPACE` names a repository nothing else does, and a
    // `MODULE.bazel` tag call is a call site like any other.
    let mut texts: Vec<(String, String)> = Vec::new();
    for name in crate::loads::WORKSPACE_FILES {
        if let Some(t) = src.text(name) {
            texts.push((name.to_string(), t));
        }
    }
    for &id in workspace {
        let path = graph.path(id).to_string();
        if let Some(t) = src.text(&path) {
            texts.push((path, t));
        }
    }
    let mut att = Attribution::scan(&texts, m);
    // The universe a fallback ranges over: every repository `MODULE.bazel`
    // names, plus every literal `name =` written anywhere in the closure. On a
    // WORKSPACE-only repository the first is empty and the second is the whole
    // answer, which is what turns "no idea, invalidate everything" into a set.
    out.repos.extend(att.literal_names.iter().cloned());
    let fallbacks: BTreeMap<String, BTreeSet<String>> = texts
        .iter()
        .map(|(p, _)| {
            let repos = match graph.id_of(p).and_then(|id| owner.get(&id)) {
                Some(r) if !r.is_empty() => r.clone(),
                _ => out.repos.clone(),
            };
            (p.clone(), repos)
        })
        .collect();
    let mut seeds_by_sym: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for e in facts.extensions.values() {
        if let Some(p) = main_bzl_path(&e.bzl, m) {
            seeds_by_sym
                .entry((p, e.name.clone()))
                .or_default()
                .extend(e.repos.iter().cloned());
        }
    }
    for r in facts.repo_rules.values() {
        if let Some(p) = main_bzl_path(&r.bzl, m) {
            seeds_by_sym
                .entry((p, r.rule.clone()))
                .or_default()
                .extend(r.repos.iter().cloned());
        }
    }
    att.solve(&fallbacks, &seeds_by_sym);

    // A main-repo label in a workspace-closure `.bzl` is an input of the
    // repositories the *occurrence* is attributed to.
    //
    // Which *kind* of input is tracked alongside: everything derived so far
    // (`MODULE.bazel` shapes) and every literal that is itself a label feeds
    // the rule; a label embedded in a larger literal is text the rule writes.
    // A pattern is target-only when no occurrence of it was the first kind.
    let mut direct: BTreeSet<Pattern> = out.patterns.keys().cloned().collect();
    let mut embedded: BTreeSet<Pattern> = BTreeSet::new();
    // A `.bzl` a repository rule loads is part of that rule's implementation,
    // so a change to it moves everything the loader can define. That is what
    // the load-graph propagation above says, and no per-occurrence reading
    // narrows it: this file may export a constant some other file's rule
    // reads. So the file-level answer stays the union of the two.
    let mut own: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for &id in workspace {
        let path = graph.path(id).to_string();
        let Some(text) = src.text(&path) else {
            continue;
        };
        let mut file_repos = att.file_repos(&path).repos;
        for (tok, l) in string_literals(&text) {
            let pats = main_repo_patterns(&l.text, m);
            if pats.is_empty() {
                continue;
            }
            let a = att.at(&path, tok);
            if a.repos.is_empty() {
                continue;
            }
            file_repos.extend(a.repos.iter().cloned());
            // A generated `.bzl` can `load()` a main-repo `.bzl` it names, so
            // an embedded `.bzl` label is still a real input.
            let feeds_rule = !l.embedded || l.text.ends_with(".bzl");
            for pat in pats {
                if feeds_rule {
                    direct.insert(pat.clone());
                } else {
                    embedded.insert(pat.clone());
                }
                out.patterns
                    .entry(pat)
                    .or_default()
                    .extend(a.repos.iter().cloned());
            }
        }
        own.insert(path, file_repos);
    }
    // Path mappings, from the workspace files as well as the `.bzl` closure.
    // A `local_repository` call names its own repository; a `symlink` does
    // not, so it is attributed from the position of the call — inside the
    // implementation of the rule the mapping is true of — and dropped when
    // that position is unattributable, rather than being handed to every
    // repository the main repository defines.
    for (path, text) in &texts {
        let maps = scan_bzl_path_maps(text, m);
        for (repo, pm) in maps.named {
            if out.repos.contains(&repo) {
                let pat = Pattern::Prefix(pm.from.clone());
                direct.insert(pat.clone());
                out.patterns.entry(pat).or_default().insert(repo.clone());
                own.entry(path.clone()).or_default().insert(repo.clone());
                out.path_maps.entry(repo).or_default().insert(pm);
            }
        }
        for (tok, pm) in &maps.symlinks {
            let a = att.at(path, *tok);
            if !a.exact || a.repos.is_empty() {
                continue;
            }
            for repo in &a.repos {
                out.path_maps
                    .entry(repo.clone())
                    .or_default()
                    .insert(pm.clone());
            }
        }
    }
    // On a WORKSPACE-only repository there are no seeds at all, so the
    // load-graph propagation says nothing and the fallback used to be the
    // empty set — which the engine reads as "no idea", and goes global. What
    // a `.bzl` there can define is instead its own attributed set plus that of
    // every file that loads it: if `bazel/repositories.bzl` changes, the
    // repositories `envoy_dependencies()` declares move, and so do those of
    // the helpers it loads, but the pips of `bazel/python_dependencies.bzl` do
    // not. That holds only while the workspace files themselves are readable:
    // a `local_repository(name = SOME_CONSTANT)` in `WORKSPACE` would define a
    // repository no file can be held responsible for, and then every file
    // keeps the whole set.
    let workspace_only = *facts == ModuleFacts::default();
    let readable = !crate::loads::WORKSPACE_FILES
        .iter()
        .any(|p| src.text(p).is_some() && att.module_is_dynamic(p));
    let mut prop: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    if workspace_only && readable {
        for &id in workspace {
            prop.insert(id, own.get(graph.path(id)).cloned().unwrap_or_default());
        }
        let mut queue: Vec<u32> = workspace.to_vec();
        while let Some(id) = queue.pop() {
            let from = prop.get(&id).cloned().unwrap_or_default();
            for &j in graph.loads(id) {
                let e = prop.entry(j).or_default();
                let before = e.len();
                e.extend(from.iter().cloned());
                if e.len() != before {
                    queue.push(j);
                }
            }
        }
    }
    for &id in workspace {
        let path = graph.path(id).to_string();
        let narrow = prop.get(&id).filter(|r| !r.is_empty());
        let mut repos = match (owner.get(&id), narrow) {
            (Some(r), _) if !r.is_empty() => r.clone(),
            (_, Some(r)) => r.clone(),
            _ => out.repos.clone(),
        };
        repos.extend(own.remove(&path).unwrap_or_default());
        out.bzl_repos.insert(path, repos);
    }
    out.target_only = embedded.difference(&direct).cloned().collect();

    out
}

/// The repo-relative path of a `.bzl` label in the main repository.
fn main_bzl_path(label: &str, m: &RepoMapping) -> Option<String> {
    match crate::loads::resolve_label(label, "", m) {
        Some(crate::loads::Load::Main(p)) => Some(p),
        _ => None,
    }
}

/// Every label-shaped token inside a Starlark file's string literals.
///
/// Two levels, because repository rules use both. A label is usually a literal
/// of its own (`attr.label(default = "@envoy//:.github/config.yml")`), but the
/// other half of the idiom is a repository rule that *writes a BUILD file*:
///
/// ```text
/// rctx.file("BUILD", '''
/// genrule(
///     tools = ["@envoy//:VERSION.txt", "@envoy//changelogs"],
/// )
/// ''')
/// ```
///
/// That whole block is one string literal, and the labels the generated
/// repository will depend on are inside it. So each literal is scanned for
/// label-shaped substrings as well as being offered whole.
///
/// The two are not the same kind of input, and the flag says which is which.
/// A literal that *is* a label is one the rule resolves: Bazel reads the file
/// and the rule may write anything it likes from it, `.bzl` files included. A
/// label found *inside* a larger literal is text the rule writes into the
/// repository, and what it names is consumed by the repository's targets at
/// build time — `@envoy_repo//:project` lists `@envoy//changelogs` in its
/// `tools`. That file's content cannot move a `.bzl` the repository exports,
/// which is what lets the engine skip re-querying the twelve hundred envoy
/// packages whose load closure reaches `@envoy_repo//:compiler.bzl` on every
/// commit that adds a changelog entry.
///
/// Comments and nesting are handled by the shared lexer, so a `//` in a
/// comment is not a label. A `//` preceded by `:` or `/` is not either, which
/// is what keeps `https://github.com/envoyproxy/envoy` out.
/// Each literal comes back with the index of its token, which is what the
/// per-occurrence attribution is keyed on.
fn string_literals(text: &str) -> Vec<(usize, Literal)> {
    let mut out = Vec::new();
    for (i, t) in lex(text).into_iter().enumerate() {
        let Tok::Str(s, e) = t.tok else {
            continue;
        };
        let lit = unescape(&text[s..e]);
        for l in labels_in(&lit) {
            if l != lit {
                out.push((
                    i,
                    Literal {
                        text: l,
                        embedded: true,
                    },
                ));
            }
        }
        out.push((
            i,
            Literal {
                text: lit,
                embedded: false,
            },
        ));
    }
    out
}

/// One string literal of a Starlark file, or a label found inside one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Literal {
    text: String,
    /// `true` for a label-shaped substring of a larger literal: text the
    /// repository rule writes, not a label it resolves.
    embedded: bool,
}

fn is_label_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b'+' | b'~' | b'/' | b':' | b'@')
}

/// Label-shaped substrings of a blob of text.
fn labels_in(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < b.len() {
        if b[i] != b'/' || b[i + 1] != b'/' {
            i += 1;
            continue;
        }
        // `https://`, and a path that merely contains `//`.
        if i > 0 && matches!(b[i - 1], b':' | b'/') {
            i += 2;
            continue;
        }
        // Expand left over an `@repo` prefix, if there is one.
        let mut start = i;
        while start > 0 && is_label_byte(b[start - 1]) && b[start - 1] != b'/' {
            start -= 1;
        }
        if start != i && b[start] != b'@' {
            i += 2;
            continue;
        }
        let mut end = i + 2;
        while end < b.len() && is_label_byte(b[end]) {
            end += 1;
        }
        if let Ok(l) = std::str::from_utf8(&b[start..end]) {
            out.push(l.trim_end_matches(['.', ':', '-']).to_string());
        }
        i = end.max(i + 2);
    }
    out
}

/// Reads `MODULE.bazel` out of a checkout. Absent (a WORKSPACE-only
/// repository) yields empty facts, which diff to no change.
pub fn read_module_facts(src: &dyn FileText) -> ModuleFacts {
    match src.text("MODULE.bazel") {
        Some(t) => scan_module(&t),
        None => ModuleFacts::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> RepoMapping {
        RepoMapping::from_dump("{\"\":\"\",\"envoy\":\"\",\"envoy_api\":\"envoy_api+\"}")
    }

    #[test]
    fn parses_bazel_deps_and_overrides() {
        let f = scan_module(
            "module(name = \"envoy\", version = \"1.0\")\n\
             bazel_dep(name = \"envoy_api\", version = \"1.40.0-dev\")\n\
             bazel_dep(name = \"apple_support\", version = \"2.3.0\", repo_name = \"build_bazel_apple_support\")\n\
             local_path_override(\n    module_name = \"envoy_api\",\n    path = \"api\",\n)\n",
        );
        assert!(f.unparsed.is_empty(), "{:?}", f.unparsed);
        assert_eq!(f.deps["envoy_api"].version, "1.40.0-dev");
        assert_eq!(f.deps["build_bazel_apple_support"].module, "apple_support");
        assert_eq!(f.local_paths["envoy_api"], "api/");
    }

    #[test]
    fn parses_extensions_tags_and_use_repo() {
        let f = scan_module(
            "pip = use_extension(\"@rules_python//python/extensions:pip.bzl\", \"pip\")\n\
             pip.parse(\n    hub_name = \"base_pip3\",\n    requirements_lock = \"//tools/base:requirements.txt\",\n)\n\
             use_repo(pip, \"base_pip3\", alias = \"fuzzing_pip3\")\n",
        );
        assert!(f.unparsed.is_empty(), "{:?}", f.unparsed);
        let e = f
            .extensions
            .values()
            .find(|e| e.name == "pip")
            .expect("extension");
        assert_eq!(e.tags.len(), 1);
        assert!(e.repos.contains("base_pip3"));
        assert!(e.repos.contains("fuzzing_pip3"));
        assert!(e.repos.contains("alias"));
        assert!(e
            .labels
            .iter()
            .any(|l| l == "//tools/base:requirements.txt"));
    }

    #[test]
    fn parses_use_repo_rule_instantiations() {
        let f = scan_module(
            "http_archive = use_repo_rule(\"//bazel:repo.bzl\", \"http_archive\")\n\
             http_archive(name = \"thing\", build_file = \"//bazel:thing.BUILD\")\n",
        );
        assert!(f.unparsed.is_empty(), "{:?}", f.unparsed);
        let r = f.repo_rules.values().next().unwrap();
        assert_eq!(r.bzl, "//bazel:repo.bzl");
        assert!(r.repos.contains("thing"));
        assert!(r.labels.iter().any(|l| l == "//bazel:thing.BUILD"));
    }

    /// `bazel/bazel` writes two of these; before they were understood, every
    /// `MODULE.bazel` edit in that repository went global.
    #[test]
    fn a_comprehension_of_tag_calls_belongs_to_its_extension() {
        let t = "maven = use_extension(\"@rules_jvm_external//:extensions.bzl\", \"maven\")\n\
                 use_repo(maven, \"maven\")\n\
                 [\n    maven.artifact(\n        group = group,\n        artifact = artifact,\n    )\n\
                 \x20   for group, artifact in [c.split(\":\") for c in [\"a:b\", \"c:d\"]]\n]\n";
        let f = scan_module(t);
        assert!(f.unparsed.is_empty(), "{:?}", f.unparsed);
        let e = f.extensions.values().next().unwrap();
        assert_eq!(e.tags.len(), 1);
        // And a change inside the comprehension is seen as a change to it.
        let g = scan_module(&t.replace("\"a:b\"", "\"a:z\""));
        assert_eq!(
            diff_module(&f, &g),
            ModuleChange::Repos(["maven".to_string()].into_iter().collect())
        );
    }

    #[test]
    fn an_unrecognised_statement_is_recorded() {
        let f = scan_module("if x:\n    bazel_dep(name = \"a\")\n");
        assert!(!f.unparsed.is_empty());
    }

    #[test]
    fn a_version_bump_names_one_repository() {
        let base = scan_module("bazel_dep(name = \"fmt\", version = \"1.0\")\n");
        let head = scan_module("bazel_dep(name = \"fmt\", version = \"2.0\")\n");
        assert_eq!(
            diff_module(&base, &head),
            ModuleChange::Repos(["fmt".to_string()].into_iter().collect())
        );
    }

    #[test]
    fn a_toolchain_module_bump_is_global() {
        let base = scan_module("bazel_dep(name = \"rules_python\", version = \"1.0\")\n");
        let head = scan_module("bazel_dep(name = \"rules_python\", version = \"2.0\")\n");
        match diff_module(&base, &head) {
            ModuleChange::Global(r) => assert!(r.contains("rules_python"), "{r}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn register_toolchains_is_global() {
        let base = scan_module("register_toolchains(\"//a:b\")\n");
        let head = scan_module("register_toolchains(\"//a:c\")\n");
        assert!(matches!(diff_module(&base, &head), ModuleChange::Global(_)));
    }

    #[test]
    fn an_unchanged_module_names_nothing() {
        let t = "bazel_dep(name = \"fmt\", version = \"1.0\")\nregister_toolchains(\"//a:b\")\n";
        assert_eq!(
            diff_module(&scan_module(t), &scan_module(t)),
            ModuleChange::Repos(BTreeSet::new())
        );
    }

    #[test]
    fn a_changed_tag_names_the_extensions_repositories() {
        let base = scan_module(
            "pip = use_extension(\"@rules_python//python/extensions:pip.bzl\", \"pip\")\n\
             pip.parse(hub_name = \"base_pip3\", requirements_lock = \"//tools:a.txt\")\n\
             use_repo(pip, \"base_pip3\")\n",
        );
        let head = scan_module(
            "pip = use_extension(\"@rules_python//python/extensions:pip.bzl\", \"pip\")\n\
             pip.parse(hub_name = \"base_pip3\", requirements_lock = \"//tools:b.txt\")\n\
             use_repo(pip, \"base_pip3\")\n",
        );
        assert_eq!(
            diff_module(&base, &head),
            ModuleChange::Repos(["base_pip3".to_string()].into_iter().collect())
        );
    }

    /// The precision question `bazel/bazel`'s `5c47f233..bfd201d1` raised: a
    /// version bump next to two extensions that between them name half a dozen
    /// repositories must name the one repository, not all of them. (What
    /// widened that change set's answer was not this function but the input map —
    /// `//:extensions.bzl` calls `ctx.path(Label("//:MODULE.bazel"))`, so
    /// every repository it defines really is declared to depend on the file.)
    #[test]
    fn a_version_bump_does_not_name_the_other_extensions_repositories() {
        let text = |v: &str| {
            format!(
                "bazel_dep(name = \"fmt\", version = \"{v}\")\n\
                 maven = use_extension(\"@rules_jvm_external//:extensions.bzl\", \"maven\")\n\
                 maven.install(artifacts = [\"g:a:1\"])\n\
                 use_repo(maven, \"maven\", \"unpinned_maven\")\n\
                 deps = use_extension(\"//:extensions.bzl\", \"build_deps\")\n\
                 use_repo(deps, \"debian_java_deps\", \"debian_cc_deps\")\n"
            )
        };
        assert_eq!(
            diff_module(&scan_module(&text("1.0")), &scan_module(&text("2.0"))),
            ModuleChange::Repos(["fmt".to_string()].into_iter().collect())
        );
    }

    /// And a changed tag names its own extension's repositories only.
    #[test]
    fn a_changed_tag_leaves_the_other_extensions_alone() {
        let text = |a: &str| {
            format!(
                "maven = use_extension(\"@rules_jvm_external//:extensions.bzl\", \"maven\")\n\
                 maven.install(artifacts = [\"{a}\"])\n\
                 use_repo(maven, \"maven\", \"unpinned_maven\")\n\
                 deps = use_extension(\"//:extensions.bzl\", \"build_deps\")\n\
                 use_repo(deps, \"debian_java_deps\", \"debian_cc_deps\")\n"
            )
        };
        assert_eq!(
            diff_module(&scan_module(&text("g:a:1")), &scan_module(&text("g:a:2"))),
            ModuleChange::Repos(
                ["maven".to_string(), "unpinned_maven".to_string()]
                    .into_iter()
                    .collect()
            )
        );
    }

    #[test]
    fn labels_become_patterns() {
        let m = m();
        assert_eq!(
            main_repo_patterns("@envoy//:.github/config.yml", &m),
            vec![Pattern::Exact(".github/config.yml".into())]
        );
        assert_eq!(
            main_repo_patterns("@envoy//changelogs", &m),
            vec![Pattern::Prefix("changelogs/".into())]
        );
        assert_eq!(
            main_repo_patterns("//changelogs:summary", &m),
            vec![
                Pattern::Exact("changelogs/summary".into()),
                Pattern::Prefix("changelogs/".into())
            ]
        );
        // Not the main repository.
        assert!(main_repo_patterns("@envoy_api//envoy:pkg", &m).is_empty());
        assert!(main_repo_patterns("not a label", &m).is_empty());
        assert!(main_repo_patterns("//", &m).is_empty());
    }

    struct Files(Vec<(&'static str, &'static str)>);

    impl FileText for Files {
        fn text(&self, path: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, t)| t.to_string())
        }
    }

    #[test]
    fn a_repository_rules_labels_become_its_inputs() {
        let module = "envoy_repo_ext = use_extension(\"//bazel:extensions.bzl\", \"envoy_repo_extension\")\n\
                      use_repo(envoy_repo_ext, \"envoy_repo\")\n\
                      local_path_override(module_name = \"envoy_api\", path = \"api\")\n\
                      bazel_dep(name = \"envoy_api\", version = \"1.0\")\n";
        let files = Files(vec![
            ("MODULE.bazel", module),
            (
                "bazel/extensions.bzl",
                "load(\"//bazel:repo.bzl\", \"envoy_repo\")\n",
            ),
            (
                "bazel/repo.bzl",
                "X = attr.label(default = \"@envoy//:.github/config.yml\")\n\
                 Y = \"@envoy//changelogs\"\n",
            ),
        ]);
        let m = m();
        let mut g = LoadGraph::new();
        let ws = g.workspace_closure(&files, &m);
        let facts = read_module_facts(&files);
        let ri = build_repo_inputs(&files, &m, &g, &ws, &facts);

        assert_eq!(
            ri.repos_for(".github/config.yml"),
            ["envoy_repo".to_string()].into_iter().collect()
        );
        assert_eq!(
            ri.repos_for("changelogs/current/x.rst"),
            ["envoy_repo".to_string()].into_iter().collect()
        );
        assert_eq!(
            ri.repos_for("api/envoy/config/v3/x.proto"),
            ["envoy_api".to_string()].into_iter().collect()
        );
        assert!(ri
            .repos_for("source/common/http/conn_manager.cc")
            .is_empty());
        // The `.bzl` files that make up the repository rule name the repo.
        assert_eq!(
            ri.bzl_repos["bazel/repo.bzl"],
            ["envoy_repo".to_string()].into_iter().collect()
        );
        // A label the rule resolves feeds it; neither of these is target-only.
        assert!(ri.target_only.is_empty(), "{:?}", ri.target_only);
    }

    #[test]
    fn a_label_inside_a_larger_literal_is_embedded() {
        let lits = string_literals(
            "A = \"@envoy//:VERSION.txt\"\nB = 'tools = [\"@envoy//changelogs\"]'\n",
        );
        let find = |text: &str| -> Vec<bool> {
            lits.iter()
                .filter(|(_, l)| l.text == text)
                .map(|(_, l)| l.embedded)
                .collect()
        };
        assert_eq!(find("@envoy//:VERSION.txt"), vec![false]);
        assert_eq!(find("@envoy//changelogs"), vec![true]);
    }

    /// envoy's `bazel/repo.bzl`: three `attr.label` defaults the rule reads,
    /// and a BUILD file it writes whose `genrule` lists `@envoy//changelogs`.
    /// The changelog entry is an input of the generated *target*; it cannot
    /// have moved `compiler.bzl`, which every package's closure loads.
    #[test]
    fn a_label_inside_text_the_rule_writes_names_targets_not_bzl_files() {
        let module = "envoy_repo_ext = use_extension(\"//bazel:extensions.bzl\", \"envoy_repo_extension\")\n\
                      use_repo(envoy_repo_ext, \"envoy_repo\")\n";
        let repo_bzl = r#"
def _impl(ctx):
    version = ctx.read(ctx.path(ctx.attr.envoy_version))
    ctx.file("version.bzl", "VERSION = '%s'" % version)
    ctx.file("BUILD", '''
genrule(
    name = "project",
    tools = ["@envoy//:VERSION.txt", "@envoy//changelogs", "@envoy//changelogs:summary"],
)
''')
    ctx.file("defs.bzl", 'load("@envoy//bazel:helper.bzl", "h")')

_envoy_repo = repository_rule(
    implementation = _impl,
    attrs = {"envoy_version": attr.label(default = "@envoy//:VERSION.txt")},
)
"#;
        let files = Files(vec![
            ("MODULE.bazel", module),
            (
                "bazel/extensions.bzl",
                "load(\"//bazel:repo.bzl\", \"envoy_repo\")\n",
            ),
            ("bazel/repo.bzl", repo_bzl),
        ]);
        let m = m();
        let mut g = LoadGraph::new();
        let ws = g.workspace_closure(&files, &m);
        let facts = read_module_facts(&files);
        let ri = build_repo_inputs(&files, &m, &g, &ws, &facts);
        let envoy_repo: BTreeSet<String> = ["envoy_repo".to_string()].into_iter().collect();

        // Named by the generated BUILD file: the repository is still built
        // from it, so rule (1) still has a repository to resolve...
        assert_eq!(ri.repos_for("changelogs/current/x.rst"), envoy_repo);
        // ...but no `.bzl` in it can have moved.
        assert!(ri
            .target_only
            .contains(&Pattern::Prefix("changelogs/".into())));
        assert!(ri
            .target_only
            .contains(&Pattern::Exact("changelogs/summary".into())));
        // Both resolved by the rule and written into its BUILD file: it feeds
        // the rule, and `version.bzl` really does change with it.
        assert_eq!(ri.repos_for("VERSION.txt"), envoy_repo);
        assert!(!ri
            .target_only
            .contains(&Pattern::Exact("VERSION.txt".into())));
        // An embedded label that is itself a `.bzl`: the generated `defs.bzl`
        // loads it, so it stays a real input.
        assert_eq!(ri.repos_for("bazel/helper.bzl"), envoy_repo);
        assert!(!ri
            .target_only
            .contains(&Pattern::Exact("bazel/helper.bzl".into())));
    }

    // ---- the path mapping ----

    #[test]
    fn a_prefix_map_rewrites_and_an_exact_one_does_not() {
        let dir = PathMap {
            from: "api/".into(),
            to: String::new(),
            exhaustive: false,
        };
        assert_eq!(
            dir.translate("api/envoy/x.proto").as_deref(),
            Some("envoy/x.proto")
        );
        assert_eq!(
            dir.translate("api/bazel/defs.bzl").as_deref(),
            Some("bazel/defs.bzl")
        );
        assert_eq!(dir.translate("source/x.cc"), None);

        let nested = PathMap {
            from: "api/".into(),
            to: "sub/".into(),
            exhaustive: false,
        };
        assert_eq!(nested.translate("api/a/b").as_deref(), Some("sub/a/b"));

        let one = PathMap {
            from: "api/BUILD".into(),
            to: "BUILD".into(),
            exhaustive: false,
        };
        assert_eq!(one.translate("api/BUILD").as_deref(), Some("BUILD"));
        assert_eq!(one.translate("api/BUILD.bazel"), None);
    }

    #[test]
    fn a_path_map_round_trips_through_the_index() {
        for pm in [
            PathMap {
                from: "api/".into(),
                to: String::new(),
                exhaustive: false,
            },
            PathMap {
                from: "a/b.txt".into(),
                to: "c/d.txt".into(),
                exhaustive: false,
            },
        ] {
            assert_eq!(PathMap::decode(&pm.encode()), Some(pm));
        }
        assert_eq!(PathMap::decode("no separator"), None);
    }

    #[test]
    fn local_repository_calls_declare_their_own_mapping() {
        let maps = scan_bzl_path_maps(
            "def deps():\n    local_repository(name = \"api\", path = \"api\")\n\
             \x20   new_local_repository(name = \"x\", path = \"third_party/x/\", build_file = \"//bazel:x.BUILD\")\n\
             \x20   local_repository(name = \"outside\", path = \"../sibling\")\n",
            &m(),
        );
        assert_eq!(
            maps.named,
            vec![
                ("api".to_string(), PathMap::exhaustive_dir("api/".into())),
                (
                    "x".to_string(),
                    PathMap::exhaustive_dir("third_party/x/".into())
                ),
            ],
            "a path outside the repository cannot be matched against a diff"
        );
    }

    #[test]
    fn a_literal_symlink_is_a_mapping_and_a_computed_one_is_not() {
        let maps = scan_bzl_path_maps(
            "def _impl(ctx):\n\
             \x20   ctx.symlink(ctx.path(Label(\"//api:BUILD\")), \"BUILD\")\n\
             \x20   ctx.symlink(\"api/bazel/\", \"bazel\")\n\
             \x20   rctx.symlink(target = Label(\"//api:versioning.txt\"), link_name = \"versioning.txt\")\n",
            &m(),
        );
        let symlinks: Vec<PathMap> = maps.symlinks.iter().map(|(_, p)| p.clone()).collect();
        assert_eq!(
            symlinks,
            vec![
                PathMap {
                    from: "api/BUILD".into(),
                    to: "BUILD".into(),
                    exhaustive: false,
                },
                PathMap {
                    from: "api/bazel/".into(),
                    to: "bazel/".into(),
                    exhaustive: false,
                },
                PathMap {
                    from: "api/versioning.txt".into(),
                    to: "versioning.txt".into(),
                    exhaustive: false,
                },
            ]
        );
    }

    /// The shape envoy's `bazel/api_binding.bzl` used before it moved to
    /// `local_path_override`. Deriving a mapping from it would be a guess, and
    /// a wrong mapping *hides* a `.bzl` edit; no mapping merely keeps the old,
    /// wider answer.
    #[test]
    fn a_computed_symlink_source_yields_no_mapping() {
        let maps = scan_bzl_path_maps(
            "def _impl(ctx):\n\
             \x20   for d in DIRS:\n\
             \x20       ctx.symlink(ctx.path(ctx.attr.envoy_root).dirname.get_child(ctx.attr.reldir).get_child(d), d)\n\
             \x20   ctx.symlink(Label(\"//api\"), \"\")\n\
             \x20   ctx.symlink(Label(\"@other//api:BUILD\"), \"BUILD\")\n\
             \x20   ctx.symlink(\"../outside\", \"x\")\n",
            &m(),
        );
        assert!(maps.symlinks.is_empty(), "{:?}", maps.symlinks);
    }

    #[test]
    fn a_local_path_override_is_the_repositorys_root() {
        let files = Files(vec![(
            "MODULE.bazel",
            "module(name = \"envoy\", version = \"1\")\n\
                 bazel_dep(name = \"envoy_api\", version = \"1.0\")\n\
                 local_path_override(module_name = \"envoy_api\", path = \"api\")\n",
        )]);
        let m = m();
        let mut g = LoadGraph::new();
        let ws = g.workspace_closure(&files, &m);
        let facts = read_module_facts(&files);
        let ri = build_repo_inputs(&files, &m, &g, &ws, &facts);
        assert_eq!(
            ri.path_maps["envoy_api"],
            [PathMap::exhaustive_dir("api/".into())]
                .into_iter()
                .collect()
        );
    }

    /// A `.bzl` that could not be attributed to a repository must not hand its
    /// symlink mapping to every repository the main repository defines.
    #[test]
    fn an_unattributable_bzls_symlinks_map_nothing() {
        let files = Files(vec![
            (
                "MODULE.bazel",
                "e = use_extension(\"//bazel:ext.bzl\", \"e\")\nuse_repo(e, \"r\")\n\
                 load(\"//bazel:loose.bzl\", \"x\")\n",
            ),
            ("bazel/ext.bzl", ""),
            (
                "bazel/loose.bzl",
                "def _impl(ctx):\n    ctx.symlink(\"data/\", \"d\")\n",
            ),
        ]);
        let m = RepoMapping::identity();
        let mut g = LoadGraph::new();
        let ws = g.workspace_closure(&files, &m);
        let facts = read_module_facts(&files);
        let ri = build_repo_inputs(&files, &m, &g, &ws, &facts);
        assert!(ri.path_maps.is_empty(), "{:?}", ri.path_maps);
    }

    #[test]
    fn an_unattributable_workspace_bzl_falls_back_to_every_repository() {
        let files = Files(vec![
            (
                "MODULE.bazel",
                "e = use_extension(\"//bazel:ext.bzl\", \"e\")\nuse_repo(e, \"r\")\n\
                 load(\"//bazel:loose.bzl\", \"x\")\n",
            ),
            ("bazel/ext.bzl", ""),
            ("bazel/loose.bzl", "P = \"//data:thing.txt\"\n"),
        ]);
        let m = RepoMapping::identity();
        let mut g = LoadGraph::new();
        let ws = g.workspace_closure(&files, &m);
        let facts = read_module_facts(&files);
        let ri = build_repo_inputs(&files, &m, &g, &ws, &facts);
        assert_eq!(
            ri.repos_for("data/thing.txt"),
            ["r".to_string()].into_iter().collect()
        );
    }

    // ---- per-occurrence attribution ----

    /// A working tree on disk, for the corpus check below.
    struct Dir(std::path::PathBuf);

    impl FileText for Dir {
        fn text(&self, path: &str) -> Option<String> {
            std::fs::read_to_string(self.0.join(path)).ok()
        }
    }

    fn inputs(files: &dyn FileText, m: &RepoMapping) -> RepoInputs {
        let mut g = LoadGraph::new();
        let ws = g.workspace_closure(files, m);
        let facts = read_module_facts(files);
        build_repo_inputs(files, m, &g, &ws, &facts)
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// envoy at 7ba51266cc60: `//bazel:extensions.bzl` declares five module
    /// extensions, and `bazel/repo.bzl` — which it loads — declares one
    /// repository rule whose `attr.label` defaults name three main-repo files.
    /// Per file, all three fed all six repositories, including `envoy_repo`,
    /// whose generated `compiler.bzl` sits in eleven hundred package load
    /// closures; per occurrence they feed `envoy_repo` alone.
    #[test]
    fn the_bzlmod_shape_attributes_an_attr_default_to_its_own_rule() {
        let files = Files(vec![
            (
                "MODULE.bazel",
                "module(name = \"envoy\", version = \"1\")\n\
                 repo_ext = use_extension(\"//bazel:extensions.bzl\", \"envoy_repo_extension\")\n\
                 use_repo(repo_ext, \"envoy_repo\")\n\
                 cfg_ext = use_extension(\"//bazel:extensions.bzl\", \"envoy_build_config_ext\")\n\
                 use_repo(cfg_ext, \"envoy_build_config\")\n\
                 cargo_ext = use_extension(\"//bazel:extensions.bzl\", \"wasm_cargo\")\n\
                 use_repo(cargo_ext, \"cu\")\n",
            ),
            (
                "bazel/extensions.bzl",
                "load(\"//bazel/external/cargo/remote:crates.bzl\", \"crate_repositories\")\n\
                 load(\":envoy_build_config.bzl\", \"default_envoy_build_config\")\n\
                 load(\":repo.bzl\", \"envoy_repo\")\n\
                 \n\
                 def _envoy_repo_impl(module_ctx):\n\
                 \x20   envoy_repo()\n\
                 \n\
                 def _envoy_build_config_impl(module_ctx):\n\
                 \x20   default_envoy_build_config(name = \"envoy_build_config\")\n\
                 \n\
                 def _wasm_cargo_impl(module_ctx):\n\
                 \x20   crate_repositories()\n\
                 \n\
                 envoy_repo_extension = module_extension(implementation = _envoy_repo_impl)\n\
                 envoy_build_config_ext = module_extension(implementation = _envoy_build_config_impl)\n\
                 wasm_cargo = module_extension(implementation = _wasm_cargo_impl)\n",
            ),
            (
                "bazel/repo.bzl",
                "def _envoy_repo_impl(repository_ctx):\n\
                 \x20   repository_ctx.file(\"BUILD\", '''\n\
                 genrule(name = \"project\", tools = [\"@envoy//changelogs\"])\n\
                 ''')\n\
                 \n\
                 _envoy_repo = repository_rule(\n\
                 \x20   implementation = _envoy_repo_impl,\n\
                 \x20   attrs = {\n\
                 \x20       \"envoy_ci_config\": attr.label(default = \"@envoy//:.github/config.yml\"),\n\
                 \x20   },\n\
                 )\n\
                 \n\
                 def envoy_repo():\n\
                 \x20   _envoy_repo(name = \"envoy_repo\")\n",
            ),
            (
                "bazel/envoy_build_config.bzl",
                "def _impl(ctx):\n\
                 \x20   ctx.symlink(ctx.attr.config, \"extensions_build_config.bzl\")\n\
                 \n\
                 default_envoy_build_config = repository_rule(\n\
                 \x20   implementation = _impl,\n\
                 \x20   attrs = {\"config\": attr.label(\
                 default = \"@envoy//source/extensions:extensions_build_config.bzl\")},\n\
                 )\n",
            ),
            (
                "bazel/external/cargo/remote/crates.bzl",
                "def crate_repositories():\n\
                 \x20   maybe(\n\
                 \x20       crates_vendor_remote_repository,\n\
                 \x20       name = \"cu\",\n\
                 \x20       defs_module = Label(\"@envoy//bazel/external/cargo/remote:defs.bzl\"),\n\
                 \x20   )\n",
            ),
        ]);
        let m = RepoMapping::from_dump("{\"\":\"\",\"envoy\":\"\"}");
        let ri = inputs(&files, &m);

        assert_eq!(ri.repos_for(".github/config.yml"), set(&["envoy_repo"]));
        assert_eq!(
            ri.repos_for("source/extensions/extensions_build_config.bzl"),
            set(&["envoy_build_config"])
        );
        assert_eq!(
            ri.repos_for("bazel/external/cargo/remote/defs.bzl"),
            set(&["cu"])
        );
        // The generated BUILD file's `tools` still name the repository, and
        // still cannot have moved a `.bzl` inside it.
        assert_eq!(
            ri.repos_for("changelogs/current/x.rst"),
            set(&["envoy_repo"])
        );
        assert!(ri
            .target_only
            .contains(&Pattern::Prefix("changelogs/".into())));
    }

    /// Pre-bzlmod envoy (issue #6): no `MODULE.bazel` at all, so every file
    /// used to fall back to the empty set — no repository named, hence a
    /// global invalidation. `envoy_dependencies()` reaches a forwarding
    /// wrapper, and `pip_parse` is called with a literal name in a helper.
    #[test]
    fn the_workspace_shape_attributes_a_requirements_lock_to_one_pip() {
        let files = Files(vec![
            (
                "WORKSPACE",
                "workspace(name = \"envoy\")\n\
                 load(\"//bazel:repositories.bzl\", \"envoy_dependencies\")\n\
                 envoy_dependencies()\n\
                 load(\"//bazel:python_dependencies.bzl\", \"envoy_python_dependencies\")\n\
                 envoy_python_dependencies()\n",
            ),
            (
                "bazel/repositories.bzl",
                "load(\"@envoy_api//bazel:envoy_http_archive.bzl\", \"envoy_http_archive\")\n\
                 \n\
                 def external_http_archive(name, **kwargs):\n\
                 \x20   envoy_http_archive(name, locations = REPOSITORY_LOCATIONS, **kwargs)\n\
                 \n\
                 def _com_google_protobuf():\n\
                 \x20   external_http_archive(\n\
                 \x20       \"com_google_protobuf\",\n\
                 \x20       patches = [\"@envoy//bazel:protobuf.patch\"],\n\
                 \x20   )\n\
                 \n\
                 def envoy_dependencies():\n\
                 \x20   local_repository(name = \"envoy_api\", path = \"api\")\n\
                 \x20   _com_google_protobuf()\n",
            ),
            (
                "bazel/python_dependencies.bzl",
                "load(\"@rules_python//python:pip.bzl\", \"pip_parse\")\n\
                 \n\
                 def envoy_python_dependencies():\n\
                 \x20   pip_parse(\n\
                 \x20       name = \"base_pip3\",\n\
                 \x20       requirements_lock = \"@envoy//tools/base:requirements.txt\",\n\
                 \x20   )\n\
                 \x20   pip_parse(\n\
                 \x20       name = \"dev_pip3\",\n\
                 \x20       requirements_lock = \"@envoy//tools/dev:requirements.txt\",\n\
                 \x20   )\n",
            ),
        ]);
        let m = RepoMapping::identity().with_main_alias("envoy");
        let ri = inputs(&files, &m);

        assert_eq!(
            ri.repos_for("tools/base/requirements.txt"),
            set(&["base_pip3"])
        );
        assert_eq!(
            ri.repos_for("tools/dev/requirements.txt"),
            set(&["dev_pip3"])
        );
        // A patch passed positionally to a forwarding wrapper.
        assert_eq!(
            ri.repos_for("bazel/protobuf.patch"),
            set(&["com_google_protobuf"])
        );
        // `local_repository` is the whole of `@envoy_api`, so an edit under
        // `api/` that is not a `.bzl` the closure loads cannot have moved one.
        assert_eq!(ri.repos_for("api/envoy/x.proto"), set(&["envoy_api"]));
        assert!(ri.path_maps["envoy_api"]
            .iter()
            .all(|pm| pm.exhaustive && pm.from == "api/"));
        // And a `.bzl` change names a set rather than nothing at all.
        assert_eq!(
            ri.bzl_repos["bazel/python_dependencies.bzl"],
            set(&["base_pip3", "dev_pip3"])
        );
    }

    /// A name built by string arithmetic cannot be evaluated, so the enclosing
    /// scope keeps the whole-file answer rather than a guess.
    #[test]
    fn a_dynamic_name_falls_back_to_the_file_set() {
        let files = Files(vec![
            (
                "MODULE.bazel",
                "module(name = \"demo\", version = \"1\")\n\
                 e = use_extension(\"//bazel:ext.bzl\", \"e\")\n\
                 use_repo(e, \"a\", \"b\")\n\
                 load(\"//bazel:loose.bzl\", \"loose\")\n",
            ),
            ("bazel/ext.bzl", "E = 1\n"),
            (
                "bazel/loose.bzl",
                "def loose():\n\
                 \x20   for p in PLATFORMS:\n\
                 \x20       http_archive(name = \"proto_\" + p, patches = [\"//bazel:x.patch\"])\n",
            ),
        ]);
        let m = RepoMapping::from_dump("{\"\":\"\",\"demo\":\"\"}");
        let ri = inputs(&files, &m);
        assert_eq!(ri.repos_for("bazel/x.patch"), set(&["a", "b"]));
    }

    /// A label written at module level is attributed the way it always was:
    /// the file's owner set. Nothing about the occurrence says more.
    #[test]
    fn a_module_level_label_keeps_the_old_answer() {
        let files = Files(vec![
            (
                "MODULE.bazel",
                "module(name = \"demo\", version = \"1\")\n\
                 e = use_extension(\"//bazel:ext.bzl\", \"e\")\n\
                 use_repo(e, \"a\", \"b\")\n",
            ),
            (
                "bazel/ext.bzl",
                "CONF = attr.label(default = \"@demo//conf:settings.yml\")\n",
            ),
        ]);
        let m = RepoMapping::from_dump("{\"\":\"\",\"demo\":\"\"}");
        let ri = inputs(&files, &m);
        assert_eq!(ri.repos_for("conf/settings.yml"), set(&["a", "b"]));
    }

    #[test]
    fn an_exhaustive_path_map_round_trips_through_the_index() {
        let pm = PathMap::exhaustive_dir("api/".into());
        assert_eq!(PathMap::decode(&pm.encode()), Some(pm.clone()));
        assert!(pm.encode().ends_with("\u{0}!"));
        // The two-field form every index written before the marker existed
        // holds reads back as the wide answer.
        assert_eq!(
            PathMap::decode("api/\u{0}"),
            Some(PathMap {
                from: "api/".into(),
                to: String::new(),
                exhaustive: false,
            })
        );
    }

    /// Run against a working tree of the envoy corpus:
    ///
    /// ```text
    /// CAMBIUM_CORPUS_DIR=/somewhere cargo test -p cambium-index \
    ///     -- --ignored --nocapture corpus_attribution
    /// ```
    #[test]
    #[ignore = "needs a working tree of the corpus; see the doc comment"]
    fn corpus_attribution() {
        let dir = std::env::var("CAMBIUM_CORPUS_DIR").expect("set CAMBIUM_CORPUS_DIR");
        let files = Dir(dir.into());
        let m = RepoMapping::identity().with_main_alias("envoy");
        let ri = inputs(&files, &m);
        println!("repos in the universe: {}", ri.repos.len());
        for p in [
            ".github/config.yml",
            "source/extensions/extensions_build_config.bzl",
            "bazel/external/cargo/remote/defs.bzl",
            "tools/base/requirements.txt",
            "api/envoy/config/core/v3/base.proto",
        ] {
            for (pat, repos) in &ri.patterns {
                if pat.matches(p) {
                    println!("   pattern {pat:?} -> {repos:?}");
                }
            }
            let repos = ri.repos_for(p);
            println!(
                "{p}: {} repo(s) {:?}",
                repos.len(),
                repos.iter().take(12).collect::<Vec<_>>()
            );
        }
        for (repo, maps) in &ri.path_maps {
            println!(
                "path map {repo}: {:?}",
                maps.iter()
                    .map(|m| (m.from.as_str(), m.to.as_str(), m.exhaustive))
                    .collect::<Vec<_>>()
            );
        }
        for b in [
            "bazel/repo.bzl",
            "bazel/extensions.bzl",
            "bazel/repositories.bzl",
            "bazel/python_dependencies.bzl",
        ] {
            if let Some(r) = ri.bzl_repos.get(b) {
                println!("bzl_repos {b}: {} repo(s)", r.len());
            }
        }
    }
}
