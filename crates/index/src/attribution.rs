//! Which repository a *single occurrence* in a workspace-closure file feeds.
//!
//! [`crate::externals`] owns the question and the tables; this module owns the
//! static reading of Starlark that answers it per occurrence rather than per
//! file. See the "Attribution" section of that module's documentation for the
//! reasoning; what follows is the shape of the analysis.
//!
//! One pass over every file in the workspace closure (`WORKSPACE*`,
//! `MODULE.bazel`, and every `.bzl` they reach) collects, per file:
//!
//! - its top-level `def`s, with their parameter names and the token range of
//!   the whole `def` statement (a top-level statement runs to the next token
//!   in column zero, which is exactly the indentation rule Starlark uses);
//! - its `load()`s, alias-aware, so a symbol can be resolved to the file that
//!   defines it;
//! - its `X = repository_rule(implementation = _f, ...)` and
//!   `E = module_extension(implementation = _g, ...)` assignments;
//! - every call in it: the callee symbol, the argument spans, and how the
//!   call names the repository it may create.
//!
//! Two fixpoints then run over the call graph: `instances`, the repository
//! names a callable can be asked to create, and `defines`, the repository
//! names a macro creates when called. Every unknown shape resolves to the
//! file's fallback set, which is what the whole file used to get.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use crate::loads::{lex, resolve_label, unescape, Lexed, Load, RepoMapping, Tok};

/// Undotted builtins that cannot create a repository.
///
/// A call to a symbol this analysis cannot resolve *might* be a macro from
/// another repository that declares repositories under names nothing here can
/// see — `envoy_dependencies()` calling into `@envoy_api`'s helpers — so a
/// literal in its scope has to keep the old, wide answer. These are the
/// exceptions: Starlark's own vocabulary, which declares nothing.
const PURE_BUILTINS: &[&str] = &[
    "Label",
    // `MODULE.bazel`'s own vocabulary. [`crate::externals::scan_module`] reads
    // these structurally, and none of them declares a repository this analysis
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

/// Starlark keywords. A `(` after one of these opens an expression, not a
/// call's arguments.
const KEYWORDS: &[&str] = &[
    "and", "assert", "break", "continue", "def", "del", "elif", "else", "for", "if", "in", "is",
    "lambda", "load", "not", "or", "pass", "return", "while",
];

/// Whether a call's `name =` is the name of a repository the main repository
/// builds.
///
/// `workspace()` and `module()` name the main repository itself;
/// `bazel_dep()` names one the registry provides, whose contents no main-repo
/// file feeds. Neither belongs in the universe a fallback ranges over.
fn names_a_repository(head: &str) -> bool {
    !matches!(
        head,
        "workspace" | "module" | "bazel_dep" | "register_toolchains" | "include"
    ) && !head.ends_with("_override")
}

/// A callable the closure defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Sym {
    Def(usize),
    Rule(usize),
    Ext(usize),
}

/// How a call names the repository it may create.
#[derive(Clone, Debug, PartialEq, Eq)]
enum NameArg {
    /// No name argument at all: not an instantiation.
    Absent,
    /// `name = "literal"`.
    Literal(String),
    /// `name = <a parameter of the enclosing def>`: a forwarding wrapper.
    Forward,
    /// A name this analysis cannot evaluate.
    Dynamic,
}

struct DefInfo {
    file: usize,
    params: Vec<String>,
    /// Token range of the whole `def` statement, header included.
    span: Range<usize>,
    /// Parameters this def forwards as some call's `name`.
    fwd: BTreeSet<String>,
    /// The rule / extension whose implementation this def is.
    role: Option<Sym>,
}

/// A `X = repository_rule(...)` or `E = module_extension(...)` binding. The
/// token range of the statement lives on the file, in `decls`.
struct RuleInfo {
    file: usize,
    name: String,
}

struct ExtInfo {
    file: usize,
    name: String,
}

/// The fields of a call site the fixpoints read, detached from `self` so the
/// sets they feed can be written in the same loop.
struct Call<'c> {
    name: &'c NameArg,
    scope: Option<usize>,
    file: usize,
}

struct CallSite {
    file: usize,
    /// Token index of the call's `(`.
    open: usize,
    /// The def the call is written in, if any.
    scope: Option<usize>,
    callee: Option<Sym>,
    /// Set when the callee is a name this analysis could not resolve and that
    /// is not a pure builtin.
    opaque: bool,
    name: NameArg,
}

/// One place a file uses a symbol it imported.
///
/// `chain` is every file the symbol was resolved through — the defining file
/// and any that re-export it — because a change to any of them changes what
/// this use does.
struct Use {
    file: usize,
    tok: usize,
    chain: Vec<usize>,
}

/// One file of the workspace closure.
struct FileScan<'a> {
    path: String,
    text: &'a str,
    toks: Vec<Lexed<'a>>,
    /// Alias -> (defining file path, original name).
    loads: BTreeMap<String, (String, String)>,
    /// Top-level names this file binds to a callable.
    syms: BTreeMap<String, Sym>,
    defs: Vec<usize>,
    /// `(statement span, rule or extension)`, for attributing a literal that
    /// sits inside the declaration itself — an `attrs = {...: attr.label(...)}`.
    decls: Vec<(Range<usize>, Sym)>,
    /// Whether any module-level (or otherwise unattributable) shape forces
    /// this file's module scope to the fallback set.
    module_dynamic: bool,
    fallback: BTreeSet<String>,
    /// Repositories `MODULE.bazel` says this file declares: the `use_repo`
    /// names of an extension it defines, the instantiations of a repository
    /// rule `use_repo_rule` names in it. Part of the file's own answer even
    /// when the declaration itself could not be read out of the text.
    seeded: BTreeSet<String>,
    /// For each token, the index of the `(` of the call or bracket it sits
    /// directly inside, or `usize::MAX`. Walking this chain is how an
    /// occurrence finds the calls that enclose it.
    parent: Vec<usize>,
}

