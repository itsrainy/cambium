//! The `MODULE.bazel` statement reader.
//!
//! bzlmod forbids control flow and function definitions in the module file,
//! so it has a fixed grammar this parser recognises statement by statement;
//! anything else lands in [`ModuleFacts::unparsed`] and makes the diff
//! escalate rather than guess.

use std::collections::BTreeMap;

use crate::starlark::{Arg, ArgKind, Expr, ExprKind, Module, Stmt};
use crate::vocabulary::{is_local_repo_rule, HARMLESS};

use super::module_facts::{Dep, ModuleFacts};
use super::path_maps::local_dir;
use super::pattern::SEP;

/// A top-level statement in one of the shapes `MODULE.bazel` is made of:
/// `f(...)`, `x = f(...)`, `x.tag(...)`, or a bare name.
struct Shape<'s, 'a> {
    /// `x` in `x = ...`.
    target: Option<&'a str>,
    /// `x` in `x.tag(...)`.
    receiver: Option<&'a str>,
    /// `f`, or `tag`.
    name: &'a str,
    /// The arguments, when the head is a call.
    args: Option<&'s [Arg<'a>]>,
}

fn shape<'s, 'a>(stmt: &'s Stmt<'a>) -> Option<Shape<'s, 'a>> {
    let (target, head) = match stmt {
        Stmt::Expr(e) => (None, e),
        Stmt::Assign(a) => (Some(a.target.as_name()?), &a.value),
        _ => return None,
    };
    let (callee, args) = match &head.kind {
        ExprKind::Call { callee, args } => (callee.as_ref(), Some(args.as_slice())),
        _ => (head, None),
    };
    let (receiver, name) = match &callee.kind {
        ExprKind::Name(n) => (None, *n),
        ExprKind::Attr { object, name } => (Some(object.as_name()?), *name),
        _ => return None,
    };
    Some(Shape {
        target,
        receiver,
        name,
        args,
    })
}

