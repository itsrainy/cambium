//! A tolerant recursive-descent parser for Starlark.
//!
//! Tolerant means: a syntax error costs the statement it is in, never the
//! file. A statement that cannot be read — and the indented block under it,
//! if it had one — becomes [`Stmt::Unparsed`] with its span, and parsing
//! resumes at the next logical line. Inside a call's argument list the loss
//! is smaller still: an argument that cannot be read becomes
//! [`ExprKind::Unparsed`] and the call is kept, because a call with a
//! literal `name =` beside one odd argument is still an instantiation the
//! attribution pass wants to see.
//!
//! The grammar is the Starlark spec's, minus nothing a Bazel file can
//! contain: `def`, `if`/`elif`/`else`, `for`, `load`, `return`, `pass`,
//! `break`, `continue`, assignment and augmented assignment, and the whole
//! expression grammar — calls with `*`/`**`, attributes, subscripts and
//! slices, lambdas, conditionals, list and dict comprehensions, implicit
//! string concatenation. Operator precedence is the spec's; the shape of a
//! `BinOp` tree is not something any consumer here reads, but getting it
//! right costs nothing.

use super::ast::*;
use super::lexer::{unescape, Span, Token, TokenKind};

type PResult<T> = Result<T, ()>;

/// Names that can never begin an atom. Starlark's keywords plus Python's
/// reserved words, which Starlark also reserves.
const RESERVED: &[&str] = &[
    "and", "as", "assert", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "load", "nonlocal",
    "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];

const AUG_OPS: &[&str] = &[
    "+=", "-=", "*=", "/=", "//=", "%=", "&=", "|=", "^=", "<<=", ">>=", "**=",
];

pub(crate) struct Parser<'a, 't> {
    text: &'a str,
    toks: &'t [Token<'a>],
    pos: usize,
    /// The end of the last token consumed that is part of the source —
    /// not a `Newline`, `Indent`, `Dedent` or `Eof`, whose spans are empty
    /// or sit on the next line. Node spans end here.
    last_end: usize,
}

impl<'a, 't> Parser<'a, 't> {
    pub(crate) fn new(text: &'a str, toks: &'t [Token<'a>]) -> Self {
        Parser {
            text,
            toks,
            pos: 0,
            last_end: 0,
        }
    }

    // ------------------------------------------------------------ tokens

    fn peek(&self) -> &TokenKind<'a> {
        &self.toks[self.pos.min(self.toks.len() - 1)].kind
    }

