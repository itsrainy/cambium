//! Which repository a *single occurrence* in a workspace-closure file feeds.
//!
//! [`crate::externals`] owns the question and the tables; this module owns the
//! static reading of Starlark that answers it per occurrence rather than per
//! file. See the "Attribution" section of that module's documentation for the
//! reasoning; what follows is the shape of the analysis.
//!
//! # Facts, then rules
//!
//! [`scan`] reads every file of the workspace closure (`WORKSPACE*`,
//! `MODULE.bazel`, and every `.bzl` they reach) through
//! [`crate::starlark::Module`] and records *facts* about it, each typed and
//! each keyed by an id: the defs ([`DefId`]), the repository rules
//! ([`RuleId`]) and module extensions ([`ExtId`]) declared, every call
//! ([`CallId`]) with the def it sits in and how it names the repository it
//! may create, every use of an imported symbol, and every string literal
//! with the def, call and declaration around it. Facts are what the text
//! says; nothing here guesses.
//!
//! [`solve`] then derives the answer as *rules* over those facts —
//! [`solve::Rule`] names them, each with the test that pins it — run to a
//! fixpoint by one driver. The state a rule writes lives on the entity it
//! is about: a def's `instances`, `defines` and `inherited` sets on its
//! [`DefState`], a file's `set` on its [`FileState`]. There is no parallel
//! table to keep in step and no mode flag: a rule reads what stands and
//! adds to it, and every set only ever grows.
//!
//! # The one fallback
//!
//! Every unknown resolves the same way: to [`Attribution::fallback`], the
//! file's own fallback set — the repositories of the seeded subtree it sits
//! under, or the whole universe — and an empty derived answer is read as
//! "nothing learned" exactly when [`Attribution::keeps_fallback`] says the
//! file is one whose emptiness cannot be trusted. Those two functions are
//! the only place the analysis decides what an unknown costs; the readers
//! of an occurrence, of a def, of a file and of a `.bzl`'s own row all go
//! through them.

mod scan;
mod solve;

use std::collections::{BTreeMap, BTreeSet};

use crate::starlark::Module;

/// An index into one of the id spaces. Each is its own type so that a def's
/// number cannot be handed to a table of rules.
macro_rules! id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub(crate) struct $name(u32);

        impl $name {
            fn new(i: usize) -> Self {
                $name(i as u32)
            }

            fn idx(self) -> usize {
                self.0 as usize
            }
        }
    };
}

id!(
    /// A top-level `def` of some closure file.
    DefId
);
id!(
    /// An `X = repository_rule(...)` binding.
    RuleId
);
id!(
    /// An `E = module_extension(...)` binding.
    ExtId
);
id!(
    /// One file of the closure.
    FileId
);
id!(
    /// One call, `f(...)` or `x.f(...)`, anywhere in a file.
    CallId
);

/// A set of repository names.
pub(crate) type Repos = BTreeSet<String>;

/// A callable the closure defines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Sym {
    Def(DefId),
    Rule(RuleId),
    Ext(ExtId),
}

/// How a call names the repository it may create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NameArg {
    /// No name argument at all: not an instantiation.
    Absent,
    /// `name = "literal"`.
    Literal(String),
    /// `name = <a parameter of the enclosing def>`: a forwarding wrapper.
    Forward,
    /// A name this analysis cannot evaluate.
    Dynamic,
}

/// A top-level `def`: what the text says about it, and what the rules
/// derive for it.
pub(crate) struct DefState {
    file: FileId,
    name: String,
    /// The ordinary parameters, in order; `*args` and `**kwargs` are not
    /// names a call can forward.
    params: Vec<String>,
    /// Parameters this def forwards as some call's `name`.
    fwd: BTreeSet<String>,
    /// The rule / extension whose implementation this def is.
    role: Option<Sym>,
    /// Whether the body holds a statement the parser could not read.
    unparsed: bool,
    // ---- derived
    /// Whether the def can run while Bazel evaluates the workspace; see
    /// [`Attribution::liveness`].
    live: bool,
    /// Whether the def's answer is its file's fallback; see
    /// [`Attribution::dynamics`].
    dynamic: bool,
    /// Repository names the def can be asked to create, as a wrapper.
    instances: Repos,
    /// Repository names the def creates when called with no name of its
    /// own.
    defines: Repos,
    /// For a helper that instantiates nothing, what its callers do.
    inherited: Repos,
}