/// The whole workspace closure, analysed.
pub(crate) struct Attribution<'a> {
    files: Vec<FileScan<'a>>,
    by_path: BTreeMap<String, usize>,
    defs: Vec<DefInfo>,
    rules: Vec<RuleInfo>,
    exts: Vec<ExtInfo>,
    /// Repository names a callable can be asked to create.
    inst_def: Vec<BTreeSet<String>>,
    inst_rule: Vec<BTreeSet<String>>,
    repos_ext: Vec<BTreeSet<String>>,
    /// Repository names a macro creates when called.
    defines: Vec<BTreeSet<String>>,
    /// Scopes whose answer is the file's fallback set.
    dynamic: Vec<bool>,
    /// Call sites, kept for the fixpoints.
    calls: Vec<CallSite>,
    /// Every place a file uses a symbol it imported from another.
    uses: Vec<Use>,
    /// For a helper def that instantiates nothing, what its callers do.
    helper: Vec<BTreeSet<String>>,
    /// Per file: everything a change to it can move. The fixpoint of
    /// [`Attribution::refine`].
    fileset: Vec<BTreeSet<String>>,
    /// True while that fixpoint is running, when an empty set means "not
    /// computed yet" rather than "nothing known". Folding the fallback in
    /// during the fixpoint would make the universe permanent everywhere,
    /// because the sets only grow.
    refining: bool,
    /// Literal `name =` values seen anywhere: the universe a fallback ranges
    /// over.
    pub(crate) literal_names: BTreeSet<String>,
}

/// What an occurrence is attributed to.
#[derive(Clone, Debug)]
pub(crate) struct Attributed {
    pub(crate) repos: BTreeSet<String>,
    /// False when the answer *is* the file's fallback set — nothing was
    /// learned about this occurrence.
    pub(crate) exact: bool,
}

impl<'a> Attribution<'a> {
    /// Reads the structure of every file. `texts` is `(path, text)`; the
    /// caller keeps the strings alive.
    pub(crate) fn scan(texts: &'a [(String, String)], m: &RepoMapping) -> Self {
        let mut a = Attribution {
            files: Vec::new(),
            by_path: BTreeMap::new(),
            defs: Vec::new(),
            rules: Vec::new(),
            exts: Vec::new(),
            inst_def: Vec::new(),
            inst_rule: Vec::new(),
            repos_ext: Vec::new(),
            defines: Vec::new(),
            dynamic: Vec::new(),
            calls: Vec::new(),
            uses: Vec::new(),
            helper: Vec::new(),
            fileset: Vec::new(),
            refining: false,
            literal_names: BTreeSet::new(),
        };
        for (i, (path, text)) in texts.iter().enumerate() {
            a.by_path.insert(path.clone(), i);
            a.files.push(FileScan {
                path: path.clone(),
                text,
                toks: lex(text),
                loads: BTreeMap::new(),
                syms: BTreeMap::new(),
                defs: Vec::new(),
                decls: Vec::new(),
                module_dynamic: false,
                fallback: BTreeSet::new(),
                seeded: BTreeSet::new(),
                parent: Vec::new(),
            });
        }
        for f in &mut a.files {
            f.parent = parents(&f.toks);
        }
        for i in 0..a.files.len() {
            a.declarations(i, m);
        }
        // Implementations, which need every file's symbols to resolve.
        for i in 0..a.files.len() {
            a.implementations(i);
        }
        // Twice: the first pass learns which parameters each def forwards as
        // a repository name, which the second pass needs to read a call's
        // positional first argument correctly.
        for _ in 0..2 {
            a.calls.clear();
            a.literal_names.clear();
            for f in &mut a.files {
                f.module_dynamic = false;
            }
            for i in 0..a.files.len() {
                a.call_sites(i);
            }
        }
        for i in 0..a.files.len() {
            a.collect_uses(i);
        }
        a
    }

    /// Every use of an imported symbol in file `f`.
    ///
    /// This is what replaces propagating a loader's whole answer down its
    /// `load()` edges. A `.bzl` can only reach a repository through code that
    /// runs: its own definitions, or a symbol another file imported from it —
    /// and the effect of *that* is bounded by where the symbol is used.
    fn collect_uses(&mut self, f: usize) {
        let mut out = Vec::new();
        {
            let file = &self.files[f];
            for (i, t) in file.toks.iter().enumerate() {
                let Tok::Name(n) = t.tok else { continue };
                if !file.loads.contains_key(n) {
                    continue;
                }
                // `x.n` is an attribute, and `n = ...` a keyword argument —
                // which is also how the alias appears in the `load()` itself.
                if matches!(
                    i.checked_sub(1).map(|j| file.toks[j].tok),
                    Some(Tok::Punct(b'.'))
                ) {
                    continue;
                }
                if matches!(file.toks.get(i + 1).map(|t| t.tok), Some(Tok::Punct(b'=')))
                    && !matches!(file.toks.get(i + 2).map(|t| t.tok), Some(Tok::Punct(b'=')))
                {
                    continue;
                }
                let chain = self.origin_chain(f, n);
                if !chain.is_empty() {
                    out.push(Use {
                        file: f,
                        tok: i,
                        chain,
                    });
                }
            }
        }
        self.uses.extend(out);
    }

