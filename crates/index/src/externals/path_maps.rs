//! Path mappings derived from Starlark: `local_repository(path = ...)` and
//! `repository_ctx.symlink(...)` calls whose both ends are literal.
//!
//! Being *wrong* here is a soundness bug — a mapping that misplaces a `.bzl`
//! would hide an edit to it — so every shape the reader is not certain of is
//! simply not recorded, and the repository keeps the wide answer.

use crate::label::Label;
use crate::loads::{lex, unescape, Lexed, RepoMapping, Tok};
use crate::starlark_old::{arg_spans, parse_args};
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
    /// the rule that makes the mapping true. The token index of the call's
    /// `(` comes back alongside so the caller can ask for that attribution.
    pub symlinks: Vec<(usize, PathMap)>,
}

/// Reads the path mappings out of one workspace-closure `.bzl`.
pub fn scan_bzl_path_maps(text: &str, m: &RepoMapping) -> BzlPathMaps {
    let toks = lex(text);
    let mut out = BzlPathMaps::default();
    for i in 0..toks.len() {
        let Tok::Name(name) = toks[i].tok else {
            continue;
        };
        if !matches!(toks.get(i + 1).map(|t| t.tok), Some(Tok::Open(b'('))) {
            continue;
        }
        if is_local_repo_rule(name) {
            let args = parse_args(text, &toks, i + 1);
            if let (Some(n), Some(d)) = (args.first("name"), args.first("path").and_then(local_dir))
            {
                out.named.push((n.to_string(), PathMap::exhaustive_dir(d)));
            }
            continue;
        }
        // `x.symlink(...)`, on any receiver: only a `repository_ctx` has one.
        if name != "symlink"
            || !matches!(
                toks.get(i.wrapping_sub(1)).map(|t| t.tok),
                Some(Tok::Punct(b'.'))
            )
        {
            continue;
        }
        let spans = arg_spans(&toks, i + 1);
        let pick = |kw: &str, pos: usize| -> Option<std::ops::Range<usize>> {
            spans
                .iter()
                .find(|(k, _)| k.as_deref() == Some(kw))
                .or_else(|| spans.iter().filter(|(k, _)| k.is_none()).nth(pos))
                .map(|(_, r)| r.clone())
        };
        let (Some(src), Some(dst)) = (pick("target", 0), pick("link_name", 1)) else {
            continue;
        };
        let (Some(src), Some(dst)) = (
            only_literal(text, &toks, src, PATHY_NAMES),
            only_literal(text, &toks, dst, &[]),
        ) else {
            continue;
        };
        if let Some(pm) = symlink_map(&src, &dst, m) {
            out.symlinks.push((i + 1, pm));
        }
    }
    out
}

/// The single string literal of an argument that is *nothing but* that
/// literal, optionally wrapped in calls whose names are all in `allowed`.
fn only_literal(
    text: &str,
    toks: &[Lexed<'_>],
    span: std::ops::Range<usize>,
    allowed: &[&str],
) -> Option<String> {
    let mut lit = None;
    for t in &toks[span] {
        match t.tok {
            Tok::Str(a, b) => {
                if lit.is_some() {
                    return None;
                }
                lit = Some(unescape(&text[a..b]));
            }
            Tok::Name(n) if allowed.contains(&n) => {}
            Tok::Open(_) | Tok::Close => {}
            Tok::Punct(b'.') => {}
            _ => return None,
        }
    }
    lit
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
