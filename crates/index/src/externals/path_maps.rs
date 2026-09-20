//! Path mappings derived from Starlark: `local_repository(path = ...)` and
//! `repository_ctx.symlink(...)` calls whose both ends are literal.
//!
//! Being *wrong* here is a soundness bug — a mapping that misplaces a `.bzl`
//! would hide an edit to it — so every shape the reader is not certain of is
//! simply not recorded, and the repository keeps the wide answer.

use crate::label::Label;
use crate::loads::RepoMapping;
use crate::starlark::{stmt_exprs, walk_stmts, Arg, ArgKind, Expr, ExprKind, Module};
use crate::vocabulary::{is_local_repo_rule, PATHY_NAMES};

use super::pattern::PathMap;

/// A `path =` attribute as a main-repo directory prefix, when it is one.
///
/// A path outside the repository is not a main-repo input: `..` cannot be
/// matched against a diff, and neither can an absolute path.
pub(crate) fn local_dir(p: &str) -> Option<String> {
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
    /// the rule that makes the mapping true. The byte offset of the call in
    /// the file comes back alongside so the caller can ask for that
    /// attribution.
    pub symlinks: Vec<(usize, PathMap)>,
    /// `<rule>(name = "quiche", build_file = "@envoy//bazel/external:quiche.BUILD")`:
    /// the repository's sources are fetched and its BUILD file is a file of
    /// this tree. The call names its own repository, like `named` does, and
    /// the second half is the `build_file` label as written — empty when the
    /// call passed `build_file_content` instead, which is the same shape with
    /// no tree file behind it. The line the call starts on comes back for the
    /// evidence string.
    pub overlays: Vec<(String, String, usize)>,
    /// `<rule>(name = "tsl", path = "third_party/tsl")` under *any* callee,
    /// where the path is a repository boundary directory of this tree: a
    /// candidate claim that the repository is that directory, with the line
    /// the call starts on for the evidence string.
    ///
    /// A candidate, not a mapping. The callee is a repository rule this
    /// reader knows nothing about, so nothing here says the rule honoured its
    /// own `path` attribute rather than reinterpreting it; the claim becomes
    /// an exhaustive mapping only once Bazel's own package list for the
    /// repository matches the directory (C2, `RepoInputs::confirm_dir`).
    /// Restricting it to the attribute named `path` is why a `patches` entry
    /// or a `build_file` label cannot fire it, and restricting it to a
    /// boundary directory is why an ordinary subdirectory cannot either.
    pub boundary_dirs: Vec<(String, String, usize)>,
}

/// Reads the path mappings out of one workspace-closure `.bzl`.
pub fn scan_bzl_path_maps(text: &str, m: &RepoMapping, boundaries: &[String]) -> BzlPathMaps {
    scan_path_maps(&Module::parse(text), m, boundaries)
}

