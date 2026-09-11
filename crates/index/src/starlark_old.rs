//! Statement-level reading of Starlark, on top of [`crate::loads::lex`].
//!
//! The lexer knows names, string literals and bracket depth and nothing more;
//! this module adds the two shapes every reader in this crate asks about — a
//! file as a list of top-level statements, and a call as its arguments — plus
//! the renderings a diff compares. `MODULE.bazel` and `WORKSPACE` are read
//! with these, and the attribution pass shares the token-span walks. This is
//! the one place a real AST should grow from.

pub(crate) use crate::attribution::{arg_spans, top_level_spans};
use crate::loads::{unescape, Lexed, Tok};

/// A call's arguments, flattened enough for the questions asked of them.
#[derive(Debug, Default)]
pub(crate) struct Args {
    pub(crate) positional: Vec<String>,
    /// Keyword arguments in source order; the value is every string literal
    /// inside it, so a list or a dict argument yields all of its entries.
    pub(crate) kw: Vec<(String, Vec<String>)>,
}

impl Args {
    pub(crate) fn first(&self, key: &str) -> Option<&str> {
        self.kw
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.first())
            .map(String::as_str)
    }

    /// Every string literal in the call, keyword and positional alike.
    pub(crate) fn strings(&self) -> impl Iterator<Item = &String> {
        self.positional
            .iter()
            .chain(self.kw.iter().flat_map(|(_, v)| v.iter()))
    }

    /// A stable rendering, for comparing two revisions of the same call.
    pub(crate) fn render(&self, head: &str) -> String {
        let mut s = String::from(head);
        s.push('(');
        for p in &self.positional {
            s.push_str(p);
            s.push(',');
        }
        for (k, v) in &self.kw {
            s.push_str(k);
            s.push('=');
            s.push_str(&v.join("|"));
            s.push(',');
        }
        s.push(')');
        s
    }
}

/// A top-level statement, as a token slice plus the shape the parser found.
pub(crate) struct Stmt<'a, 't> {
    toks: &'t [Lexed<'a>],
}

impl<'a, 't> Stmt<'a, 't> {
    pub(crate) fn toks(&self) -> &'t [Lexed<'a>] {
        self.toks
    }
}

/// Splits a token stream into top-level statements: every token that starts a
/// line at bracket depth zero begins one.
pub(crate) fn statements<'a, 't>(toks: &'t [Lexed<'a>]) -> Vec<Stmt<'a, 't>> {
    top_level_spans(toks)
        .into_iter()
        .map(|r| Stmt { toks: &toks[r] })
        .collect()
}

/// Arguments of the call whose `(` is at `open`, which must be an open bracket.
/// The arguments of the call whose `(` is at `open`, with each argument
/// flattened to the string literals inside it.
///
/// Argument splitting is [`arg_spans`]'s job — it is the same walk, and having
/// two of them meant two chances to disagree about what starts a keyword
/// argument. This adds only the flattening: a list or dict argument yields
/// every literal in it, at any nesting depth, which is what the callers ask
/// of `Args`.
pub(crate) fn parse_args(text: &str, toks: &[Lexed<'_>], open: usize) -> Args {
    let mut out = Args::default();
    for (key, span) in arg_spans(toks, open) {
        let vals = toks[span].iter().filter_map(|t| match t.tok {
            Tok::Str(s, e) => Some(unescape(&text[s..e])),
            _ => None,
        });
        match key {
            Some(k) => out.kw.push((k, vals.collect())),
            None => out.positional.extend(vals),
        }
    }
    out
}

/// A textual rendering of a statement. `limit` truncates it for a
/// human-facing message; a rendering that is *compared* must not be
/// truncated, or a change past the cut would be invisible.
pub(crate) fn render_limited(text: &str, t: &[Lexed<'_>], limit: usize) -> String {
    let mut s = String::new();
    for x in t.iter().take(limit) {
        match x.tok {
            Tok::Name(n) => s.push_str(n),
            Tok::Str(a, b) => {
                s.push('"');
                s.push_str(&text[a..b]);
                s.push('"');
            }
            Tok::Open(b) => s.push(b as char),
            Tok::Close => s.push(')'),
            Tok::Punct(p) => s.push(p as char),
            Tok::Other => s.push('?'),
        }
    }
    s
}

pub(crate) fn render_stmt(text: &str, t: &[Lexed<'_>]) -> String {
    render_limited(text, t, 24)
}

/// The whole statement, token by token. Two statements with the same rendering
/// are the same call with the same arguments — symbols and punctuation
/// included, not only the string literals [`Args::render`] keeps.
pub(crate) fn render_all(text: &str, t: &[Lexed<'_>]) -> String {
    render_limited(text, t, usize::MAX)
}
