//! Render an AST back to canonical QQL text.
//!
//! This is the second half of the parser's safety net: for every statement,
//! `parse(pretty(parse(x)))` must equal `parse(x)`. The fuzzer leans on
//! that invariant hard. It is also what `show tables` prints.

use quanty_core::Value;

use crate::ast::*;

pub fn pretty(stmt: &Statement) -> String {
    match stmt {
        Statement::TableDef(def) => {
            let cols: Vec<String> = def.columns.iter().map(column).collect();
            format!("table {} {{ {} }}", def.name, cols.join(", "))
        }
        Statement::DropTable { name } => format!("drop table {name}"),
        Statement::Put { table, rows } => {
            let rows: Vec<String> = rows
                .iter()
                .map(|fields| {
                    let fs: Vec<String> = fields
                        .iter()
                        .map(|(c, e)| format!("{c}: {}", expr(e)))
                        .collect();
                    format!("{{ {} }}", fs.join(", "))
                })
                .collect();
            format!("put {table} {}", rows.join(", "))
        }
        Statement::Get(g) => {
            let mut out = format!("get {}", g.table);
            for j in &g.joins {
                let kw = match j.kind {
                    JoinKind::Inner => "join",
                    JoinKind::Left => "left join",
                };
                out.push_str(&format!(" {kw} {} on {}", j.table, expr(&j.on)));
            }
            if let Some(cols) = &g.projection {
                let cols: Vec<String> = cols.iter().map(ToString::to_string).collect();
                out.push_str(&format!(" {{ {} }}", cols.join(", ")));
            }
            match g.as_of {
                Some(AsOf::Commit(n)) => out.push_str(&format!(" as of {n}")),
                Some(AsOf::Time(n)) => out.push_str(&format!(" as of time {n}")),
                None => {}
            }
            if let Some(f) = &g.filter {
                out.push_str(&format!(" where {}", expr(f)));
            }
            if let Some((col, dir)) = &g.order {
                let dir = match dir {
                    Direction::Asc => "asc",
                    Direction::Desc => "desc",
                };
                out.push_str(&format!(" order by {col} {dir}"));
            }
            if let Some(n) = g.limit {
                out.push_str(&format!(" limit {n}"));
            }
            out
        }
        Statement::Set {
            table,
            filter,
            assigns,
        } => {
            let mut out = format!("set {table}");
            if let Some(f) = filter {
                out.push_str(&format!(" where {}", expr(f)));
            }
            let asns: Vec<String> = assigns
                .iter()
                .map(|a| format!("{} = {}", a.column, expr(&a.expr)))
                .collect();
            out.push_str(&format!(" {{ {} }}", asns.join(", ")));
            out
        }
        Statement::Del { table, filter } => {
            let mut out = format!("del {table}");
            if let Some(f) = filter {
                out.push_str(&format!(" where {}", expr(f)));
            }
            out
        }
        Statement::IndexDef { table, column } => format!("index {table}.{column}"),
        Statement::ShowTables => "show tables".to_string(),
        Statement::Branch { name, at: Some(n) } => format!("branch {name} at {n}"),
        Statement::Branch { name, at: None } => format!("branch {name}"),
        Statement::Switch { name } => format!("switch {name}"),
        Statement::Merge { name } => format!("merge {name}"),
        Statement::DropBranch { name } => format!("drop branch {name}"),
        Statement::ShowBranches => "show branches".to_string(),
        Statement::Log => "log".to_string(),
        Statement::Gc { keep } => format!("gc keep {keep}"),
        Statement::Begin => "begin".to_string(),
        Statement::Commit => "commit".to_string(),
        Statement::Rollback => "rollback".to_string(),
        Statement::Explain(inner) => format!("explain {}", pretty(inner)),
    }
}

fn column(c: &ColumnDef) -> String {
    let mut out = format!("{}: {}", c.name, c.ty.as_str());
    if let Some(d) = &c.default {
        out.push_str(&format!(" = {}", literal(d)));
    }
    if c.key {
        out.push_str(" @key");
    }
    if c.index {
        out.push_str(" @index");
    }
    if c.nullable {
        out.push_str(" @null");
    }
    out
}

pub fn expr(e: &Expr) -> String {
    match e {
        Expr::Literal(v) => literal(v),
        Expr::Column(c) => c.to_string(),
        Expr::Unary(UnaryOp::Neg, inner) => format!("(-{})", expr(inner)),
        Expr::Unary(UnaryOp::Not, inner) => format!("(not {})", expr(inner)),
        Expr::Binary(l, op, r) => format!("({} {} {})", expr(l), op.as_str(), expr(r)),
    }
}

pub fn literal(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Int(i) => i.to_string(),
        // {:?} keeps a trailing .0 on whole floats, so the literal stays a
        // float literal when it is parsed again
        Value::Float(f) => format!("{f:?}"),
        Value::Text(s) => {
            let escaped = s
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\t', "\\t")
                .replace('\0', "\\0");
            format!("\"{escaped}\"")
        }
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("x\"{hex}\"")
        }
    }
}
