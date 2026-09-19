//! Starlark, read into a syntax tree.
//!
//! Every reader in this crate that looks past a file's `load()`s — the
//! `MODULE.bazel` and `WORKSPACE` statement readers, the path-map scanner,
//! and the per-occurrence attribution — reads the same [`Module`]: a
//! tolerant parse of the whole file into [`Stmt`]s and [`Expr`]s, each
//! carrying its byte [`Span`]. They ask it for calls, definitions,
//! assignments, loads and string literals, and nothing else, which is why
//! the tree is complete for the grammar but plain in its representation.
//!
//! # Why an in-house parser
//!
//! The alternative was the `starlark_syntax` crate of starlark-rust. It was
//! not taken, for three reasons that each would have sufficed. Its parser
//! fails a file on the first error, and this crate needs the opposite: a
//! `MODULE.bazel` with one statement it does not understand must still
//! yield every fact from the statements around it, and record the one it
//! could not read in [`crate::externals::ModuleFacts::unparsed`] — see
//! [`parser`], whose failures are local to a statement. Its dependency
//! footprint (its own arena, `lalrpop` runtime, `dupe`, `starlark_map`,
//! `thiserror` and a dozen more) is out of proportion to the five node kinds
//! the consumers here read. And this crate's MSRV is 1.80, below what recent
//! releases of that crate compile on. The lexer already existed for the load
//! scan; the parser on top of it is six hundred lines.
//!
//! # What the tree covers
//!
//! The whole statement and expression grammar of Bazel's Starlark dialect:
//! `def`, `if`/`elif`/`else`, `for`, `load`, `return`, `pass`, `break`,
//! `continue`, assignment and augmented assignment; calls with keyword,
//! `*` and `**` arguments, attribute and subscript access, slices, lambdas,
//! conditionals, list and dict comprehensions, every operator at its
//! precedence, and string literals with escapes decoded and implicit
//! concatenation joined. Line continuations, bracketed multi-line
//! expressions, comments and blank lines are the lexer's business
//! ([`lexer`]).
//!
//! What it does not do: evaluate anything. A `name = "a" + b` is a `BinOp`,
//! and it is the consumer's job to say that it cannot read that as a name.
//!
//! # Tolerance, and what it must never cost
//!
//! A syntax error costs one statement: it becomes [`Stmt::Unparsed`] with
//! its span, the indented block under it included, and the parse resumes at
//! the next logical line. Inside a call's arguments the loss is one
//! argument, [`ExprKind::Unparsed`]. Every consumer treats an `Unparsed`
//! node as "something ran here that I could not read": the module reader
//! records it, the attribution pass makes the enclosing scope fall back to
//! its wide answer, and the string literals under it are still found —
//! [`Module::strings_in`] reads them off the tokens — so an unreadable
//! statement can widen an answer and never narrow one.

pub(crate) mod ast;
pub(crate) mod lexer;
mod parser;

pub(crate) use ast::{
    stmt_exprs, walk_stmts, Arg, ArgKind, Assign, Clause, Expr, ExprKind, ParamKind, Stmt,
};
pub(crate) use lexer::{unescape, Span};

use lexer::{Token, TokenKind};

/// A parsed file: its statements, and the tokens they were read from.
pub(crate) struct Module<'a> {
    pub(crate) stmts: Vec<Stmt<'a>>,
    text: &'a str,
    tokens: Vec<Token<'a>>,
}

impl<'a> Module<'a> {
    pub(crate) fn parse(text: &'a str) -> Self {
        let tokens = lexer::lex(text);
        let stmts = parser::Parser::new(text, &tokens).module();
        Module {
            stmts,
            text,
            tokens,
        }
    }

    /// The 1-based line a byte offset falls on. Only ever used to spell a
    /// piece of evidence for a human to read.
    pub(crate) fn line_of(&self, offset: usize) -> usize {
        self.text[..offset.min(self.text.len())]
            .bytes()
            .filter(|&b| b == b'\n')
            .count()
            + 1
    }

