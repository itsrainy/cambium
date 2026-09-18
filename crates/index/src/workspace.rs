//! A static, statement-level diff of the legacy `WORKSPACE` file.
//!
//! # Why this is possible at all
//!
//! `WORKSPACE` is imperative, and that is why any edit to it was global:
//! repositories are declared by arbitrary macro code, order matters, and
//! `native.existing_rules()` lets a later statement's meaning depend on an
//! earlier one. Evaluating it statically is out of the question.
//!
//! But a *diff* does not have to evaluate the file. It only has to answer:
//! given that these two texts differ, which repositories can have different
//! contents? For the statements real workspaces are made of that question has
//! an answer that needs no evaluation:
//!
//! - `http_archive(name = "rules_oci", url = ...)` names the repository it
//!   declares. If the call's text moved, that repository moved, and no other
//!   one did — the call cannot reach into a sibling declaration.
//! - `deps()`, loaded from `//dev:deps.bzl`, declares whatever that `.bzl`
//!   declares. The index already records which repositories each
//!   workspace-closure `.bzl` can define, for exactly this reason, so a change
//!   to such a call dirties that set.
//! - a declaration's `name` is only the whole story when the callee is a
//!   repository rule, so a call that came from a main-repo `.bzl` charges that
//!   file's repositories too, in case the callee is a macro that declares a
//!   family of them.
//! - a `load()` whose label did not move contributes nothing on its own: the
//!   symbols it brings in are used by other top-level statements, which are
//!   their own entries in this diff and are compared in their own right.
//!
//! Everything else — a call this parser cannot classify, an assignment, a
//! loop, a conditional, `bind`, `workspace()`, toolchain registration, a
//! `local_repository` whose `path` moved, a statement that appeared, vanished
//! or moved relative to its neighbours — is [`WorkspaceChange::Global`], with
//! a reason naming the statement. On sourcegraph, whose six `WORKSPACE`
//! commits in three hundred were all global before this, three become a named
//! repository set and three stay global on the toolchain rule.
//!
//! # Soundness
//!
//! Claim: if a repository's contents can differ between the two texts, this
//! diff either names it or returns `Global`.
//!
//! 1. Statements are matched by identity, not position: a declaration by its
//!    `name`, a `load()` by its label, an unnamed call by its callee. A
//!    statement present on one side only, or one whose text moved, is
//!    classified; a statement whose text is identical on both sides declares
//!    the same repository from the same arguments.
//! 2. That last step is only true if the *context* is the same, which is why
//!    the diff insists the statements two sides have in common appear in the
//!    same relative order, and escalates otherwise. Reordering is the one way
//!    an imperative file can change an unchanged statement's meaning.
//! 3. A call's text is compared as its whole token rendering, not as its
//!    string literals: `crane_version = LATEST_CRANE_VERSION` moving to
//!    another symbol has to be visible.
//! 4. Anything unclassified escalates. The parser recognises one shape — a
//!    single top-level call — and everything else is opaque.
//!
//! The residue this shares with the rest of the module: a macro is taken at
//! the word of the `.bzl` it comes from, and a repository rule that reads a
//! main-repo file by computed path is invisible here as everywhere else.

use std::collections::{BTreeMap, BTreeSet};

use crate::label::{Label, Repo};
use crate::starlark::{Arg, Expr, ExprKind, Module, Stmt};
use crate::util::diff_maps;
use crate::vocabulary::{is_local_repo_rule, registers_toolchains, NOT_JUST_A_REPOSITORY};

/// What a `WORKSPACE` edit means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceChange {
    /// Nothing this diff can localise: invalidate everything, for this reason.
    Global(String),
    /// A statement that registers toolchains or execution platforms moved. As
    /// global as [`WorkspaceChange::Global`], and kept apart so the run can
    /// report it under the toolchain code rather than the stage's own: the
    /// gap is the graph's (toolchain resolution has no edge), not the stage's.
    Registration(String),
    /// These repositories moved, and these workspace-closure `.bzl` files had
    /// a call of theirs change — whatever repositories *they* can define moved
    /// too, which only the index can say.
    Moved {
        repos: BTreeSet<String>,
        bzl_paths: BTreeSet<String>,
    },
}

