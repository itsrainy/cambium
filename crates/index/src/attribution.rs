//! Which repository a *single occurrence* in a workspace-closure file feeds.
//!
//! [`crate::externals`] owns the question and the tables; this module owns the
//! static reading of Starlark that answers it per occurrence rather than per
//! file. See the "Attribution" section of that module's documentation for the
//! reasoning; what follows is the shape of the analysis.
//!
//! One pass over every file in the workspace closure (`WORKSPACE*`,
//! `MODULE.bazel`, and every `.bzl` they reach) reads its
//! [`crate::starlark::Module`] and collects, per file:
//!
//! - its top-level `def`s, with their parameter names;
//! - its `load()`s, alias-aware, so a symbol can be resolved to the file that
//!   defines it;
//! - its `X = repository_rule(implementation = _f, ...)` and
//!   `E = module_extension(implementation = _g, ...)` assignments;
//! - every call in it: the callee symbol, the def it is written in, the call
//!   enclosing it, and how it names the repository it may create;
//! - every string literal, with the same context, keyed by its byte offset,
//!   which is how [`Attribution::at`] answers for one.
//!
//! Two fixpoints then run over the call graph: `instances`, the repository
//! names a callable can be asked to create, and `defines`, the repository
//! names a macro creates when called. Every unknown shape resolves to the
//! file's fallback set, which is what the whole file used to get — and a
//! statement the parser could not read makes the scope it sits in fall back
//! outright.

use std::collections::{BTreeMap, BTreeSet};

use crate::loads::{resolve_label, Load, RepoMapping};
use crate::starlark::{stmt_exprs, Arg, ArgKind, Expr, ExprKind, Module, ParamKind, Span, Stmt};
use crate::vocabulary::{names_a_repository, KEYWORDS, PURE_BUILTINS};

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
    name: String,
    params: Vec<String>,
    /// Parameters this def forwards as some call's `name`.
    fwd: BTreeSet<String>,
    /// The rule / extension whose implementation this def is.
    role: Option<Sym>,
}

/// A `X = repository_rule(...)` or `E = module_extension(...)` binding.
struct RuleInfo {
    file: usize,
    name: String,
    /// The `implementation =` argument, when it is a bare name.
    implementation: Option<String>,
}

struct ExtInfo {
    file: usize,
    name: String,
    implementation: Option<String>,
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
    /// Byte offset of the call in its file.
    offset: usize,
    /// The def the call is written in, if any.
    scope: Option<usize>,
    callee: Option<Sym>,
    /// Set when the callee is a name this analysis could not resolve and that
    /// is not a pure builtin.
    opaque: bool,
    /// The callee as written, for the name-based fallback of
    /// [`Attribution::liveness`] when it could not be resolved.
    head: String,
    /// How the call names the repository it *instantiates*: `Absent` for a
    /// method call, which instantiates nothing this analysis can see.
    name: NameArg,
    /// The `name =` as written, method call or not: what a literal *beside*
    /// it is attributed to. `native.new_local_repository(name =
    /// "com_google_absl", build_file = "//third_party/absl:absl.BUILD")`
    /// creates no instance the fixpoints can follow, but the label in it
    /// still feeds `com_google_absl` and nothing else.
    named: NameArg,
}

/// One place a file uses a symbol it imported.
///
/// `chain` is every file the symbol was resolved through — the defining file
/// and any that re-export it — because a change to any of them changes what
/// this use does.
struct Use {
    file: usize,
    /// The byte offset of the use, an occurrence like any other.
    offset: usize,
    /// The name as written.
    name: String,
    /// The call this use is the callee of, when it is one.
    call: Option<usize>,
    chain: Vec<usize>,
}

/// Where an occurrence sits: the def, the innermost enclosing call, and the
/// rule or extension declaration, if any.
#[derive(Clone, Copy, Debug)]
struct Occ {
    scope: Option<usize>,
    call: Option<usize>,
    decl: Option<Sym>,
}

