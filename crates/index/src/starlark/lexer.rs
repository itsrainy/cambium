//! Tokens with byte spans and block structure, for [`super::parser`].
//!
//! [`crate::loads::lex`] answers one question — where are the `load()` calls
//! — and is kept small and fast for that, because it runs on every BUILD file
//! of every package. The parser needs more from its tokens: a byte span on
//! every one, so that a node can say where it is; the operators as units, so
//! that `==` is not two `=`; and the line structure Starlark's grammar is
//! written in, which is what `Newline`, `Indent` and `Dedent` carry. The two
//! lexers share the string scanner, so they agree about where a literal
//! ends.
//!
//! Errors are tokens: an unterminated string or a stray byte becomes
//! [`TokenKind::Error`], and the parser decides how much of the file it
//! poisons (one statement, never more).

use std::ops::Range;

/// A byte range of the source text.
pub(crate) type Span = Range<usize>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TokenKind<'a> {
    /// An identifier or a keyword; the parser tells them apart.
    Name(&'a str),
    Int,
    Float,
    /// A string literal; the span is the *contents*, without the quotes, and
    /// [`unescape`] decodes it.
    Str(Span),
    /// An operator or a punctuation mark, brackets included.
    Op(&'static str),
    /// The end of a logical line at bracket depth zero.
    Newline,
    Indent,
    Dedent,
    /// A byte the lexer could not read: an unterminated string, a lone `\`,
    /// a character outside the language.
    Error,
    Eof,
}

#[derive(Clone, Debug)]
pub(crate) struct Token<'a> {
    pub(crate) kind: TokenKind<'a>,
    pub(crate) span: Span,
}

/// Longest first, so that the greedy match below is right.
const OPS: &[&str] = &[
    "**=", "//=", ">>=", "<<=", "!=", "==", "<=", ">=", "+=", "-=", "*=", "/=", "%=", "&=", "|=",
    "^=", "**", "//", "<<", ">>", "+", "-", "*", "/", "%", "<", ">", "=", "&", "|", "^", "~", ".",
    ",", ":", ";", "(", ")", "[", "]", "{", "}",
];

/// Tokenises a whole file. The result ends in `Eof`, after a `Newline` for
/// the last logical line and a `Dedent` for every block still open.
pub(crate) fn lex(text: &str) -> Vec<Token<'_>> {
    let b = text.as_bytes();
    let mut out: Vec<Token<'_>> = Vec::with_capacity(text.len() / 4);
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut indents: Vec<usize> = vec![0];
    let mut line_start = true;
    while i < b.len() {
        if line_start && depth == 0 {
            // Measure the indentation, and skip the line entirely when it is
            // blank or a comment: neither one is a logical line.
            let mut j = i;
            let mut col = 0usize;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                col += if b[j] == b'\t' { 8 - col % 8 } else { 1 };
                j += 1;
            }
            if j >= b.len() {
                break;
            }
            if b[j] == b'\n' || b[j] == b'\r' || b[j] == b'#' {
                while j < b.len() && b[j] != b'\n' {
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            line_start = false;
            i = j;
            let top = *indents.last().unwrap_or(&0);
            if col > top {
                indents.push(col);
                out.push(Token {
                    kind: TokenKind::Indent,
                    span: i..i,
                });
            } else if col < top {
                while indents.len() > 1 && *indents.last().unwrap_or(&0) > col {
                    indents.pop();
                    out.push(Token {
                        kind: TokenKind::Dedent,
                        span: i..i,
                    });
                }
                // A dedent to a column no enclosing block opened at. Python
                // rejects it; here it is a new block at that column, which
                // the parser reads as an unexpected indent and skips.
                if *indents.last().unwrap_or(&0) < col {
                    indents.push(col);
                    out.push(Token {
                        kind: TokenKind::Indent,
                        span: i..i,
                    });
                }
            }
            continue;
        }
        let c = b[i];
        match c {
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'\n' => {
                if depth == 0 {
                    out.push(Token {
                        kind: TokenKind::Newline,
                        span: i..i + 1,
                    });
                    line_start = true;
                }
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'\\' => {
                // A backslash joins the next line onto this one.
                let mut j = i + 1;
                if j < b.len() && b[j] == b'\r' {
                    j += 1;
                }
                if j < b.len() && b[j] == b'\n' {
                    i = j + 1;
                } else {
                    out.push(Token {
                        kind: TokenKind::Error,
                        span: i..i + 1,
                    });
                    i += 1;
                }
            }
            b'"' | b'\'' => {
                let (span, next) = lex_string(b, i, false);
                out.push(string_token(span, i, next));
                i = next;
            }
            _ if c.is_ascii_digit() => {
                let (kind, next) = lex_number(b, i);
                out.push(Token {
                    kind,
                    span: i..next,
                });
                i = next;
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
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
                    out.push(string_token(span, start, next));
                    i = next;
                } else {
                    out.push(Token {
                        kind: TokenKind::Name(&text[start..i]),
                        span: start..i,
                    });
                }
            }
            _ => {
                let rest = &b[i..];
                match OPS.iter().find(|op| rest.starts_with(op.as_bytes())) {
                    Some(op) => {
                        match *op {
                            "(" | "[" | "{" => depth += 1,
                            ")" | "]" | "}" => depth = depth.saturating_sub(1),
                            _ => {}
                        }
                        out.push(Token {
                            kind: TokenKind::Op(op),
                            span: i..i + op.len(),
                        });
                        i += op.len();
                    }
                    None => {
                        // One whole character, so the span stays on a
                        // boundary of the `str`.
                        let n = text[i..].chars().next().map_or(1, char::len_utf8);
                        out.push(Token {
                            kind: TokenKind::Error,
                            span: i..i + n,
                        });
                        i += n;
                    }
                }
            }
        }
    }
    let end = b.len();
    if !line_start {
        out.push(Token {
            kind: TokenKind::Newline,
            span: end..end,
        });
    }
    while indents.len() > 1 {
        indents.pop();
        out.push(Token {
            kind: TokenKind::Dedent,
            span: end..end,
        });
    }
    out.push(Token {
        kind: TokenKind::Eof,
        span: end..end,
    });
    out
}

