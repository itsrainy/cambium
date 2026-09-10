//! Static `load()` scanning, and the `.bzl` load closure it produces.
//!
//! # Why this exists
//!
//! A changed `.bzl` file dirties every package whose BUILD file reaches it
//! through `load()`. Bazel can answer that with `rbuildfiles`, but only in Sky
//! Query mode (`--universe_scope=//...`), which loads the whole universe —
//! external repositories included — and therefore finishes with errors (exit
//! 3) on any machine that cannot fetch all of them. A partial `rbuildfiles`
//! answer is unusable: the package it dropped may be exactly the one that
//! loads the changed file, so the only sound response was to invalidate
//! everything. On the real subjects that fired on 5 of 150 commits for `bazel/bazel`.
//!
//! `load()` is a static, purely syntactic construct: it is only legal at the
//! top level of a file, its first argument must be a string literal, and it
//! cannot be computed. So the closure can be read straight off the text, with
//! no Bazel process and no failure mode. That is what this module does.
//!
//! # Soundness
//!
//! The claim is: if a package's rules can change because some `.bzl` file
//! changed, then that `.bzl` is in the package's stored closure.
//!
//! 1. A package's rule definitions come from its BUILD file plus the symbols
//!    it `load()`s, transitively. Starlark has no other mechanism for a BUILD
//!    file to reach code in another file — no import by computed path, no
//!    implicit prelude the user can edit (the internal prelude ships with
//!    Bazel, so it moves only when Bazel does, which is a version change and
//!    already an invalidator).
//! 2. `load` is only legal as a top-level statement, and its label argument
//!    must be a string literal — Bazel's parser rejects both otherwise. So a
//!    scan of top-level statements with a real string/comment lexer sees every
//!    load a file performs.
//! 3. The walk stops at the first label into an external repository. That is
//!    not a gap: cambium's index only covers the main repository, an external
//!    repository's contents cannot change without `MODULE.bazel`/`WORKSPACE`
//!    moving (an invalidator) or a lockfile moving (an invalidator), and the
//!    external labels are recorded anyway so a future MODULE-diffing pass can
//!    map them.
//! 4. A `.bzl` reachable only from `MODULE.bazel`/`WORKSPACE` (through
//!    `load`, `use_extension` or `use_repo_rule`) defines repository rules.
//!    Changing it can change the contents of an external repository, which is
//!    outside the index entirely, so the engine escalates that to a global
//!    invalidation — the same answer the `rbuildfiles` path gave when it saw a
//!    workspace-level loader.
//! 5. A `.bzl` in neither closure is loaded by nothing and can affect nothing
//!    — *provided the closure that was stored is complete*. That is a claim
//!    about the index-time scan, not about Starlark, and it can fail: a load
//!    label the walk could not resolve, an `@<own module name>//...` spelling
//!    read as external because the mapping had no main alias, a BUILD file
//!    the index-time checkout did not have, a package tombstoned as partial.
//!    So the engine does not take the empty answer on trust: when a *changed*
//!    `.bzl` has no node here, it re-derives one hop of the inverse load graph
//!    from the head checkout and follows whatever loads it (issue #7,
//!    `crate::engine`'s witness in cambium-engine). This module's job is
//!    unchanged; the witness is what makes point 5 checkable rather than
//!    assumed.
//!
//! The one thing a static scan cannot see that `rbuildfiles` could is a BUILD
//! file that is *generated* rather than checked in. Such a file is not in the
//! git diff either, so it was never on this path.
//!
//! # Repository mapping
//!
//! Under bzlmod an apparent repository name (`@rules_shell`) is local to the
//! file that writes it; the canonical name is what identifies the repository
//! globally. `bazel mod dump_repo_mapping ""` prints the root module's mapping
//! as one JSON object on stdout, apparent name to canonical name. Measured on
//! a two-package bzlmod workspace depending on `rules_shell`:
//!
//! ```text
//! 9.2.0  {"":"","demo":"","rules_shell":"rules_shell+","bazel_tools":"bazel_tools"}
//! 8.5.1  {"":"","demo":"","rules_shell":"rules_shell+","bazel_tools":"bazel_tools",
//!         "local_config_platform":"local_config_platform"}
//! 7.4.1  {"":"", "__main__":"", "demo":"", "rules_shell":"rules_shell~", ...}
//!        — plus every WORKSPACE-suffix repo (~90 entries), and `~` rather
//!        than `+` as the canonical-name separator.
//! ```
//!
//! Two things are stable across all three: the main repository maps to the
//! **empty** canonical name (as do `""`, the module's own name, and 7.4.1's
//! `__main__`), and the output is a single JSON object on stdout. Everything
//! else — the separator character, how many repos appear — varies, which is
//! exactly why the canonical name is captured rather than derived. When the
//! command is unavailable (WORKSPACE mode, Bazel < 7.1) the mapping falls back
//! to the identity and a warning; identity is safe here because a canonical
//! name is only ever used to *name* an external load, never to walk into one.

use rustc_hash::FxHashMap;

use crate::data::IndexRef;

