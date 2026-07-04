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
    /// `explain <statement>`
    Explain(Box<Statement>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Get {
    pub table: String,
    /// None means all columns in declaration order.
    pub projection: Option<Vec<String>>,
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
