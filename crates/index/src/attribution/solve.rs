//! The rules: what the facts of [`super::scan`] derive, and in what order.
//!
//! Every rule is a variant of [`Rule`], applied by one driver
//! ([`Attribution::fixpoint`]) until nothing changes. The driver runs in
//! stages, and the staging is load-bearing: `Defines` must reach its
//! fixpoint before `Helper` starts, or a helper's inherited repositories
//! would flow back into what its caller *defines*; and `Live` is decided
//! from the scan alone, before any rule widens a module scope. The stages
//! are what the old three loops were; the rules inside them are what the
//! old loop bodies did, each with a name and the test that pins it.

use super::{Answer, Attribution, CallId, DefId, FileId, NameArg, Repos, Sym};
use std::collections::{BTreeMap, BTreeSet};

/// The rules of the derivation. Each reads the state as it stands and adds
/// to one entity's set; a rule returns whether it added anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rule {
    /// `instances(callee) ⊇ name(call)` for every call whose scope is live
    /// or module-level: a literal `name =` adds itself; a forwarded name
    /// adds the enclosing wrapper's own instances; a name this analysis
    /// cannot evaluate adds the file's fallback, and so does a wrapper
    /// called without any name it could forward (the repository it creates
    /// is unknowable from here).
    ///
    /// Pinned by `the_workspace_shape_attributes_a_requirements_lock_to_one_pip`
    /// (a forwarding wrapper, positional and keyword) and
    /// `a_dynamic_name_falls_back_to_the_file_set`.
    Instances,
    /// `defines(def) ⊇` what each call in `def` creates: a literal name,
    /// plus the side effects of the rule it instantiates; or, for an
    /// unnamed call, everything the callee creates when called.
    ///
    /// Pinned by `a_loaded_file_answers_for_what_its_own_symbol_does`
    /// (`init()` defines `local_config_pcl` through `pcl_configure`) and
    /// `a_rules_implementation_declares_its_side_effects`.
    Defines,
    /// A def that is not an implementation, not dynamic, defines nothing
    /// and forwards nothing — apollo's `_find_cuda_config(repository_ctx)`,
    /// which only reads a file — inherits what its call sites are attributed
    /// to, and so belongs to the repositories of the rule whose
    /// implementation calls it.
    ///
    /// Pinned by `a_helper_that_declares_nothing_is_answered_by_its_callers`
    /// and `an_implementation_of_an_uninstantiated_rule_is_still_a_helper`.
    Helper,
    /// `set(file) ⊇` what the file defines by itself: what `MODULE.bazel`
    /// seeded it with, what its live defs and its declarations create, what
    /// its module-level calls name, and its fallback when its module level
    /// could not be read.
    ///
    /// Pinned by `a_loaded_file_answers_for_what_its_own_symbol_does` (the
    /// root file declares everything; each init file its own).
    Own,
    /// `set(x) ⊇` what each use of a symbol exported by `x` does, for every
    /// file `x` the symbol was resolved through. Calling an imported
    /// function does exactly what that function does, plus whatever the
    /// call names outright; any other use — a constant read into an
    /// expression — is attributed like any other occurrence, and falls back
    /// to the using file's set when nothing was learned.
    ///
    /// Pinned by `a_constant_is_answered_by_where_it_is_read`.
    Uses,
}

impl<'a> Attribution<'a> {
    /// Runs the rules. `fallbacks` is the per-file answer for anything
    /// unattributable, and `wide` names the files whose fallback is the whole
    /// universe rather than a seeded subtree's; `seeds` come from
    /// `MODULE.bazel`.
    pub(crate) fn solve(
        &mut self,
        fallbacks: &BTreeMap<String, BTreeSet<String>>,
        seeds: &BTreeMap<(String, String), BTreeSet<String>>,
        wide: &BTreeSet<String>,
    ) {
        self.seed(fallbacks, seeds, wide);
        self.extension_reach();
        self.dynamics();
        self.fixpoint(&[Rule::Instances]);
        self.fixpoint(&[Rule::Defines]);
        // A file the rules learn nothing about keeps its fallback, and its
        // dependents inherit that on the next pass — but only once the
        // pass has converged, or the fallback would flow before the finer
        // answers had a chance to.
        loop {
            self.fixpoint(&[Rule::Helper, Rule::Own, Rule::Uses]);
            if !self.promote() {
                break;
            }
        }
    }