/// Files whose loads make up the workspace closure, most-preferred first.
/// All that exist are scanned: a repository can carry both.
pub const WORKSPACE_FILES: &[&str] = &[
    "MODULE.bazel",
    "WORKSPACE",
    "WORKSPACE.bazel",
    "WORKSPACE.bzlmod",
];

/// Reads Starlark source out of some checkout.
///
/// A trait rather than a path so the synthetic graph in the property tests can
/// serve the same text it would have emitted, and so a run at a revision the
/// working tree is not on can read through `git show`.
pub trait FileText {
    /// Contents of a repo-relative path, or `None` if it does not exist.
    fn text(&self, path: &str) -> Option<String>;
}

impl<F: Fn(&str) -> Option<String>> FileText for F {
    fn text(&self, path: &str) -> Option<String> {
        self(path)
    }
}

/// Apparent repository name -> canonical repository name.
#[derive(Clone, Debug, Default)]
pub struct RepoMapping {
    map: FxHashMap<String, String>,
}

impl RepoMapping {
    /// No mapping: every apparent name is its own canonical name. What a
    /// WORKSPACE-mode repository, or a Bazel too old for
    /// `mod dump_repo_mapping`, gets.
    pub fn identity() -> Self {
        Self::default()
    }

    /// The mapping an index already carries, apparent name -> canonical name.
    ///
    /// A stored index records the mapping the run captured
    /// (`RepoInputs::canonical`), so work that has to re-derive the externals
    /// tables away from the machine — the offline replay's `--rederive` — can
    /// rebuild the mapping without `bazel mod dump_repo_mapping`.
    ///
    /// An empty canonical name is "not recorded": the index of a WORKSPACE-mode
    /// run, where no `bazel mod dump_repo_mapping` ever ran, stores `""` for
    /// every repository. Those are left out, so they resolve to themselves
    /// like any unmapped name. Read in as entries they would each become a
    /// main-repository alias — the empty canonical name is how the dump
    /// spells the root module — and `load("@bazel_gazelle//:deps.bzl")` would
    /// resolve to the loading file itself, which is how a replay's
    /// `--rederive` once attributed sourcegraph's `deps.bzl` to every
    /// repository in the universe.
    pub fn from_canonical<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            map: pairs
                .into_iter()
                .filter(|(a, c)| !c.is_empty() || a.is_empty())
                .map(|(a, c)| (a.to_string(), c.to_string()))
                .collect(),
        }
    }

    /// Parses the JSON object `bazel mod dump_repo_mapping ""` prints.
    ///
    /// Lenient by design: the command prints progress lines on stderr but has
    /// been seen to interleave, so the first `{...}` object in the text wins,
    /// and anything unparseable yields the identity mapping rather than an
    /// error. Getting this wrong costs precision on external load *names*, not
    /// soundness.
    pub fn from_dump(s: &str) -> Self {
        let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) else {
            tracing::warn!(
                "dump_repo_mapping output had no JSON object; falling back to identity mapping"
            );
            return Self::identity();
        };
        match serde_json::from_str::<FxHashMap<String, String>>(&s[start..=end]) {
            Ok(map) => Self { map },
            Err(e) => {
                tracing::warn!("dump_repo_mapping output was not valid JSON ({e}); falling back to identity mapping");
                Self::identity()
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Whether every apparent name is its own canonical name: the mapping
    /// [`identity`](Self::identity) gives, or one read back from an index a
    /// WORKSPACE-mode run wrote. A bzlmod mapping renames at least its
    /// `bazel_dep`s (`rules_cc` -> `rules_cc+`). Main-repository aliases —
    /// entries with the empty canonical name — say nothing about where the
    /// mapping came from and are not counted.
    pub fn is_identity(&self) -> bool {
        self.map.iter().all(|(k, v)| k == v || v.is_empty())
    }

    /// Adds an apparent name that means the main repository.
    ///
    /// The root module's own name always does — `@envoy//:VERSION.txt` inside
    /// envoy is a main-repo label — and `bazel mod dump_repo_mapping` says so.
    /// But that command is unavailable in WORKSPACE mode and before Bazel 7.1,
    /// where the mapping falls back to the identity, and there `@envoy` would
    /// read as a *foreign* repository. For the load closure that only costs
    /// precision, but [`crate::externals`] uses the same test to decide which
    /// files feed a repository, and getting it wrong there loses an edge. So
    /// the root module's name, which `MODULE.bazel` states outright, is added
    /// to whatever mapping is in hand.
    pub fn with_main_alias(mut self, name: &str) -> Self {
        if !name.is_empty() {
            self.map.insert(name.to_string(), String::new());
        }
        self
    }

    /// The canonical name for an apparent name. An unmapped name is its own
    /// canonical name, which is what WORKSPACE mode does anyway.
    pub fn canonical<'a>(&'a self, apparent: &'a str) -> &'a str {
        self.map
            .get(apparent)
            .map(String::as_str)
            .unwrap_or(apparent)
    }

    /// True if an apparent name refers to the main repository. The dump maps
    /// the root module's own name (and 7.4.1's `__main__`) to the empty
    /// canonical name; `@//foo` writes it directly.
    pub fn is_main(&self, apparent: &str) -> bool {
        apparent.is_empty() || self.canonical(apparent).is_empty()
    }
}

/// Where a `load()` label points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Load {
    /// A repo-relative path in the main repository. Walked.
    Main(String),
    /// A canonical `@@repo//pkg:file.bzl` label. Recorded, never walked.
    External(String),
}

