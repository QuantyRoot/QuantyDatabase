//! The SQL parser. Recursive descent onto the same AST the QQL parser
//! builds, so both front ends run through one planner and one executor.
//!
//! Scope and semantics are documented in docs/SQL.md and ADR-014. The short
//! version: a pragmatic sqlite-flavored subset, engine semantics underneath,
//! and everything outside the subset fails with an error that says so
//! instead of parsing into something subtly different.

use quanty_core::Value;

use crate::ast::*;
use crate::error::ParseError;
use crate::sql::lexer::{lex, Spanned, Tok};

/// Parse exactly one SQL statement. A trailing semicolon is fine, a second
/// statement is not.
pub fn parse_sql(source: &str) -> Result<Statement, ParseError> {
    let tokens = lex(source)?;
    let mut p = Parser { tokens, pos: 0 };
    let stmt = p.statement()?;
    p.eat(&Tok::Semi);
    if p.peek() != &Tok::Eof {
        return Err(ParseError::at(
            p.at(),
            "one statement per call; found more input after the statement",
        ));
    }
    Ok(stmt)
}

/// Words that never act as bare identifiers. Quoting always works: a column
/// named order is written "order". The list is longer than the supported
/// grammar on purpose, so unsupported SQL fails on the keyword with a clear
/// message instead of misparsing it as a name.
const RESERVED: &[&str] = &[
    "all",
    "and",
    "as",
    "asc",
    "begin",
    "between",
    "by",
    "case",
    "cast",
    "check",
    "collate",
    "commit",
    "constraint",
    "create",
    "cross",
    "default",
    "delete",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "escape",
    "except",
    "exists",
    "explain",
    "foreign",
    "from",
    "full",
    "glob",
    "group",
    "having",
    "if",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "left",
    "like",
    "limit",
    "match",
    "natural",
    "not",
    "null",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "references",
    "regexp",
    "release",
    "replace",
    "right",
    "rollback",
    "savepoint",
    "select",
    "set",
    "show",
    "table",
    "then",
    "transaction",
    "true",
    "union",
    "unique",
    "update",
    "using",
    "values",
    "when",
    "where",
    "with",
];

struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].token
    }

    fn at(&self) -> usize {
        self.tokens[self.pos].at
    }

    fn bump(&mut self) -> Tok {
        let t = self.tokens[self.pos].token.clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, token: &Tok) -> bool {
        if self.peek() == token {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: Tok, what: &str) -> Result<(), ParseError> {
        if self.peek() == &token {
            self.bump();
            Ok(())
        } else {
            Err(ParseError::at(
                self.at(),
                format!("expected {what}, found {}", show(self.peek())),
            ))
        }
    }

    /// The keyword at the cursor, lowercased, if the cursor is on a bare
    /// word. Quoted identifiers never count as keywords.
    fn kw(&self) -> Option<String> {
        match self.peek() {
            Tok::Word(w) => Some(w.to_ascii_lowercase()),
            _ => None,
        }
    }

    fn at_kw(&self, kw: &str) -> bool {
        self.kw().as_deref() == Some(kw)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.at_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str, what: &str) -> Result<(), ParseError> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(ParseError::at(
                self.at(),
                format!("expected {what}, found {}", show(self.peek())),
            ))
        }
    }

    /// An identifier: a bare non-reserved word (case preserved) or a quoted
    /// name. Quoted names must still have identifier shape, because every
    /// name in the catalog has to render back into QQL, the canonical
    /// language (see ADR-014).
    fn ident(&mut self, what: &str) -> Result<String, ParseError> {
        let at = self.at();
        match self.peek().clone() {
            Tok::Word(w) => {
                if RESERVED.contains(&w.to_ascii_lowercase().as_str()) {
                    return Err(ParseError::at(
                        at,
                        format!(
                            "'{w}' is a reserved word here; quote it (\"{w}\") to use it as a name"
                        ),
                    ));
                }
                self.bump();
                Ok(w)
            }
            Tok::Quoted(s) => {
                let shaped = !s.is_empty()
                    && s.bytes()
                        .next()
                        .is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
                    && s.bytes().all(|b| b == b'_' || b.is_ascii_alphanumeric());
                if !shaped {
                    return Err(ParseError::at(
                        at,
                        format!(
                            "the name {s:?} does not fit the catalog; names are letters, digits and underscores, starting with a letter or underscore"
                        ),
                    ));
                }
                self.bump();
                Ok(s)
            }
            other => Err(ParseError::at(
                at,
                format!("expected {what}, found {}", show(&other)),
            )),
        }
    }

    fn not_yet(&self, what: &str) -> ParseError {
        ParseError::at(self.at(), format!("not supported yet: {what}"))
    }

    // ------------------------------------------------------------------
    // statements

    fn statement(&mut self) -> Result<Statement, ParseError> {
        let Some(head) = self.kw() else {
            return Err(ParseError::at(
                self.at(),
                format!(
                    "expected a statement (select, insert, update, delete, create, drop, explain), found {}",
                    show(self.peek())
                ),
            ));
        };
        match head.as_str() {
            "select" => {
                self.bump();
                self.select()
            }
            "insert" => {
                self.bump();
                self.insert()
            }
            "update" => {
                self.bump();
                self.update()
            }
            "delete" => {
                self.bump();
                self.delete()
            }
            "create" => {
                self.bump();
                self.create()
            }
            "drop" => {
                self.bump();
                self.drop()
            }
            "explain" => {
                self.bump();
                // sqlite spells it explain query plan; plain explain works too
                if self.eat_kw("query") {
                    self.expect_kw("plan", "'plan' after 'explain query'")?;
                }
                Ok(Statement::Explain(Box::new(self.statement()?)))
            }
            "show" => {
                self.bump();
                self.expect_kw("tables", "'tables' after 'show'")?;
                Ok(Statement::ShowTables)
            }
            "begin" | "commit" | "rollback" | "end" | "savepoint" | "release" => {
                Err(self.not_yet("transactions across statements"))
            }
            "with" => Err(self.not_yet("with (common table expressions)")),
            "values" => Err(self.not_yet("values as a bare statement")),
            "replace" => Err(self.not_yet("replace")),
            "alter" => Err(self.not_yet("alter table")),
            "pragma" | "vacuum" | "analyze" | "attach" | "detach" | "reindex" => {
                Err(ParseError::at(
                    self.at(),
                    format!("'{head}' is not supported"),
                ))
            }
            _ => Err(ParseError::at(
                self.at(),
                format!(
                    "expected a statement (select, insert, update, delete, create, drop, explain), found {}",
                    show(self.peek())
                ),
            )),
        }
    }

    fn select(&mut self) -> Result<Statement, ParseError> {
        if self.at_kw("distinct") {
            return Err(self.not_yet("select distinct"));
        }
        self.eat_kw("all"); // the default, spelled out

        let projection = if self.eat(&Tok::Star) {
            None
        } else {
            let mut cols = Vec::new();
            loop {
                if !matches!(self.peek(), Tok::Word(_) | Tok::Quoted(_)) {
                    return Err(ParseError::at(
                        self.at(),
                        "the select list takes plain column names or * for now; expressions are not supported yet",
                    ));
                }
                let name = self.ident("a column name")?;
                if self.peek() == &Tok::Dot {
                    return Err(self.not_yet("qualified column names"));
                }
                if self.peek() == &Tok::LParen {
                    return Err(self.not_yet("functions and aggregates"));
                }
                if self.at_kw("as")
                    || matches!(self.peek(), Tok::Word(w) if !RESERVED.contains(&w.to_ascii_lowercase().as_str()))
                {
                    return Err(self.not_yet("column aliases"));
                }
                cols.push(name);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            Some(cols)
        };

        self.expect_kw("from", "'from' (select without from is not supported yet)")?;
        let table = self.table_name()?;

        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };

        if self.at_kw("group") || self.at_kw("having") {
            return Err(self.not_yet("group by and aggregates"));
        }
        if self.at_kw("window") {
            return Err(self.not_yet("window functions"));
        }
        if self.at_kw("union") || self.at_kw("intersect") || self.at_kw("except") {
            return Err(self.not_yet("compound selects"));
        }

        let order = if self.eat_kw("order") {
            self.expect_kw("by", "'by' after 'order'")?;
            let col = self.ident("a column name to order by")?;
            if self.at_kw("collate") {
                return Err(self.not_yet("collate"));
            }
            let dir = if self.eat_kw("desc") {
                Direction::Desc
            } else {
                self.eat_kw("asc");
                Direction::Asc
            };
            if self.at_kw("nulls") {
                return Err(
                    self.not_yet("nulls first / nulls last (nulls sort first ascending, always)")
                );
            }
            if self.peek() == &Tok::Comma {
                return Err(self.not_yet("more than one order by column"));
            }
            Some((col, dir))
        } else {
            None
        };

        let limit = if self.eat_kw("limit") {
            let at = self.at();
            let Tok::Int(n) = self.peek().clone() else {
                return Err(ParseError::at(
                    at,
                    "limit takes a plain non-negative integer for now",
                ));
            };
            if n < 0 {
                return Err(ParseError::at(at, "limit wants a number >= 0"));
            }
            self.bump();
            if self.at_kw("offset") || self.peek() == &Tok::Comma {
                return Err(self.not_yet("offset"));
            }
            Some(n as u64)
        } else {
            None
        };

        Ok(Statement::Get(Get {
            table,
            projection,
            as_of: None,
            filter,
            order,
            limit,
        }))
    }

    /// A table reference: a bare name. Everything a bigger select would
    /// hang here (aliases, joins, subqueries) fails with a targeted error.
    fn table_name(&mut self) -> Result<String, ParseError> {
        let name = self.ident("a table name")?;
        if self.peek() == &Tok::Dot {
            return Err(ParseError::at(
                self.at(),
                "database-qualified names are not supported; there is one database per file",
            ));
        }
        if let Some(word) = self.kw() {
            match word.as_str() {
                "join" | "inner" | "left" | "right" | "full" | "cross" | "natural" => {
                    return Err(self.not_yet("joins"));
                }
                "as" => return Err(self.not_yet("table aliases")),
                w if !RESERVED.contains(&w) => {
                    return Err(self.not_yet("table aliases"));
                }
                _ => {}
            }
        }
        if matches!(self.peek(), Tok::Quoted(_)) {
            return Err(self.not_yet("table aliases"));
        }
        if self.peek() == &Tok::Comma {
            return Err(self.not_yet("joins"));
        }
        Ok(name)
    }

    fn insert(&mut self) -> Result<Statement, ParseError> {
        if self.at_kw("or") {
            return Err(self.not_yet("insert or (replace, ignore, ...)"));
        }
        self.expect_kw("into", "'into' after 'insert'")?;
        let table = self.ident("a table name")?;

        if self.at_kw("default") {
            return Err(self.not_yet("insert ... default values"));
        }
        if self.at_kw("select") {
            return Err(self.not_yet("insert from select"));
        }
        if self.peek() != &Tok::LParen {
            return Err(ParseError::at(
                self.at(),
                "insert needs an explicit column list, like insert into t (a, b) values (1, 2); positional inserts are not supported yet",
            ));
        }
        self.bump();
        let mut columns = Vec::new();
        loop {
            columns.push(self.ident("a column name")?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen, "')' after the column list")?;

        if self.at_kw("select") {
            return Err(self.not_yet("insert from select"));
        }
        self.expect_kw("values", "'values'")?;

        let mut rows: Vec<Vec<(String, Expr)>> = Vec::new();
        loop {
            let row_at = self.at();
            self.expect(Tok::LParen, "'(' to open a row")?;
            let mut values = Vec::new();
            loop {
                values.push(self.expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen, "')' after the row")?;
            if values.len() != columns.len() {
                return Err(ParseError::at(
                    row_at,
                    format!(
                        "row {} has {} values for {} named columns",
                        rows.len() + 1,
                        values.len(),
                        columns.len()
                    ),
                ));
            }
            rows.push(columns.iter().cloned().zip(values).collect());
            if !self.eat(&Tok::Comma) {
                break;
            }
        }

        if self.at_kw("on") {
            return Err(self.not_yet("on conflict / upsert"));
        }
        if self.at_kw("returning") {
            return Err(self.not_yet("returning"));
        }
        Ok(Statement::Put { table, rows })
    }

    fn update(&mut self) -> Result<Statement, ParseError> {
        if self.at_kw("or") {
            return Err(self.not_yet("update or (replace, ignore, ...)"));
        }
        let table = self.ident("a table name")?;
        self.expect_kw("set", "'set' after the table name")?;
        let mut assigns = Vec::new();
        loop {
            let column = self.ident("a column name")?;
            self.expect(Tok::Eq, "'=' in the set list")?;
            let expr = self.expr()?;
            assigns.push(Assign { column, expr });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        if self.at_kw("from") {
            return Err(self.not_yet("update ... from"));
        }
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        if self.at_kw("returning") {
            return Err(self.not_yet("returning"));
        }
        if self.at_kw("order") || self.at_kw("limit") {
            return Err(self.not_yet("order by / limit on update"));
        }
        Ok(Statement::Set {
            table,
            filter,
            assigns,
        })
    }

    fn delete(&mut self) -> Result<Statement, ParseError> {
        self.expect_kw("from", "'from' after 'delete'")?;
        let table = self.ident("a table name")?;
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        if self.at_kw("returning") {
            return Err(self.not_yet("returning"));
        }
        if self.at_kw("order") || self.at_kw("limit") {
            return Err(self.not_yet("order by / limit on delete"));
        }
        Ok(Statement::Del { table, filter })
    }

    fn create(&mut self) -> Result<Statement, ParseError> {
        if self.at_kw("temp") || self.at_kw("temporary") {
            return Err(self.not_yet("temporary tables"));
        }
        if self.at_kw("unique") {
            return Err(self.not_yet("unique indexes"));
        }
        if self.eat_kw("table") {
            return self.create_table();
        }
        if self.eat_kw("index") {
            return self.create_index();
        }
        if let Some(w) = self.kw() {
            if matches!(w.as_str(), "view" | "trigger" | "virtual") {
                return Err(ParseError::at(
                    self.at(),
                    format!("'create {w}' is not supported"),
                ));
            }
        }
        Err(ParseError::at(
            self.at(),
            format!(
                "expected 'table' or 'index' after 'create', found {}",
                show(self.peek())
            ),
        ))
    }

    fn create_table(&mut self) -> Result<Statement, ParseError> {
        if self.at_kw("if") {
            return Err(self.not_yet("'if not exists'"));
        }
        let name = self.ident("a table name")?;
        if self.peek() == &Tok::Dot {
            return Err(ParseError::at(
                self.at(),
                "database-qualified names are not supported; there is one database per file",
            ));
        }
        if self.at_kw("as") {
            return Err(self.not_yet("create table as select"));
        }
        self.expect(Tok::LParen, "'(' to open the column list")?;

        let mut columns: Vec<ColumnDef> = Vec::new();
        let mut saw_table_pk = false;
        loop {
            match self.kw().as_deref() {
                Some("primary") | Some("unique") | Some("check") | Some("foreign")
                | Some("constraint") => {
                    self.table_constraint(&mut columns, &mut saw_table_pk)?;
                }
                _ => {
                    columns.push(self.column_def(&columns, saw_table_pk)?);
                }
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen, "')' after the column list")?;

        // table options: both are properties this engine has anyway. every
        // table clusters by its primary key (there is no rowid to be
        // without) and every column is strictly typed.
        loop {
            if self.eat_kw("without") {
                self.expect_kw("rowid", "'rowid' after 'without'")?;
            } else if !(self.eat_kw("strict") || self.eat(&Tok::Comma)) {
                break;
            }
        }

        if !columns.iter().any(|c| c.key) {
            return Err(ParseError::at(
                self.at(),
                format!("table '{name}' needs a primary key"),
            ));
        }
        Ok(Statement::TableDef(TableDef { name, columns }))
    }

    fn column_def(
        &mut self,
        earlier: &[ColumnDef],
        saw_table_pk: bool,
    ) -> Result<ColumnDef, ParseError> {
        let name = self.ident("a column name")?;
        let ty = self.type_name()?;

        let mut col = ColumnDef {
            name,
            ty,
            // sql columns are nullable unless something says otherwise;
            // the opposite default from QQL, resolved here in the parser
            nullable: true,
            key: false,
            index: false,
            default: None,
        };

        while let Some(word) = self.kw() {
            match word.as_str() {
                "primary" => {
                    self.bump();
                    self.expect_kw("key", "'key' after 'primary'")?;
                    if saw_table_pk || earlier.iter().any(|c| c.key) {
                        return Err(ParseError::at(
                            self.at(),
                            "the table already has a primary key; use one table-level primary key (a, b) for a composite key",
                        ));
                    }
                    self.eat_kw("asc");
                    if self.at_kw("desc") {
                        return Err(self.not_yet("descending primary keys"));
                    }
                    if self.at_kw("autoincrement") {
                        return Err(self.not_yet("autoincrement"));
                    }
                    if self.at_kw("on") {
                        return Err(self.not_yet("on conflict clauses"));
                    }
                    col.key = true;
                }
                "not" => {
                    self.bump();
                    self.expect_kw("null", "'null' after 'not'")?;
                    col.nullable = false;
                }
                "null" => {
                    self.bump();
                }
                "default" => {
                    self.bump();
                    let at = self.at();
                    if self.peek() == &Tok::Minus || self.peek() == &Tok::LParen {
                        return Err(ParseError::at(
                            at,
                            "defaults must be plain literals, not expressions",
                        ));
                    }
                    col.default = Some(self.literal_value()?.ok_or_else(|| {
                        ParseError::at(at, "defaults must be plain literals, not expressions")
                    })?);
                }
                "unique" => return Err(self.not_yet("unique constraints")),
                "check" => return Err(self.not_yet("check constraints")),
                "collate" => return Err(self.not_yet("collate")),
                "generated" | "as" => return Err(self.not_yet("generated columns")),
                "references" => {
                    self.bump();
                    self.foreign_key_tail()?;
                }
                "constraint" => {
                    self.bump();
                    self.ident("a constraint name")?;
                }
                _ => break,
            }
        }
        if col.key {
            // a primary key column is never nullable, said or unsaid
            col.nullable = false;
        }
        Ok(col)
    }

    /// Type names map onto the five storage types. The lossy ones follow
    /// sqlite affinity: numeric and decimal land on float, the date family
    /// lands on text. Documented in docs/SQL.md.
    fn type_name(&mut self) -> Result<TypeName, ParseError> {
        let at = self.at();
        let Some(word) = self.kw() else {
            return Err(ParseError::at(
                at,
                format!("expected a column type, found {}", show(self.peek())),
            ));
        };
        self.bump();
        let ty = match word.as_str() {
            "int" | "integer" | "bigint" | "smallint" | "tinyint" | "mediumint" | "int2"
            | "int4" | "int8" => TypeName::Int,
            "real" | "float" | "numeric" | "decimal" => TypeName::Float,
            "double" => {
                self.eat_kw("precision");
                TypeName::Float
            }
            "text" | "varchar" | "nvarchar" | "char" | "nchar" | "clob" | "string" => {
                TypeName::Text
            }
            "character" => {
                self.eat_kw("varying");
                TypeName::Text
            }
            "date" | "datetime" | "timestamp" => TypeName::Text,
            "blob" => TypeName::Bytes,
            "boolean" | "bool" => TypeName::Bool,
            other => {
                return Err(ParseError::at(
                    at,
                    format!(
                        "unknown column type '{other}' (use integer, real, text, blob or boolean)"
                    ),
                ))
            }
        };
        // a length like varchar(160) or numeric(10, 2) parses and means
        // nothing, the same treatment sqlite gives it
        if self.eat(&Tok::LParen) {
            loop {
                match self.peek() {
                    Tok::Int(_) => {
                        self.bump();
                    }
                    _ => {
                        return Err(ParseError::at(
                            self.at(),
                            format!(
                                "expected a number in the type length, found {}",
                                show(self.peek())
                            ),
                        ))
                    }
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen, "')' after the type length")?;
        }
        Ok(ty)
    }

    fn table_constraint(
        &mut self,
        columns: &mut [ColumnDef],
        saw_table_pk: &mut bool,
    ) -> Result<(), ParseError> {
        if self.eat_kw("constraint") {
            self.ident("a constraint name")?;
        }
        let at = self.at();
        match self.kw().as_deref() {
            Some("primary") => {
                self.bump();
                self.expect_kw("key", "'key' after 'primary'")?;
                if *saw_table_pk || columns.iter().any(|c| c.key) {
                    return Err(ParseError::at(at, "the table already has a primary key"));
                }
                self.expect(Tok::LParen, "'(' to open the key column list")?;
                let mut last_index: Option<usize> = None;
                loop {
                    let col_at = self.at();
                    let name = self.ident("a key column name")?;
                    self.eat_kw("asc");
                    if self.at_kw("desc") {
                        return Err(self.not_yet("descending primary keys"));
                    }
                    let Some(index) = columns.iter().position(|c| c.name == name) else {
                        return Err(ParseError::at(
                            col_at,
                            format!("unknown column '{name}' in the primary key"),
                        ));
                    };
                    if last_index.is_some_and(|last| index <= last) {
                        // key column order is declaration order, a
                        // reordered composite key cannot be expressed yet
                        return Err(ParseError::at(
                            col_at,
                            "composite primary keys must list columns in declaration order (other orders are not supported yet)",
                        ));
                    }
                    last_index = Some(index);
                    columns[index].key = true;
                    columns[index].nullable = false;
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RParen, "')' after the key column list")?;
                if self.at_kw("on") {
                    return Err(self.not_yet("on conflict clauses"));
                }
                *saw_table_pk = true;
                Ok(())
            }
            Some("unique") => Err(self.not_yet("unique constraints")),
            Some("check") => Err(self.not_yet("check constraints")),
            Some("foreign") => {
                self.bump();
                self.expect_kw("key", "'key' after 'foreign'")?;
                self.expect(Tok::LParen, "'(' to open the foreign key column list")?;
                loop {
                    self.ident("a column name")?;
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RParen, "')' after the foreign key column list")?;
                self.expect_kw("references", "'references'")?;
                self.foreign_key_tail()
            }
            _ => Err(ParseError::at(
                at,
                format!("expected a table constraint, found {}", show(self.peek())),
            )),
        }
    }

    /// Everything after references: parsed, validated for shape, then
    /// dropped. Foreign keys are accepted but not enforced, the behavior
    /// sqlite ships with by default (see ADR-014).
    fn foreign_key_tail(&mut self) -> Result<(), ParseError> {
        self.ident("the referenced table name")?;
        if self.eat(&Tok::LParen) {
            loop {
                self.ident("a referenced column name")?;
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen, "')' after the referenced columns")?;
        }
        loop {
            if self.eat_kw("on") {
                if !(self.eat_kw("delete") || self.eat_kw("update")) {
                    return Err(ParseError::at(
                        self.at(),
                        "expected 'delete' or 'update' after 'on'",
                    ));
                }
                if self.eat_kw("set") {
                    if !(self.eat_kw("null") || self.eat_kw("default")) {
                        return Err(ParseError::at(
                            self.at(),
                            "expected 'null' or 'default' after 'set'",
                        ));
                    }
                } else if self.eat_kw("cascade") || self.eat_kw("restrict") {
                } else if self.eat_kw("no") {
                    self.expect_kw("action", "'action' after 'no'")?;
                } else {
                    return Err(ParseError::at(
                        self.at(),
                        "expected a foreign key action (cascade, restrict, set null, set default, no action)",
                    ));
                }
            } else if self.eat_kw("match") {
                self.ident("a match name")?;
            } else if self.eat_kw("deferrable") {
                self.deferred_tail()?;
            } else if self.eat_kw("not") {
                self.expect_kw("deferrable", "'deferrable' after 'not'")?;
                self.deferred_tail()?;
            } else {
                break;
            }
        }
        Ok(())
    }

    fn deferred_tail(&mut self) -> Result<(), ParseError> {
        if self.eat_kw("initially") && !(self.eat_kw("deferred") || self.eat_kw("immediate")) {
            return Err(ParseError::at(
                self.at(),
                "expected 'deferred' or 'immediate' after 'initially'",
            ));
        }
        Ok(())
    }

    fn create_index(&mut self) -> Result<Statement, ParseError> {
        if self.at_kw("if") {
            return Err(self.not_yet("'if not exists'"));
        }
        // the name is required by the grammar and dropped by the engine:
        // an index is identified by (table, column) here, like in QQL
        self.ident("an index name")?;
        self.expect_kw("on", "'on' after the index name")?;
        let table = self.ident("a table name")?;
        self.expect(Tok::LParen, "'(' to open the index column list")?;
        let column = self.ident("a column name")?;
        self.eat_kw("asc");
        if self.at_kw("desc") {
            return Err(self.not_yet("descending indexes"));
        }
        if self.at_kw("collate") {
            return Err(self.not_yet("collate"));
        }
        if self.peek() == &Tok::Comma {
            return Err(self.not_yet("multi-column indexes"));
        }
        self.expect(Tok::RParen, "')' after the index column")?;
        if self.at_kw("where") {
            return Err(self.not_yet("partial indexes"));
        }
        Ok(Statement::IndexDef { table, column })
    }

    fn drop(&mut self) -> Result<Statement, ParseError> {
        if self.eat_kw("table") {
            if self.at_kw("if") {
                return Err(self.not_yet("'if exists'"));
            }
            let name = self.ident("a table name")?;
            return Ok(Statement::DropTable { name });
        }
        if self.at_kw("index") {
            return Err(self.not_yet("drop index (indexes live and die with their table for now)"));
        }
        if let Some(w) = self.kw() {
            if matches!(w.as_str(), "view" | "trigger") {
                return Err(ParseError::at(
                    self.at(),
                    format!("'drop {w}' is not supported"),
                ));
            }
        }
        Err(ParseError::at(
            self.at(),
            format!("expected 'table' after 'drop', found {}", show(self.peek())),
        ))
    }

    // ------------------------------------------------------------------
    // expressions, same precedence ladder as QQL

    fn expr(&mut self) -> Result<Expr, ParseError> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.and_expr()?;
        while self.eat_kw("or") {
            let rhs = self.and_expr()?;
            lhs = Expr::Binary(Box::new(lhs), BinaryOp::Or, Box::new(rhs));
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.not_expr()?;
        while self.eat_kw("and") {
            let rhs = self.not_expr()?;
            lhs = Expr::Binary(Box::new(lhs), BinaryOp::And, Box::new(rhs));
        }
        Ok(lhs)
    }

    fn not_expr(&mut self) -> Result<Expr, ParseError> {
        if self.at_kw("not")
            && !matches!(&self.tokens.get(self.pos + 1), Some(s) if matches!(&s.token, Tok::Word(w) if matches!(w.to_ascii_lowercase().as_str(), "between" | "in" | "like" | "glob" | "regexp" | "match")))
        {
            self.bump();
            return Ok(Expr::Unary(UnaryOp::Not, Box::new(self.not_expr()?)));
        }
        self.cmp_expr()
    }

    fn cmp_expr(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.add_expr()?;
        let at = self.at();
        let op = match self.peek() {
            Tok::Eq => Some(BinaryOp::Eq),
            Tok::NotEq => Some(BinaryOp::NotEq),
            Tok::Lt => Some(BinaryOp::Lt),
            Tok::LtEq => Some(BinaryOp::LtEq),
            Tok::Gt => Some(BinaryOp::Gt),
            Tok::GtEq => Some(BinaryOp::GtEq),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let rhs = self.add_expr()?;
            // sql three-valued logic says a comparison with null never
            // matches; the engine's null rules say null = null holds. the
            // literal spelling is rejected so neither meaning is guessed
            // at; is null / is not null spell the intent exactly (ADR-014)
            if matches!(lhs, Expr::Literal(Value::Null))
                || matches!(rhs, Expr::Literal(Value::Null))
            {
                return Err(ParseError::at(
                    at,
                    "comparing with null never matches in sql; write is null or is not null",
                ));
            }
            return Ok(Expr::Binary(Box::new(lhs), op, Box::new(rhs)));
        }
        if self.eat_kw("is") {
            // sqlite's is / is not are null-safe comparisons, which is
            // exactly what the engine's = and != do
            let negated = self.eat_kw("not");
            let rhs = self.add_expr()?;
            let op = if negated {
                BinaryOp::NotEq
            } else {
                BinaryOp::Eq
            };
            return Ok(Expr::Binary(Box::new(lhs), op, Box::new(rhs)));
        }
        let negated_tail = self.at_kw("not");
        let tail_kw = if negated_tail {
            self.tokens.get(self.pos + 1).and_then(|s| match &s.token {
                Tok::Word(w) => Some(w.to_ascii_lowercase()),
                _ => None,
            })
        } else {
            self.kw()
        };
        if let Some(word) = tail_kw {
            match word.as_str() {
                "between" => return Err(self.not_yet("between (spell it with two comparisons)")),
                "in" => return Err(self.not_yet("in (...)")),
                "like" | "glob" | "regexp" | "match" => {
                    return Err(self.not_yet("like and pattern matching"))
                }
                _ => {}
            }
        }
        Ok(lhs)
    }

    fn add_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinaryOp::Add,
                Tok::Minus => BinaryOp::Sub,
                // || concatenates text; the engine spells that + on text
                Tok::Concat => BinaryOp::Add,
                _ => break,
            };
            self.bump();
            let rhs = self.mul_expr()?;
            lhs = Expr::Binary(Box::new(lhs), op, Box::new(rhs));
        }
        Ok(lhs)
    }

    fn mul_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinaryOp::Mul,
                Tok::Slash => BinaryOp::Div,
                Tok::Percent => BinaryOp::Mod,
                _ => break,
            };
            self.bump();
            let rhs = self.unary_expr()?;
            lhs = Expr::Binary(Box::new(lhs), op, Box::new(rhs));
        }
        Ok(lhs)
    }

    fn unary_expr(&mut self) -> Result<Expr, ParseError> {
        if self.eat(&Tok::Minus) {
            return Ok(Expr::Unary(UnaryOp::Neg, Box::new(self.unary_expr()?)));
        }
        if self.eat(&Tok::Plus) {
            // unary plus is a no-op, sqlite agrees
            return self.unary_expr();
        }
        self.primary_expr()
    }

    fn primary_expr(&mut self) -> Result<Expr, ParseError> {
        if let Some(v) = self.literal_value()? {
            return Ok(Expr::Literal(v));
        }
        let at = self.at();
        match self.peek().clone() {
            Tok::LParen => {
                self.bump();
                if self.at_kw("select") {
                    return Err(self.not_yet("subqueries"));
                }
                let inner = self.expr()?;
                self.expect(Tok::RParen, "')'")?;
                Ok(inner)
            }
            Tok::Word(w) => {
                match w.to_ascii_lowercase().as_str() {
                    "case" => return Err(self.not_yet("case expressions")),
                    "cast" => return Err(self.not_yet("cast")),
                    "exists" => return Err(self.not_yet("exists")),
                    "select" => return Err(self.not_yet("subqueries")),
                    _ => {}
                }
                let name = self.ident("a column name or value")?;
                if self.peek() == &Tok::LParen {
                    return Err(self.not_yet("functions and aggregates"));
                }
                if self.peek() == &Tok::Dot {
                    return Err(self.not_yet("qualified column names"));
                }
                Ok(Expr::Column(name))
            }
            Tok::Quoted(_) => {
                let name = self.ident("a column name")?;
                if self.peek() == &Tok::Dot {
                    return Err(self.not_yet("qualified column names"));
                }
                Ok(Expr::Column(name))
            }
            other => Err(ParseError::at(
                at,
                format!("expected a value or column, found {}", show(&other)),
            )),
        }
    }

    /// Literal if the next token is one. true, false and null match
    /// case-insensitively like every other keyword.
    fn literal_value(&mut self) -> Result<Option<Value>, ParseError> {
        let v = match self.peek().clone() {
            Tok::Int(i) => Value::Int(i),
            Tok::Float(f) => Value::Float(f),
            Tok::Str(s) => Value::Text(s),
            Tok::Blob(b) => Value::Bytes(b),
            Tok::Word(w) => match w.to_ascii_lowercase().as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                "null" => Value::Null,
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        self.bump();
        Ok(Some(v))
    }
}