    /// Every file a symbol written in `f` was resolved through.
    fn origin_chain(&self, f: usize, name: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let mut file = f;
        let mut name = name.to_string();
        for _ in 0..16 {
            if self.files[file].syms.contains_key(&name) {
                return out;
            }
            let Some((path, orig)) = self.files[file].loads.get(&name) else {
                return out;
            };
            let Some(&next) = self.by_path.get(path) else {
                return out;
            };
            out.push(next);
            file = next;
            name = orig.clone();
        }
        out
    }

    /// Top-level `def`s, `load`s and rule / extension assignments.
    fn declarations(&mut self, f: usize, m: &RepoMapping) {
        let file = &self.files[f];
        let (text, toks) = (file.text, &file.toks);
        let pkg = crate::loads::owning_dir(&file.path);
        let mut loads = BTreeMap::new();
        let mut syms = BTreeMap::new();
        let mut defs = Vec::new();
        let mut decls = Vec::new();
        let mut new_defs: Vec<DefInfo> = Vec::new();
        let mut new_rules: Vec<RuleInfo> = Vec::new();
        let mut new_exts: Vec<ExtInfo> = Vec::new();

        for span in top_level_spans(toks) {
            let t = &toks[span.clone()];
            match (t.first().map(|x| x.tok), t.get(1).map(|x| x.tok)) {
                // `def name(params):`
                (Some(Tok::Name("def")), Some(Tok::Name(name))) => {
                    let Some(Tok::Open(b'(')) = t.get(2).map(|x| x.tok) else {
                        continue;
                    };
                    let params = arg_spans(t, 2)
                        .into_iter()
                        .filter_map(|(k, r)| match k {
                            Some(k) => Some(k),
                            None => match t[r.clone()].first().map(|x| x.tok) {
                                Some(Tok::Name(n)) => Some(n.to_string()),
                                _ => None,
                            },
                        })
                        .collect();
                    let id = self.defs.len() + new_defs.len();
                    syms.insert(name.to_string(), Sym::Def(id));
                    defs.push(id);
                    new_defs.push(DefInfo {
                        file: f,
                        params,
                        span: span.clone(),
                        fwd: BTreeSet::new(),
                        role: None,
                    });
                }
                // `X = repository_rule(...)` / `E = module_extension(...)`
                (Some(Tok::Name(name)), Some(Tok::Punct(b'='))) => {
                    let Some(Tok::Name(head)) = t.get(2).map(|x| x.tok) else {
                        continue;
                    };
                    if !matches!(t.get(3).map(|x| x.tok), Some(Tok::Open(b'('))) {
                        continue;
                    }
                    match head {
                        "repository_rule" => {
                            let id = self.rules.len() + new_rules.len();
                            syms.insert(name.to_string(), Sym::Rule(id));
                            decls.push((span.clone(), Sym::Rule(id)));
                            new_rules.push(RuleInfo {
                                file: f,
                                name: name.to_string(),
                            });
                        }
                        "module_extension" => {
                            let id = self.exts.len() + new_exts.len();
                            syms.insert(name.to_string(), Sym::Ext(id));
                            decls.push((span.clone(), Sym::Ext(id)));
                            new_exts.push(ExtInfo {
                                file: f,
                                name: name.to_string(),
                            });
                        }
                        _ => {}
                    }
                }
                // `load("//pkg:f.bzl", "sym", alias = "sym")`
                (Some(Tok::Name("load")), Some(Tok::Open(b'('))) => {
                    let label = match t.get(2).map(|x| x.tok) {
                        Some(Tok::Str(s, e)) => unescape(&text[s..e]),
                        _ => continue,
                    };
                    let Some(Load::Main(p)) = resolve_label(&label, &pkg, m) else {
                        continue;
                    };
                    for (k, r) in arg_spans(t, 1).into_iter().skip(1) {
                        let Some(Tok::Str(s, e)) = t[r].first().map(|x| x.tok) else {
                            continue;
                        };
                        let orig = unescape(&text[s..e]);
                        let alias = k.unwrap_or_else(|| orig.clone());
                        loads.insert(alias, (p.clone(), orig));
                    }
                }
                _ => {}
            }
        }
        self.defs.extend(new_defs);
        self.rules.extend(new_rules);
        self.exts.extend(new_exts);
        let file = &mut self.files[f];
        file.loads = loads;
        file.syms = syms;
        file.defs = defs;
        file.decls = decls;
    }

    /// `implementation = _f` on a rule or extension declaration, which makes
    /// `_f`'s body part of that rule's or extension's definition.
    fn implementations(&mut self, f: usize) {
        let decls = self.files[f].decls.clone();
        for (span, sym) in decls {
            let toks = &self.files[f].toks;
            let Some(imp) = arg_spans(&toks[span.clone()], 3)
                .into_iter()
                .find_map(|(k, r)| (k.as_deref() == Some("implementation")).then_some(r))
            else {
                continue;
            };
            let sub = &toks[span.clone()][imp];
            let [one] = sub else { continue };
            let Tok::Name(n) = one.tok else { continue };
            if let Some(Sym::Def(d)) = self.resolve(f, n) {
                self.defs[d].role = Some(sym);
            }
        }
    }