    /// The `Seeded` facts: what `MODULE.bazel` and the load graph say before
    /// any text is read. `use_repo_rule("//bazel:repo.bzl", "http_archive")`
    /// and `use_extension("//bazel:extensions.bzl", "envoy_repo_extension")`
    /// name the instances of a rule and the repositories of an extension.
    fn seed(
        &mut self,
        fallbacks: &BTreeMap<String, BTreeSet<String>>,
        seeds: &BTreeMap<(String, String), BTreeSet<String>>,
        wide: &BTreeSet<String>,
    ) {
        for i in 0..self.files.len() {
            let path = self.files[i].path.clone();
            let fallback = fallbacks
                .get(&path)
                .map(|s| self.interner.set(s.iter().map(String::as_str)))
                .unwrap_or_default();
            self.files[i].fallback = fallback;
            self.files[i].wide = wide.contains(&path);
            for ((p, _), repos) in seeds {
                if *p == path {
                    let s = self.interner.set(repos.iter().map(String::as_str));
                    self.files[i].seeded.union_with(&s);
                }
            }
        }
        for r in 0..self.rules.len() {
            let key = (
                self.file(self.rules[r].file).path.clone(),
                self.rules[r].name.clone(),
            );
            if let Some(s) = seeds.get(&key) {
                let s = self.interner.set(s.iter().map(String::as_str));
                self.rules[r].instances.union_with(&s);
            }
        }
        for e in 0..self.exts.len() {
            let key = (
                self.file(self.exts[e].file).path.clone(),
                self.exts[e].name.clone(),
            );
            if let Some(s) = seeds.get(&key) {
                let s = self.interner.set(s.iter().map(String::as_str));
                self.exts[e].repos.union_with(&s);
            }
        }
    }

    /// A repository rule an extension's implementation reaches, however many
    /// macros deep, can be instantiated for any of that extension's
    /// repositories.
    ///
    /// Pinned by `the_bzlmod_shape_attributes_an_attr_default_to_its_own_rule`.
    fn extension_reach(&mut self) {
        for e in 0..self.exts.len() {
            let role = Sym::Ext(super::ExtId::new(e));
            let Some(d) = self.def_ids().find(|d| self.def(*d).role == Some(role)) else {
                continue;
            };
            let repos = self.exts[e].repos.clone();
            for sym in self.reachable(d) {
                if let Sym::Rule(r) = sym {
                    self.rules[r.idx()].instances.union_with(&repos);
                }
            }
        }
    }

    /// `Dynamic`: a scope is dynamic when a call in it names a repository
    /// this analysis cannot evaluate, calls something it cannot resolve, or
    /// holds a statement the parser could not read — and the scope is one
    /// the workspace runs. A dynamic name in a def the workspace never runs
    /// (a BUILD macro's `name = name + "_lib"`) makes nothing unreadable. At
    /// module level the same conditions make the file's module scope
    /// dynamic.
    ///
    /// Pinned by `a_dynamic_name_falls_back_to_the_file_set` and
    /// `an_unattributable_workspace_bzl_falls_back_to_every_repository`.
    fn dynamics(&mut self) {
        let mut dyn_defs: Vec<DefId> = Vec::new();
        let mut dyn_files: Vec<FileId> = Vec::new();
        for c in &self.calls {
            if !(matches!(c.name, NameArg::Dynamic) || c.opaque) {
                continue;
            }
            match c.scope {
                Some(d) if self.def(d).live => {
                    if !self.def(d).dynamic && !dyn_defs.contains(&d) {
                        tracing::debug!(
                            file = %self.file(c.file).path,
                            def = %self.def(d).name,
                            call = %c.head,
                            why = if c.opaque { "unresolved callee" } else { "dynamic name" },
                            "a call this analysis cannot read; the def keeps the wide answer"
                        );
                    }
                    dyn_defs.push(d);
                }
                Some(_) => {}
                None => dyn_files.push(c.file),
            }
        }
        for d in dyn_defs {
            self.defs[d.idx()].dynamic = true;
        }
        for f in dyn_files {
            self.files[f.idx()].module_dynamic = true;
        }
        for f in self.file_ids() {
            if self.file(f).unreadable {
                self.files[f.idx()].module_dynamic = true;
            }
        }
        for d in self.def_ids() {
            if self.def(d).unparsed && self.def(d).live {
                self.defs[d.idx()].dynamic = true;
            }
        }
    }