fn string_token(span: Option<(usize, usize)>, start: usize, next: usize) -> Token<'static> {
    match span {
        Some((s, e)) => Token {
            kind: TokenKind::Str(s..e),
            span: start..next,
        },
        None => Token {
            kind: TokenKind::Error,
            span: start..next,
        },
    }
}

fn lex_number(b: &[u8], start: usize) -> (TokenKind<'static>, usize) {
    let mut i = start;
    if b[i] == b'0'
        && i + 1 < b.len()
        && matches!(b[i + 1], b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
    {
        i += 2;
        while i < b.len() && b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        return (TokenKind::Int, i);
    }
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut float = false;
    if i < b.len() && b[i] == b'.' {
        float = true;
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            float = true;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    (
        if float {
            TokenKind::Float
        } else {
            TokenKind::Int
        },
        i,
    )
}

pub(crate) fn is_string_prefix(s: &[u8]) -> bool {
    s.len() <= 2 && s.iter().all(|c| matches!(c, b'r' | b'R' | b'b' | b'B'))
}

/// Consumes a string literal starting at `i` (a quote). Returns the span of
/// the *contents* and the index just past the literal.
pub(crate) fn lex_string(b: &[u8], i: usize, raw: bool) -> (Option<(usize, usize)>, usize) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<String> {
        lex(text)
            .iter()
            .map(|t| match &t.kind {
                TokenKind::Name(n) => n.to_string(),
                TokenKind::Int => "INT".into(),
                TokenKind::Float => "FLOAT".into(),
                TokenKind::Str(s) => format!("{:?}", &text[s.clone()]),
                TokenKind::Op(o) => o.to_string(),
                TokenKind::Newline => "NL".into(),
                TokenKind::Indent => "IN".into(),
                TokenKind::Dedent => "OUT".into(),
                TokenKind::Error => "ERR".into(),
                TokenKind::Eof => "EOF".into(),
            })
            .collect()
    }

    #[test]
    fn blocks_operators_and_literals() {
        let t = "def f(x):\n    if x == 1:\n        return r\"a\" + 2.5\n\n# c\ny = f(**k)\n";
        assert_eq!(
            kinds(t),
            [
                "def", "f", "(", "x", ")", ":", "NL", "IN", "if", "x", "==", "INT", ":", "NL",
                "IN", "return", "\"a\"", "+", "FLOAT", "NL", "OUT", "OUT", "y", "=", "f", "(",
                "**", "k", ")", "NL", "EOF"
            ]
        );
    }

    #[test]
    fn brackets_and_backslashes_join_lines() {
        let t = "f(\n  a,\n  # c\n  b)\nx = 1 + \\\n    2\n";
        assert_eq!(
            kinds(t),
            ["f", "(", "a", ",", "b", ")", "NL", "x", "=", "INT", "+", "INT", "NL", "EOF"]
        );
    }

    #[test]
    fn errors_are_tokens_and_the_file_still_ends() {
        assert_eq!(
            kinds("x = 'abc\ny = 1\n"),
            ["x", "=", "ERR", "NL", "y", "=", "INT", "NL", "EOF"]
        );
        assert_eq!(kinds("a $ b"), ["a", "ERR", "b", "NL", "EOF"]);
        assert_eq!(kinds("a \\ b\n"), ["a", "ERR", "b", "NL", "EOF"]);
    }

    #[test]
    fn a_dedent_to_no_open_level_is_a_new_block() {
        let t = "if a:\n        b\n    c\nd\n";
        assert_eq!(
            kinds(t),
            [
                "if", "a", ":", "NL", "IN", "b", "NL", "OUT", "IN", "c", "NL", "OUT", "d", "NL",
                "EOF"
            ]
        );
    }

    #[test]
    fn a_file_without_a_final_newline_gets_one() {
        assert_eq!(kinds("x = 1"), ["x", "=", "INT", "NL", "EOF"]);
        assert_eq!(kinds(""), ["EOF"]);
        assert_eq!(kinds("# only\n\n"), ["EOF"]);
    }
}