/// One file of the workspace closure.
struct FileScan<'a> {
    path: String,
    module: Module<'a>,
    /// Alias -> (defining file path, original name).
    loads: BTreeMap<String, (String, String)>,
    /// Top-level names this file binds to a callable.
    syms: BTreeMap<String, Sym>,
    defs: Vec<usize>,
    /// The rules and extensions declared here, for the file's own answer.
    decls: Vec<Sym>,
    /// Whether any module-level (or otherwise unattributable) shape forces
    /// this file's module scope to the fallback set.
    module_dynamic: bool,
    fallback: BTreeSet<String>,
    /// Whether [`FileScan::fallback`] is the whole universe rather than the
    /// repositories of a seeded subtree the file sits under.
    wide: bool,
    /// Repositories `MODULE.bazel` says this file declares: the `use_repo`
    /// names of an extension it defines, the instantiations of a repository
    /// rule `use_repo_rule` names in it. Part of the file's own answer even
    /// when the declaration itself could not be read out of the text.
    seeded: BTreeSet<String>,
    /// Every string literal and every call, by byte offset, with its context.
    occ: BTreeMap<usize, Occ>,
    /// Defs whose body holds something the parser could not read.
    unparsed_in: BTreeSet<usize>,
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
    /// Every def named as a value rather than called: `(file, scope, name)`.
    named_defs: Vec<(usize, Option<usize>, String)>,
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
    /// Whether a def can run while Bazel evaluates the workspace: a rule or
    /// extension implementation, a def called at module level of a closure
    /// file, a def a live def calls or names, or any def in a file with a
    /// module-level call this analysis cannot read. A def that is none of
    /// those is a BUILD-file macro that a load chain dragged into the
    /// closure — `go_sql_gen` in a `tools/rules/sqlgen.bzl` that
    /// `tools/rules/go.bzl` happens to load — and its labels are rule
    /// attributes the graph already carries, not repository inputs. Read as
    /// inputs with the file's fallback they fed every repository in the
    /// universe, and one such label seeded 3,819 repositories at once.
    live: Vec<bool>,
    /// Every literal `name =` with the def it was written in, before
    /// liveness decides which of them the universe keeps.
    named: Vec<(Option<usize>, String)>,
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

/// The context a walk over one file carries.
#[derive(Clone, Copy)]
struct Ctx {
    scope: Option<usize>,
    call: Option<usize>,
    decl: Option<Sym>,
}