    /// Every call in a file, with the def it is written in and how it names
    /// the repository it may create.
    fn call_sites(&mut self, f: usize) {
        let mut found: Vec<CallSite> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        let mut fwd: Vec<(usize, String)> = Vec::new();
        let mut module_dynamic = false;
        {
            let file = &self.files[f];
            let toks = &file.toks;
            for i in 0..toks.len() {
                if !matches!(toks[i].tok, Tok::Open(b'(')) {
                    continue;
                }
                // A call is `Name(`, and not `x.Name(` — a method call has no
                // symbol this analysis can resolve, but it may still carry a
                // literal `name =`, which is what `native.bind(name = ...)`
                // and `maybe(rule, name = ...)` are read for.
                let Some(Tok::Name(head)) = i.checked_sub(1).map(|j| toks[j].tok) else {
                    continue;
                };
                // `return (a or b)`, `if (x)`, `not (y)`: a keyword before a
                // parenthesised expression is not a call, and reading it as
                // one — as a symbol the closure cannot resolve — made every
                // helper that wraps a condition in brackets unattributable.
                if KEYWORDS.contains(&head) {
                    continue;
                }
                // `def f(x)` is a declaration, not a call to `f`. Read as one
                // it would give every def a call site, which is what decides
                // whether a helper inherits its callers' repositories or keeps
                // the fallback.
                if matches!(
                    i.checked_sub(2).map(|j| toks[j].tok),
                    Some(Tok::Name("def"))
                ) {
                    continue;
                }
                let dotted = matches!(
                    i.checked_sub(2).map(|j| toks[j].tok),
                    Some(Tok::Punct(b'.'))
                );
                let scope = self.scope_of(f, i);
                let callee = if dotted { None } else { self.resolve(f, head) };
                let spans = arg_spans(toks, i);
                // A method call is not an instantiation: `native.bind(name =
                // "ssl")` declares a `//external` alias, not a repository, and
                // reading its `name` would attribute the labels beside it to a
                // repository that does not exist. Its literals fall through to
                // the scope, which is the wide answer.
                let name = if dotted {
                    NameArg::Absent
                } else {
                    self.name_arg(scope, callee, head, toks, file.text, &spans)
                };
                if let NameArg::Literal(l) = &name {
                    if names_a_repository(head) {
                        names.push(l.clone());
                    }
                }
                if let (NameArg::Forward, Some(d)) = (&name, scope) {
                    if let Some(p) = self.forwarded_param(scope, toks, &spans) {
                        fwd.push((d, p));
                    }
                }
                let opaque = !dotted
                    && callee.is_none()
                    && !PURE_BUILTINS.contains(&head)
                    && matches!(name, NameArg::Absent)
                    && !spans.is_empty();
                if opaque && scope.is_none() {
                    module_dynamic = true;
                }
                found.push(CallSite {
                    file: f,
                    open: i,
                    scope,
                    callee,
                    opaque,
                    name,
                });
            }
        }
        for (d, p) in fwd {
            self.defs[d].fwd.insert(p);
        }
        self.literal_names.extend(names);
        self.files[f].module_dynamic |= module_dynamic;
        self.calls.extend(found);
    }

    /// The def a token index falls inside, if any.
    fn scope_of(&self, f: usize, tok: usize) -> Option<usize> {
        self.files[f]
            .defs
            .iter()
            .copied()
            .find(|d| self.defs[*d].span.contains(&tok) && self.defs[*d].span.start != tok)
    }