/// [`scan_bzl_path_maps`] on a file already parsed.
pub(crate) fn scan_path_maps(
    module: &Module<'_>,
    m: &RepoMapping,
    boundaries: &[String],
) -> BzlPathMaps {
    let mut out = BzlPathMaps::default();
    walk_stmts(&module.stmts, &mut |st| {
        for e in stmt_exprs(st) {
            e.walk(&mut |x| {
                let ExprKind::Call { callee, args } = &x.kind else {
                    return;
                };
                // An overlay declaration, under any callee: `build_file` is
                // a repository-rule attribute and nothing else has one. Read
                // by key, because which attribute a label came from is the
                // fact — a `patches` entry is a label too.
                if args
                    .iter()
                    .any(|a| matches!(a.keyword(), Some("build_file" | "build_file_content")))
                {
                    let a = module.args(args);
                    if let Some(n) = a.first("name") {
                        let bf = a.first("build_file").unwrap_or_default().to_string();
                        out.overlays
                            .push((n.to_string(), bf, module.line_of(x.span.start)));
                    }
                }
                // A call of any rule whose literal `path =` is a boundary
                // directory: a candidate `Dir` claim. Read before the callee
                // is looked at, because the whole point is that the callee is
                // a repository rule with a name this reader has never seen.
                if !boundaries.is_empty() {
                    let a = module.args(args);
                    if let (Some(n), Some(d)) =
                        (a.first("name"), a.first("path").and_then(local_dir))
                    {
                        let is_known = matches!(&callee.kind, ExprKind::Name(k) if is_local_repo_rule(k));
                        if !is_known && boundaries.iter().any(|b| trimmed(b) == trimmed(&d)) {
                            out.boundary_dirs
                                .push((n.to_string(), d, module.line_of(x.span.start)));
                        }
                    }
                }
                match &callee.kind {
                    ExprKind::Name(name) if is_local_repo_rule(name) => {
                        let args = module.args(args);
                        if let (Some(n), Some(d)) =
                            (args.first("name"), args.first("path").and_then(local_dir))
                        {
                            out.named.push((n.to_string(), PathMap::exhaustive_dir(d)));
                        }
                    }
                    // `x.symlink(...)`, on any receiver: only a
                    // `repository_ctx` has one.
                    ExprKind::Attr {
                        name: "symlink", ..
                    } => {
                        let (Some(src), Some(dst)) =
                            (pick(args, "target", 0), pick(args, "link_name", 1))
                        else {
                            return;
                        };
                        let (Some(src), Some(dst)) =
                            (only_literal(src, PATHY_NAMES), only_literal(dst, &[]))
                        else {
                            return;
                        };
                        if let Some(pm) = symlink_map(&src, &dst, m) {
                            out.symlinks.push((x.span.start, pm));
                        }
                    }
                    _ => {}
                }
            });
        }
    });
    out
}

/// A directory with any trailing slash removed, so a boundary (`third_party/tsl`)
/// and a `local_dir` (`third_party/tsl/`) compare equal.
pub(crate) fn trimmed(d: &str) -> &str {
    d.trim_end_matches('/')
}

/// The argument passed as keyword `kw`, or else the `pos`th positional one.
fn pick<'s, 'a>(args: &'s [Arg<'a>], kw: &str, pos: usize) -> Option<&'s Expr<'a>> {
    args.iter()
        .find(|a| a.keyword() == Some(kw))
        .or_else(|| {
            args.iter()
                .filter(|a| matches!(a.kind, ArgKind::Positional(_)))
                .nth(pos)
        })
        .map(Arg::value)
}

/// The single string literal of an argument that is *nothing but* that
/// literal, optionally wrapped in calls and attribute accesses whose names
/// are all in `allowed`.
fn only_literal(e: &Expr<'_>, allowed: &[&str]) -> Option<String> {
    match &e.kind {
        ExprKind::Str(s) => Some(s.clone()),
        ExprKind::Name(n) if allowed.contains(n) => None,
        ExprKind::Attr { object, name } if allowed.contains(name) => only_literal(object, allowed),
        ExprKind::Call { callee, args } => {
            let head = callee.head()?;
            if !allowed.contains(&head) {
                return None;
            }
            // The callee's own receiver chain must be allowed names too.
            if let ExprKind::Attr { object, .. } = &callee.kind {
                if !only_names(object, allowed) {
                    return None;
                }
            }
            let [one] = args.as_slice() else {
                return None;
            };
            only_literal(one.value(), allowed)
        }
        _ => None,
    }
}

/// Whether an expression is nothing but names from `allowed`, joined by `.`.
fn only_names(e: &Expr<'_>, allowed: &[&str]) -> bool {
    match &e.kind {
        ExprKind::Name(n) => allowed.contains(n),
        ExprKind::Attr { object, name } => allowed.contains(name) && only_names(object, allowed),
        _ => false,
    }
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
        main_repo_target(src, m)?.path()
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

/// `//pkg:name` in the main repository, with its target written out. `None`
/// for any other label: another repository, a relative `:name`, the `//pkg`
/// shorthand, or a package that starts with `/`.
fn main_repo_target<'a>(label: &'a str, m: &RepoMapping) -> Option<Label<'a>> {
    let l = Label::parse(label)?;
    if l.is_relative() || !l.is_main(m) {
        return None;
    }
    let name = l.explicit_name()?;
    (!name.is_empty() && !l.package().starts_with('/')).then_some(l)
}