    /// The driver: applies `rules` in order, round after round, until a
    /// whole round adds nothing. Every set only grows, so it terminates;
    /// the bound is a guard against a pathological closure, not the
    /// expected exit.
    fn fixpoint(&mut self, rules: &[Rule]) {
        let bound = self.defs.len() + self.rules.len() + self.files.len() + 2;
        for _ in 0..bound {
            let mut changed = false;
            for r in rules {
                changed |= self.apply(*r);
            }
            if !changed {
                break;
            }
        }
    }

    fn apply(&mut self, rule: Rule) -> bool {
        match rule {
            Rule::Instances => self.instances(),
            Rule::Defines => self.defines(),
            Rule::Helper => self.helper(),
            Rule::Own => self.own(),
            Rule::Uses => self.uses(),
        }
    }

    fn instances(&mut self) -> bool {
        let mut changed = false;
        for c in 0..self.calls.len() {
            let call = &self.calls[c];
            let Some(target) = call.callee else { continue };
            if call.scope.is_some_and(|d| !self.def(d).live) {
                continue;
            }
            let add: Repos = match &call.name {
                NameArg::Literal(l) => Repos::single(*l),
                NameArg::Forward => match call.scope {
                    Some(d) => self.def(d).instances.clone(),
                    None => Repos::new(),
                },
                NameArg::Dynamic => self.fallback(call.file),
                NameArg::Absent => match target {
                    Sym::Def(d) if !self.def(d).fwd.is_empty() => self.fallback(call.file),
                    _ => continue,
                },
            };
            let slot = match target {
                Sym::Def(d) => &mut self.defs[d.idx()].instances,
                Sym::Rule(r) => &mut self.rules[r.idx()].instances,
                Sym::Ext(e) => &mut self.exts[e.idx()].repos,
            };
            changed |= slot.union_with(&add);
        }
        changed
    }

    fn defines(&mut self) -> bool {
        let mut changed = false;
        for c in 0..self.calls.len() {
            let call = &self.calls[c];
            let Some(d) = call.scope else { continue };
            let add: Repos = match (&call.name, call.callee) {
                (NameArg::Literal(l), t) => {
                    // Instantiating a repository rule creates the instance
                    // *and* whatever its implementation declares by name.
                    let mut out = Repos::single(*l);
                    if let Some(t @ (Sym::Rule(_) | Sym::Ext(_))) = t {
                        out.union_with(&self.side_effects(t));
                    }
                    out
                }
                (NameArg::Absent, Some(t)) => self.value_of(t),
                _ => continue,
            };
            changed |= self.defs[d.idx()].defines.union_with(&add);
        }
        changed
    }

    fn helper(&mut self) -> bool {
        let mut inherited: Vec<(DefId, Repos)> = Vec::new();
        for d in self.def_ids() {
            let def = self.def(d);
            if def.role.is_some() || def.dynamic {
                continue;
            }
            if !def.defines.is_empty() || !def.fwd.is_empty() {
                continue;
            }
            let mut repos = Repos::new();
            for c in &self.calls {
                if c.callee != Some(Sym::Def(d)) {
                    continue;
                }
                let a = self.at_offset(c.file, c.offset);
                repos.union_with(&if a.exact {
                    a.repos
                } else {
                    self.file(c.file).set.clone()
                });
            }
            inherited.push((d, repos));
        }
        let mut changed = false;
        for (d, repos) in inherited {
            changed |= self.defs[d.idx()].inherited.union_with(&repos);
        }
        changed
    }

