//! The `MODULE.bazel` statement reader.
//!
//! bzlmod forbids control flow and function definitions in the module file,
//! so it has a fixed grammar this parser recognises statement by statement;
//! anything else lands in [`ModuleFacts::unparsed`] and makes the diff
//! escalate rather than guess.

use std::collections::BTreeMap;

use crate::loads::{lex, unescape, Lexed, Tok};
use crate::starlark::{parse_args, render_all, render_limited, statements};

use super::module_facts::{Dep, ModuleFacts};
use super::path_maps::{is_local_repo_rule, local_dir};
use super::pattern::SEP;

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
        let t = st.toks();
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
                .any(|x| matches!(x.tok, Tok::Name(_) | Tok::Open(_) | Tok::Close))
            {
                f.unparsed.push(render_all(text, t));
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
        if !matches!(t.get(call_at).map(|x| x.tok), Some(Tok::Open(b'('))) {
            if !HARMLESS.contains(&name) {
                f.unparsed.push(render_all(text, t));
            }
            continue;
        }
        let args = parse_args(text, t, call_at);

        if let Some(method) = dotted {
            // A tag call on an extension proxy.
            let Some(key) = ext_vars.get(name) else {
                f.unparsed.push(render_all(text, t));
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
                    f.unparsed.push(render_all(text, t));
                    continue;
                };
                let Some(key) = ext_vars.get(var) else {
                    f.unparsed.push(render_all(text, t));
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
                // The same shape as `use_repo`: the first argument is an
                // extension proxy, which is a name rather than a literal and
                // so never reaches `Args`.
                let Some(Tok::Name(var)) = t.get(call_at + 1).map(|x| x.tok) else {
                    f.unparsed.push(render_all(text, t));
                    continue;
                };
                let Some(key) = ext_vars.get(var) else {
                    f.unparsed.push(render_all(text, t));
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
            _ => f.unparsed.push(render_all(text, t)),
        }
    }
    f
}

/// The extension a bracketed statement's tag calls belong to, when every
/// `x.tag(` in it names the same known extension proxy.
fn comprehension_target(t: &[Lexed<'_>], ext_vars: &BTreeMap<String, String>) -> Option<String> {
    let mut found: Option<String> = None;
    for w in t.windows(4) {
        let (Tok::Name(v), Tok::Punct(b'.'), Tok::Name(_), Tok::Open(b'(')) =
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