/// Resolves a `load()` label written inside package `from_pkg`.
///
/// Handles `//pkg:file.bzl`, `//pkg:sub/file.bzl`, `:file.bzl`,
/// `@repo//pkg:file.bzl`, `@@canonical//pkg:file.bzl`, and `@//pkg:file.bzl`
/// (the main repository, spelled explicitly).
pub fn resolve_label(label: &str, from_pkg: &str, m: &RepoMapping) -> Option<Load> {
    let (repo, rest) = match label.strip_prefix("@@") {
        // A doubled `@` means the name is already canonical.
        Some(rest) => match rest.split_once("//") {
            Some((r, rest)) => (Some(r.to_string()), format!("//{rest}")),
            None => return None,
        },
        None => match label.strip_prefix('@') {
            Some(rest) => match rest.split_once("//") {
                Some((r, rest)) => (Some(m.canonical(r).to_string()), format!("//{rest}")),
                // `@repo` on its own is not a file label.
                None => return None,
            },
            None => (None, label.to_string()),
        },
    };

    // An external repository: name it canonically and stop.
    if let Some(repo) = &repo {
        if !repo.is_empty() {
            let (pkg, name) = split_target(&rest)?;
            return Some(Load::External(format!("@@{repo}//{pkg}:{name}")));
        }
    }

    let (pkg, name) = match rest.strip_prefix("//") {
        Some(rest) => {
            let (pkg, name) = split_target_body(rest)?;
            (pkg.to_string(), name.to_string())
        }
        // Relative to the loading file's own package. A bare `file.bzl` with
        // no leading colon is not a legal load label in any supported Bazel.
        None => (from_pkg.to_string(), rest.strip_prefix(':')?.to_string()),
    };
    if name.is_empty() {
        return None;
    }
    Some(Load::Main(if pkg.is_empty() {
        name
    } else {
        format!("{pkg}/{name}")
    }))
}

/// `//pkg:name` -> `(pkg, name)`, for a label already known to start `//`.
fn split_target(label: &str) -> Option<(String, String)> {
    let rest = label.strip_prefix("//")?;
    let (pkg, name) = split_target_body(rest)?;
    Some((pkg.to_string(), name.to_string()))
}

fn split_target_body(rest: &str) -> Option<(&str, &str)> {
    match rest.split_once(':') {
        Some((pkg, name)) => Some((pkg, name)),
        // `//pkg/file.bzl` is not valid, but `//pkg` with an implicit target
        // name never names a `.bzl`; treat it as unresolvable.
        None => None,
    }
}

// ------------------------------------------------------------------ the lexer

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tok<'a> {
    Name(&'a str),
    Str(usize, usize),
    /// An opening bracket, with the byte itself: `x(y)` is a call and `x[y]`
    /// is a subscript, and reading the second as the first makes every
    /// indexing expression look like a call to a symbol nothing declares.
    Open(u8),
    Close,
    /// One punctuation byte: `=`, `.`, `,` and friends. Kept rather than
    /// collapsed into `Other` because [`crate::externals`] reads statement
    /// shapes (`x = f(...)`, `x.tag(...)`) out of the same token stream.
    Punct(u8),
    Other,
}

/// One token plus whether it starts a line.
pub(crate) struct Lexed<'a> {
    pub(crate) tok: Tok<'a>,
    pub(crate) col0: bool,
}

/// Tokenises far enough to find `load()` calls and nothing further: names,
/// string literals, and bracket nesting. Comments and string contents are
/// consumed as units, which is the whole point — a `load(` inside a comment or
/// inside a docstring is not a load.
pub(crate) fn lex(text: &str) -> Vec<Lexed<'_>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        let col0 = i == 0 || b[i - 1] == b'\n';
        match c {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'(' | b'[' | b'{' => {
                out.push(Lexed {
                    tok: Tok::Open(c),
                    col0,
                });
                i += 1;
            }
            b')' | b']' | b'}' => {
                out.push(Lexed {
                    tok: Tok::Close,
                    col0,
                });
                i += 1;
            }
            b'"' | b'\'' => {
                let (span, next) = lex_string(b, i, false);
                i = next;
                match span {
                    Some((s, e)) => out.push(Lexed {
                        tok: Tok::Str(s, e),
                        col0,
                    }),
                    None => out.push(Lexed {
                        tok: Tok::Other,
                        col0,
                    }),
                }
            }
            _ if c.is_ascii_alphanumeric() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                // A string prefix (`r"..."`, `b'...'`, `rb"..."`) belongs to
                // the literal that follows, not to a name.
                if i < b.len() && (b[i] == b'"' || b[i] == b'\'') && is_string_prefix(&b[start..i])
                {
                    let raw = b[start..i].iter().any(|c| *c == b'r' || *c == b'R');
                    let (span, next) = lex_string(b, i, raw);
                    i = next;
                    match span {
                        Some((s, e)) => out.push(Lexed {
                            tok: Tok::Str(s, e),
                            col0,
                        }),
                        None => out.push(Lexed {
                            tok: Tok::Other,
                            col0,
                        }),
                    }
                } else {
                    out.push(Lexed {
                        tok: Tok::Name(&text[start..i]),
                        col0,
                    });
                }
            }
            _ => {
                out.push(Lexed {
                    tok: if c.is_ascii_punctuation() {
                        Tok::Punct(c)
                    } else {
                        Tok::Other
                    },
                    col0,
                });
                i += 1;
            }
        }
    }
    out
}