    /// The tokens whose spans start inside `span`.
    fn tokens_in(&self, span: &Span) -> &[Token<'a>] {
        let from = self.tokens.partition_point(|t| t.span.start < span.start);
        let to = self.tokens.partition_point(|t| t.span.start < span.end);
        &self.tokens[from..to.max(from)]
    }

    /// The string literals under a span, as `(literal span, value)`, in
    /// source order. This is how the literals of an `Unparsed` node are
    /// read: off the tokens, since there is no tree.
    pub(crate) fn strings_in(&self, span: &Span) -> Vec<(Span, String)> {
        self.tokens_in(span)
            .iter()
            .filter_map(|t| match &t.kind {
                TokenKind::Str(s) => Some((t.span.clone(), unescape(&self.text[s.clone()]))),
                _ => None,
            })
            .collect()
    }

    /// Every string literal inside an expression, in source order, the ones
    /// under an `Unparsed` node included.
    pub(crate) fn expr_strings(&self, e: &Expr<'a>) -> Vec<String> {
        let mut out = Vec::new();
        e.walk(&mut |x| match &x.kind {
            ExprKind::Str(s) => out.push(s.clone()),
            ExprKind::Unparsed => out.extend(self.strings_in(&x.span).into_iter().map(|(_, s)| s)),
            _ => {}
        });
        out
    }

