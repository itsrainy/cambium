//! The facts: what the text of every closure file says, before any rule
//! runs. One walk over each file's [`Module`], with the context — enclosing
//! def, enclosing call, enclosing declaration — carried down the tree.

use std::collections::{BTreeMap, BTreeSet};

use crate::loads::{resolve_label, Load, RepoMapping};
use crate::starlark::{
    stmt_exprs, Arg, ArgKind, Clause, Expr, ExprKind, Module, ParamKind, Span, Stmt,
};
use crate::vocabulary::{names_a_repository, KEYWORDS, PURE_BUILTINS};

use super::{
    Attribution, CallId, CallSite, DefId, DefState, ExtId, ExtState, FileId, FileState, Interner,
    NameArg, Occ, RepoId, Repos, RuleId, RuleState, Sym, Use,
};

/// The context a walk over one file carries.
#[derive(Clone, Copy)]
struct Ctx {
    scope: Option<DefId>,
    call: Option<CallId>,
    decl: Option<Sym>,
}

/// What one walk over a file found, before it is folded into the analysis.
#[derive(Default)]
struct Found {
    calls: Vec<CallSite>,
    names: Vec<(Option<DefId>, RepoId)>,
    fwd: Vec<(DefId, String)>,
    uses: Vec<(usize, String, Option<CallId>)>,
    named_defs: Vec<(Option<DefId>, String)>,
    occ: BTreeMap<usize, Occ>,
    unreadable: bool,
    unparsed_in: BTreeSet<DefId>,
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
            calls: Vec::new(),
            uses: Vec::new(),
            named_defs: Vec::new(),
            named: Vec::new(),
            interner: Interner::default(),
            literal_names: BTreeSet::new(),
        };
        for (i, (path, text)) in texts.iter().enumerate() {
            a.by_path.insert(path.clone(), FileId::new(i));
            a.files.push(FileState {
                path: path.clone(),
                module: Module::parse(text),
                loads: BTreeMap::new(),
                syms: BTreeMap::new(),
                defs: Vec::new(),
                decls: Vec::new(),
                unreadable: false,
                occ: BTreeMap::new(),
                fallback: Repos::new(),
                wide: false,
                seeded: Repos::new(),
                module_dynamic: false,
                set: Repos::new(),
            });
        }
        for f in a.file_ids() {
            a.declarations(f, m);
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
                f.unreadable = false;
                f.occ.clear();
            }
            for d in &mut a.defs {
                d.unparsed = false;
            }
            for f in a.file_ids() {
                a.walk_file(f);
            }
        }
        a.liveness();
        a
    }

    /// Top-level `def`s, `load`s and rule / extension assignments.
    fn declarations(&mut self, f: FileId, m: &RepoMapping) {
        let file = self.file(f);
        let pkg = crate::loads::owning_dir(&file.path);
        let mut loads = BTreeMap::new();
        let mut syms = BTreeMap::new();
        let mut defs = Vec::new();
        let mut decls = Vec::new();
        let mut new_defs: Vec<DefState> = Vec::new();
        let mut new_rules: Vec<RuleState> = Vec::new();
        let mut new_exts: Vec<ExtState> = Vec::new();

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
                    let id = DefId::new(self.defs.len() + new_defs.len());
                    syms.insert(d.name.to_string(), Sym::Def(id));
                    defs.push(id);
                    new_defs.push(DefState {
                        file: f,
                        name: d.name.to_string(),
                        params,
                        fwd: BTreeSet::new(),
                        role: None,
                        unparsed: false,
                        live: true,
                        dynamic: false,
                        instances: Repos::new(),
                        defines: Repos::new(),
                        inherited: Repos::new(),
                    });
                }
                // `X = repository_rule(...)` / `E = module_extension(...)`
                Stmt::Assign(a) => {
                    let Some((name, callee, args)) = declaration(a) else {
                        continue;
                    };
                    let implementation = args
                        .iter()
                        .find(|x| x.keyword() == Some("implementation"))
                        .and_then(|x| x.value().as_name())
                        .map(str::to_string);
                    match callee {
                        "repository_rule" => {
                            let id = RuleId::new(self.rules.len() + new_rules.len());
                            syms.insert(name.to_string(), Sym::Rule(id));
                            decls.push(Sym::Rule(id));
                            new_rules.push(RuleState {
                                file: f,
                                name: name.to_string(),
                                implementation,
                                instances: Repos::new(),
                            });
                        }
                        _ => {
                            let id = ExtId::new(self.exts.len() + new_exts.len());
                            syms.insert(name.to_string(), Sym::Ext(id));
                            decls.push(Sym::Ext(id));
                            new_exts.push(ExtState {
                                file: f,
                                name: name.to_string(),
                                implementation,
                                repos: Repos::new(),
                            });
                        }
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
        let file = &mut self.files[f.idx()];
        file.loads = loads;
        file.syms = syms;
        file.defs = defs;
        file.decls = decls;
    }

    /// `implementation = _f` on a rule or extension declaration, which makes
    /// `_f`'s body part of that rule's or extension's definition.
    fn implementations(&mut self) {
        let mut roles: Vec<(DefId, Sym)> = Vec::new();
        for (r, rule) in self.rules.iter().enumerate() {
            if let Some(n) = &rule.implementation {
                if let Some(Sym::Def(d)) = self.resolve(rule.file, n) {
                    roles.push((d, Sym::Rule(RuleId::new(r))));
                }
            }
        }
        for (e, ext) in self.exts.iter().enumerate() {
            if let Some(n) = &ext.implementation {
                if let Some(Sym::Def(d)) = self.resolve(ext.file, n) {
                    roles.push((d, Sym::Ext(ExtId::new(e))));
                }
            }
        }
        for (d, role) in roles {
            self.defs[d.idx()].role = Some(role);
        }
    }

    /// One walk over a file: every call, literal, use and named def, with
    /// the context it sits in.
    fn walk_file(&mut self, f: FileId) {
        let mut found = Found::default();
        // The walk borrows `self` shared and the interner exclusively.
        let mut interner = std::mem::take(&mut self.interner);
        {
            let file = self.file(f);
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
                    Stmt::Assign(a) if declaration(a).is_some() => {
                        ctx.decl = decl_syms.next();
                    }
                    _ => {}
                }
                self.walk_stmt(f, st, ctx, &mut found, &mut interner);
            }
        }
        self.interner = interner;
        for (d, p) in found.fwd {
            self.defs[d.idx()].fwd.insert(p);
        }
        for d in found.unparsed_in {
            self.defs[d.idx()].unparsed = true;
        }
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
        let file = &mut self.files[f.idx()];
        file.unreadable |= found.unreadable;
        file.occ = found.occ;
        self.calls.extend(found.calls);
    }

    fn walk_stmt(
        &self,
        f: FileId,
        st: &Stmt<'a>,
        ctx: Ctx,
        found: &mut Found,
        interner: &mut Interner,
    ) {
        match st {
            Stmt::Unparsed(span) => self.unreadable(f, span, ctx, found),
            Stmt::Assign(a) => {
                // A bare name on the left of `=` is bound, not used; it
                // still names a def, for liveness, when it is one.
                if let Some(n) = a.target.as_name() {
                    found.named_defs.push((ctx.scope, n.to_string()));
                    self.walk_expr(f, &a.value, ctx, found, interner);
                } else {
                    self.walk_expr(f, &a.target, ctx, found, interner);
                    self.walk_expr(f, &a.value, ctx, found, interner);
                }
            }
            _ => {
                for e in stmt_exprs(st) {
                    self.walk_expr(f, e, ctx, found, interner);
                }
            }
        }
        match st {
            Stmt::Def(d) => {
                for s in &d.body {
                    self.walk_stmt(f, s, ctx, found, interner);
                }
            }
            Stmt::If(i) => {
                for (_, body) in &i.branches {
                    for s in body {
                        self.walk_stmt(f, s, ctx, found, interner);
                    }
                }
            }
            Stmt::For(l) => {
                for s in &l.body {
                    self.walk_stmt(f, s, ctx, found, interner);
                }
            }
            _ => {}
        }
    }

    /// Something the parser could not read: its literals are still
    /// occurrences, and the scope it sits in can no longer be read in full.
    fn unreadable(&self, f: FileId, span: &Span, ctx: Ctx, found: &mut Found) {
        let occ = Occ {
            scope: ctx.scope,
            call: ctx.call,
            decl: ctx.decl,
        };
        for (s, _) in self.file(f).module.strings_in(span) {
            found.occ.insert(s.start, occ);
        }
        match ctx.scope {
            Some(d) => {
                found.unparsed_in.insert(d);
            }
            None => found.unreadable = true,
        }
    }

    fn walk_expr(
        &self,
        f: FileId,
        e: &Expr<'a>,
        ctx: Ctx,
        found: &mut Found,
        interner: &mut Interner,
    ) {
        let file = self.file(f);
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
                let call = self.call_site(f, callee, args, ctx, found, interner);
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
                    ExprKind::Attr { object, .. } => {
                        self.walk_expr(f, object, ctx, found, interner)
                    }
                    _ => self.walk_expr(f, callee, ctx, found, interner),
                }
                let inner = Ctx {
                    call: call.or(ctx.call),
                    ..ctx
                };
                for a in args {
                    self.walk_expr(f, a.value(), inner, found, interner);
                }
            }
            ExprKind::Attr { object, .. } => self.walk_expr(f, object, ctx, found, interner),
            ExprKind::Index { object, index } => {
                self.walk_expr(f, object, ctx, found, interner);
                self.walk_expr(f, index, ctx, found, interner);
            }
            ExprKind::Slice { object, parts } => {
                self.walk_expr(f, object, ctx, found, interner);
                for p in parts.iter().flatten() {
                    self.walk_expr(f, p, ctx, found, interner);
                }
            }
            ExprKind::List(items) | ExprKind::Tuple(items) => {
                for x in items {
                    self.walk_expr(f, x, ctx, found, interner);
                }
            }
            ExprKind::Dict(items) => {
                for (k, v) in items {
                    self.walk_expr(f, k, ctx, found, interner);
                    self.walk_expr(f, v, ctx, found, interner);
                }
            }
            ExprKind::BinOp { lhs, rhs, .. } => {
                self.walk_expr(f, lhs, ctx, found, interner);
                self.walk_expr(f, rhs, ctx, found, interner);
            }
            ExprKind::UnOp { operand, .. } => self.walk_expr(f, operand, ctx, found, interner),
            ExprKind::Cond { then, test, orelse } => {
                self.walk_expr(f, then, ctx, found, interner);
                self.walk_expr(f, test, ctx, found, interner);
                self.walk_expr(f, orelse, ctx, found, interner);
            }
            ExprKind::Lambda { params, body } => {
                for p in params {
                    if let Some(d) = &p.default {
                        self.walk_expr(f, d, ctx, found, interner);
                    }
                }
                self.walk_expr(f, body, ctx, found, interner);
            }
            ExprKind::Comprehension { element, clauses } => {
                self.walk_expr(f, element, ctx, found, interner);
                for c in clauses {
                    match c {
                        Clause::For { target, iter } => {
                            self.walk_expr(f, target, ctx, found, interner);
                            self.walk_expr(f, iter, ctx, found, interner);
                        }
                        Clause::If(x) => self.walk_expr(f, x, ctx, found, interner),
                    }
                }
            }
            ExprKind::Int | ExprKind::Float => {}
        }
    }

    /// Records one call, and returns its id when it is a call site this
    /// analysis reads: `f(...)` or `x.f(...)`, `f` not a keyword.
    fn call_site(
        &self,
        f: FileId,
        callee: &Expr<'a>,
        args: &[Arg<'a>],
        ctx: Ctx,
        found: &mut Found,
        interner: &mut Interner,
    ) -> Option<CallId> {
        let file = self.file(f);
        let head = callee.head()?;
        let dotted = matches!(callee.kind, ExprKind::Attr { .. });
        // `return (a or b)`, `if (x)`, `not (y)`: a keyword before a
        // parenthesised expression is not a call.
        if !dotted && KEYWORDS.contains(&head) {
            return None;
        }
        let scope = ctx.scope;
        let resolved = if dotted { None } else { self.resolve(f, head) };
        let named = self.name_arg(scope, resolved, head, args, interner);
        // A method call is not an instantiation: `native.bind(name =
        // "ssl")` declares a `//external` alias, not a repository, and
        // following its `name` into the fixpoints would create a repository
        // that does not exist. What a literal beside it is attributed to is
        // `named`'s business.
        let name = if dotted {
            NameArg::Absent
        } else {
            named.clone()
        };
        if let NameArg::Literal(l) = &name {
            if names_a_repository(head) {
                found.names.push((scope, *l));
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
            found.unreadable = true;
            tracing::debug!(
                file = %file.path,
                call = head,
                "a module-level call this analysis cannot read; the file keeps the wide answer"
            );
        }
        let id = CallId::new(self.calls.len() + found.calls.len());
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
    fn forwarded_param(&self, scope: Option<DefId>, args: &[Arg<'a>]) -> Option<String> {
        let params = &self.def(scope?).params;
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
    ///
    /// - `name = "x"` is `Literal`; `name = p` for a parameter `p` of the
    ///   enclosing def is `Forward`; anything else under `name =` — string
    ///   arithmetic, a loop variable, `*args` — is `Dynamic`. Pinned by
    ///   `a_dynamic_name_falls_back_to_the_file_set`.
    /// - a callee known to forward a parameter under another spelling is
    ///   read under that keyword too.
    /// - a positional first argument is the name when the callee is a def
    ///   whose first parameter is `name` or one it forwards — envoy's
    ///   `external_http_archive("com_google_protobuf", patches = [...])`.
    ///   Pinned by `the_workspace_shape_attributes_a_requirements_lock_to_one_pip`.
    /// - a pure builtin forwards nothing: `Label(dep)` names no repository,
    ///   and reading its argument as one would turn apollo's `clean_dep(dep)`
    ///   into a wrapper whose every call site declares a label-shaped
    ///   "repository". Pinned by
    ///   `a_helper_that_forwards_into_a_builtin_is_not_a_wrapper`.
    fn name_arg(
        &self,
        scope: Option<DefId>,
        callee: Option<Sym>,
        head: &str,
        args: &[Arg<'a>],
        interner: &mut Interner,
    ) -> NameArg {
        let params = scope.map(|d| self.def(d).params.as_slice()).unwrap_or(&[]);
        let mut classify = |a: &Arg<'a>| -> NameArg {
            match &a.kind {
                ArgKind::Positional(e) | ArgKind::Keyword(_, e) => match &e.kind {
                    ExprKind::Str(s) => NameArg::Literal(interner.intern(s)),
                    ExprKind::Name(n) if params.iter().any(|p| p == n) => NameArg::Forward,
                    _ => NameArg::Dynamic,
                },
                ArgKind::Star(_) | ArgKind::StarStar(_) => NameArg::Dynamic,
            }
        };
        // The keyword the callee reads as a repository name: `name`, or a
        // parameter it is known to forward under another spelling.
        let mut keys: Vec<&str> = vec!["name"];
        if let Some(Sym::Def(d)) = callee {
            keys.extend(self.def(d).fwd.iter().map(String::as_str));
        }
        for k in &keys {
            if let Some(a) = args.iter().find(|a| a.keyword() == Some(k)) {
                return classify(a);
            }
        }
        let Some(first) = args.iter().find(|a| a.keyword().is_none()) else {
            return NameArg::Absent;
        };
        match callee {
            Some(Sym::Def(d)) => {
                let def = self.def(d);
                let p0 = def.params.first().map(String::as_str).unwrap_or("");
                if p0 == "name" || def.fwd.contains(p0) {
                    return classify(first);
                }
                NameArg::Absent
            }
            _ if PURE_BUILTINS.contains(&head) => NameArg::Absent,
            _ => match classify(first) {
                NameArg::Forward => NameArg::Forward,
                _ => NameArg::Absent,
            },
        }
    }

    /// Resolves a symbol written in file `f`, through `load()` aliases.
    pub(super) fn resolve(&self, f: FileId, name: &str) -> Option<Sym> {
        let mut file = f;
        let mut name = name.to_string();
        for _ in 0..16 {
            if let Some(s) = self.file(file).syms.get(&name) {
                return Some(*s);
            }
            let (path, orig) = self.file(file).loads.get(&name)?;
            file = *self.by_path.get(path)?;
            name = orig.clone();
        }
        None
    }

    /// Every file a symbol written in `f` was resolved through.
    fn origin_chain(&self, f: FileId, name: &str) -> Vec<FileId> {
        let mut out = Vec::new();
        let mut file = f;
        let mut name = name.to_string();
        for _ in 0..16 {
            if self.file(file).syms.contains_key(&name) {
                return out;
            }
            let Some((path, orig)) = self.file(file).loads.get(&name) else {
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

    /// `Live`: which defs can run while Bazel evaluates
    /// the workspace, and the universe of literal names those leave.
    ///
    /// Roots: a rule or extension implementation, a def called or named at
    /// module level of a closure file, and every def of a file with
    /// something unreadable at module level. Edges: a call or a naming from
    /// one def to another; and a call this analysis could not resolve may
    /// still be one of our defs under an alias the resolver missed, so every
    /// def of that name is live wherever the call is — losing an edge is the
    /// one direction this must not err in. A def named as a value —
    /// `_maybe(_go_repo, ...)`, `implementation = _f` on a declaration the
    /// scanner did not read — runs wherever the value is used.
    ///
    /// Pinned by `a_macro_only_build_files_call_feeds_no_repository` and
    /// `a_def_nothing_calls_reads_nothing_and_a_called_one_keeps_the_fallback`.
    fn liveness(&mut self) {
        let n = self.defs.len();
        let mut live = vec![false; n];
        let mut edges: Vec<Vec<DefId>> = vec![Vec::new(); n];
        let mut roots: Vec<DefId> = Vec::new();
        let mut by_name: BTreeMap<&str, Vec<DefId>> = BTreeMap::new();
        for d in self.def_ids() {
            let def = self.def(d);
            if def.role.is_some() || self.file(def.file).unreadable {
                roots.push(d);
            }
            by_name.entry(def.name.as_str()).or_default().push(d);
        }
        for c in &self.calls {
            match c.callee {
                Some(Sym::Def(callee)) => match c.scope {
                    Some(d) => edges[d.idx()].push(callee),
                    None => roots.push(callee),
                },
                None if c.opaque => {
                    for &d in by_name
                        .get(c.head.as_str())
                        .map(Vec::as_slice)
                        .unwrap_or(&[])
                    {
                        match c.scope {
                            Some(s) => edges[s.idx()].push(d),
                            None => roots.push(d),
                        }
                    }
                }
                _ => {}
            }
        }
        for (f, scope, name) in &self.named_defs {
            let Some(Sym::Def(c)) = self.resolve(*f, name) else {
                continue;
            };
            match scope {
                Some(d) if *d != c => edges[d.idx()].push(c),
                Some(_) => {}
                None => roots.push(c),
            }
        }
        let mut stack = roots;
        while let Some(d) = stack.pop() {
            if live[d.idx()] {
                continue;
            }
            live[d.idx()] = true;
            stack.extend(edges[d.idx()].iter().copied());
        }
        for (d, l) in live.iter().enumerate() {
            self.defs[d].live = *l;
        }
        self.literal_names = self
            .named
            .iter()
            .filter(|(scope, _)| scope.map_or(true, |d| self.def(d).live))
            .map(|(_, l)| self.interner.name(*l).to_string())
            .collect();
    }
}

/// `X = repository_rule(...)` or `E = module_extension(...)`: the bound
/// name, the callee, and the arguments.
fn declaration<'s, 'a>(
    a: &'s crate::starlark::Assign<'a>,
) -> Option<(&'a str, &'a str, &'s [Arg<'a>])> {
    let name = a.target.as_name()?;
    let ExprKind::Call { callee, args } = &a.value.kind else {
        return None;
    };
    match callee.as_name()? {
        c @ ("repository_rule" | "module_extension") => Some((name, c, args.as_slice())),
        _ => None,
    }
}
