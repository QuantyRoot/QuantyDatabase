//! The QQL abstract syntax tree.

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
    /// None means all columns in declaration order.
    pub projection: Option<Vec<String>>,
    /// Read from history instead of the branch head.
    pub as_of: Option<AsOf>,
    pub filter: Option<Expr>,
    pub order: Option<(String, Direction)>,
    pub limit: Option<u64>,
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
    Column(String),
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