pub(crate) struct RuleState {
    file: FileId,
    name: String,
    /// The `implementation =` argument, when it is a bare name.
    implementation: Option<String>,
    /// Repository names the rule is instantiated under.
    instances: Repos,
}

pub(crate) struct ExtState {
    file: FileId,
    name: String,
    implementation: Option<String>,
    /// The `use_repo` names of the extension, and whatever else the rules
    /// find it creating.
    repos: Repos,
}

pub(crate) struct CallSite {
    file: FileId,
    /// Byte offset of the call in its file.
    offset: usize,
    /// The def the call is written in, if any.
    scope: Option<DefId>,
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
pub(crate) struct Use {
    file: FileId,
    /// The byte offset of the use, an occurrence like any other.
    offset: usize,
    /// The name as written.
    name: String,
    /// The call this use is the callee of, when it is one.
    call: Option<CallId>,
    chain: Vec<FileId>,
}

/// Where an occurrence sits: the def, the innermost enclosing call, and the
/// rule or extension declaration, if any.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Occ {
    scope: Option<DefId>,
    call: Option<CallId>,
    decl: Option<Sym>,
}

/// One file of the workspace closure.
pub(crate) struct FileState<'a> {
    path: String,
    module: Module<'a>,
    /// Alias -> (defining file path, original name).
    loads: BTreeMap<String, (String, String)>,
    /// Top-level names this file binds to a callable.
    syms: BTreeMap<String, Sym>,
    defs: Vec<DefId>,
    /// The rules and extensions declared here, for the file's own answer.
    decls: Vec<Sym>,
    /// Whether the scan met something at module level it could not read: a
    /// call to a symbol the closure cannot resolve, a statement the parser
    /// could not parse. Known before the rules run, which is what lets
    /// [`Attribution::liveness`] take every def of such a file as live.
    unreadable: bool,
    /// Every string literal and every call, by byte offset, with its context.
    occ: BTreeMap<usize, Occ>,
    // ---- seeded
    /// What an unknown in this file resolves to; see
    /// [`Attribution::fallback`].
    fallback: Repos,
    /// Whether that fallback is the whole universe rather than the
    /// repositories of a seeded subtree the file sits under.
    wide: bool,
    /// Repositories `MODULE.bazel` says this file declares: the `use_repo`
    /// names of an extension it defines, the instantiations of a repository
    /// rule `use_repo_rule` names in it. Part of the file's own answer even
    /// when the declaration itself could not be read out of the text.
    seeded: Repos,
    // ---- derived
    /// [`FileState::unreadable`], or a module-level call whose name the
    /// rules could not evaluate: the module scope's answer is the fallback.
    module_dynamic: bool,
    /// Everything a change to the file can move; see [`solve::Rule::Own`]
    /// and [`solve::Rule::Uses`].
    set: Repos,
}

/// The whole workspace closure, analysed.
pub(crate) struct Attribution<'a> {
    files: Vec<FileState<'a>>,
    by_path: BTreeMap<String, FileId>,
    defs: Vec<DefState>,
    rules: Vec<RuleState>,
    exts: Vec<ExtState>,
    calls: Vec<CallSite>,
    uses: Vec<Use>,
    /// Every def named as a value rather than called: `(file, scope, name)`.
    named_defs: Vec<(FileId, Option<DefId>, String)>,
    /// Every literal `name =` with the def it was written in, before
    /// liveness decides which of them the universe keeps.
    named: Vec<(Option<DefId>, String)>,
    /// Literal `name =` values seen anywhere: the universe a fallback ranges
    /// over.
    pub(crate) literal_names: BTreeSet<String>,
}

/// What an occurrence is attributed to.
#[derive(Clone, Debug)]
pub(crate) struct Attributed {
    pub(crate) repos: Repos,
    /// False when the answer *is* the file's fallback set — nothing was
    /// learned about this occurrence.
    pub(crate) exact: bool,
}

