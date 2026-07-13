//! The abstract syntax tree, shared by both front ends.

use std::fmt;

use quanty_core::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `table users { id: int @key, name: text @index, score: int = 0 }`
    TableDef(TableDef),
    /// `drop table users`
    DropTable { name: String },
    /// `put users { id: 1, name: "a" }, { id: 2, name: "b" }`
    Put {
        table: String,
        rows: Vec<Vec<(String, Expr)>>,
    },
    /// `get users { name, score } where score > 10 order by score desc limit 5`
    Get(Get),
    /// `set users where id = 1 { score += 5 }`
    Set {
        table: String,
        filter: Option<Expr>,
        assigns: Vec<Assign>,
    },
    /// `del users where id = 1`
    Del { table: String, filter: Option<Expr> },
    /// `index users.name`
    IndexDef { table: String, column: String },
    /// `show tables`
    ShowTables,
    /// `branch experiment` or `branch fix at 42`
    Branch { name: String, at: Option<u64> },
    /// `switch experiment`
    Switch { name: String },
    /// `merge experiment` (fast-forward only for now)
    Merge { name: String },
    /// `drop branch experiment`
    DropBranch { name: String },
    /// `show branches`
    ShowBranches,
    /// `log`, the current branch's history
    Log,
    /// `gc keep 10`
    Gc { keep: u64 },
    /// `begin`, open an explicit transaction spanning statements
    Begin,
    /// `commit`, make the open transaction durable
    Commit,
    /// `rollback`, discard the open transaction
    Rollback,
    /// `explain <statement>`
    Explain(Box<Statement>),
}

/// `as of 42` pins a commit id; `as of time 1700000000000` resolves the
/// newest commit at or before a unix millisecond timestamp on the current
/// branch. Commit ids are what `log` and every commit acknowledgment print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsOf {
    Commit(u64),
    Time(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Get {
    pub table: String,
    /// Joined tables in written order; the join tree is left-deep.
    pub joins: Vec<Join>,
    /// None means all columns of all tables in declaration order.
    pub projection: Option<Vec<ColumnRef>>,
    /// Read from history instead of the branch head.
    pub as_of: Option<AsOf>,
    pub filter: Option<Expr>,
    pub order: Option<(ColumnRef, Direction)>,
    pub limit: Option<u64>,
}

/// `join orders on users.id = orders.user_id`, optionally `left join`.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: String,
    pub on: Expr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
}

/// A column reference, optionally qualified: `score` or `users.score`.
/// Statements over one table rarely need the qualifier; joins need it
/// wherever a bare name would be ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    pub table: Option<String>,
    pub column: String,
}

impl ColumnRef {
    pub fn bare(column: impl Into<String>) -> Self {
        ColumnRef {
            table: None,
            column: column.into(),
        }
    }

    pub fn qualified(table: impl Into<String>, column: impl Into<String>) -> Self {
        ColumnRef {
            table: Some(table.into()),
            column: column.into(),
        }
    }
}

impl fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.table {
            Some(t) => write!(f, "{t}.{}", self.column),
            None => write!(f, "{}", self.column),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableDef {
    pub name: String,
    pub columns: Vec<ColumnDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: TypeName,
    pub nullable: bool,
    pub key: bool,
    pub index: bool,
    pub default: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeName {
    Int,
    Float,
    Text,
    Bytes,
    Bool,
}

impl TypeName {
    pub fn as_str(self) -> &'static str {
        match self {
            TypeName::Int => "int",
            TypeName::Float => "float",
            TypeName::Text => "text",
            TypeName::Bytes => "bytes",
            TypeName::Bool => "bool",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assign {
    pub column: String,
    /// `+=` and friends are desugared by the parser into `column = column op expr`,
    /// so execution only ever sees plain assignment.
    pub expr: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Column(ColumnRef),
    Unary(UnaryOp, Box<Expr>),
    Binary(Box<Expr>, BinaryOp, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl BinaryOp {
    pub fn as_str(self) -> &'static str {
        match self {
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::And => "and",
            BinaryOp::Or => "or",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
        }
    }
}