fn is_string_prefix(s: &[u8]) -> bool {
    s.len() <= 2 && s.iter().all(|c| matches!(c, b'r' | b'R' | b'b' | b'B'))
}

/// Consumes a string literal starting at `i` (a quote). Returns the span of
/// the *contents* and the index just past the literal.
fn lex_string(b: &[u8], i: usize, raw: bool) -> (Option<(usize, usize)>, usize) {
    let q = b[i];
    let triple = b.len() >= i + 3 && b[i + 1] == q && b[i + 2] == q;
    let open = if triple { 3 } else { 1 };
    let mut j = i + open;
    let start = j;
    while j < b.len() {
        if b[j] == b'\\' && !raw {
            j += 2;
            continue;
        }
        if b[j] == q {
            if !triple {
                return (Some((start, j)), j + 1);
            }
            if b.len() >= j + 3 && b[j + 1] == q && b[j + 2] == q {
                return (Some((start, j)), j + 3);
            }
        }
        // An unterminated single-quoted string ends at the newline, as in
        // Starlark; a triple-quoted one does not.
        if b[j] == b'\n' && !triple {
            return (None, j);
        }
        j += 1;
    }
    (None, b.len())
}

/// Decodes the handful of escapes a label could plausibly contain. Anything
/// else is left alone: this is a label, not arbitrary text.
pub(crate) fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

/// The label of every top-level `load()` in a Starlark file, in order.
///
/// Multi-line calls, either quoting style, aliases (`alias = "sym"`) and
/// comments between arguments are all handled; a `load(` in a comment or
/// inside a string is not a load.
pub fn scan_loads(text: &str) -> Vec<String> {
    scan(text, &["load"], true)
}

/// As [`scan_loads`], plus the `.bzl` labels `MODULE.bazel` reaches through
/// `use_extension()` and `use_repo_rule()`. Those are calls in an assignment,
/// not top-level statements, so they are matched anywhere in code.
pub fn scan_workspace_loads(text: &str) -> Vec<String> {
    let mut out = scan(text, &["load"], true);
    out.extend(scan(text, &["use_extension", "use_repo_rule"], false));
    out
}