/// How a statement is matched against the other side.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    /// A call with a literal `name = "x"`: the repository it declares.
    Repo(String),
    /// A `load()`, by its label.
    Load(String),
    /// A call with no literal name, by callee and by which call of that callee
    /// it is.
    Call(String, usize),
    /// Anything else, by position among the opaque statements.
    Opaque(usize),
}

/// One classified statement.
///
/// Every field is derived from the statement's token slice, so two `Parsed`
/// under the same [`Key`] are equal exactly when their `text` is: that is
/// what "did it change" means here, and what the diff compares.
#[derive(PartialEq, Eq)]
struct Parsed {
    key: Key,
    /// The callee, for a call.
    callee: String,
    /// `path = "..."`, when the call has one.
    path: Option<String>,
    /// The whole statement, token by token, which is what "did it change"
    /// means here.
    text: String,
    /// A short rendering for a message.
    short: String,
}

/// Reads one side into classified statements, plus its `load()` map from
/// symbol to the label it came from.
fn scan(text: &str) -> (Vec<Parsed>, BTreeMap<String, String>) {
    let m = Module::parse(text);
    let mut out: Vec<Parsed> = Vec::new();
    let mut loaded: BTreeMap<String, String> = BTreeMap::new();
    let mut calls: BTreeMap<String, usize> = BTreeMap::new();
    let mut opaque = 0usize;
    for st in &m.stmts {
        let span = st.span();
        let mut p = Parsed {
            key: Key::Opaque(opaque),
            callee: String::new(),
            path: None,
            text: m.render_all(&span),
            short: m.render_short(&span),
        };
        match call_shape(st) {
            None => opaque += 1,
            Some((callee, args)) => {
                let args = m.args(args);
                p.callee = callee.to_string();
                p.path = args.first("path").map(String::from);
                if callee == "load" {
                    let label = args.positional.first().cloned().unwrap_or_default();
                    // `load("//dev:deps.bzl", "deps", alias = "other")`: both
                    // the positional symbols and the values of the keyword
                    // aliases name something in that file.
                    for s in args.positional.iter().skip(1) {
                        loaded.insert(s.clone(), label.clone());
                    }
                    for (k, v) in &args.kw {
                        loaded.insert(k.clone(), label.clone());
                        for s in v {
                            loaded.insert(s.clone(), label.clone());
                        }
                    }
                    p.key = Key::Load(label);
                } else if let Some(name) = args.first("name") {
                    p.key = Key::Repo(name.to_string());
                } else {
                    let n = calls.entry(callee.to_string()).or_default();
                    p.key = Key::Call(callee.to_string(), *n);
                    *n += 1;
                }
            }
        }
        out.push(p);
    }
    (out, loaded)
}