impl Attributed {
    fn exact(repos: Repos) -> Self {
        Attributed { repos, exact: true }
    }

    fn inexact(repos: Repos) -> Self {
        Attributed {
            repos,
            exact: false,
        }
    }
}

impl<'a> Attribution<'a> {
    fn file(&self, f: FileId) -> &FileState<'a> {
        &self.files[f.idx()]
    }

    fn def(&self, d: DefId) -> &DefState {
        &self.defs[d.idx()]
    }

    fn rule(&self, r: RuleId) -> &RuleState {
        &self.rules[r.idx()]
    }

    fn ext(&self, e: ExtId) -> &ExtState {
        &self.exts[e.idx()]
    }

    fn call(&self, c: CallId) -> &CallSite {
        &self.calls[c.idx()]
    }

    fn def_ids(&self) -> impl Iterator<Item = DefId> {
        (0..self.defs.len()).map(DefId::new)
    }

    fn file_ids(&self) -> impl Iterator<Item = FileId> {
        (0..self.files.len()).map(FileId::new)
    }

    /// The files read, as `(path, module)`, for the callers that walk the
    /// literals themselves.
    pub(crate) fn modules(&self) -> impl Iterator<Item = (&str, &Module<'a>)> {
        self.files.iter().map(|f| (f.path.as_str(), &f.module))
    }

    /// What an unknown in file `f` resolves to: the repositories of the
    /// seeded subtree the file sits under, or the whole universe. The one
    /// answer every unreadable shape degrades to.
    fn fallback(&self, f: FileId) -> Repos {
        self.file(f).fallback.clone()
    }

    /// Whether an empty derived answer for file `f` means "nothing learned"
    /// rather than "nothing there", so that the file keeps its fallback.
    ///
    /// It does when something in it could not be read — a module-level call
    /// the closure cannot resolve, or a def the workspace runs whose body
    /// names a repository this analysis cannot evaluate — or when the
    /// fallback is a seeded subtree's few repositories rather than the
    /// universe (envoy's `repo.bzl`, whose module-level constants are the
    /// extension's inputs). A file read in full, found to define nothing,
    /// and whose only fallback is everything defines nothing: a `.bzl` of
    /// BUILD-file macros that a load chain dragged into the workspace
    /// closure is not the universe, and handing it the universe handed it
    /// to every file that imported a symbol from it (issue #9). Pinned by
    /// `a_readable_file_that_defines_nothing_is_not_the_universe`.
    fn keeps_fallback(&self, f: FileId) -> bool {
        let file = self.file(f);
        !file.wide
            || file.module_dynamic
            || file
                .defs
                .iter()
                .any(|&d| self.def(d).live && self.def(d).dynamic)
    }

    /// The fallback a `.bzl`'s own row takes when the rules derived nothing
    /// for it: [`Attribution::fallback`] when the file
    /// [`keeps_fallback`](Attribution::keeps_fallback), and nothing
    /// otherwise. `None` for a file this analysis never read.
    pub(crate) fn fallback_for(&self, path: &str) -> Option<Repos> {
        let f = *self.by_path.get(path)?;
        Some(if self.keeps_fallback(f) {
            self.fallback(f)
        } else {
            Repos::new()
        })
    }

    /// Everything a change to a file can move: what it defines itself, plus
    /// what every use of the symbols it exports does.
    pub(crate) fn file_repos(&self, path: &str) -> Attributed {
        let Some(&f) = self.by_path.get(path) else {
            return Attributed::inexact(Repos::new());
        };
        Attributed {
            repos: self.file(f).set.clone(),
            exact: !self.file(f).module_dynamic,
        }
    }

    /// The repositories an occurrence at byte `offset` of file `path` is
    /// attributed to.
    pub(crate) fn at(&self, path: &str, offset: usize) -> Attributed {
        match self.by_path.get(path) {
            Some(&f) => self.at_offset(f, offset),
            None => Attributed::inexact(Repos::new()),
        }
    }
}