fn show(token: &Tok) -> String {
    match token {
        Tok::Word(w) => format!("'{w}'"),
        Tok::Quoted(s) => format!("the name {s:?}"),
        Tok::Int(i) => format!("the number {i}"),
        Tok::Float(f) => format!("the number {f}"),
        Tok::Str(s) => format!("the string '{s}'"),
        Tok::Blob(_) => "a blob literal".to_string(),
        Tok::Eof => "the end of the statement".to_string(),
        Tok::LParen => "'('".to_string(),
        Tok::RParen => "')'".to_string(),
        Tok::Comma => "','".to_string(),
        Tok::Semi => "';'".to_string(),
        Tok::Dot => "'.'".to_string(),
        Tok::Eq => "'='".to_string(),
        Tok::NotEq => "'!='".to_string(),
        Tok::Lt => "'<'".to_string(),
        Tok::LtEq => "'<='".to_string(),
        Tok::Gt => "'>'".to_string(),
        Tok::GtEq => "'>='".to_string(),
        Tok::Plus => "'+'".to_string(),
        Tok::Minus => "'-'".to_string(),
        Tok::Star => "'*'".to_string(),
        Tok::Slash => "'/'".to_string(),
        Tok::Percent => "'%'".to_string(),
        Tok::Concat => "'||'".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse as parse_qql;

    /// The core promise of the second front end: sql lowers onto the exact
    /// AST the equivalent QQL builds.
    #[test]
    fn sql_and_qql_build_the_same_ast() {
        for (sql, qql) in [
            (
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INT NOT NULL DEFAULT 0, bio TEXT)",
                r#"table users { id: int @key, name: text, score: int = 0, bio: text @null }"#,
            ),
            (
                "INSERT INTO users (id, name) VALUES (1, 'elchi'), (2, 'mira')",
                r#"put users { id: 1, name: "elchi" }, { id: 2, name: "mira" }"#,
            ),
            (
                "SELECT name, score FROM users WHERE score > 10 ORDER BY score DESC LIMIT 5;",
                r#"get users { name, score } where score > 10 order by score desc limit 5"#,
            ),
            (
                "select * from users where not (score % 2 = 0)",
                r#"get users where not (score % 2 = 0)"#,
            ),
            (
                "UPDATE users SET score = score + 5, name = 'neu' WHERE id = 1",
                r#"set users where id = 1 { score += 5, name = "neu" }"#,
            ),
            ("DELETE FROM users WHERE score < 0", r#"del users where score < 0"#),
            ("CREATE INDEX idx_users_name ON users (name)", r#"index users.name"#),
            ("DROP TABLE users", r#"drop table users"#),
            ("EXPLAIN QUERY PLAN SELECT * FROM users WHERE id = 7", r#"explain get users where id = 7"#),
            ("SELECT a FROM t WHERE b IS NULL AND c IS NOT NULL", r#"get t { a } where b = null and c != null"#),
            ("SELECT a FROM t WHERE a = x'c0ffee' OR a <> x''", r#"get t { a } where a = x"c0ffee" or a != x"""#),
        ] {
            let via_sql = parse_sql(sql).unwrap_or_else(|e| panic!("sql failed: {sql}\n{e}"));
            let via_qql = parse_qql(qql).unwrap_or_else(|e| panic!("qql failed: {qql}\n{e}"));
            assert_eq!(via_sql, via_qql, "different ASTs for\n  sql: {sql}\n  qql: {qql}");
        }
    }

    #[test]
    fn quoting_and_case() {
        // keywords are case-insensitive, identifiers keep their case
        let a = parse_sql("SeLeCt Name FrOm Artist").unwrap();
        let b = parse_qql("get Artist { Name }").unwrap();
        assert_eq!(a, b);

        // quoting bypasses reserved words, in all three styles
        let q = parse_sql(r#"SELECT "order", [limit], `set` FROM "table""#).unwrap();
        let Statement::Get(get) = q else {
            panic!("not a get")
        };
        assert_eq!(get.table, "table");
        assert_eq!(
            get.projection,
            Some(vec!["order".into(), "limit".into(), "set".into()])
        );
    }

    #[test]
    fn table_level_composite_key_in_declaration_order() {
        let sql = "CREATE TABLE pt (playlist INTEGER NOT NULL, track INTEGER NOT NULL, \
                   PRIMARY KEY (playlist, track), \
                   FOREIGN KEY (track) REFERENCES tracks (id) ON DELETE CASCADE)";
        let qql = "table pt { playlist: int @key, track: int @key }";
        assert_eq!(parse_sql(sql).unwrap(), parse_qql(qql).unwrap());

        let reordered = "CREATE TABLE pt (a INTEGER, b INTEGER, PRIMARY KEY (b, a))";
        let err = parse_sql(reordered).unwrap_err();
        assert!(err.message.contains("declaration order"), "{err}");
    }

    #[test]
    fn realistic_sqlite_master_ddl_parses() {
        // the shape the chinook schema is written in: bracket quoting,
        // nvarchar/numeric/datetime types, named constraints, fk actions
        let sql = "CREATE TABLE [Invoice] (\
                     [InvoiceId] INTEGER NOT NULL, \
                     [CustomerId] INTEGER NOT NULL, \
                     [InvoiceDate] DATETIME NOT NULL, \
                     [BillingAddress] NVARCHAR(70), \
                     [Total] NUMERIC(10,2) NOT NULL, \
                     CONSTRAINT [PK_Invoice] PRIMARY KEY ([InvoiceId]), \
                     FOREIGN KEY ([CustomerId]) REFERENCES [Customer] ([CustomerId]) \
                       ON DELETE NO ACTION ON UPDATE NO ACTION)";
        let Statement::TableDef(def) = parse_sql(sql).unwrap() else {
            panic!("not a table def")
        };
        assert_eq!(def.name, "Invoice");
        assert_eq!(def.columns.len(), 5);
        assert!(def.columns[0].key && !def.columns[0].nullable);
        assert_eq!(def.columns[2].ty, TypeName::Text); // datetime
        assert!(def.columns[3].nullable); // no not null
        assert_eq!(def.columns[4].ty, TypeName::Float); // numeric
    }

    #[test]
    fn null_comparisons_are_rejected_with_a_pointer() {
        for bad in [
            "select * from t where a = null",
            "select * from t where null <> a",
            "select * from t where a < null",
        ] {
            let err = parse_sql(bad).unwrap_err();
            assert!(err.message.contains("is null"), "{bad}: {err}");
        }
        // and the spelled-out forms work
        assert!(parse_sql("select * from t where a is null").is_ok());
        assert!(parse_sql("select * from t where a is not null").is_ok());
        // is as a general null-safe comparison, like sqlite
        assert_eq!(
            parse_sql("select * from t where a is b").unwrap(),
            parse_qql("get t where a = b").unwrap()
        );
    }

    #[test]
    fn unsupported_sql_names_the_missing_piece() {
        for (input, needle) in [
            ("SELECT * FROM a JOIN b ON a.id = b.id", "joins"),
            ("SELECT * FROM a, b", "joins"),
            ("SELECT count(x) FROM t", "aggregates"),
            ("SELECT * FROM t GROUP BY a", "group by"),
            ("SELECT DISTINCT a FROM t", "distinct"),
            ("SELECT * FROM t LIMIT 5 OFFSET 2", "offset"),
            ("INSERT INTO t VALUES (1)", "column list"),
            ("BEGIN", "transactions"),
            ("SELECT * FROM t WHERE a LIKE 'x%'", "pattern matching"),
            ("SELECT * FROM t WHERE a IN (1, 2)", "in (...)"),
            ("SELECT * FROM t WHERE a BETWEEN 1 AND 2", "between"),
            (
                "CREATE TABLE t (a INTEGER PRIMARY KEY AUTOINCREMENT)",
                "autoincrement",
            ),
            ("CREATE UNIQUE INDEX i ON t (a)", "unique indexes"),
            (
                "CREATE TABLE IF NOT EXISTS t (a INTEGER PRIMARY KEY)",
                "if not exists",
            ),
            ("PRAGMA journal_mode", "not supported"),
        ] {
            let err = parse_sql(input).unwrap_err();
            assert!(
                err.message.contains(needle),
                "{input}\n  wanted a message containing {needle:?}\n  got: {err}"
            );
        }
    }

    #[test]
    fn one_statement_per_call() {
        assert!(parse_sql("select * from t;").is_ok());
        let err = parse_sql("select * from t; select * from u").unwrap_err();
        assert!(err.message.contains("one statement per call"), "{err}");
    }

    #[test]
    fn a_table_without_a_key_is_rejected_at_parse_time() {
        let err = parse_sql("CREATE TABLE t (a INTEGER, b TEXT)").unwrap_err();
        assert!(err.message.contains("needs a primary key"), "{err}");
    }
}