    /// A call's arguments, flattened to the string literals in them.
    pub(crate) fn args(&self, args: &[Arg<'a>]) -> Args {
        let mut out = Args::default();
        for a in args {
            let vals = self.expr_strings(a.value());
            match a.keyword() {
                Some(k) => out.kw.push((k.to_string(), vals)),
                None => out.positional.extend(vals),
            }
        }
        out
    }

    /// A textual rendering of a span, token by token, at most `limit`
    /// tokens. Whitespace and comments do not appear, so two spellings of
    /// the same statement render the same. `limit` truncates it for a
    /// human-facing message; a rendering that is *compared* must not be
    /// truncated, or a change past the cut would be invisible.
    pub(crate) fn render(&self, span: &Span, limit: usize) -> String {
        let mut s = String::new();
        for t in self.tokens_in(span).iter().take(limit) {
            match &t.kind {
                TokenKind::Name(n) => s.push_str(n),
                TokenKind::Int | TokenKind::Float => s.push_str(&self.text[t.span.clone()]),
                TokenKind::Str(c) => {
                    s.push('"');
                    s.push_str(&self.text[c.clone()]);
                    s.push('"');
                }
                TokenKind::Op(o) => s.push_str(o),
                TokenKind::Error => s.push('?'),
                TokenKind::Newline | TokenKind::Indent | TokenKind::Dedent | TokenKind::Eof => {}
            }
        }
        s
    }

    /// [`Module::render`] with the whole span: what a diff compares.
    pub(crate) fn render_all(&self, span: &Span) -> String {
        self.render(span, usize::MAX)
    }

    /// [`Module::render`] cut short, for a message.
    pub(crate) fn render_short(&self, span: &Span) -> String {
        self.render(span, 24)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn shapes(text: &str) -> Vec<String> {
        Module::parse(text).stmts.iter().map(shape).collect()
    }

    fn shape(s: &Stmt<'_>) -> String {
        match s {
            Stmt::Load(l) => format!(
                "load({:?}, {:?})",
                l.label().unwrap_or(""),
                l.items().collect::<Vec<_>>()
            ),
            Stmt::Def(d) => format!(
                "def {}({}) {{{}}}",
                d.name,
                d.params
                    .iter()
                    .map(|p| match p.kind {
                        ParamKind::Normal if p.default.is_some() => format!("{}=", p.name),
                        ParamKind::Normal => p.name.to_string(),
                        ParamKind::Star => format!("*{}", p.name),
                        ParamKind::StarStar => format!("**{}", p.name),
                    })
                    .collect::<Vec<_>>()
                    .join(","),
                d.body.iter().map(shape).collect::<Vec<_>>().join("; ")
            ),
            Stmt::Assign(a) => format!("{} = {}", expr(&a.target), expr(&a.value)),
            Stmt::AugAssign(a) => format!("{} {} {}", expr(&a.target), a.op, expr(&a.value)),
            Stmt::Expr(e) => expr(e),
            Stmt::If(i) => i
                .branches
                .iter()
                .map(|(t, b)| {
                    format!(
                        "{} {{{}}}",
                        t.as_ref()
                            .map_or("else".to_string(), |t| format!("if {}", expr(t))),
                        b.iter().map(shape).collect::<Vec<_>>().join("; ")
                    )
                })
                .collect::<Vec<_>>()
                .join(" "),
            Stmt::For(f) => format!(
                "for {} in {} {{{}}}",
                expr(&f.target),
                expr(&f.iter),
                f.body.iter().map(shape).collect::<Vec<_>>().join("; ")
            ),
            Stmt::Return(r) => {
                format!("return {}", r.value.as_ref().map_or_else(String::new, expr))
            }
            Stmt::Pass(_) => "pass".into(),
            Stmt::Break(_) => "break".into(),
            Stmt::Continue(_) => "continue".into(),
            Stmt::Unparsed(s) => format!("?[{}..{}]", s.start, s.end),
        }
    }

    fn expr(e: &Expr<'_>) -> String {
        match &e.kind {
            ExprKind::Call { callee, args } => format!(
                "{}({})",
                expr(callee),
                args.iter()
                    .map(|a| match &a.kind {
                        ArgKind::Positional(e) => expr(e),
                        ArgKind::Keyword(k, e) => format!("{k}={}", expr(e)),
                        ArgKind::Star(e) => format!("*{}", expr(e)),
                        ArgKind::StarStar(e) => format!("**{}", expr(e)),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ExprKind::Name(n) => n.to_string(),
            ExprKind::Attr { object, name } => format!("{}.{name}", expr(object)),
            ExprKind::Index { object, index } => format!("{}[{}]", expr(object), expr(index)),
            ExprKind::Slice { object, parts } => format!(
                "{}[{}]",
                expr(object),
                parts
                    .iter()
                    .map(|p| p.as_ref().map_or_else(String::new, expr))
                    .collect::<Vec<_>>()
                    .join(":")
            ),
            ExprKind::Str(s) => format!("{s:?}"),
            ExprKind::Int => "INT".into(),
            ExprKind::Float => "FLOAT".into(),
            ExprKind::List(v) => format!("[{}]", v.iter().map(expr).collect::<Vec<_>>().join(", ")),
            ExprKind::Tuple(v) => {
                format!("({})", v.iter().map(expr).collect::<Vec<_>>().join(", "))
            }
            ExprKind::Dict(v) => format!(
                "{{{}}}",
                v.iter()
                    .map(|(k, v)| format!("{}: {}", expr(k), expr(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ExprKind::BinOp { op, lhs, rhs } => format!("({} {op} {})", expr(lhs), expr(rhs)),
            ExprKind::UnOp { op, operand } => format!("({op} {})", expr(operand)),
            ExprKind::Cond { then, test, orelse } => {
                format!("({} if {} else {})", expr(then), expr(test), expr(orelse))
            }
            ExprKind::Lambda { params, body } => format!(
                "(lambda {}: {})",
                params.iter().map(|p| p.name).collect::<Vec<_>>().join(","),
                expr(body)
            ),
            ExprKind::Comprehension { element, clauses } => format!(
                "[{} {}]",
                expr(element),
                clauses
                    .iter()
                    .map(|c| match c {
                        Clause::For { target, iter } =>
                            format!("for {} in {}", expr(target), expr(iter)),
                        Clause::If(e) => format!("if {}", expr(e)),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            ExprKind::Unparsed => "?".into(),
        }
    }

    #[test]
    fn statements_and_blocks() {
        let t = "load(\"//a:b.bzl\", \"x\", y = \"z\")\n\
                 def f(a, b = 1, *args, **kwargs):\n\
                 \x20   if a and not b == 2:\n\
                 \x20       return a.c[0]\n\
                 \x20   elif b: pass\n\
                 \x20   else:\n\
                 \x20       for i, j in pairs: x += 1; continue\n\
                 \x20   return\n\
                 X = f(1, *a, **k)\n";
        assert_eq!(
            shapes(t),
            [
                "load(\"//a:b.bzl\", [(\"x\", \"x\"), (\"y\", \"z\")])",
                "def f(a,b=,*args,**kwargs) {if (a and (not (b == INT))) {return a.c[INT]} \
                 if b {pass} else {for (i, j) in pairs {x += INT; continue}}; return }",
                "X = f(INT, *a, **k)",
            ]
        );
    }

    #[test]
    fn expressions_at_every_precedence() {
        let t =
            "x = -a ** 2 + b * c // d - (e or f and g) if h not in i else [j for j in k if j]\n\
                 y = {\"k\": lambda q, r = 1: q[1:2] + r, **z}\n\
                 w = \"a\" 'b' + c.d(e = 1).f[g][:h]\n";
        assert_eq!(
            shapes(t),
            [
                "x = ((((- (a ** INT)) + ((b * c) // d)) - (e or (f and g))) if (h not in i) \
                 else [j for j in k if j])",
                "?[81..124]",
                "w = (\"ab\" + c.d(e=INT).f[g][:h])",
            ]
        );
        // A dict with `**` inside is not Starlark, and only that statement
        // was lost; `strings_in` still finds its literal.
        let m = Module::parse(t);
        let Stmt::Unparsed(span) = &m.stmts[1] else {
            panic!()
        };
        assert_eq!(m.strings_in(span), vec![(86..89, "k".to_string())]);
    }

    #[test]
    fn an_error_costs_one_statement_and_its_block() {
        let t = "a = 1\nif x\n    b = 2\n    c = 3\nd = 4\ne = f(1 2, name = \"n\")\n";
        assert_eq!(
            shapes(t),
            [
                "a = INT",
                "?[6..30]",
                "d = INT",
                "e = f(INT, ?, name=\"n\")",
            ]
        );
        // An indent nothing opened is skipped as a block of its own.
        assert_eq!(shapes("    a = 1\nb = 2\n"), ["?[4..9]", "b = INT"]);
        // A bracket that never closes swallows the rest of the file, which
        // is still one statement.
        assert_eq!(shapes("x = f(1,\ny = 2\n"), ["?[0..14]"]);
    }

    #[test]
    fn comprehensions_conditionals_and_strings_in_arguments() {
        let t = "[m.artifact(group = g) for g, a in [c.split(\":\") for c in [\"a:b\"]]]\n\
                 {k: v for k, v in d.items()}\n\
                 f(x if y else z, r\"\\d\" \"e\")\n\
                 x = (1)\n\
                 t = 1,\n";
        assert_eq!(
            shapes(t),
            [
                "[m.artifact(group=g) for (g, a) in [c.split(\":\") for c in [\"a:b\"]]]",
                "[(k, v) for (k, v) in d.items()]",
                "f((x if y else z), \"de\")",
                "x = INT",
                "t = (INT)",
            ]
        );
    }

    #[test]
    fn spans_cover_the_statement_and_nothing_after_it() {
        let t = "def f():\n    x = 1\n\n# comment\ny = 2\n";
        let m = Module::parse(t);
        assert_eq!(m.stmts[0].span(), 0..18);
        assert_eq!(m.stmts[1].span(), 30..35);
        assert_eq!(m.render_all(&m.stmts[0].span()), "deff():x=1");
        assert_eq!(m.render(&m.stmts[0].span(), 2), "deff");
    }

    #[test]
    fn args_flatten_to_their_literals() {
        let m = Module::parse("f(\"a\", [\"b\", \"c\"], k = {\"d\": \"e\"}, *[\"g\"])\n");
        let Stmt::Expr(Expr {
            kind: ExprKind::Call { args, .. },
            ..
        }) = &m.stmts[0]
        else {
            panic!()
        };
        let a = m.args(args);
        assert_eq!(a.positional, ["a", "b", "c", "g"]);
        assert_eq!(a.kw, [("k".to_string(), vec!["d".to_string(), "e".into()])]);
        assert_eq!(a.render("f"), "f(a,b,c,g,k=d|e,)");
    }
}