    fn own(&mut self) -> bool {
        let owns: Vec<Repos> = self.file_ids().map(|f| self.own_repos(f)).collect();
        let mut changed = false;
        for (f, o) in owns.into_iter().enumerate() {
            changed |= self.files[f].set.union_with(&o);
        }
        changed
    }

    fn uses(&mut self) -> bool {
        let mut folded: Vec<(FileId, Repos)> = Vec::new();
        for u in &self.uses {
            let v = self.use_value(u);
            if v.is_empty() {
                continue;
            }
            for &x in &u.chain {
                folded.push((x, v.clone()));
            }
        }
        let mut changed = false;
        for (x, v) in folded {
            changed |= self.files[x.idx()].set.union_with(&v);
        }
        changed
    }

    /// A file the rounds learned nothing about keeps its fallback, when it
    /// is a file whose emptiness cannot be trusted. Whether any did.
    fn promote(&mut self) -> bool {
        let mut promoted = false;
        for f in self.file_ids() {
            let file = self.file(f);
            if file.set.is_empty() && !file.fallback.is_empty() && self.keeps_fallback(f) {
                self.files[f.idx()].set = self.fallback(f);
                promoted = true;
            }
        }
        promoted
    }

    // ---------------------------------------------------------- queries

    /// What one use of an imported symbol can move; see [`Rule::Uses`].
    fn use_value(&self, u: &super::Use) -> Repos {
        if let Some(c) = u.call {
            let mut out = Repos::new();
            match self.resolve(u.file, &u.name) {
                Some(sym) => {
                    out.union_with(&self.value_of(sym));
                }
                // A symbol the closure re-exports from elsewhere: nothing is
                // known about what calling it does.
                None => return self.file(u.file).set.clone(),
            }
            if let NameArg::Literal(l) = &self.call(c).name {
                out.insert(*l);
            }
            return out;
        }
        let a = self.at_offset(u.file, u.offset);
        if a.exact {
            a.repos
        } else {
            self.file(u.file).set.clone()
        }
    }

    /// Every callable reachable from a def through calls, transitively.
    fn reachable(&self, from: DefId) -> BTreeSet<Sym> {
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
    /// real answer here — a helper that instantiates nothing creates nothing
    /// — which is why this is not [`Attribution::def_repos`]. A def nothing
    /// in the workspace runs creates nothing, however its body reads; the
    /// use of it that asked was in dead code too.
    fn value_of(&self, s: Sym) -> Repos {
        match s {
            Sym::Def(d) if !self.def(d).live => Repos::new(),
            Sym::Def(d) => self.raw_def_repos(d),
            Sym::Rule(_) | Sym::Ext(_) => self.rule_repos(s),
        }
    }

    /// Everything instantiating a rule or running an extension creates: the
    /// instances themselves, and whatever its implementation declares by
    /// name.
    fn rule_repos(&self, s: Sym) -> Repos {
        let mut out = match s {
            Sym::Rule(r) => self.rule(r).instances.clone(),
            Sym::Ext(e) => self.ext(e).repos.clone(),
            Sym::Def(_) => Repos::new(),
        };
        out.union_with(&self.side_effects(s));
        out
    }

    /// `SideEffects`: what a rule's or extension's implementation declares
    /// by name, over and above the instance itself — the `cuda-include` and
    /// `cuda-lib` repositories apollo's `cuda_configure` writes alongside
    /// `local_config_cuda`. An implementation that could not be read in full
    /// declares the fallback.
    ///
    /// Pinned by `a_rules_implementation_declares_its_side_effects`.
    fn side_effects(&self, s: Sym) -> Repos {
        let mut out = Repos::new();
        for d in self.def_ids() {
            let def = self.def(d);
            if def.role == Some(s) {
                if def.dynamic {
                    out.union_with(&self.fallback(def.file));
                } else {
                    out.union_with(&def.defines);
                }
            }
        }
        out
    }

    /// The repositories a def can instantiate or define, with no fallback.
    fn raw_def_repos(&self, d: DefId) -> Repos {
        let def = self.def(d);
        if let Some(role) = def.role {
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
        if def.dynamic {
            return self.fallback(def.file);
        }
        let mut repos = def.defines.clone();
        if !def.fwd.is_empty() {
            repos.union_with(&def.instances);
        }
        if repos.is_empty() {
            repos.union_with(&def.inherited);
        }
        repos
    }

    /// The repositories a def's *body* is attributed to.
    ///
    /// An empty answer means nothing was learned — a repository rule whose
    /// instantiation this analysis never saw, a macro nothing calls — and the
    /// occurrence keeps the file's fallback rather than being attributed to
    /// no repository at all, which would lose an edge.
    fn def_repos(&self, d: DefId) -> Answer {
        let def = self.def(d);
        if !def.live {
            return Answer::exact(Repos::new());
        }
        let repos = self.raw_def_repos(d);
        if repos.is_empty() {
            return Answer::inexact(self.fallback(def.file));
        }
        Answer {
            exact: !def.dynamic,
            repos,
        }
    }

    /// What a file defines by itself; see [`Rule::Own`].
    fn own_repos(&self, f: FileId) -> Repos {
        let file = self.file(f);
        let mut repos = file.seeded.clone();
        for &d in &file.defs {
            if self.def(d).live {
                repos.union_with(&self.raw_def_repos(d));
            }
        }
        for sym in &file.decls {
            repos.union_with(&self.rule_repos(*sym));
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
                    repos.insert(*l);
                }
                NameArg::Absent => {
                    if let Some(t) = call.callee {
                        repos.union_with(&self.value_of(t));
                    }
                }
                _ => {}
            }
        }
        if file.module_dynamic {
            repos.union_with(&self.fallback(f));
        }
        repos
    }