/// The callee and arguments of a statement that is exactly one top-level
/// call, `f(...)`, or a `load(...)`.
///
/// An assignment, a dotted call, a comprehension, a `for` or an `if` all fail
/// this and are opaque.
fn call_shape<'s, 'a>(st: &'s Stmt<'a>) -> Option<(&'a str, &'s [Arg<'a>])> {
    match st {
        Stmt::Load(l) => Some(("load", &l.args)),
        Stmt::Expr(Expr {
            kind: ExprKind::Call { callee, args },
            ..
        }) => callee.as_name().map(|n| (n, args.as_slice())),
        _ => None,
    }
}

/// Diffs two `WORKSPACE` texts.
pub fn diff_workspace(base: &str, head: &str) -> WorkspaceChange {
    let (bs, bl) = scan(base);
    let (hs, hl) = scan(head);
    fn index(v: &[Parsed]) -> Result<BTreeMap<Key, &Parsed>, String> {
        let mut m = BTreeMap::new();
        for p in v {
            if m.insert(p.key.clone(), p).is_some() {
                return Err(format!(
                    "`{}` appears twice at the top level, so cambium cannot match the two \
                     revisions of it up",
                    p.short
                ));
            }
        }
        Ok(m)
    }
    let (bi, hi) = match (index(&bs), index(&hs)) {
        (Ok(b), Ok(h)) => (b, h),
        (Err(e), _) | (_, Err(e)) => return WorkspaceChange::Global(e),
    };

    // The statements both sides have must appear in the same relative order:
    // in an imperative file, moving a declaration past another can change what
    // the second one sees.
    let order = |v: &[Parsed], other: &BTreeMap<Key, &Parsed>| -> Vec<Key> {
        v.iter()
            .filter(|p| other.contains_key(&p.key))
            .map(|p| p.key.clone())
            .collect()
    };
    if order(&bs, &hi) != order(&hs, &bi) {
        return WorkspaceChange::Global(
            "`WORKSPACE` statements were reordered; the file is imperative, so a declaration \
             that moved past another can change what the other one sees"
                .into(),
        );
    }

    let mut repos = BTreeSet::new();
    let mut bzl_paths = BTreeSet::new();
    for (k, b, h) in diff_maps(&bi, &hi) {
        let one = h.or(b).expect("a key comes from one of the two sides");
        if registers_toolchains(&one.callee) {
            return WorkspaceChange::Registration(format!(
                "`{}` registers toolchains, and toolchain resolution has no edge in the query \
                 graph",
                one.short
            ));
        }
        match k {
            Key::Opaque(_) => {
                return WorkspaceChange::Global(format!(
                    "`WORKSPACE` statement `{}` is not one cambium can classify",
                    one.short
                ))
            }
            Key::Load(label) => {
                // Both sides have the same label: only the symbol list moved,
                // and the statements that use those symbols are compared on
                // their own. A label that appeared or vanished is a different
                // file being loaded, which can declare anything.
                if b.is_some() && h.is_some() {
                    continue;
                }
                return WorkspaceChange::Global(format!(
                    "`WORKSPACE` loads `{label}` on only one side of the diff"
                ));
            }
            Key::Repo(name) => {
                if NOT_JUST_A_REPOSITORY.contains(&one.callee.as_str()) {
                    return WorkspaceChange::Global(format!(
                        "`{}` is not confined to the repository it names",
                        one.short
                    ));
                }
                if is_local_repo_rule(&one.callee)
                    && b.map(|p| &p.path) != h.map(|p| &p.path)
                    && b.is_some()
                    && h.is_some()
                {
                    return WorkspaceChange::Global(format!(
                        "`{}` maps a different main-repo directory into `{name}`, so cambium's \
                         recorded mapping for it is stale",
                        one.short
                    ));
                }
                repos.insert(name.clone());
                // A `name =` is only the whole story if the callee is a
                // repository rule. A *macro* with a `name` may declare a
                // family of repositories around it, so when the macro comes
                // from a main-repo `.bzl` the repositories that file can
                // define are charged as well. A macro from an external `.bzl`
                // is taken at the word of its `name`; that is the same
                // assumption the rest of this module makes about calls it
                // cannot see the body of.
                if let Some(p) = loaded_main_bzl(&one.callee, &hl, &bl) {
                    bzl_paths.insert(p);
                }
            }
            Key::Call(callee, _) => {
                // An unnamed call declares whatever the `.bzl` it came from
                // declares — if it came from a main-repo `.bzl` at all.
                match loaded_main_bzl(callee, &hl, &bl) {
                    Some(p) => {
                        bzl_paths.insert(p);
                    }
                    None => {
                        return WorkspaceChange::Global(format!(
                            "`{}` changed, and cambium cannot say which main-repo `.bzl` \
                             defines it",
                            one.short
                        ))
                    }
                }
            }
        }
    }
    WorkspaceChange::Moved { repos, bzl_paths }
}

/// The main-repo `.bzl` path a symbol was loaded from, preferring the head
/// side's `load()` map.
fn loaded_main_bzl(
    callee: &str,
    head: &BTreeMap<String, String>,
    base: &BTreeMap<String, String>,
) -> Option<String> {
    head.get(callee)
        .or_else(|| base.get(callee))
        .and_then(|l| literal_main_bzl_path(l))
}

/// `//dev:deps.bzl` -> `dev/deps.bzl`, for a label that spells the main
/// repository out as `//` or `@//`.
///
/// This is deliberately narrower than the mapped reading the index build uses
/// (`externals::inputs::main_bzl_path`): a `WORKSPACE` diff is two texts with
/// no repository mapping in hand, so `@sg//dev:deps.bzl` — the main
/// repository under its `workspace(name = "sg")` alias — is not recognised
/// here, and a changed call to a macro loaded that way stays global. That is
/// the sound side; following the alias would need the mapping to be right.
fn literal_main_bzl_path(label: &str) -> Option<String> {
    let l = Label::parse(label)?;
    if l.is_relative() || !matches!(l.repo(), Repo::Main | Repo::Apparent("")) {
        return None;
    }
    let name = l.explicit_name()?;
    name.ends_with(".bzl").then(|| l.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(c: WorkspaceChange) -> (BTreeSet<String>, BTreeSet<String>) {
        match c {
            WorkspaceChange::Moved { repos, bzl_paths } => (repos, bzl_paths),
            other => panic!("{other:?}"),
        }
    }

    fn global(c: WorkspaceChange) -> String {
        match c {
            WorkspaceChange::Global(g) => g,
            other => panic!("{other:?}"),
        }
    }

    fn registration(c: WorkspaceChange) -> String {
        match c {
            WorkspaceChange::Registration(g) => g,
            other => panic!("{other:?}"),
        }
    }

    const BASE: &str = "workspace(name = \"sg\")\n\
         load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \"http_archive\")\n\
         http_archive(\n    name = \"rules_oci\",\n    sha256 = \"aaa\",\n)\n\
         http_archive(\n    name = \"rules_go\",\n    sha256 = \"bbb\",\n)\n\
         load(\"//dev:deps.bzl\", \"deps\")\n\
         deps(setting = \"one\")\n";

    /// The sourcegraph shape: one `http_archive`'s arguments moved.
    #[test]
    fn a_changed_declaration_names_one_repository() {
        let head = BASE.replace("\"aaa\"", "\"zzz\"");
        let (repos, bzls) = moved(diff_workspace(BASE, &head));
        assert_eq!(repos, ["rules_oci".to_string()].into());
        assert!(bzls.is_empty());
    }

    #[test]
    fn an_added_declaration_names_it_too() {
        let head = format!("{BASE}http_archive(\n    name = \"extra\",\n)\n");
        let (repos, _) = moved(diff_workspace(BASE, &head));
        assert_eq!(repos, ["extra".to_string()].into());
        // And removing it again is the same statement moving the other way.
        let (repos, _) = moved(diff_workspace(&head, BASE));
        assert_eq!(repos, ["extra".to_string()].into());
    }

    /// A macro from a main-repo `.bzl`: the file is named, and the index says
    /// which repositories it can define.
    #[test]
    fn a_changed_macro_call_names_its_bzl() {
        let head = BASE.replace("setting = \"one\"", "setting = \"two\"");
        let (repos, bzls) = moved(diff_workspace(BASE, &head));
        assert!(repos.is_empty());
        assert_eq!(bzls, ["dev/deps.bzl".to_string()].into());
    }

    /// A keyword argument whose value is a symbol rather than a literal still
    /// has to be seen to move.
    #[test]
    fn a_non_literal_argument_that_moved_is_seen() {
        let base = format!("{BASE}other(name = \"r\", v = SYMBOL_A)\n");
        let head = format!("{BASE}other(name = \"r\", v = SYMBOL_B)\n");
        let (repos, _) = moved(diff_workspace(&base, &head));
        assert_eq!(repos, ["r".to_string()].into());
    }

    #[test]
    fn a_load_that_only_changed_its_symbols_is_nothing() {
        let head = BASE.replace(
            "\"//dev:deps.bzl\", \"deps\"",
            "\"//dev:deps.bzl\", \"deps\", \"more\"",
        );
        let (repos, bzls) = moved(diff_workspace(BASE, &head));
        assert!(repos.is_empty() && bzls.is_empty(), "{repos:?} {bzls:?}");
    }

    #[test]
    fn a_load_of_a_different_label_is_global() {
        let head = BASE.replace("//dev:deps.bzl", "//dev:other.bzl");
        assert!(global(diff_workspace(BASE, &head)).contains("only one side"));
    }

    #[test]
    fn a_macro_from_an_external_bzl_is_global() {
        let base = format!(
            "{BASE}load(\"@rules_oci//oci:deps.bzl\", \"oci_deps\")\noci_deps(a = \"1\")\n"
        );
        let head = base.replace("a = \"1\"", "a = \"2\"");
        assert!(global(diff_workspace(&base, &head)).contains("oci_deps"));
    }

    #[test]
    fn a_macro_with_no_load_at_all_is_global() {
        let base = format!("{BASE}mystery(a = \"1\")\n");
        let head = base.replace("a = \"1\"", "a = \"2\"");
        assert!(global(diff_workspace(&base, &head)).contains("mystery"));
    }

    #[test]
    fn toolchain_registration_is_global() {
        for call in [
            "register_toolchains(\"//x:y\")",
            "go_register_toolchains(version = \"V\")",
            "oci_register_toolchains(name = \"oci\", v = \"1\")",
        ] {
            let base = format!("{BASE}{call}\n");
            let head = base
                .replace("V", "W")
                .replace("\"1\"", "\"2\"")
                .replace("//x:y", "//x:z");
            assert!(
                registration(diff_workspace(&base, &head)).contains("toolchain"),
                "{call}"
            );
        }
    }

    #[test]
    fn the_workspace_name_and_bind_are_global() {
        let head = BASE.replace("name = \"sg\"", "name = \"sourcegraph\"");
        assert!(global(diff_workspace(BASE, &head)).contains("workspace"));
        let base = format!("{BASE}bind(name = \"b\", actual = \"//a:b\")\n");
        let head = base.replace("//a:b", "//a:c");
        assert!(global(diff_workspace(&base, &head)).contains("confined"));
    }

    #[test]
    fn a_local_repository_that_moved_its_path_is_global() {
        let base = format!("{BASE}local_repository(name = \"l\", path = \"a\")\n");
        let head = base.replace("path = \"a\"", "path = \"b\"");
        assert!(global(diff_workspace(&base, &head)).contains("stale"));
        // Its other arguments moving is an ordinary repository change.
        let head = base.replace(
            "name = \"l\", path = \"a\"",
            "name = \"l\", path = \"a\", x = \"1\"",
        );
        assert_eq!(
            moved(diff_workspace(&base, &head)).0,
            ["l".to_string()].into()
        );
    }

    #[test]
    fn an_assignment_or_a_loop_is_global() {
        for extra in [
            "VERSION = \"1\"\n",
            "[http_archive(name = n) for n in NAMES]\n",
            "if x:\n    http_archive(name = \"c\")\n",
        ] {
            let base = format!("{BASE}{extra}");
            let head = base
                .replace('1', "2")
                .replace("NAMES", "OTHERS")
                .replace("\"c\"", "\"d\"");
            assert!(
                global(diff_workspace(&base, &head)).contains("classify"),
                "{extra}"
            );
        }
    }

    #[test]
    fn reordering_two_declarations_is_global() {
        let head = "workspace(name = \"sg\")\n\
             load(\"@bazel_tools//tools/build_defs/repo:http.bzl\", \"http_archive\")\n\
             http_archive(\n    name = \"rules_go\",\n    sha256 = \"bbb\",\n)\n\
             http_archive(\n    name = \"rules_oci\",\n    sha256 = \"aaa\",\n)\n\
             load(\"//dev:deps.bzl\", \"deps\")\n\
             deps(setting = \"one\")\n";
        assert!(global(diff_workspace(BASE, head)).contains("reordered"));
    }

    #[test]
    fn an_unchanged_file_moves_nothing() {
        let (repos, bzls) = moved(diff_workspace(BASE, BASE));
        assert!(repos.is_empty() && bzls.is_empty());
    }
}