/// Reads a `MODULE.bazel`'s structure.
///
/// The parser recognises the fixed grammar `MODULE.bazel` actually has —
/// bzlmod forbids control flow and function definitions there — and records
/// anything else in [`ModuleFacts::unparsed`] so the engine can escalate
/// rather than silently under-approximate.
pub fn scan_module(text: &str) -> ModuleFacts {
    let m = Module::parse(text);
    let mut f = ModuleFacts::default();
    // Variable name -> the extension / repo-rule key it was bound to.
    let mut ext_vars: BTreeMap<String, String> = BTreeMap::new();
    let mut rule_vars: BTreeMap<String, String> = BTreeMap::new();

    for st in &m.stmts {
        let span = st.span();
        // A `load()` is an edge the load graph already has.
        if matches!(st, Stmt::Load(_)) {
            continue;
        }
        let Some(sh) = shape(st) else {
            // A bare string (a docstring) says nothing.
            if matches!(
                st,
                Stmt::Expr(Expr {
                    kind: ExprKind::Str(_),
                    ..
                })
            ) {
                continue;
            }
            // A list comprehension of tag calls — `[maven.artifact(...) for a
            // in ARTIFACTS]`, which `bazel/bazel` writes twice. The loop body
            // cannot be evaluated statically, but it does not have to be: what
            // the diff needs is a rendering that moves when the statement's
            // text moves, and the extension it belongs to. Control flow is
            // not read this way: an `if` around a tag call is a statement
            // this reader does not know.
            if matches!(st, Stmt::Expr(_) | Stmt::Assign(_)) {
                if let Some(key) = comprehension_target(st, &ext_vars) {
                    let e = f.extensions.entry(key).or_default();
                    e.tags.push(m.render_all(&span));
                    e.labels
                        .extend(m.strings_in(&span).into_iter().map(|(_, s)| s));
                    continue;
                }
            }
            f.unparsed.push(m.render_all(&span));
            continue;
        };
        let Some(args) = sh.args else {
            if !HARMLESS.contains(&sh.name) {
                f.unparsed.push(m.render_all(&span));
            }
            continue;
        };
        let args = m.args(args);
        let name = sh.name;

        if let Some(var) = sh.receiver {
            // A tag call on an extension proxy.
            let Some(key) = ext_vars.get(var) else {
                f.unparsed.push(m.render_all(&span));
                continue;
            };
            let e = f.extensions.entry(key.clone()).or_default();
            e.tags.push(args.render(name));
            e.labels.extend(args.strings().cloned());
            continue;
        }

        // `use_repo(proxy, ...)`: the first argument is an extension proxy,
        // which is a name rather than a literal and so never reaches `Args`.
        let proxy = sh.args.and_then(|a| a.first()).and_then(|a| match &a.kind {
            ArgKind::Positional(e) => e.as_name(),
            _ => None,
        });

        match (sh.target, name) {
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
                let Some(key) = proxy.and_then(|v| ext_vars.get(v)) else {
                    f.unparsed.push(m.render_all(&span));
                    continue;
                };
                let e = f.extensions.entry(key.clone()).or_default();
                for (k, v) in &args.kw {
                    e.repos.insert(k.clone());
                    e.repos.extend(v.iter().cloned());
                    if let Some(internal) = v.first() {
                        e.renames.insert(internal.clone(), k.clone());
                    }
                }
                e.repos.extend(args.positional.iter().cloned());
            }
            (None, "override_repo") | (None, "inject_repo") => {
                let Some(key) = proxy.and_then(|v| ext_vars.get(v)) else {
                    f.unparsed.push(m.render_all(&span));
                    continue;
                };
                let e = f.extensions.entry(key.clone()).or_default();
                e.overrides.push(args.render(name));
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
                let module = args.first("module_name").unwrap_or_default().to_string();
                f.overrides.insert(module.clone(), args.render(n));
                if n == "local_path_override" {
                    if let Some(d) = args.first("path").and_then(local_dir) {
                        f.local_paths.insert(module, d);
                    }
                }
            }
            (None, n) if HARMLESS.contains(&n) => {}
            (Some(v), n) if rule_vars.contains_key(n) => {
                // Assigning a repo-rule proxy to another name; follow it.
                let key = rule_vars[n].clone();
                rule_vars.insert(v.to_string(), key);
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
                    // `build_file = "@envoy//bazel/external:quiche.BUILD"`
                    // is the other half of the same idiom: the sources come
                    // from elsewhere and the BUILD file is a file of this
                    // tree. Read by key rather than swept out of
                    // `args.strings()` below, because *which* attribute a
                    // label came from is the whole fact — a patch and a
                    // build_file are both labels and only one of them is the
                    // package's BUILD file.
                    if let Some(bf) = args.first("build_file") {
                        f.build_files.insert(name.to_string(), bf.to_string());
                    } else if args.first("build_file_content").is_some() {
                        f.build_files.insert(name.to_string(), String::new());
                    }
                }
                r.labels.extend(args.strings().cloned());
            }
            _ => f.unparsed.push(m.render_all(&span)),
        }
    }
    f
}

/// The extension a bracketed statement's tag calls belong to, when every
/// `x.tag(` in it names the same known extension proxy.
fn comprehension_target(st: &Stmt<'_>, ext_vars: &BTreeMap<String, String>) -> Option<String> {
    let mut found: Option<String> = None;
    let mut agree = true;
    for e in crate::starlark::stmt_exprs(st) {
        e.walk(&mut |x| {
            let ExprKind::Call { callee, .. } = &x.kind else {
                return;
            };
            let ExprKind::Attr { object, .. } = &callee.kind else {
                return;
            };
            // Other calls in the same statement — `coord.split(":")` in
            // `bazel/bazel`'s nested comprehension — are not tag calls.
            let Some(key) = object.as_name().and_then(|v| ext_vars.get(v)) else {
                return;
            };
            if found.get_or_insert_with(|| key.clone()) != key {
                agree = false;
            }
        });
    }
    agree.then_some(found).flatten()
}
