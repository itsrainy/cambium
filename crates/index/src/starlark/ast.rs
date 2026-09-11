//! The syntax tree [`super::parser`] builds. Every node carries the byte
//! span of the source it was read from, which is how a consumer names an
//! occurrence — a string literal, a call — to the attribution pass.
//!
//! Names borrow from the source text; string literal values are owned, since
//! escapes are decoded and adjacent literals are joined.

use super::lexer::Span;

/// A statement. `Unparsed` is the parser's local failure: the statement (and
/// the indented block under it, if it had one) could not be read, and its
/// span is all that is known about it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Stmt<'a> {
    Load(Load<'a>),
    Def(Def<'a>),
    Assign(Assign<'a>),
    AugAssign(AugAssign<'a>),
    Expr(Expr<'a>),
    If(If<'a>),
    For(For<'a>),
    Return(Return<'a>),
    Pass(Span),
    Break(Span),
    Continue(Span),
    Unparsed(Span),
}

impl Stmt<'_> {
    pub(crate) fn span(&self) -> Span {
        match self {
            Stmt::Load(s) => s.span.clone(),
            Stmt::Def(s) => s.span.clone(),
            Stmt::Assign(s) => s.span.clone(),
            Stmt::AugAssign(s) => s.span.clone(),
            Stmt::Expr(e) => e.span.clone(),
            Stmt::If(s) => s.span.clone(),
            Stmt::For(s) => s.span.clone(),
            Stmt::Return(s) => s.span.clone(),
            Stmt::Pass(s) | Stmt::Break(s) | Stmt::Continue(s) | Stmt::Unparsed(s) => s.clone(),
        }
    }
}

/// `load("//pkg:file.bzl", "sym", alias = "sym")`, with its arguments kept
/// as written; [`Load::label`] and [`Load::items`] read the usual shape.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Load<'a> {
    pub(crate) span: Span,
    pub(crate) args: Vec<Arg<'a>>,
}

impl<'a> Load<'a> {
    /// The label, when the first argument is a literal (it must be, for
    /// Bazel to accept the statement).
    pub(crate) fn label(&self) -> Option<&str> {
        match self.args.first() {
            Some(Arg {
                kind: ArgKind::Positional(e),
                ..
            }) => e.as_str(),
            _ => None,
        }
    }

    /// `(alias, original name)` for every symbol imported, in order; a
    /// positional `"sym"` is its own alias.
    pub(crate) fn items(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.args.iter().skip(1).filter_map(|a| match &a.kind {
            ArgKind::Positional(e) => e.as_str().map(|s| (s, s)),
            ArgKind::Keyword(k, e) => e.as_str().map(|s| (*k, s)),
            _ => None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Def<'a> {
    pub(crate) span: Span,
    pub(crate) name: &'a str,
    pub(crate) params: Vec<Param<'a>>,
    pub(crate) body: Vec<Stmt<'a>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Param<'a> {
    pub(crate) span: Span,
    pub(crate) name: &'a str,
    pub(crate) kind: ParamKind,
    pub(crate) default: Option<Expr<'a>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ParamKind {
    Normal,
    /// `*args`, or the bare `*` that ends the positional parameters (then
    /// the name is empty).
    Star,
    /// `**kwargs`.
    StarStar,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Assign<'a> {
    pub(crate) span: Span,
    pub(crate) target: Expr<'a>,
    pub(crate) value: Expr<'a>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AugAssign<'a> {
    pub(crate) span: Span,
    pub(crate) target: Expr<'a>,
    pub(crate) op: &'static str,
    pub(crate) value: Expr<'a>,
}

/// `if` / `elif` / `else`: one branch per arm, the `else` arm with no test.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct If<'a> {
    pub(crate) span: Span,
    pub(crate) branches: Vec<(Option<Expr<'a>>, Vec<Stmt<'a>>)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct For<'a> {
    pub(crate) span: Span,
    pub(crate) target: Expr<'a>,
    pub(crate) iter: Expr<'a>,
    pub(crate) body: Vec<Stmt<'a>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Return<'a> {
    pub(crate) span: Span,
    pub(crate) value: Option<Expr<'a>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Expr<'a> {
    pub(crate) span: Span,
    pub(crate) kind: ExprKind<'a>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ExprKind<'a> {
    Call {
        callee: Box<Expr<'a>>,
        args: Vec<Arg<'a>>,
    },
    Name(&'a str),
    Attr {
        object: Box<Expr<'a>>,
        name: &'a str,
    },
    Index {
        object: Box<Expr<'a>>,
        index: Box<Expr<'a>>,
    },
    /// `a[lo:hi:step]`, each part optional.
    Slice {
        object: Box<Expr<'a>>,
        parts: Vec<Option<Expr<'a>>>,
    },
    /// A string literal, escapes decoded, adjacent literals joined
    /// (`"a" "b"` is the one string `ab`).
    Str(String),
    Int,
    Float,
    List(Vec<Expr<'a>>),
    Dict(Vec<(Expr<'a>, Expr<'a>)>),
    Tuple(Vec<Expr<'a>>),
    BinOp {
        op: &'static str,
        lhs: Box<Expr<'a>>,
        rhs: Box<Expr<'a>>,
    },
    UnOp {
        op: &'static str,
        operand: Box<Expr<'a>>,
    },
    /// `then if test else orelse`.
    Cond {
        then: Box<Expr<'a>>,
        test: Box<Expr<'a>>,
        orelse: Box<Expr<'a>>,
    },
    Lambda {
        params: Vec<Param<'a>>,
        body: Box<Expr<'a>>,
    },
    /// A list or dict comprehension; a dict's element is a `Tuple` of key
    /// and value.
    Comprehension {
        element: Box<Expr<'a>>,
        clauses: Vec<Clause<'a>>,
    },
    /// An expression the parser could not read, recovered at the next `,`
    /// or closing bracket of the argument list it sat in.
    Unparsed,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Clause<'a> {
    For { target: Expr<'a>, iter: Expr<'a> },
    If(Expr<'a>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Arg<'a> {
    pub(crate) span: Span,
    pub(crate) kind: ArgKind<'a>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ArgKind<'a> {
    Positional(Expr<'a>),
    Keyword(&'a str, Expr<'a>),
    Star(Expr<'a>),
    StarStar(Expr<'a>),
}

impl<'a> Arg<'a> {
    pub(crate) fn value(&self) -> &Expr<'a> {
        match &self.kind {
            ArgKind::Positional(e)
            | ArgKind::Keyword(_, e)
            | ArgKind::Star(e)
            | ArgKind::StarStar(e) => e,
        }
    }

    pub(crate) fn keyword(&self) -> Option<&'a str> {
        match &self.kind {
            ArgKind::Keyword(k, _) => Some(k),
            _ => None,
        }
    }
}

impl<'a> Expr<'a> {
    /// The literal's value, for a string literal.
    pub(crate) fn as_str(&self) -> Option<&str> {
        match &self.kind {
            ExprKind::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_name(&self) -> Option<&'a str> {
        match &self.kind {
            ExprKind::Name(n) => Some(n),
            _ => None,
        }
    }

    /// The name a call is written under: `f` for `f(...)`, `m` for
    /// `x.m(...)`. `None` for a callee of any other shape.
    pub(crate) fn head(&self) -> Option<&'a str> {
        match &self.kind {
            ExprKind::Name(n) => Some(n),
            ExprKind::Attr { name, .. } => Some(name),
            _ => None,
        }
    }

    /// Visits this expression and every expression under it, outer before
    /// inner and in source order.
    pub(crate) fn walk(&self, f: &mut dyn FnMut(&Expr<'a>)) {
        f(self);
        match &self.kind {
            ExprKind::Call { callee, args } => {
                callee.walk(f);
                for a in args {
                    a.value().walk(f);
                }
            }
            ExprKind::Attr { object, .. } => object.walk(f),
            ExprKind::Index { object, index } => {
                object.walk(f);
                index.walk(f);
            }
            ExprKind::Slice { object, parts } => {
                object.walk(f);
                for p in parts.iter().flatten() {
                    p.walk(f);
                }
            }
            ExprKind::List(items) | ExprKind::Tuple(items) => {
                for e in items {
                    e.walk(f);
                }
            }
            ExprKind::Dict(items) => {
                for (k, v) in items {
                    k.walk(f);
                    v.walk(f);
                }
            }
            ExprKind::BinOp { lhs, rhs, .. } => {
                lhs.walk(f);
                rhs.walk(f);
            }
            ExprKind::UnOp { operand, .. } => operand.walk(f),
            ExprKind::Cond { then, test, orelse } => {
                then.walk(f);
                test.walk(f);
                orelse.walk(f);
            }
            ExprKind::Lambda { params, body } => {
                for p in params {
                    if let Some(d) = &p.default {
                        d.walk(f);
                    }
                }
                body.walk(f);
            }
            ExprKind::Comprehension { element, clauses } => {
                element.walk(f);
                for c in clauses {
                    match c {
                        Clause::For { target, iter } => {
                            target.walk(f);
                            iter.walk(f);
                        }
                        Clause::If(e) => e.walk(f),
                    }
                }
            }
            ExprKind::Name(_)
            | ExprKind::Str(_)
            | ExprKind::Int
            | ExprKind::Float
            | ExprKind::Unparsed => {}
        }
    }
}

/// Visits every statement of a block and of the blocks nested in it, outer
/// before inner and in source order.
pub(crate) fn walk_stmts<'a>(stmts: &[Stmt<'a>], f: &mut dyn FnMut(&Stmt<'a>)) {
    for s in stmts {
        f(s);
        match s {
            Stmt::Def(d) => walk_stmts(&d.body, f),
            Stmt::If(i) => {
                for (_, body) in &i.branches {
                    walk_stmts(body, f);
                }
            }
            Stmt::For(l) => walk_stmts(&l.body, f),
            _ => {}
        }
    }
}

/// The expressions a statement holds directly (not those of nested
/// statements), in source order.
pub(crate) fn stmt_exprs<'s, 'a>(s: &'s Stmt<'a>) -> Vec<&'s Expr<'a>> {
    let mut out = Vec::new();
    match s {
        Stmt::Load(l) => out.extend(l.args.iter().map(Arg::value)),
        Stmt::Def(d) => out.extend(d.params.iter().filter_map(|p| p.default.as_ref())),
        Stmt::Assign(a) => {
            out.push(&a.target);
            out.push(&a.value);
        }
        Stmt::AugAssign(a) => {
            out.push(&a.target);
            out.push(&a.value);
        }
        Stmt::Expr(e) => out.push(e),
        Stmt::If(i) => out.extend(i.branches.iter().filter_map(|(t, _)| t.as_ref())),
        Stmt::For(l) => {
            out.push(&l.target);
            out.push(&l.iter);
        }
        Stmt::Return(r) => out.extend(r.value.as_ref()),
        Stmt::Pass(_) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Unparsed(_) => {}
    }
    out
}