    /// The occurrence ladder, innermost first:
    ///
    /// 1. inside a def nothing in the workspace runs, nothing: a BUILD-file
    ///    macro's rule call names a target, not a repository. Pinned by
    ///    `a_macro_only_build_files_call_feeds_no_repository`.
    /// 2. the innermost enclosing call with a literal `name =`, method call
    ///    or not. Pinned by
    ///    `the_workspace_shape_attributes_a_requirements_lock_to_one_pip`
    ///    and `a_helper_that_forwards_into_a_builtin_is_not_a_wrapper`.
    /// 3. inside a `repository_rule(...)` / `module_extension(...)`
    ///    declaration — an `attrs =` default the rule itself resolves — the
    ///    rule's repositories. Pinned by
    ///    `the_bzlmod_shape_attributes_an_attr_default_to_its_own_rule`.
    /// 4. the enclosing def's repositories. Pinned by
    ///    `a_helper_that_declares_nothing_is_answered_by_its_callers`.
    /// 5. module level: the file's set, inexact — nothing about the
    ///    occurrence itself was read, and a mapping must not be derived from
    ///    a set this wide. Pinned by `a_module_level_label_keeps_the_old_answer`.
    ///
    /// An offset the scan never saw is answered like the module level.
    pub(super) fn at_offset(&self, f: FileId, offset: usize) -> Answer {
        let file = self.file(f);
        let Some(occ) = file.occ.get(&offset) else {
            return Answer::inexact(file.set.clone());
        };
        if let Some(d) = occ.scope {
            if !self.def(d).live {
                return Answer::exact(Repos::new());
            }
        }
        let mut call = occ.call;
        while let Some(c) = call {
            if let NameArg::Literal(l) = &self.call(c).named {
                return Answer::exact(Repos::single(*l));
            }
            call = self.parent_call(c);
        }
        if let Some(sym) = occ.decl {
            let repos = self.rule_repos(sym);
            return if repos.is_empty() {
                Answer::inexact(self.fallback(f))
            } else {
                Answer::exact(repos)
            };
        }
        match occ.scope {
            Some(d) => self.def_repos(d),
            None => Answer::inexact(file.set.clone()),
        }
    }

    /// The call a call site is written inside, if any.
    fn parent_call(&self, c: CallId) -> Option<CallId> {
        let call = self.call(c);
        self.file(call.file)
            .occ
            .get(&call.offset)
            .and_then(|o| o.call)
    }
}