/// What one walk over a file found, before it is folded into the analysis.
#[derive(Default)]
struct Found {
    calls: Vec<CallSite>,
    names: Vec<(Option<usize>, String)>,
    fwd: Vec<(usize, String)>,
    uses: Vec<(usize, String, Option<usize>)>,
    named_defs: Vec<(Option<usize>, String)>,
    occ: BTreeMap<usize, Occ>,
    module_dynamic: bool,
    unparsed_in: BTreeSet<usize>,
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
            named_defs: Vec::new(),
            helper: Vec::new(),
            fileset: Vec::new(),
            refining: false,
            literal_names: BTreeSet::new(),
            live: Vec::new(),
            named: Vec::new(),
        };
        for (i, (path, text)) in texts.iter().enumerate() {
            a.by_path.insert(path.clone(), i);
            a.files.push(FileScan {
                path: path.clone(),
                module: Module::parse(text),
                loads: BTreeMap::new(),
                syms: BTreeMap::new(),
                defs: Vec::new(),
                decls: Vec::new(),
                module_dynamic: false,
                fallback: BTreeSet::new(),
                wide: false,
                seeded: BTreeSet::new(),
                occ: BTreeMap::new(),
                unparsed_in: BTreeSet::new(),
            });
        }
        for i in 0..a.files.len() {
            a.declarations(i, m);
        }
        // Implementations, which need every file's symbols to resolve.
        a.implementations();
        // Twice: the first pass learns which parameters each def forwards as
        // a repository name, which the second pass needs to read a call's
        // positional first argument correctly.
        for _ in 0..2 {
            a.calls.clear();
            a.literal_names.clear();
            a.named.clear();
            a.uses.clear();
            a.named_defs.clear();
            for f in &mut a.files {
                f.module_dynamic = false;
                f.occ.clear();
                f.unparsed_in.clear();
            }
            for i in 0..a.files.len() {
                a.walk_file(i);
            }
        }
        a.liveness();
        a
    }

    /// The files read, as `(path, module)`, for the callers that walk the
    /// literals themselves.
    pub(crate) fn modules(&self) -> impl Iterator<Item = (&str, &Module<'a>)> {
        self.files.iter().map(|f| (f.path.as_str(), &f.module))
    }

    /// Computes [`Attribution::live`] and rebuilds the universe of literal
    /// names from the live defs and module level only.
    fn liveness(&mut self) {
        let n = self.defs.len();
        let mut live = vec![false; n];
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut roots: Vec<usize> = Vec::new();
        let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for d in 0..n {
            if self.defs[d].role.is_some() || self.files[self.defs[d].file].module_dynamic {
                roots.push(d);
            }
            by_name
                .entry(self.defs[d].name.as_str())
                .or_default()
                .push(d);
        }
        for c in &self.calls {
            match c.callee {
                Some(Sym::Def(callee)) => match c.scope {
                    Some(d) => edges[d].push(callee),
                    None => roots.push(callee),
                },
                // A call this analysis could not resolve may still be one of
                // our defs under an alias the resolver missed: every def of
                // that name is live wherever the call is. Losing an edge is
                // the one direction this must not err in.
                None if c.opaque => {
                    for &d in by_name
                        .get(c.head.as_str())
                        .map(Vec::as_slice)
                        .unwrap_or(&[])
                    {
                        match c.scope {
                            Some(s) => edges[s].push(d),
                            None => roots.push(d),
                        }
                    }
                }
                _ => {}
            }
        }
        // A def named as a value — `_maybe(_go_repo, ...)`, `implementation
        // = _f` on a declaration the scanner did not read — runs wherever the
        // value is used, so it is live wherever it is named.
        for (f, scope, name) in &self.named_defs {
            let Some(Sym::Def(c)) = self.resolve(*f, name) else {
                continue;
            };
            match scope {
                Some(d) if *d != c => edges[*d].push(c),
                Some(_) => {}
                None => roots.push(c),
            }
        }
        let mut stack = roots;
        while let Some(d) = stack.pop() {
            if live[d] {
                continue;
            }
            live[d] = true;
            stack.extend(edges[d].iter().copied());
        }
        self.live = live;
        self.literal_names = self
            .named
            .iter()
            .filter(|(scope, _)| scope.map_or(true, |d| self.live[d]))
            .map(|(_, l)| l.clone())
            .collect();
    }

    /// Whether def `d` can run during workspace evaluation; see
    /// [`Attribution::live`]. Before [`Attribution::liveness`] ran, every def
    /// is.
    fn is_live(&self, d: usize) -> bool {
        self.live.get(d).copied().unwrap_or(true)
    }

    /// Whether file `f` keeps its fallback when the rounds learn nothing
    /// about it. It does when something in it could not be read — a
    /// module-level call the closure cannot resolve, or a def the workspace
    /// runs whose body names a repository this analysis cannot evaluate — or
    /// when the fallback is a seeded subtree's few repositories rather than
    /// the universe (envoy's `repo.bzl`, whose module-level constants are the
    /// extension's inputs). A file read in full, found to define nothing, and
    /// whose only fallback is everything defines nothing: a `.bzl` of
    /// BUILD-file macros that a load chain dragged into the workspace closure
    /// is not the universe, and handing it the universe handed it to every
    /// file that imported a symbol from it (issue #9).
    fn keeps_fallback(&self, f: usize) -> bool {
        !self.files[f].wide
            || self.files[f].module_dynamic
            || self.files[f]
                .defs
                .iter()
                .any(|&d| self.is_live(d) && self.dynamic.get(d).copied().unwrap_or(false))
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
        let pkg = crate::loads::owning_dir(&file.path);
        let mut loads = BTreeMap::new();
        let mut syms = BTreeMap::new();
        let mut defs = Vec::new();
        let mut decls = Vec::new();
        let mut new_defs: Vec<DefInfo> = Vec::new();
        let mut new_rules: Vec<RuleInfo> = Vec::new();
        let mut new_exts: Vec<ExtInfo> = Vec::new();

        for st in &file.module.stmts {
            match st {
                // `def name(params):`
                Stmt::Def(d) => {
                    let params = d
                        .params
                        .iter()
                        .filter(|p| p.kind == ParamKind::Normal)
                        .map(|p| p.name.to_string())
                        .collect();
                    let id = self.defs.len() + new_defs.len();
                    syms.insert(d.name.to_string(), Sym::Def(id));
                    defs.push(id);
                    new_defs.push(DefInfo {
                        file: f,
                        name: d.name.to_string(),
                        params,
                        fwd: BTreeSet::new(),
                        role: None,
                    });
                }
                // `X = repository_rule(...)` / `E = module_extension(...)`
                Stmt::Assign(a) => {
                    let (Some(name), ExprKind::Call { callee, args }) =
                        (a.target.as_name(), &a.value.kind)
                    else {
                        continue;
                    };
                    let implementation = args
                        .iter()
                        .find(|x| x.keyword() == Some("implementation"))
                        .and_then(|x| x.value().as_name())
                        .map(str::to_string);
                    match callee.as_name() {
                        Some("repository_rule") => {
                            let id = self.rules.len() + new_rules.len();
                            syms.insert(name.to_string(), Sym::Rule(id));
                            decls.push(Sym::Rule(id));
                            new_rules.push(RuleInfo {
                                file: f,
                                name: name.to_string(),
                                implementation,
                            });
                        }
                        Some("module_extension") => {
                            let id = self.exts.len() + new_exts.len();
                            syms.insert(name.to_string(), Sym::Ext(id));
                            decls.push(Sym::Ext(id));
                            new_exts.push(ExtInfo {
                                file: f,
                                name: name.to_string(),
                                implementation,
                            });
                        }
                        _ => {}
                    }
                }
                // `load("//pkg:f.bzl", "sym", alias = "sym")`
                Stmt::Load(l) => {
                    let Some(label) = l.label() else { continue };
                    let Some(Load::Main(p)) = resolve_label(label, &pkg, m) else {
                        continue;
                    };
                    for (alias, orig) in l.items() {
                        loads.insert(alias.to_string(), (p.clone(), orig.to_string()));
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
    fn implementations(&mut self) {
        let mut roles: Vec<(usize, Sym)> = Vec::new();
        for (r, rule) in self.rules.iter().enumerate() {
            if let Some(n) = &rule.implementation {
                if let Some(Sym::Def(d)) = self.resolve(rule.file, n) {
                    roles.push((d, Sym::Rule(r)));
                }
            }
        }
        for (e, ext) in self.exts.iter().enumerate() {
            if let Some(n) = &ext.implementation {
                if let Some(Sym::Def(d)) = self.resolve(ext.file, n) {
                    roles.push((d, Sym::Ext(e)));
                }
            }
        }
        for (d, role) in roles {
            self.defs[d].role = Some(role);
        }
    }

    /// One walk over a file: every call, literal, use and named def, with
    /// the context it sits in.
    fn walk_file(&mut self, f: usize) {
        let mut found = Found::default();
        {
            let file = &self.files[f];
            // Top-level defs in source order are the ids `declarations`
            // handed out in source order, and likewise the declarations.
            let mut def_ids = file.defs.iter().copied();
            let mut decl_syms = file.decls.iter().copied();
            for st in &file.module.stmts {
                let mut ctx = Ctx {
                    scope: None,
                    call: None,
                    decl: None,
                };
                match st {
                    Stmt::Def(_) => ctx.scope = def_ids.next(),
                    Stmt::Assign(a)
                        if a.target.as_name().is_some()
                            && matches!(
                                &a.value.kind,
                                ExprKind::Call { callee, .. }
                                    if matches!(
                                        callee.as_name(),
                                        Some("repository_rule" | "module_extension")
                                    )
                            ) =>
                    {
                        ctx.decl = decl_syms.next();
                    }
                    _ => {}
                }
                self.walk_stmt(f, st, ctx, &mut found);
            }
        }
        for (d, p) in found.fwd {
            self.defs[d].fwd.insert(p);
        }
        self.literal_names
            .extend(found.names.iter().map(|(_, l)| l.clone()));
        self.named.extend(found.names);
        for (offset, name, call) in found.uses {
            let chain = self.origin_chain(f, &name);
            if !chain.is_empty() {
                self.uses.push(Use {
                    file: f,
                    offset,
                    name,
                    call,
                    chain,
                });
            }
        }
        self.named_defs
            .extend(found.named_defs.into_iter().map(|(s, n)| (f, s, n)));
        let file = &mut self.files[f];
        file.module_dynamic |= found.module_dynamic;
        file.occ = found.occ;
        file.unparsed_in = found.unparsed_in;
        self.calls.extend(found.calls);
    }

    fn walk_stmt(&self, f: usize, st: &Stmt<'a>, ctx: Ctx, found: &mut Found) {
        match st {
            Stmt::Unparsed(span) => self.unreadable(f, span, ctx, found),
            Stmt::Assign(a) => {
                // A bare name on the left of `=` is bound, not used; it
                // still names a def, for liveness, when it is one.
                if let Some(n) = a.target.as_name() {
                    found.named_defs.push((ctx.scope, n.to_string()));
                    self.walk_expr(f, &a.value, ctx, found);
                } else {
                    self.walk_expr(f, &a.target, ctx, found);
                    self.walk_expr(f, &a.value, ctx, found);
                }
            }
            Stmt::Load(l) => {
                // The label and symbol names are literals, not uses.
                for a in &l.args {
                    self.walk_expr(f, a.value(), ctx, found);
                }
            }
            _ => {
                for e in stmt_exprs(st) {
                    self.walk_expr(f, e, ctx, found);
                }
            }
        }
        match st {
            Stmt::Def(d) => {
                for s in &d.body {
                    self.walk_stmt(f, s, ctx, found);
                }
            }
            Stmt::If(i) => {
                for (_, body) in &i.branches {
                    for s in body {
                        self.walk_stmt(f, s, ctx, found);
                    }
                }
            }
            Stmt::For(l) => {
                for s in &l.body {
                    self.walk_stmt(f, s, ctx, found);
                }
            }
            _ => {}
        }
    }

    /// Something the parser could not read: its literals are still
    /// occurrences, and the scope it sits in can no longer be read in full.
    fn unreadable(&self, f: usize, span: &Span, ctx: Ctx, found: &mut Found) {
        let occ = Occ {
            scope: ctx.scope,
            call: ctx.call,
            decl: ctx.decl,
        };
        for (s, _) in self.files[f].module.strings_in(span) {
            found.occ.insert(s.start, occ);
        }
        match ctx.scope {
            Some(d) => {
                found.unparsed_in.insert(d);
            }
            None => found.module_dynamic = true,
        }
    }

    fn walk_expr(&self, f: usize, e: &Expr<'a>, ctx: Ctx, found: &mut Found) {
        let file = &self.files[f];
        let occ = Occ {
            scope: ctx.scope,
            call: ctx.call,
            decl: ctx.decl,
        };
        match &e.kind {
            ExprKind::Str(_) => {
                found.occ.insert(e.span.start, occ);
            }
            ExprKind::Unparsed => self.unreadable(f, &e.span, ctx, found),
            ExprKind::Name(n) => {
                found.named_defs.push((ctx.scope, n.to_string()));
                if file.loads.contains_key(*n) {
                    found.occ.insert(e.span.start, occ);
                    found.uses.push((e.span.start, n.to_string(), None));
                }
            }
            ExprKind::Call { callee, args } => {
                let call = self.call_site(f, callee, args, ctx, found);
                found.occ.insert(e.span.start, occ);
                match &callee.kind {
                    // The callee is a use of its name, as a call; and a
                    // keyword is never a call, so `if (x)` is not one.
                    ExprKind::Name(n) => {
                        if let Some(c) = call {
                            if file.loads.contains_key(*n) {
                                found.uses.push((callee.span.start, n.to_string(), Some(c)));
                            }
                        } else if !KEYWORDS.contains(n) {
                            found.named_defs.push((ctx.scope, n.to_string()));
                        }
                    }
                    ExprKind::Attr { object, .. } => self.walk_expr(f, object, ctx, found),
                    _ => self.walk_expr(f, callee, ctx, found),
                }
                let inner = Ctx {
                    call: call.or(ctx.call),
                    ..ctx
                };
                for a in args {
                    self.walk_expr(f, a.value(), inner, found);
                }
            }
            ExprKind::Attr { object, .. } => self.walk_expr(f, object, ctx, found),
            ExprKind::Index { object, index } => {
                self.walk_expr(f, object, ctx, found);
                self.walk_expr(f, index, ctx, found);
            }
            ExprKind::Slice { object, parts } => {
                self.walk_expr(f, object, ctx, found);
                for p in parts.iter().flatten() {
                    self.walk_expr(f, p, ctx, found);
                }
            }
            ExprKind::List(items) | ExprKind::Tuple(items) => {
                for x in items {
                    self.walk_expr(f, x, ctx, found);
                }
            }
            ExprKind::Dict(items) => {
                for (k, v) in items {
                    self.walk_expr(f, k, ctx, found);
                    self.walk_expr(f, v, ctx, found);
                }
            }
            ExprKind::BinOp { lhs, rhs, .. } => {
                self.walk_expr(f, lhs, ctx, found);
                self.walk_expr(f, rhs, ctx, found);
            }
            ExprKind::UnOp { operand, .. } => self.walk_expr(f, operand, ctx, found),
            ExprKind::Cond { then, test, orelse } => {
                self.walk_expr(f, then, ctx, found);
                self.walk_expr(f, test, ctx, found);
                self.walk_expr(f, orelse, ctx, found);
            }
            ExprKind::Lambda { params, body } => {
                for p in params {
                    if let Some(d) = &p.default {
                        self.walk_expr(f, d, ctx, found);
                    }
                }
                self.walk_expr(f, body, ctx, found);
            }
            ExprKind::Comprehension { element, clauses } => {
                self.walk_expr(f, element, ctx, found);
                for c in clauses {
                    match c {
                        crate::starlark::Clause::For { target, iter } => {
                            self.walk_expr(f, target, ctx, found);
                            self.walk_expr(f, iter, ctx, found);
                        }
                        crate::starlark::Clause::If(x) => self.walk_expr(f, x, ctx, found),
                    }
                }
            }
            ExprKind::Int | ExprKind::Float => {}
        }
    }

    /// Records one call, and returns its index when it is a call site this
    /// analysis reads: `f(...)` or `x.f(...)`, `f` not a keyword.
    fn call_site(
        &self,
        f: usize,
        callee: &Expr<'a>,
        args: &[Arg<'a>],
        ctx: Ctx,
        found: &mut Found,
    ) -> Option<usize> {
        let file = &self.files[f];
        let head = callee.head()?;
        let dotted = matches!(callee.kind, ExprKind::Attr { .. });
        // `return (a or b)`, `if (x)`, `not (y)`: a keyword before a
        // parenthesised expression is not a call.
        if !dotted && KEYWORDS.contains(&head) {
            return None;
        }
        let scope = ctx.scope;
        let resolved = if dotted { None } else { self.resolve(f, head) };
        // A method call is not an instantiation: `native.bind(name =
        // "ssl")` declares a `//external` alias, not a repository, and
        // reading its `name` would attribute the labels beside it to a
        // repository that does not exist. Its literals fall through to
        // the scope, which is the wide answer.
        let named = self.name_arg(scope, resolved, head, args);
        let name = if dotted {
            NameArg::Absent
        } else {
            named.clone()
        };
        if let NameArg::Literal(l) = &name {
            if names_a_repository(head) {
                found.names.push((scope, l.clone()));
            }
        }
        if let (NameArg::Forward, Some(d)) = (&name, scope) {
            if let Some(p) = self.forwarded_param(scope, args) {
                found.fwd.push((d, p));
            }
        }
        let opaque = !dotted
            && resolved.is_none()
            && !PURE_BUILTINS.contains(&head)
            && matches!(name, NameArg::Absent)
            && !args.is_empty();
        if opaque && scope.is_none() {
            found.module_dynamic = true;
            tracing::debug!(
                file = %file.path,
                call = head,
                "a module-level call this analysis cannot read; the file keeps the wide answer"
            );
        }
        let id = self.calls.len() + found.calls.len();
        found.calls.push(CallSite {
            file: f,
            offset: callee.span.start,
            scope,
            callee: resolved,
            opaque,
            head: head.to_string(),
            name,
            named,
        });
        Some(id)
    }

    /// The parameter a forwarding call passes as its name.
    fn forwarded_param(&self, scope: Option<usize>, args: &[Arg<'a>]) -> Option<String> {
        let d = scope?;
        let params = &self.defs[d].params;
        for a in args {
            let k = a.keyword();
            if k.is_some() && k != Some("name") {
                continue;
            }
            if matches!(a.kind, ArgKind::Positional(_) | ArgKind::Keyword(..)) {
                if let Some(n) = a.value().as_name() {
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
        args: &[Arg<'a>],
    ) -> NameArg {
        let params = scope
            .map(|d| self.defs[d].params.clone())
            .unwrap_or_default();
        let classify = |a: &Arg<'a>| -> NameArg {
            match &a.kind {
                ArgKind::Positional(e) | ArgKind::Keyword(_, e) => match &e.kind {
                    ExprKind::Str(s) => NameArg::Literal(s.clone()),
                    ExprKind::Name(n) if params.iter().any(|p| p == n) => NameArg::Forward,
                    _ => NameArg::Dynamic,
                },
                // `*args` / `**kwargs`: whatever is in there.
                ArgKind::Star(_) | ArgKind::StarStar(_) => NameArg::Dynamic,
            }
        };
        // The keyword the callee reads as a repository name: `name`, or a
        // parameter it is known to forward under another spelling.
        let mut keys: Vec<String> = vec!["name".to_string()];
        if let Some(Sym::Def(d)) = callee {
            keys.extend(self.defs[d].fwd.iter().cloned());
        }
        for k in &keys {
            if let Some(a) = args.iter().find(|a| a.keyword() == Some(k.as_str())) {
                return classify(a);
            }
        }
        let Some(first) = args.iter().find(|a| a.keyword().is_none()) else {
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
                    return classify(first);
                }
                NameArg::Absent
            }
            // `Label(dep)` / `str(name)` forward nothing: a pure builtin
            // creates no repository, and reading its argument as a name would
            // turn apollo's `clean_dep(dep)` into a wrapper whose every call
            // site declares a label-shaped "repository".
            _ if PURE_BUILTINS.contains(&head) => NameArg::Absent,
            _ => match classify(first) {
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
    /// unattributable, and `wide` names the files whose fallback is the whole
    /// universe rather than a seeded subtree's; `seeds` come from
    /// `MODULE.bazel`.
    pub(crate) fn solve(
        &mut self,
        fallbacks: &BTreeMap<String, BTreeSet<String>>,
        seeds: &BTreeMap<(String, String), BTreeSet<String>>,
        wide: &BTreeSet<String>,
    ) {
        for i in 0..self.files.len() {
            let path = self.files[i].path.clone();
            self.files[i].fallback = fallbacks.get(&path).cloned().unwrap_or_default();
            self.files[i].wide = wide.contains(&path);
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
        // analysis cannot evaluate, or calls something it cannot resolve —
        // or holds a statement the parser could not read at all.
        let dynamics: Vec<(usize, Option<usize>, String, bool)> = self
            .calls
            .iter()
            .filter(|c| matches!(c.name, NameArg::Dynamic) || c.opaque)
            // A dynamic name in a def the workspace never runs — a BUILD
            // macro's `name = name + "_lib"` — makes nothing unreadable.
            .filter(|c| c.scope.map_or(true, |d| self.is_live(d)))
            .map(|c| (c.file, c.scope, c.head.clone(), c.opaque))
            .collect();
        for (file, scope, head, opaque) in dynamics {
            match scope {
                Some(d) => {
                    if !self.dynamic[d] {
                        tracing::debug!(
                            file = %self.files[file].path,
                            def = %self.defs[d].name,
                            call = %head,
                            why = if opaque { "unresolved callee" } else { "dynamic name" },
                            "a call this analysis cannot read; the def keeps the wide answer"
                        );
                    }
                    self.dynamic[d] = true
                }
                None => self.files[file].module_dynamic = true,
            }
        }
        for f in 0..self.files.len() {
            for &d in &self.files[f].unparsed_in {
                if self.is_live(d) {
                    self.dynamic[d] = true;
                }
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
                    if call.scope.is_some_and(|d| !self.is_live(d)) {
                        continue;
                    }
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
                if self.fileset[f].is_empty()
                    && !self.files[f].fallback.is_empty()
                    && self.keeps_fallback(f)
                {
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
                    let a = self.at_offset(c.file, c.offset);
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
        if let Some(c) = u.call {
            let mut out = BTreeSet::new();
            if let Some(sym) = self.resolve(u.file, &u.name) {
                out.extend(self.value_of(sym));
            } else {
                // A symbol the closure re-exports from elsewhere: nothing is
                // known about what calling it does.
                return self.set_of(u.file);
            }
            if let NameArg::Literal(l) = &self.calls[c].name {
                out.insert(l.clone());
            }
            return out;
        }
        let a = self.at_offset(u.file, u.offset);
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
            // A def nothing in the workspace runs creates nothing, however
            // its body reads; the use of it that asked was in dead code too.
            Sym::Def(d) if !self.is_live(d) => BTreeSet::new(),
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
            let repos = self.rule_repos(role);
            // The implementation of a rule nothing instantiates is still a
            // def, and may be called as a helper by one that is: xla's
            // `_create_local_rocm_repository` implements `remote_rocm_configure`,
            // never declared, and is what `rocm_configure`'s implementation
            // calls to write the repository. An empty role answer must not
            // hide the call sites, or every template that helper reads falls
            // to the universe.
            if !repos.is_empty() {
                return repos;
            }
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
        if !self.is_live(d) {
            return Attributed {
                repos: BTreeSet::new(),
                exact: true,
            };
        }
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

    /// The repositories an occurrence at byte `offset` of file `path` is
    /// attributed to.
    pub(crate) fn at(&self, path: &str, offset: usize) -> Attributed {
        let Some(&f) = self.by_path.get(path) else {
            return Attributed {
                repos: BTreeSet::new(),
                exact: false,
            };
        };
        self.at_offset(f, offset)
    }

    /// [`Attribution::at`], by file index.
    fn at_offset(&self, f: usize, offset: usize) -> Attributed {
        let file = &self.files[f];
        // An offset the walk never saw is answered like the module level:
        // the wide answer, nothing learned.
        let Some(occ) = file.occ.get(&offset) else {
            return Attributed {
                repos: self.set_of(f),
                exact: false,
            };
        };
        // Inside a def nothing in the workspace runs: a BUILD-file macro's
        // rule call names a target, not a repository, and its labels are
        // that target's attributes.
        if let Some(d) = occ.scope {
            if !self.is_live(d) {
                return Attributed {
                    repos: BTreeSet::new(),
                    exact: true,
                };
            }
        }
        // The innermost enclosing call that names a repository outright.
        let mut call = occ.call;
        while let Some(c) = call {
            if let NameArg::Literal(l) = &self.calls[c].named {
                return Attributed {
                    repos: [l.clone()].into_iter().collect(),
                    exact: true,
                };
            }
            call = self.parent_call(c);
        }
        // Inside a `repository_rule(...)` / `module_extension(...)`
        // declaration: an attribute default the rule itself resolves.
        if let Some(sym) = occ.decl {
            let repos = self.rule_repos(sym);
            return if repos.is_empty() {
                Attributed {
                    repos: file.fallback.clone(),
                    exact: false,
                }
            } else {
                Attributed { repos, exact: true }
            };
        }
        match occ.scope {
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

    /// The call a call site is written inside, if any.
    fn parent_call(&self, c: usize) -> Option<usize> {
        let call = &self.calls[c];
        self.files[call.file]
            .occ
            .get(&call.offset)
            .and_then(|o| o.call)
    }

    /// Everything a change to file `f` can move, with the universe as the
    /// last resort.
    fn set_of(&self, f: usize) -> BTreeSet<String> {
        match self.fileset.get(f) {
            Some(r) if !r.is_empty() => r.clone(),
            _ if self.refining || !self.keeps_fallback(f) => BTreeSet::new(),
            _ => self.files[f].fallback.clone(),
        }
    }

    /// Whether a file's empty answer is a real one: nothing in it was
    /// unreadable and its fallback would have been the whole universe. See
    /// [`Attribution::keeps_fallback`].
    pub(crate) fn read_in_full(&self, path: &str) -> bool {
        self.by_path
            .get(path)
            .is_some_and(|&f| !self.keeps_fallback(f))
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
            if self.is_live(d) {
                repos.extend(self.raw_def_repos(d));
            }
        }
        for sym in &file.decls {
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
}
