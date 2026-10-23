//! The syntax tree the parser builds and the interpreter walks.

use crate::diagnostic::Pos;
use std::sync::Arc;

#[derive(Debug)]
pub(crate) enum Stmt {
    Let {
        name: String,
        value: Expr,
        pos: Pos,
    },
    If {
        branches: Vec<(Expr, Vec<Stmt>)>,
        otherwise: Option<Vec<Stmt>>,
    },
    For {
        var: String,
        iterable: Expr,
        body: Vec<Stmt>,
        pos: Pos,
    },
    Return {
        value: Expr,
        pos: Pos,
    },
}

#[derive(Debug)]
pub(crate) struct Expr {
    pub kind: ExprKind,
    pub pos: Pos,
}

#[derive(Debug)]
pub(crate) enum ExprKind {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Template(Vec<Segment>),
    List(Vec<Expr>),
    Record(Vec<Field>),
    Name(String),
    Member {
        object: Box<Expr>,
        field: String,
        optional: bool,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
    },
    Method {
        receiver: Box<Expr>,
        name: String,
        args: Vec<Expr>,
        optional: bool,
    },
    Lambda(Arc<Lambda>),
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
        default: Box<Expr>,
    },
    /// `cond ? then : otherwise`.
    Cond {
        cond: Box<Expr>,
        then: Box<Expr>,
        otherwise: Box<Expr>,
    },
    Sql {
        db: Box<Expr>,
        parts: Vec<Segment>,
    },
}

/// A piece of an interpolated string or SQL block.
#[derive(Debug)]
pub(crate) enum Segment {
    Text(String),
    Hole(Expr),
}

/// One entry of a record literal.
#[derive(Debug)]
pub(crate) enum Field {
    Pair(String, Expr),
    /// `...record`: its fields, each overridden by a later entry of the same key.
    Spread(Expr),
}

#[derive(Debug)]
pub(crate) struct Lambda {
    pub params: Vec<String>,
    pub body: Body,
}

/// What a lambda runs: `x => value`, or `x => { statements }` ending in `return`.
#[derive(Debug)]
pub(crate) enum Body {
    Expr(Expr),
    Block(Vec<Stmt>),
}

#[derive(Debug)]
pub(crate) struct Arm {
    pub patterns: Vec<Pattern>,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Pattern {
    Str(String),
    Int(i64),
    Bool(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOp {
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Coalesce,
}

impl BinaryOp {
    pub(crate) fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Rem => "%",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::And => "&&",
            BinaryOp::Or => "||",
            BinaryOp::Coalesce => "??",
        }
    }
}

impl Stmt {
    pub(crate) fn pos(&self) -> Pos {
        match self {
            Stmt::Let { pos, .. } | Stmt::For { pos, .. } | Stmt::Return { pos, .. } => *pos,
            Stmt::If { branches, .. } => branches.first().map_or(Pos::START, |(c, _)| c.pos),
        }
    }
}