    fn peek_at(&self, n: usize) -> &TokenKind<'a> {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].kind
    }

    fn start(&self) -> usize {
        self.toks[self.pos.min(self.toks.len() - 1)].span.start
    }

    fn advance(&mut self) {
        if self.pos >= self.toks.len() - 1 {
            return;
        }
        let t = &self.toks[self.pos];
        if !matches!(
            t.kind,
            TokenKind::Newline | TokenKind::Indent | TokenKind::Dedent | TokenKind::Eof
        ) {
            self.last_end = t.span.end;
        }
        self.pos += 1;
    }

    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), TokenKind::Op(o) if *o == op)
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), TokenKind::Name(n) if *n == kw)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> PResult<()> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(())
        }
    }

    fn expect_kw(&mut self, kw: &str) -> PResult<()> {
        if self.is_kw(kw) {
            self.advance();
            Ok(())
        } else {
            Err(())
        }
    }

    fn at_line_end(&self) -> bool {
        matches!(self.peek(), TokenKind::Newline | TokenKind::Eof) || self.is_op(";")
    }

    // -------------------------------------------------------- statements

    /// The whole file. Never fails: what cannot be read is `Unparsed`.
    pub(crate) fn module(&mut self) -> Vec<Stmt<'a>> {
        let mut out = Vec::new();
        loop {
            match self.peek() {
                TokenKind::Eof => break,
                TokenKind::Newline | TokenKind::Dedent => self.advance(),
                TokenKind::Indent => out.push(self.skip_block()),
                _ => self.statement_line(&mut out),
            }
        }
        out
    }

    /// The statements of an indented block, up to and including its
    /// `Dedent`.
    fn block(&mut self) -> Vec<Stmt<'a>> {
        let mut out = Vec::new();
        loop {
            match self.peek() {
                TokenKind::Eof => break,
                TokenKind::Dedent => {
                    self.advance();
                    break;
                }
                TokenKind::Newline => self.advance(),
                TokenKind::Indent => out.push(self.skip_block()),
                _ => self.statement_line(&mut out),
            }
        }
        out
    }

    /// One logical line — a compound statement with its suite, or simple
    /// statements separated by `;` — with recovery: on any error the whole
    /// line, and the block under it, is one `Unparsed`.
    fn statement_line(&mut self, out: &mut Vec<Stmt<'a>>) {
        let start = self.pos;
        let mut line = Vec::new();
        match self.statements_on_line(&mut line) {
            Ok(()) => out.extend(line),
            Err(()) => {
                self.pos = start;
                out.push(Stmt::Unparsed(self.skip_line()));
            }
        }
    }

    fn statements_on_line(&mut self, line: &mut Vec<Stmt<'a>>) -> PResult<()> {
        if matches!(self.peek(), TokenKind::Name("def" | "if" | "for")) {
            line.push(self.compound()?);
            return Ok(());
        }
        loop {
            line.push(self.simple()?);
            if self.eat_op(";") {
                if matches!(self.peek(), TokenKind::Newline) {
                    self.advance();
                    return Ok(());
                }
                if matches!(self.peek(), TokenKind::Eof) {
                    return Ok(());
                }
                continue;
            }
            match self.peek() {
                TokenKind::Newline => {
                    self.advance();
                    return Ok(());
                }
                TokenKind::Eof => return Ok(()),
                _ => return Err(()),
            }
        }
    }

    /// Consumes through the end of the current logical line, and the
    /// indented block that follows it if there is one. The span of what was
    /// skipped.
    fn skip_line(&mut self) -> Span {
        let start = self.start();
        let mut end = start;
        loop {
            match self.peek() {
                TokenKind::Eof => break,
                TokenKind::Newline => {
                    self.advance();
                    break;
                }
                _ => {
                    end = self.toks[self.pos].span.end.max(end);
                    self.advance();
                }
            }
        }
        if matches!(self.peek(), TokenKind::Indent) {
            if let Stmt::Unparsed(s) = self.skip_block() {
                end = s.end.max(end);
            }
        }
        start..end
    }

    /// Consumes a balanced `Indent` .. `Dedent` block as one `Unparsed`.
    fn skip_block(&mut self) -> Stmt<'a> {
        let start = self.start();
        let mut end = start;
        let mut depth = 0usize;
        loop {
            match self.peek() {
                TokenKind::Eof => break,
                TokenKind::Indent => depth += 1,
                TokenKind::Dedent => {
                    depth -= 1;
                    if depth == 0 {
                        self.advance();
                        break;
                    }
                }
                TokenKind::Newline => {}
                _ => end = self.toks[self.pos].span.end.max(end),
            }
            self.advance();
        }
        Stmt::Unparsed(start..end)
    }

    fn simple(&mut self) -> PResult<Stmt<'a>> {
        let start = self.start();
        match self.peek() {
            TokenKind::Name("load") if matches!(self.peek_at(1), TokenKind::Op("(")) => {
                self.advance();
                self.advance();
                let args = self.call_args()?;
                Ok(Stmt::Load(Load {
                    span: start..self.last_end,
                    args,
                }))
            }
            TokenKind::Name("return") => {
                self.advance();
                let value = if self.at_line_end() {
                    None
                } else {
                    Some(self.expr_list()?)
                };
                Ok(Stmt::Return(Return {
                    span: start..self.last_end,
                    value,
                }))
            }
            TokenKind::Name("pass") => {
                self.advance();
                Ok(Stmt::Pass(start..self.last_end))
            }
            TokenKind::Name("break") => {
                self.advance();
                Ok(Stmt::Break(start..self.last_end))
            }
            TokenKind::Name("continue") => {
                self.advance();
                Ok(Stmt::Continue(start..self.last_end))
            }
            _ => {
                let target = self.expr_list()?;
                if self.eat_op("=") {
                    let value = self.expr_list()?;
                    return Ok(Stmt::Assign(Assign {
                        span: start..self.last_end,
                        target,
                        value,
                    }));
                }
                if let TokenKind::Op(op) = self.peek() {
                    if let Some(op) = AUG_OPS.iter().find(|o| *o == op) {
                        self.advance();
                        let value = self.expr_list()?;
                        return Ok(Stmt::AugAssign(AugAssign {
                            span: start..self.last_end,
                            target,
                            op,
                            value,
                        }));
                    }
                }
                Ok(Stmt::Expr(target))
            }
        }
    }

    fn compound(&mut self) -> PResult<Stmt<'a>> {
        let start = self.start();
        match self.peek() {
            TokenKind::Name("def") => {
                self.advance();
                let TokenKind::Name(name) = *self.peek() else {
                    return Err(());
                };
                if RESERVED.contains(&name) {
                    return Err(());
                }
                self.advance();
                self.expect_op("(")?;
                let params = self.params(")")?;
                self.expect_op(")")?;
                self.expect_op(":")?;
                let body = self.suite()?;
                Ok(Stmt::Def(Def {
                    span: start..self.last_end,
                    name,
                    params,
                    body,
                }))
            }
            TokenKind::Name("if") => {
                self.advance();
                let test = self.expr()?;
                self.expect_op(":")?;
                let body = self.suite()?;
                let mut branches = vec![(Some(test), body)];
                loop {
                    if self.is_kw("elif") {
                        self.advance();
                        let test = self.expr()?;
                        self.expect_op(":")?;
                        let body = self.suite()?;
                        branches.push((Some(test), body));
                    } else if self.is_kw("else") {
                        self.advance();
                        self.expect_op(":")?;
                        let body = self.suite()?;
                        branches.push((None, body));
                        break;
                    } else {
                        break;
                    }
                }
                Ok(Stmt::If(If {
                    span: start..self.last_end,
                    branches,
                }))
            }
            TokenKind::Name("for") => {
                self.advance();
                let target = self.targets()?;
                self.expect_kw("in")?;
                let iter = self.expr_list()?;
                self.expect_op(":")?;
                let body = self.suite()?;
                Ok(Stmt::For(For {
                    span: start..self.last_end,
                    target,
                    iter,
                    body,
                }))
            }
            _ => Err(()),
        }
    }

    /// What follows a compound statement's `:` — an indented block, or
    /// simple statements on the same line.
    fn suite(&mut self) -> PResult<Vec<Stmt<'a>>> {
        if matches!(self.peek(), TokenKind::Newline) {
            self.advance();
            if !matches!(self.peek(), TokenKind::Indent) {
                return Err(());
            }
            self.advance();
            return Ok(self.block());
        }
        let mut line = Vec::new();
        if matches!(self.peek(), TokenKind::Name("def" | "if" | "for")) {
            return Err(());
        }
        self.statements_on_line(&mut line)?;
        Ok(line)
    }

    fn params(&mut self, close: &str) -> PResult<Vec<Param<'a>>> {
        let mut out = Vec::new();
        while !self.is_op(close) {
            let start = self.start();
            let kind = if self.eat_op("**") {
                ParamKind::StarStar
            } else if self.eat_op("*") {
                ParamKind::Star
            } else {
                ParamKind::Normal
            };
            let name = match *self.peek() {
                TokenKind::Name(n) if !RESERVED.contains(&n) => {
                    self.advance();
                    n
                }
                // A bare `*` ends the positional parameters.
                _ if kind == ParamKind::Star && (self.is_op(",") || self.is_op(close)) => "",
                _ => return Err(()),
            };
            let default = if kind == ParamKind::Normal && self.eat_op("=") {
                Some(self.expr()?)
            } else {
                None
            };
            out.push(Param {
                span: start..self.last_end,
                name,
                kind,
                default,
            });
            if !self.eat_op(",") {
                break;
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------- expressions

    fn expr(&mut self) -> PResult<Expr<'a>> {
        let start = self.start();
        if self.is_kw("lambda") {
            self.advance();
            let params = self.params(":")?;
            self.expect_op(":")?;
            let body = self.expr()?;
            return Ok(Expr {
                span: start..self.last_end,
                kind: ExprKind::Lambda {
                    params,
                    body: Box::new(body),
                },
            });
        }
        let then = self.binary(1)?;
        if self.is_kw("if") {
            self.advance();
            let test = self.binary(1)?;
            self.expect_kw("else")?;
            let orelse = self.expr()?;
            return Ok(Expr {
                span: then.span.start..self.last_end,
                kind: ExprKind::Cond {
                    then: Box::new(then),
                    test: Box::new(test),
                    orelse: Box::new(orelse),
                },
            });
        }
        Ok(then)
    }

    /// `a, b, c` without brackets: a tuple when there is a comma.
    fn expr_list(&mut self) -> PResult<Expr<'a>> {
        let first = self.expr()?;
        if !self.is_op(",") {
            return Ok(first);
        }
        let start = first.span.start;
        let mut items = vec![first];
        while self.eat_op(",") {
            if !self.starts_expr() {
                break;
            }
            items.push(self.expr()?);
        }
        Ok(Expr {
            span: start..self.last_end,
            kind: ExprKind::Tuple(items),
        })
    }

    fn starts_expr(&self) -> bool {
        match self.peek() {
            TokenKind::Name(n) => !RESERVED.contains(n) || matches!(*n, "not" | "lambda"),
            TokenKind::Int | TokenKind::Float | TokenKind::Str(_) => true,
            TokenKind::Op(o) => matches!(*o, "(" | "[" | "{" | "-" | "+" | "~"),
            _ => false,
        }
    }

    /// `for`-loop targets: primaries, not full expressions, because `in`
    /// ends them.
    fn targets(&mut self) -> PResult<Expr<'a>> {
        let first = self.primary()?;
        if !self.is_op(",") {
            return Ok(first);
        }
        let start = first.span.start;
        let mut items = vec![first];
        while self.eat_op(",") {
            if self.is_kw("in") {
                break;
            }
            items.push(self.primary()?);
        }
        Ok(Expr {
            span: start..self.last_end,
            kind: ExprKind::Tuple(items),
        })
    }

    /// The binary operator at the cursor: its spelling, precedence, and
    /// width in tokens (`not in` is two).
    fn binop(&self) -> Option<(&'static str, u8, usize)> {
        Some(match self.peek() {
            TokenKind::Name("or") => ("or", 1, 1),
            TokenKind::Name("and") => ("and", 2, 1),
            TokenKind::Name("not") if matches!(self.peek_at(1), TokenKind::Name("in")) => {
                ("not in", 4, 2)
            }
            TokenKind::Name("in") => ("in", 4, 1),
            TokenKind::Op(o) => {
                let prec = match *o {
                    "==" | "!=" | "<" | ">" | "<=" | ">=" => 4,
                    "|" => 5,
                    "^" => 6,
                    "&" => 7,
                    "<<" | ">>" => 8,
                    "+" | "-" => 9,
                    "*" | "/" | "//" | "%" => 10,
                    "**" => 12,
                    _ => return None,
                };
                (o, prec, 1)
            }
            _ => return None,
        })
    }

    fn binary(&mut self, min: u8) -> PResult<Expr<'a>> {
        let mut lhs = self.unary()?;
        while let Some((op, prec, width)) = self.binop() {
            if prec < min {
                break;
            }
            for _ in 0..width {
                self.advance();
            }
            // `**` is right-associative; everything else is left.
            let rhs = if op == "**" {
                self.binary(prec)?
            } else {
                self.binary(prec + 1)?
            };
            lhs = Expr {
                span: lhs.span.start..rhs.span.end,
                kind: ExprKind::BinOp {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
            };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> PResult<Expr<'a>> {
        let start = self.start();
        let op: &'static str = match self.peek() {
            TokenKind::Name("not") => "not",
            TokenKind::Op("-") => "-",
            TokenKind::Op("+") => "+",
            TokenKind::Op("~") => "~",
            _ => return self.primary(),
        };
        self.advance();
        // `not` binds looser than a comparison; the arithmetic signs bind
        // tighter than everything but `**`.
        let operand = self.binary(if op == "not" { 4 } else { 12 })?;
        Ok(Expr {
            span: start..self.last_end,
            kind: ExprKind::UnOp {
                op,
                operand: Box::new(operand),
            },
        })
    }

    fn primary(&mut self) -> PResult<Expr<'a>> {
        let mut e = self.atom()?;
        loop {
            if self.eat_op("(") {
                let args = self.call_args()?;
                e = Expr {
                    span: e.span.start..self.last_end,
                    kind: ExprKind::Call {
                        callee: Box::new(e),
                        args,
                    },
                };
            } else if self.eat_op(".") {
                let TokenKind::Name(name) = *self.peek() else {
                    return Err(());
                };
                self.advance();
                e = Expr {
                    span: e.span.start..self.last_end,
                    kind: ExprKind::Attr {
                        object: Box::new(e),
                        name,
                    },
                };
            } else if self.eat_op("[") {
                e = self.subscript(e)?;
            } else {
                return Ok(e);
            }
        }
    }

    /// After `[`: an index or a slice, through the `]`.
    fn subscript(&mut self, object: Expr<'a>) -> PResult<Expr<'a>> {
        let part = |p: &mut Self| -> PResult<Option<Expr<'a>>> {
            if p.is_op(":") || p.is_op("]") {
                Ok(None)
            } else {
                Ok(Some(p.expr()?))
            }
        };
        let first = part(self)?;
        if self.eat_op("]") {
            let Some(index) = first else {
                return Err(());
            };
            return Ok(Expr {
                span: object.span.start..self.last_end,
                kind: ExprKind::Index {
                    object: Box::new(object),
                    index: Box::new(index),
                },
            });
        }
        let mut parts = vec![first];
        while self.eat_op(":") {
            parts.push(part(self)?);
        }
        self.expect_op("]")?;
        Ok(Expr {
            span: object.span.start..self.last_end,
            kind: ExprKind::Slice {
                object: Box::new(object),
                parts,
            },
        })
    }

    /// After `(`: the arguments, through the `)`. An argument that cannot
    /// be read is skipped to the next `,` or the `)` and kept as
    /// `Unparsed`, so the call survives it.
    fn call_args(&mut self) -> PResult<Vec<Arg<'a>>> {
        let mut out = Vec::new();
        loop {
            if self.eat_op(")") {
                return Ok(out);
            }
            let start = self.pos;
            match self.arg() {
                Ok(a) => out.push(a),
                Err(()) => {
                    self.pos = start;
                    out.push(self.skip_arg()?);
                }
            }
            if !self.is_op(",") && !self.is_op(")") {
                out.push(self.skip_arg()?);
            }
            if self.eat_op(",") {
                continue;
            }
            self.expect_op(")")?;
            return Ok(out);
        }
    }

    fn arg(&mut self) -> PResult<Arg<'a>> {
        let start = self.start();
        let kind = if self.eat_op("**") {
            ArgKind::StarStar(self.expr()?)
        } else if self.eat_op("*") {
            ArgKind::Star(self.expr()?)
        } else if let (TokenKind::Name(n), TokenKind::Op("=")) = (self.peek(), self.peek_at(1)) {
            let n = *n;
            self.advance();
            self.advance();
            ArgKind::Keyword(n, self.expr()?)
        } else {
            ArgKind::Positional(self.expr()?)
        };
        Ok(Arg {
            span: start..self.last_end,
            kind,
        })
    }

    /// Consumes to the next `,` or closing bracket at this depth. Fails at
    /// end of file, which means the bracket never closed.
    fn skip_arg(&mut self) -> PResult<Arg<'a>> {
        let start = self.start();
        let mut end = start;
        let mut depth = 0usize;
        loop {
            match self.peek() {
                TokenKind::Eof => return Err(()),
                TokenKind::Op("(" | "[" | "{") => depth += 1,
                TokenKind::Op(")" | "]" | "}") => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                TokenKind::Op(",") if depth == 0 => break,
                _ => {}
            }
            end = self.toks[self.pos].span.end.max(end);
            self.advance();
        }
        Ok(Arg {
            span: start..end,
            kind: ArgKind::Positional(Expr {
                span: start..end,
                kind: ExprKind::Unparsed,
            }),
        })
    }

    fn atom(&mut self) -> PResult<Expr<'a>> {
        let start = self.start();
        let kind = match self.peek().clone() {
            TokenKind::Name(n) => {
                if RESERVED.contains(&n) {
                    return Err(());
                }
                self.advance();
                ExprKind::Name(n)
            }
            TokenKind::Int => {
                self.advance();
                ExprKind::Int
            }
            TokenKind::Float => {
                self.advance();
                ExprKind::Float
            }
            TokenKind::Str(s) => {
                let mut value = unescape(&self.text[s]);
                self.advance();
                // Implicit concatenation: adjacent literals are one string.
                while let TokenKind::Str(s) = self.peek().clone() {
                    value.push_str(&unescape(&self.text[s]));
                    self.advance();
                }
                ExprKind::Str(value)
            }
            TokenKind::Op("(") => {
                self.advance();
                if self.eat_op(")") {
                    ExprKind::Tuple(Vec::new())
                } else {
                    let first = self.expr()?;
                    if self.is_kw("for") {
                        let clauses = self.clauses()?;
                        self.expect_op(")")?;
                        ExprKind::Comprehension {
                            element: Box::new(first),
                            clauses,
                        }
                    } else if self.is_op(",") {
                        let mut items = vec![first];
                        while self.eat_op(",") {
                            if self.is_op(")") {
                                break;
                            }
                            items.push(self.expr()?);
                        }
                        self.expect_op(")")?;
                        ExprKind::Tuple(items)
                    } else {
                        // A parenthesised expression is that expression,
                        // span and all.
                        self.expect_op(")")?;
                        return Ok(first);
                    }
                }
            }
            TokenKind::Op("[") => {
                self.advance();
                if self.eat_op("]") {
                    ExprKind::List(Vec::new())
                } else {
                    let first = self.expr()?;
                    if self.is_kw("for") {
                        let clauses = self.clauses()?;
                        self.expect_op("]")?;
                        ExprKind::Comprehension {
                            element: Box::new(first),
                            clauses,
                        }
                    } else {
                        let mut items = vec![first];
                        while self.eat_op(",") {
                            if self.is_op("]") {
                                break;
                            }
                            items.push(self.expr()?);
                        }
                        self.expect_op("]")?;
                        ExprKind::List(items)
                    }
                }
            }
            TokenKind::Op("{") => {
                self.advance();
                if self.eat_op("}") {
                    ExprKind::Dict(Vec::new())
                } else {
                    let key = self.expr()?;
                    if self.eat_op(":") {
                        let value = self.expr()?;
                        if self.is_kw("for") {
                            let clauses = self.clauses()?;
                            self.expect_op("}")?;
                            let span = key.span.start..value.span.end;
                            ExprKind::Comprehension {
                                element: Box::new(Expr {
                                    span,
                                    kind: ExprKind::Tuple(vec![key, value]),
                                }),
                                clauses,
                            }
                        } else {
                            let mut items = vec![(key, value)];
                            while self.eat_op(",") {
                                if self.is_op("}") {
                                    break;
                                }
                                let k = self.expr()?;
                                self.expect_op(":")?;
                                let v = self.expr()?;
                                items.push((k, v));
                            }
                            self.expect_op("}")?;
                            ExprKind::Dict(items)
                        }
                    } else {
                        // A set display is not Starlark; read as a list so
                        // that its literals are still seen.
                        let mut items = vec![key];
                        while self.eat_op(",") {
                            if self.is_op("}") {
                                break;
                            }
                            items.push(self.expr()?);
                        }
                        self.expect_op("}")?;
                        ExprKind::List(items)
                    }
                }
            }
            _ => return Err(()),
        };
        Ok(Expr {
            span: start..self.last_end,
            kind,
        })
    }

    fn clauses(&mut self) -> PResult<Vec<Clause<'a>>> {
        let mut out = Vec::new();
        loop {
            if self.is_kw("for") {
                self.advance();
                let target = self.targets()?;
                self.expect_kw("in")?;
                let iter = self.binary(1)?;
                out.push(Clause::For { target, iter });
            } else if self.is_kw("if") {
                self.advance();
                out.push(Clause::If(self.binary(1)?));
            } else {
                return Ok(out);
            }
        }
    }
}