/// First string argument of every call to one of `names`.
///
/// `top_level_only` restricts the match to a name that starts a line at
/// bracket depth zero, which is exactly where `load` is legal.
fn scan(text: &str, names: &[&str], top_level_only: bool) -> Vec<String> {
    let toks = lex(text);
    let mut out = Vec::new();
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        match t.tok {
            Tok::Open(_) => depth += 1,
            Tok::Close => depth -= 1,
            Tok::Name(n) if names.contains(&n) => {
                if top_level_only && (!t.col0 || depth != 0) {
                    continue;
                }
                if !matches!(toks.get(i + 1).map(|t| t.tok), Some(Tok::Open(_))) {
                    continue;
                }
                // First string literal inside the call.
                let mut d = 0i32;
                for u in &toks[i + 1..] {
                    match u.tok {
                        Tok::Open(_) => d += 1,
                        Tok::Close => {
                            d -= 1;
                            if d == 0 {
                                break;
                            }
                        }
                        Tok::Str(s, e) if d == 1 => {
                            out.push(unescape(&text[s..e]));
                            break;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ------------------------------------------------------------- the load graph

/// The main-repository `.bzl` load graph: which `.bzl` files exist, what each
/// one loads directly, and which external labels it names.
///
/// Parsing is memoised per file — a `.bzl` loaded by a thousand packages is
/// read and lexed once — and a closure is then a breadth-first walk over the
/// memoised edges, which costs the size of the answer.
#[derive(Clone, Debug, Default)]
pub struct LoadGraph {
    ids: FxHashMap<String, u32>,
    paths: Vec<String>,
    direct: Vec<Vec<u32>>,
    external: Vec<Vec<String>>,
    parsed: Vec<bool>,
}

impl LoadGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds the graph from an index's stored section, so an incremental run
    /// re-reads only the files whose text can have moved.
    pub fn from_index(v: IndexRef<'_>) -> Self {
        let n = v.num_bzl() as usize;
        let mut g = Self {
            ids: FxHashMap::default(),
            paths: Vec::with_capacity(n),
            direct: Vec::with_capacity(n),
            external: Vec::with_capacity(n),
            parsed: vec![true; n],
        };
        for i in 0..n as u32 {
            let p = v.bzl_path(i).to_string();
            g.ids.insert(p.clone(), i);
            g.paths.push(p);
            g.direct.push(v.bzl_loads(i).to_vec());
            g.external
                .push(v.bzl_external(i).iter().map(|s| s.to_string()).collect());
        }
        g
    }

    /// Forgets what a file loads, so the next walk re-reads it. Called for
    /// every `.bzl` in the change set.
    pub fn invalidate(&mut self, path: &str) {
        if let Some(&id) = self.ids.get(path) {
            self.parsed[id as usize] = false;
            self.direct[id as usize].clear();
            self.external[id as usize].clear();
        }
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn path(&self, id: u32) -> &str {
        &self.paths[id as usize]
    }

    pub fn loads(&self, id: u32) -> &[u32] {
        &self.direct[id as usize]
    }

    pub fn external(&self, id: u32) -> &[String] {
        &self.external[id as usize]
    }

    pub fn id_of(&self, path: &str) -> Option<u32> {
        self.ids.get(path).copied()
    }

    fn intern(&mut self, path: &str) -> u32 {
        if let Some(&id) = self.ids.get(path) {
            return id;
        }
        let id = self.paths.len() as u32;
        self.ids.insert(path.to_string(), id);
        self.paths.push(path.to_string());
        self.direct.push(Vec::new());
        self.external.push(Vec::new());
        self.parsed.push(false);
        id
    }

    /// Reads and parses one `.bzl` if it has not been parsed already.
    fn ensure(&mut self, id: u32, src: &dyn FileText, m: &RepoMapping) {
        if self.parsed[id as usize] {
            return;
        }
        self.parsed[id as usize] = true;
        let path = self.paths[id as usize].clone();
        let Some(text) = src.text(&path) else {
            // A `.bzl` that was deleted, or one this checkout does not have.
            // Its consumers are dirtied by the change itself.
            return;
        };
        let pkg = owning_dir(&path);
        let (main, ext) = self.resolve_all(&scan_loads(&text), &pkg, m);
        self.direct[id as usize] = main;
        self.external[id as usize] = ext;
    }

    fn resolve_all(
        &mut self,
        labels: &[String],
        from_pkg: &str,
        m: &RepoMapping,
    ) -> (Vec<u32>, Vec<String>) {
        let mut main = Vec::new();
        let mut ext = Vec::new();
        for l in labels {
            match resolve_label(l, from_pkg, m) {
                Some(Load::Main(p)) => main.push(self.intern(&p)),
                Some(Load::External(l)) => ext.push(l),
                None => {}
            }
        }
        main.sort_unstable();
        main.dedup();
        ext.sort();
        ext.dedup();
        (main, ext)
    }

    /// Transitive closure of a set of roots, sorted. Cycle-safe: a `.bzl`
    /// cycle is a load error in Bazel, but a half-written tree can contain
    /// one and the walk must terminate anyway.
    fn closure(&mut self, roots: Vec<u32>, src: &dyn FileText, m: &RepoMapping) -> Vec<u32> {
        let mut seen: Vec<u32> = Vec::new();
        let mut stack = roots;
        let mut visited = FxHashMap::default();
        while let Some(id) = stack.pop() {
            if visited.insert(id, ()).is_some() {
                continue;
            }
            seen.push(id);
            self.ensure(id, src, m);
            stack.extend_from_slice(&self.direct[id as usize]);
        }
        seen.sort_unstable();
        seen.dedup();
        seen
    }

    /// The `.bzl` closure of a package, read from its BUILD file. Empty when
    /// the package has no BUILD file in this checkout.
    ///
    /// The external labels the BUILD file loads *directly* come back
    /// alongside. They have no `.bzl` node to hang on — the walk stops at an
    /// external repository, and the BUILD file is not a node itself — so
    /// they would otherwise be lost, and a package that loads
    /// `@rules_foo//:defs.bzl` without a main-repo `.bzl` in between would be
    /// invisible to the repository-dirtying rule. (Issue #4.)
    pub fn package_closure(
        &mut self,
        pkg: &str,
        src: &dyn FileText,
        m: &RepoMapping,
    ) -> PackageClosure {
        let mut roots = Vec::new();
        let mut external = Vec::new();
        for name in ["BUILD.bazel", "BUILD"] {
            let path = if pkg.is_empty() {
                name.to_string()
            } else {
                format!("{pkg}/{name}")
            };
            let Some(text) = src.text(&path) else {
                continue;
            };
            let (main, ext) = self.resolve_all(&scan_loads(&text), pkg, m);
            roots.extend(main);
            external = ext;
            // Bazel reads exactly one of the two, preferring BUILD.bazel.
            break;
        }
        PackageClosure {
            bzl: self.closure(roots, src, m),
            external,
        }
    }

    /// The `.bzl` closure of the workspace files: `load()` in `WORKSPACE`, and
    /// `load` / `use_extension` / `use_repo_rule` in `MODULE.bazel`. A `.bzl`
    /// in here defines repository rules.
    pub fn workspace_closure(&mut self, src: &dyn FileText, m: &RepoMapping) -> Vec<u32> {
        let mut roots = Vec::new();
        for name in WORKSPACE_FILES {
            let Some(text) = src.text(name) else {
                continue;
            };
            let (main, _) = self.resolve_all(&scan_workspace_loads(&text), "", m);
            roots.extend(main);
        }
        self.closure(roots, src, m)
    }
}

/// What one package's BUILD file loads, transitively: the main-repo `.bzl`
/// ids, and the canonical external labels its own `load()`s name. The external
/// labels the *`.bzl` files* stop at are recorded on those files
/// ([`LoadGraph::external`]); these are the BUILD file's own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageClosure {
    pub bzl: Vec<u32>,
    pub external: Vec<String>,
}

/// The directory a file sits in, which for a `.bzl` is the package it is
/// written relative to. Using the directory rather than the owning package is
/// deliberate: a relative `:other.bzl` load resolves against the *file's own*
/// directory-as-package, and a `.bzl` in a subdirectory of a package can only
/// be loaded by its full `//pkg:sub/file.bzl` label anyway.
pub fn owning_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

/// The name a `WORKSPACE` file gives the main repository
/// (`workspace(name = "envoy")`).
///
/// In WORKSPACE mode `bazel mod dump_repo_mapping` does not exist, so the
/// mapping is the identity and `@envoy//pkg:f.bzl` would read as a foreign
/// repository. This is the alias that says otherwise — the WORKSPACE
/// counterpart of `MODULE.bazel`'s `module(name = ...)` — and it is fed to
/// [`RepoMapping::with_main_alias`] by callers that resolve load labels read
/// out of a checkout rather than out of a stored index.
pub fn workspace_name(text: &str) -> Option<String> {
    scan(text, &["workspace"], true).into_iter().next()
}

/// True for a file Bazel reads as Starlark: a BUILD file, a `.bzl`, or one of
/// the workspace files.
pub fn is_starlark_file(name: &str) -> bool {
    name.ends_with(".bzl")
        || name == "BUILD"
        || name == "BUILD.bazel"
        || WORKSPACE_FILES.contains(&name)
}

/// Every Starlark file of a checkout, as repo-relative paths, sorted.
///
/// Directories listed in `.bazelignore` are skipped, as are `.git` and
/// anything reached through a symlink: Bazel's `bazel-out` and `bazel-<repo>`
/// convenience symlinks point into the output base, and following them would
/// walk the whole build tree (and can loop).
///
/// Paths only — the caller opens each file once, if at all.
pub fn starlark_files(root: &std::path::Path) -> Vec<String> {
    let ignored = read_bazelignore(root);
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        let here = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&dir)
        };
        let Ok(entries) = std::fs::read_dir(&here) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            let rel = if dir.is_empty() {
                name.clone()
            } else {
                format!("{dir}/{name}")
            };
            if ft.is_dir() {
                if name == ".git" || ignored.contains(&rel) {
                    continue;
                }
                stack.push(rel);
            } else if is_starlark_file(&name) {
                out.push(rel);
            }
        }
    }
    out.sort();
    out
}

/// The directory paths `.bazelignore` names, without a trailing slash. The
/// file is a plain list of repo-relative directories, one per line; it
/// supports no globs, and blank lines are ignored.
fn read_bazelignore(root: &std::path::Path) -> rustc_hash::FxHashSet<String> {
    let Ok(text) = std::fs::read_to_string(root.join(".bazelignore")) else {
        return rustc_hash::FxHashSet::default();
    };
    text.lines()
        .map(|l| l.trim().trim_end_matches('/'))
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A WORKSPACE-mode index stores no canonical names; reading them back
    /// must not turn every repository into the main one.
    #[test]
    fn an_unrecorded_canonical_name_is_not_a_main_alias() {
        let m = RepoMapping::from_canonical([("bazel_gazelle", ""), ("rules_cc", "rules_cc+")]);
        assert!(!m.is_main("bazel_gazelle"));
        assert_eq!(m.canonical("bazel_gazelle"), "bazel_gazelle");
        assert_eq!(m.canonical("rules_cc"), "rules_cc+");
        assert!(RepoMapping::from_canonical([("a", ""), ("b", "")]).is_identity());
        assert!(RepoMapping::identity()
            .with_main_alias("envoy")
            .is_identity());
        assert!(!m.is_identity());
        assert_eq!(
            resolve_label(
                "@bazel_gazelle//:deps.bzl",
                "",
                &RepoMapping::from_canonical([("bazel_gazelle", "")])
            ),
            Some(Load::External("@@bazel_gazelle//:deps.bzl".into()))
        );
    }

    fn labels(text: &str) -> Vec<String> {
        scan_loads(text)
    }

    #[test]
    fn scans_the_ordinary_shapes() {
        assert_eq!(labels("load(\"//a:b.bzl\", \"x\")\n"), vec!["//a:b.bzl"]);
        assert_eq!(labels("load('//a:b.bzl', 'x')\n"), vec!["//a:b.bzl"]);
        assert_eq!(
            labels("load(\"//a:b.bzl\", alias = \"x\", \"y\")\n"),
            vec!["//a:b.bzl"]
        );
    }

    #[test]
    fn scans_multi_line_calls() {
        let t = "load(\n    \"//a:b.bzl\",\n    \"x\",  # a comment\n    y = \"z\",\n)\n\nload(\"//c:d.bzl\", \"q\")\n";
        assert_eq!(labels(t), vec!["//a:b.bzl", "//c:d.bzl"]);
    }

    #[test]
    fn a_load_in_a_comment_or_string_is_not_a_load() {
        assert!(labels("# load(\"//a:b.bzl\", \"x\")\n").is_empty());
        assert!(labels("X = \"load(\\\"//a:b.bzl\\\", \\\"x\\\")\"\n").is_empty());
        assert!(labels("DOC = \"\"\"\nload(\"//a:b.bzl\", \"x\")\n\"\"\"\n").is_empty());
        // Indented: not a top-level statement, so not a legal load.
        assert!(labels("def f():\n    load(\"//a:b.bzl\", \"x\")\n").is_empty());
        // Inside a call's arguments.
        assert!(labels("f(\n  load(\"//a:b.bzl\", \"x\"),\n)\n").is_empty());
    }

    #[test]
    fn handles_raw_and_prefixed_strings() {
        assert_eq!(labels("load(r\"//a:b.bzl\", \"x\")\n"), vec!["//a:b.bzl"]);
    }

    #[test]
    fn resolves_every_label_shape() {
        let m = RepoMapping::identity();
        assert_eq!(
            resolve_label("//a/b:c.bzl", "z", &m),
            Some(Load::Main("a/b/c.bzl".into()))
        );
        assert_eq!(
            resolve_label("//a:sub/c.bzl", "z", &m),
            Some(Load::Main("a/sub/c.bzl".into()))
        );
        assert_eq!(
            resolve_label("//:c.bzl", "z", &m),
            Some(Load::Main("c.bzl".into()))
        );
        assert_eq!(
            resolve_label(":c.bzl", "a/b", &m),
            Some(Load::Main("a/b/c.bzl".into()))
        );
        assert_eq!(
            resolve_label(":c.bzl", "", &m),
            Some(Load::Main("c.bzl".into()))
        );
        assert_eq!(
            resolve_label("@//a:c.bzl", "z", &m),
            Some(Load::Main("a/c.bzl".into()))
        );
        assert_eq!(resolve_label("c.bzl", "a", &m), None);
        assert_eq!(resolve_label("@repo", "a", &m), None);
    }

    #[test]
    fn external_labels_are_canonicalised_and_never_walked() {
        let m =
            RepoMapping::from_dump("{\"\":\"\",\"demo\":\"\",\"rules_shell\":\"rules_shell+\"}");
        assert_eq!(
            resolve_label("@rules_shell//shell:sh_binary.bzl", "a", &m),
            Some(Load::External("@@rules_shell+//shell:sh_binary.bzl".into()))
        );
        // Already canonical.
        assert_eq!(
            resolve_label("@@rules_shell+//shell:sh_binary.bzl", "a", &m),
            Some(Load::External("@@rules_shell+//shell:sh_binary.bzl".into()))
        );
        // The root module's own apparent name is the main repository.
        assert_eq!(
            resolve_label("@demo//a:c.bzl", "z", &m),
            Some(Load::Main("a/c.bzl".into()))
        );
        // 7.4.1's `~` separator survives verbatim.
        let old = RepoMapping::from_dump("{\"rules_shell\":\"rules_shell~\",\"__main__\":\"\"}");
        assert_eq!(
            resolve_label("@rules_shell//shell:sh_binary.bzl", "a", &old),
            Some(Load::External("@@rules_shell~//shell:sh_binary.bzl".into()))
        );
        assert!(old.is_main("__main__"));
    }

    #[test]
    fn workspace_scan_sees_extensions_and_repo_rules() {
        let t = "bazel_dep(name = \"x\")\next = use_extension(\"//bazel:ext.bzl\", \"ext\")\n\
                 http_archive = use_repo_rule(\"//bazel:repo.bzl\", \"http_archive\")\n\
                 load(\"//bazel:w.bzl\", \"w\")\n";
        let mut got = scan_workspace_loads(t);
        got.sort();
        assert_eq!(
            got,
            vec!["//bazel:ext.bzl", "//bazel:repo.bzl", "//bazel:w.bzl"]
        );
    }

    /// A tiny in-memory checkout.
    struct Files(Vec<(&'static str, &'static str)>);

    impl FileText for Files {
        fn text(&self, path: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, t)| t.to_string())
        }
    }

    fn closure_paths(g: &LoadGraph, ids: &[u32]) -> Vec<String> {
        let mut v: Vec<String> = ids.iter().map(|&i| g.path(i).to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn walks_a_diamond_once() {
        let f = Files(vec![
            ("app/BUILD.bazel", "load(\"//bazel:a.bzl\", \"a\")\n"),
            (
                "bazel/a.bzl",
                "load(\"//bazel:b.bzl\", \"b\")\nload(\":c.bzl\", \"c\")\n",
            ),
            ("bazel/b.bzl", "load(\"//bazel:d.bzl\", \"d\")\n"),
            ("bazel/c.bzl", "load(\"//bazel:d.bzl\", \"d\")\n"),
            ("bazel/d.bzl", "X = 1\n"),
        ]);
        let m = RepoMapping::identity();
        let mut g = LoadGraph::new();
        let c = g.package_closure("app", &f, &m).bzl;
        assert_eq!(
            closure_paths(&g, &c),
            vec![
                "bazel/a.bzl".to_string(),
                "bazel/b.bzl".into(),
                "bazel/c.bzl".into(),
                "bazel/d.bzl".into()
            ]
        );
    }

    #[test]
    fn a_load_cycle_terminates() {
        let f = Files(vec![
            ("p/BUILD", "load(\"//p:a.bzl\", \"a\")\n"),
            ("p/a.bzl", "load(\"//p:b.bzl\", \"b\")\n"),
            ("p/b.bzl", "load(\"//p:a.bzl\", \"a\")\n"),
        ]);
        let mut g = LoadGraph::new();
        let c = g.package_closure("p", &f, &RepoMapping::identity()).bzl;
        assert_eq!(closure_paths(&g, &c), vec!["p/a.bzl", "p/b.bzl"]);
    }

    #[test]
    fn the_walk_stops_at_an_external_repository() {
        let f = Files(vec![
            ("app/BUILD", "load(\"//bazel:defs.bzl\", \"x\")\n"),
            (
                "bazel/defs.bzl",
                "load(\"@rules_shell//shell:sh_binary.bzl\", \"sh_binary\")\n",
            ),
        ]);
        let m = RepoMapping::from_dump("{\"rules_shell\":\"rules_shell+\"}");
        let mut g = LoadGraph::new();
        let c = g.package_closure("app", &f, &m);
        assert_eq!(closure_paths(&g, &c.bzl), vec!["bazel/defs.bzl"]);
        let id = g.id_of("bazel/defs.bzl").unwrap();
        assert_eq!(g.external(id), ["@@rules_shell+//shell:sh_binary.bzl"]);
        assert!(
            c.external.is_empty(),
            "the BUILD file itself loads nothing external"
        );
    }

    #[test]
    fn a_build_files_own_external_load_is_kept() {
        // Issue #4: the external half of the BUILD file's direct loads used
        // to be discarded, so a package in this shape had no way to be found
        // when `rules_shell` moved.
        let f = Files(vec![
            (
                "app/BUILD",
                "load(\"@rules_shell//shell:sh_binary.bzl\", \"sh_binary\")\n\
                 load(\"//bazel:defs.bzl\", \"x\")\n\
                 load(\"@rules_cc//cc:defs.bzl\", \"cc_library\")\n",
            ),
            ("bazel/defs.bzl", "X = 1\n"),
        ]);
        let m =
            RepoMapping::from_dump("{\"rules_shell\":\"rules_shell+\",\"rules_cc\":\"rules_cc+\"}");
        let mut g = LoadGraph::new();
        let c = g.package_closure("app", &f, &m);
        assert_eq!(closure_paths(&g, &c.bzl), vec!["bazel/defs.bzl"]);
        assert_eq!(
            c.external,
            [
                "@@rules_cc+//cc:defs.bzl".to_string(),
                "@@rules_shell+//shell:sh_binary.bzl".into()
            ],
            "sorted, and only the BUILD file's own"
        );
        let id = g.id_of("bazel/defs.bzl").unwrap();
        assert!(g.external(id).is_empty());
    }

    #[test]
    fn a_missing_build_file_has_an_empty_closure() {
        let mut g = LoadGraph::new();
        assert_eq!(
            g.package_closure("nope", &Files(vec![]), &RepoMapping::identity()),
            PackageClosure::default()
        );
    }

    #[test]
    fn build_bazel_wins_over_build() {
        let f = Files(vec![
            ("p/BUILD.bazel", "load(\"//p:a.bzl\", \"a\")\n"),
            ("p/BUILD", "load(\"//p:b.bzl\", \"b\")\n"),
            ("p/a.bzl", ""),
            ("p/b.bzl", ""),
        ]);
        let mut g = LoadGraph::new();
        let c = g.package_closure("p", &f, &RepoMapping::identity()).bzl;
        assert_eq!(closure_paths(&g, &c), vec!["p/a.bzl"]);
    }

    #[test]
    fn invalidate_forces_a_reparse() {
        let mut g = LoadGraph::new();
        let m = RepoMapping::identity();
        let f1 = Files(vec![
            ("p/BUILD", "load(\"//p:a.bzl\", \"a\")\n"),
            ("p/a.bzl", ""),
        ]);
        assert_eq!(g.package_closure("p", &f1, &m).bzl.len(), 1);
        let f2 = Files(vec![
            ("p/BUILD", "load(\"//p:a.bzl\", \"a\")\n"),
            ("p/a.bzl", "load(\"//p:b.bzl\", \"b\")\n"),
            ("p/b.bzl", ""),
        ]);
        // Without invalidation the memoised parse stands.
        assert_eq!(g.package_closure("p", &f2, &m).bzl.len(), 1);
        g.invalidate("p/a.bzl");
        assert_eq!(g.package_closure("p", &f2, &m).bzl.len(), 2);
    }
}