    /// The parameter a forwarding call passes as its name.
    fn forwarded_param(
        &self,
        scope: Option<usize>,
        toks: &[Lexed<'_>],
        spans: &[(Option<String>, Range<usize>)],
    ) -> Option<String> {
        let d = scope?;
        let params = &self.defs[d].params;
        for (k, r) in spans {
            if k.is_some() && k.as_deref() != Some("name") {
                continue;
            }
            if let [one] = &toks[r.clone()] {
                if let Tok::Name(n) = one.tok {
                    if params.iter().any(|p| p == n) {
                        return Some(n.to_string());
                    }
                }
            }
            if k.is_some() {
                break;
            }
        }
        None
    }

    /// How a call names the repository it may create.
    fn name_arg(
        &self,
        scope: Option<usize>,
        callee: Option<Sym>,
        head: &str,
        toks: &[Lexed<'_>],
        text: &str,
        spans: &[(Option<String>, Range<usize>)],
    ) -> NameArg {
        let params = scope
            .map(|d| self.defs[d].params.clone())
            .unwrap_or_default();
        let classify = |r: Range<usize>| -> NameArg {
            match &toks[r] {
                [one] => match one.tok {
                    Tok::Str(s, e) => NameArg::Literal(unescape(&text[s..e])),
                    Tok::Name(n) if params.iter().any(|p| p == n) => NameArg::Forward,
                    _ => NameArg::Dynamic,
                },
                _ => NameArg::Dynamic,
            }
        };
        // The keyword the callee reads as a repository name: `name`, or a
        // parameter it is known to forward under another spelling.
        let mut keys: Vec<String> = vec!["name".to_string()];
        if let Some(Sym::Def(d)) = callee {
            keys.extend(self.defs[d].fwd.iter().cloned());
        }
        for k in &keys {
            if let Some((_, r)) = spans
                .iter()
                .find(|(key, _)| key.as_deref() == Some(k.as_str()))
            {
                return classify(r.clone());
            }
        }
        let Some((_, first)) = spans.iter().find(|(k, _)| k.is_none()) else {
            return NameArg::Absent;
        };
        // A positional first argument is the name when the callee is a def
        // whose first parameter is one it forwards as a name — envoy's
        // `external_http_archive("com_google_protobuf", patches = [...])` —
        // or, for a callee this analysis cannot resolve, when it is literally
        // the enclosing def's own forwarded parameter.
        match callee {
            Some(Sym::Def(d)) => {
                let p0 = self.defs[d].params.first().cloned().unwrap_or_default();
                if p0 == "name" || self.defs[d].fwd.contains(&p0) {
                    return classify(first.clone());
                }
                NameArg::Absent
            }
            // `Label(dep)` / `str(name)` forward nothing: a pure builtin
            // creates no repository, and reading its argument as a name would
            // turn apollo's `clean_dep(dep)` into a wrapper whose every call
            // site declares a label-shaped "repository".
            _ if PURE_BUILTINS.contains(&head) => NameArg::Absent,
            _ => match classify(first.clone()) {
                NameArg::Forward => NameArg::Forward,
                _ => NameArg::Absent,
            },
        }
    }

    /// Resolves a symbol written in file `f`, through `load()` aliases.
    fn resolve(&self, f: usize, name: &str) -> Option<Sym> {
        let mut file = f;
        let mut name = name.to_string();
        for _ in 0..16 {
            if let Some(s) = self.files[file].syms.get(&name) {
                return Some(*s);
            }
            let (path, orig) = self.files[file].loads.get(&name)?;
            file = *self.by_path.get(path)?;
            name = orig.clone();
        }
        None
    }

    /// Runs the two fixpoints. `fallbacks` is the per-file answer for anything
    /// unattributable; `seed_rules` and `seed_exts` come from `MODULE.bazel`.
    pub(crate) fn solve(
        &mut self,
        fallbacks: &BTreeMap<String, BTreeSet<String>>,
        seeds: &BTreeMap<(String, String), BTreeSet<String>>,
    ) {
        for i in 0..self.files.len() {
            let path = self.files[i].path.clone();
            self.files[i].fallback = fallbacks.get(&path).cloned().unwrap_or_default();
            for ((p, _), repos) in seeds {
                if *p == path {
                    self.files[i].seeded.extend(repos.iter().cloned());
                }
            }
        }
        self.inst_def = vec![BTreeSet::new(); self.defs.len()];
        self.inst_rule = vec![BTreeSet::new(); self.rules.len()];
        self.repos_ext = vec![BTreeSet::new(); self.exts.len()];
        self.defines = vec![BTreeSet::new(); self.defs.len()];
        self.dynamic = vec![false; self.defs.len()];

        // `use_repo_rule("//bazel:repo.bzl", "http_archive")` and
        // `use_extension("//bazel:extensions.bzl", "envoy_repo_extension")`.
        for r in 0..self.rules.len() {
            let key = (
                self.files[self.rules[r].file].path.clone(),
                self.rules[r].name.clone(),
            );
            if let Some(s) = seeds.get(&key) {
                self.inst_rule[r].extend(s.iter().cloned());
            }
        }
        for e in 0..self.exts.len() {
            let key = (
                self.files[self.exts[e].file].path.clone(),
                self.exts[e].name.clone(),
            );
            if let Some(s) = seeds.get(&key) {
                self.repos_ext[e].extend(s.iter().cloned());
            }
        }

        // A repository rule an extension's implementation reaches, however
        // many macros deep, can be instantiated for any of that extension's
        // repositories.
        for e in 0..self.exts.len() {
            let Some(d) = (0..self.defs.len()).find(|d| self.defs[*d].role == Some(Sym::Ext(e)))
            else {
                continue;
            };
            let repos = self.repos_ext[e].clone();
            for sym in self.reachable(d) {
                if let Sym::Rule(r) = sym {
                    self.inst_rule[r].extend(repos.iter().cloned());
                }
            }
        }

        // A call site's scope is dynamic when it names a repository this
        // analysis cannot evaluate, or calls something it cannot resolve.
        let dynamics: Vec<(usize, Option<usize>)> = self
            .calls
            .iter()
            .filter(|c| matches!(c.name, NameArg::Dynamic) || c.opaque)
            .map(|c| (c.file, c.scope))
            .collect();
        for (file, scope) in dynamics {
            match scope {
                Some(d) => self.dynamic[d] = true,
                None => self.files[file].module_dynamic = true,
            }
        }

        // `instances`: a literal name at a call site, a name forwarded from
        // the enclosing def, or — for a name this analysis cannot evaluate —
        // the whole fallback set.
        for _ in 0..self.defs.len() + self.rules.len() + 2 {
            let mut changed = false;
            for c in 0..self.calls.len() {
                let (target, name, scope, file) = {
                    let call = &self.calls[c];
                    let Some(target) = call.callee else { continue };
                    (target, call.name.clone(), call.scope, call.file)
                };
                let call = Call {
                    name: &name,
                    scope,
                    file,
                };
                let add: BTreeSet<String> = match &call.name {
                    NameArg::Literal(l) => [l.clone()].into_iter().collect(),
                    NameArg::Forward => match call.scope {
                        Some(d) => self.inst_def[d].clone(),
                        None => BTreeSet::new(),
                    },
                    NameArg::Dynamic => self.files[call.file].fallback.clone(),
                    NameArg::Absent => {
                        // A wrapper called without a name it can forward: the
                        // repository it creates is unknowable from here.
                        if let Sym::Def(d) = target {
                            if !self.defs[d].fwd.is_empty() {
                                self.files[call.file].fallback.clone()
                            } else {
                                continue;
                            }
                        } else {
                            continue;
                        }
                    }
                };
                if add.is_empty() {
                    continue;
                }
                let slot = match target {
                    Sym::Def(d) => &mut self.inst_def[d],
                    Sym::Rule(r) => &mut self.inst_rule[r],
                    Sym::Ext(e) => &mut self.repos_ext[e],
                };
                let before = slot.len();
                slot.extend(add);
                changed |= slot.len() != before;
            }
            if !changed {
                break;
            }
        }

        // `defines`: what a macro creates when it is called with no name of
        // its own — its literal instantiations plus those of the macros it
        // calls.
        for _ in 0..self.defs.len() + 2 {
            let mut changed = false;
            for c in 0..self.calls.len() {
                let (d, name, callee) = {
                    let call = &self.calls[c];
                    let Some(d) = call.scope else { continue };
                    (d, call.name.clone(), call.callee)
                };
                let add: BTreeSet<String> = match (&name, callee) {
                    (NameArg::Literal(l), t) => {
                        // Instantiating a repository rule creates the instance
                        // *and* whatever its implementation declares by name.
                        let mut out: BTreeSet<String> = [l.clone()].into_iter().collect();
                        if let Some(t @ (Sym::Rule(_) | Sym::Ext(_))) = t {
                            out.extend(self.side_effects(t));
                        }
                        out
                    }
                    (NameArg::Absent, Some(t)) => self.value_of(t),
                    _ => continue,
                };
                if add.is_empty() {
                    continue;
                }
                let before = self.defines[d].len();
                self.defines[d].extend(add);
                changed |= self.defines[d].len() != before;
            }
            if !changed {
                break;
            }
        }

        self.refine();
    }

    /// The two refinements that need each other: what a helper def inherits
    /// from its callers, and what a file's set is once its loaders' use sites
    /// are folded in.
    ///
    /// Both only ever grow, so the loop terminates; the bound is a guard
    /// against a pathological closure, not the expected exit.
    fn refine(&mut self) {
        self.helper = vec![BTreeSet::new(); self.defs.len()];
        self.fileset = vec![BTreeSet::new(); self.files.len()];
        self.refining = true;
        for _ in 0..self.files.len() + 2 {
            self.round();
            // A file the rounds above learned nothing about keeps the
            // fallback, and its dependents inherit that on the next round.
            let mut promoted = false;
            for f in 0..self.files.len() {
                if self.fileset[f].is_empty() && !self.files[f].fallback.is_empty() {
                    self.fileset[f] = self.files[f].fallback.clone();
                    promoted = true;
                }
            }
            if !promoted {
                break;
            }
        }
        self.refining = false;
    }

    /// One fixpoint of the helper and file sets, on the sets as they stand.
    fn round(&mut self) {
        for _ in 0..self.files.len() + self.defs.len() + 2 {
            let mut changed = false;

            // A def that instantiates nothing is answered by its callers.
            let mut inherited: Vec<(usize, BTreeSet<String>)> = Vec::new();
            for d in 0..self.defs.len() {
                if self.defs[d].role.is_some() || self.dynamic[d] {
                    continue;
                }
                if !self.defines[d].is_empty() || !self.defs[d].fwd.is_empty() {
                    continue;
                }
                let mut repos = BTreeSet::new();
                for c in &self.calls {
                    if c.callee != Some(Sym::Def(d)) {
                        continue;
                    }
                    let a = self.at_tok(c.file, c.open);
                    repos.extend(if a.exact {
                        a.repos
                    } else {
                        self.set_of(c.file)
                    });
                }
                inherited.push((d, repos));
            }
            for (d, repos) in inherited {
                let before = self.helper[d].len();
                self.helper[d].extend(repos);
                changed |= self.helper[d].len() != before;
            }

            // What each file defines by itself.
            let owns: Vec<BTreeSet<String>> =
                (0..self.files.len()).map(|f| self.own_repos(f)).collect();
            for (f, o) in owns.into_iter().enumerate() {
                let before = self.fileset[f].len();
                self.fileset[f].extend(o);
                changed |= self.fileset[f].len() != before;
            }

            // And what every use of a symbol it exports does.
            let mut folded: Vec<(usize, BTreeSet<String>)> = Vec::new();
            for u in &self.uses {
                let v = self.use_value(u);
                if v.is_empty() {
                    continue;
                }
                for &x in &u.chain {
                    folded.push((x, v.clone()));
                }
            }
            for (x, v) in folded {
                let before = self.fileset[x].len();
                self.fileset[x].extend(v);
                changed |= self.fileset[x].len() != before;
            }

            if !changed {
                break;
            }
        }
    }

    /// What one use of an imported symbol can move.
    ///
    /// Calling an imported function does exactly what that function does, plus
    /// whatever the call site names outright — `pcl()` in a root file's
    /// `apollo_repositories()` is answered by `init()`'s own repositories, not
    /// by everything that root file declares. Any other use — a constant read
    /// into an expression — is attributed like any other occurrence, and falls
    /// back to the using file's own set when nothing was learned.
    fn use_value(&self, u: &Use) -> BTreeSet<String> {
        let file = &self.files[u.file];
        let name = match file.toks[u.tok].tok {
            Tok::Name(n) => n,
            _ => return BTreeSet::new(),
        };
        if matches!(
            file.toks.get(u.tok + 1).map(|t| t.tok),
            Some(Tok::Open(b'('))
        ) {
            let mut out = BTreeSet::new();
            if let Some(sym) = self.resolve(u.file, name) {
                out.extend(self.value_of(sym));
            } else {
                // A symbol the closure re-exports from elsewhere: nothing is
                // known about what calling it does.
                return self.set_of(u.file);
            }
            let open = u.tok + 1;
            let spans = arg_spans(&file.toks, open);
            let scope = self.scope_of(u.file, open);
            let callee = self.resolve(u.file, name);
            if let NameArg::Literal(l) =
                self.name_arg(scope, callee, name, &file.toks, file.text, &spans)
            {
                out.insert(l);
            }
            return out;
        }
        let a = self.at_tok(u.file, u.tok);
        if a.exact {
            a.repos
        } else {
            self.set_of(u.file)
        }
    }

    /// Every callable reachable from a def through calls, transitively.
    fn reachable(&self, from: usize) -> BTreeSet<Sym> {
        let mut seen: BTreeSet<Sym> = BTreeSet::new();
        let mut stack = vec![Sym::Def(from)];
        while let Some(s) = stack.pop() {
            if !seen.insert(s) {
                continue;
            }
            let Sym::Def(d) = s else { continue };
            for call in &self.calls {
                if call.scope == Some(d) {
                    if let Some(t) = call.callee {
                        stack.push(t);
                    }
                }
            }
        }
        seen.remove(&Sym::Def(from));
        seen
    }

    /// What calling a callable with no name of its own can create. Empty is a
    /// real answer here — a helper that instantiates nothing creates nothing —
    /// which is why this is not [`Attribution::def_repos`].
    fn value_of(&self, s: Sym) -> BTreeSet<String> {
        match s {
            Sym::Def(d) => self.raw_def_repos(d),
            Sym::Rule(r) => self.rule_repos(Sym::Rule(r)),
            Sym::Ext(e) => self.rule_repos(Sym::Ext(e)),
        }
    }

    /// Everything instantiating a rule or running an extension creates: the
    /// instances themselves, and whatever its implementation declares by name
    /// — the `cuda-include` and `cuda-lib` repositories apollo's
    /// `cuda_configure` writes alongside `local_config_cuda`.
    fn rule_repos(&self, s: Sym) -> BTreeSet<String> {
        let mut out = match s {
            Sym::Rule(r) => self.inst_rule[r].clone(),
            Sym::Ext(e) => self.repos_ext[e].clone(),
            Sym::Def(_) => BTreeSet::new(),
        };
        out.extend(self.side_effects(s));
        out
    }

    /// What a rule's or extension's implementation declares by name, over and
    /// above the instance itself.
    fn side_effects(&self, s: Sym) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for d in 0..self.defs.len() {
            if self.defs[d].role == Some(s) {
                if self.dynamic[d] {
                    out.extend(self.files[self.defs[d].file].fallback.iter().cloned());
                } else {
                    out.extend(self.defines[d].iter().cloned());
                }
            }
        }
        out
    }

    /// The repositories a def can instantiate or define, with no fallback.
    fn raw_def_repos(&self, d: usize) -> BTreeSet<String> {
        if let Some(role) = self.defs[d].role {
            return self.rule_repos(role);
        }
        if self.dynamic[d] {
            return self.files[self.defs[d].file].fallback.clone();
        }
        let mut repos = self.defines[d].clone();
        if !self.defs[d].fwd.is_empty() {
            repos.extend(self.inst_def[d].iter().cloned());
        }
        if repos.is_empty() {
            // A helper that instantiates nothing — apollo's
            // `_find_cuda_config(repository_ctx)`, which only reads a file —
            // is answered by where it is called from. Empty until
            // [`Attribution::refine`] has run.
            if let Some(h) = self.helper.get(d) {
                repos.extend(h.iter().cloned());
            }
        }
        repos
    }

    /// The repositories a def's *body* is attributed to.
    ///
    /// An empty answer means nothing was learned — a repository rule whose
    /// instantiation this analysis never saw, a macro nothing calls — and the
    /// occurrence keeps the old whole-file answer rather than being attributed
    /// to no repository at all, which would lose an edge.
    fn def_repos(&self, d: usize) -> Attributed {
        let repos = self.raw_def_repos(d);
        let exact = !repos.is_empty() && !self.dynamic[d];
        if repos.is_empty() {
            return Attributed {
                repos: self.files[self.defs[d].file].fallback.clone(),
                exact: false,
            };
        }
        Attributed { repos, exact }
    }

    /// The repositories an occurrence at token index `tok` of file `path` is
    /// attributed to.
    pub(crate) fn at(&self, path: &str, tok: usize) -> Attributed {
        let Some(&f) = self.by_path.get(path) else {
            return Attributed {
                repos: BTreeSet::new(),
                exact: false,
            };
        };
        self.at_tok(f, tok)
    }

    /// [`Attribution::at`], by file index.
    fn at_tok(&self, f: usize, tok: usize) -> Attributed {
        let file = &self.files[f];
        // The innermost enclosing call that names a repository outright.
        for open in self.enclosing_calls(f, tok) {
            if !matches!(file.toks[open].tok, Tok::Open(b'(')) {
                continue;
            }
            let spans = arg_spans(&file.toks, open);
            let scope = self.scope_of(f, open);
            let head = match open.checked_sub(1).map(|j| file.toks[j].tok) {
                Some(Tok::Name(h)) => h,
                _ => continue,
            };
            let dotted = matches!(
                open.checked_sub(2).map(|j| file.toks[j].tok),
                Some(Tok::Punct(b'.'))
            );
            let callee = if dotted { None } else { self.resolve(f, head) };
            if let NameArg::Literal(l) =
                self.name_arg(scope, callee, head, &file.toks, file.text, &spans)
            {
                return Attributed {
                    repos: [l].into_iter().collect(),
                    exact: true,
                };
            }
        }
        // Inside a `repository_rule(...)` / `module_extension(...)`
        // declaration: an attribute default the rule itself resolves.
        for (span, sym) in &file.decls {
            if !span.contains(&tok) {
                continue;
            }
            let repos = self.rule_repos(*sym);
            return if repos.is_empty() {
                Attributed {
                    repos: file.fallback.clone(),
                    exact: false,
                }
            } else {
                Attributed { repos, exact: true }
            };
        }
        match self.scope_of(f, tok) {
            Some(d) => self.def_repos(d),
            // Module level. A constant, a docstring, an `attr.label` outside
            // any declaration: it reaches a repository only through this
            // file's own definitions or through a symbol another file imports
            // from it, and both are what the file's set is. `exact` stays
            // false — nothing about the occurrence itself was read, and a
            // mapping must not be derived from a set this wide.
            None => Attributed {
                repos: self.set_of(f),
                exact: false,
            },
        }
    }

    /// Everything a change to file `f` can move, with the universe as the
    /// last resort.
    fn set_of(&self, f: usize) -> BTreeSet<String> {
        match self.fileset.get(f) {
            Some(r) if !r.is_empty() => r.clone(),
            _ if self.refining => BTreeSet::new(),
            _ => self.files[f].fallback.clone(),
        }
    }

    /// Everything a change to a file can move: what it defines itself, plus
    /// what every use of the symbols it exports does.
    pub(crate) fn file_repos(&self, path: &str) -> Attributed {
        let Some(&f) = self.by_path.get(path) else {
            return Attributed {
                repos: BTreeSet::new(),
                exact: false,
            };
        };
        Attributed {
            repos: self.set_of(f),
            exact: !self.files[f].module_dynamic,
        }
    }

    /// What a file defines by itself, before its loaders' use sites are
    /// folded in.
    fn own_repos(&self, f: usize) -> BTreeSet<String> {
        let file = &self.files[f];
        let mut repos = file.seeded.clone();
        for &d in &file.defs {
            repos.extend(self.raw_def_repos(d));
        }
        for (_, sym) in &file.decls {
            repos.extend(self.rule_repos(*sym));
        }
        // Module-level calls with a literal name, and anything module-level
        // this analysis could not read.
        for call in self
            .calls
            .iter()
            .filter(|c| c.file == f && c.scope.is_none())
        {
            match &call.name {
                NameArg::Literal(l) => {
                    repos.insert(l.clone());
                }
                NameArg::Absent => {
                    if let Some(t) = call.callee {
                        repos.extend(self.value_of(t));
                    }
                }
                _ => {}
            }
        }
        if file.module_dynamic {
            repos.extend(file.fallback.iter().cloned());
        }
        repos
    }

    /// The token indices of the brackets enclosing a token, innermost first.
    fn enclosing_calls(&self, f: usize, tok: usize) -> Vec<usize> {
        let parent = &self.files[f].parent;
        let mut out = Vec::new();
        let mut i = match parent.get(tok) {
            Some(&p) if p != usize::MAX => p,
            _ => return out,
        };
        loop {
            out.push(i);
            match parent.get(i) {
                Some(&p) if p != usize::MAX => i = p,
                _ => return out,
            }
        }
    }
}

/// For each token, the `(` of the bracket it sits directly inside.
fn parents(toks: &[Lexed<'_>]) -> Vec<usize> {
    let mut out = vec![usize::MAX; toks.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match t.tok {
            Tok::Open(_) => {
                out[i] = stack.last().copied().unwrap_or(usize::MAX);
                stack.push(i);
            }
            Tok::Close => {
                stack.pop();
                out[i] = stack.last().copied().unwrap_or(usize::MAX);
            }
            _ => out[i] = stack.last().copied().unwrap_or(usize::MAX),
        }
    }
    out
}

/// The token ranges of a file's top-level statements: every token in column
/// zero at bracket depth zero starts one, which is exactly Starlark's own
/// rule, so a `def`'s range runs to the end of its indented body.
pub(crate) fn top_level_spans(toks: &[Lexed<'_>]) -> Vec<Range<usize>> {
    let mut starts = Vec::new();
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        if t.col0 && depth == 0 {
            starts.push(i);
        }
        match t.tok {
            Tok::Open(_) => depth += 1,
            Tok::Close => depth -= 1,
            _ => {}
        }
    }
    starts.push(toks.len());
    starts
        .windows(2)
        .filter(|w| w[1] > w[0])
        .map(|w| w[0]..w[1])
        .collect()
}

/// The `(keyword, token range)` of every argument of the call whose `(` is at
/// `open`.
pub(crate) fn arg_spans(
    toks: &[Lexed<'_>],
    open: usize,
) -> Vec<(Option<String>, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = open + 1;
    let mut key: Option<String> = None;
    let mut i = open;
    while i < toks.len() {
        match toks[i].tok {
            Tok::Open(_) => depth += 1,
            Tok::Close => {
                depth -= 1;
                if depth == 0 {
                    if i > start {
                        out.push((key.take(), start..i));
                    }
                    break;
                }
            }
            Tok::Punct(b',') if depth == 1 => {
                if i > start {
                    out.push((key.take(), start..i));
                }
                start = i + 1;
            }
            Tok::Name(n)
                if depth == 1
                    && i == start
                    && matches!(toks.get(i + 1).map(|t| t.tok), Some(Tok::Punct(b'=')))
                    && !matches!(toks.get(i + 2).map(|t| t.tok), Some(Tok::Punct(b'='))) =>
            {
                key = Some(n.to_string());
                start = i + 2;
            }
            _ => {}
        }
        i += 1;
    }
    out
}
