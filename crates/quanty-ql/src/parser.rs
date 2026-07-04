//! The QQL parser. Recursive descent, no generator, because the error
//! messages are the product here.

use quanty_core::Value;

use crate::ast::*;
use crate::error::ParseError;
use crate::lexer::{lex, Spanned, Token};

/// Parse exactly one statement. Trailing input is an error.
pub fn parse(source: &str) -> Result<Statement, ParseError> {
    let tokens = lex(source)?;
    let mut p = Parser { tokens, pos: 0 };
    let stmt = p.statement()?;
    p.expect_eof()?;
    Ok(stmt)
}

struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos].token
    }

    fn at(&self) -> usize {
        self.tokens[self.pos].at
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].token.clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == token {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: Token, what: &str) -> Result<(), ParseError> {
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

    /// Is the next token this exact keyword? Keywords are just idents; QQL
    /// has no reserved words, context decides.
    fn eat_kw(&mut self, kw: &str) -> bool {
        if matches!(self.peek(), Token::Ident(w) if w == kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<(), ParseError> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(ParseError::at(
                self.at(),
                format!("expected the word '{kw}', found {}", show(self.peek())),
            ))
        }
    }

    fn ident(&mut self, what: &str) -> Result<String, ParseError> {
        match self.peek().clone() {
            Token::Ident(w) => {
                self.bump();
                Ok(w)
            }
            other => Err(ParseError::at(
                self.at(),
                format!("expected {what}, found {}", show(&other)),
            )),
        }
    }

    fn expect_eof(&mut self) -> Result<(), ParseError> {
        if self.peek() == &Token::Eof {
            Ok(())
        } else {
            Err(ParseError::at(
                self.at(),
                format!("statement ended, but there is more: {}", show(self.peek())),
            ))
        }
    }

    // -----------------------------------------------------------------
    // statements
    // -----------------------------------------------------------------

    fn statement(&mut self) -> Result<Statement, ParseError> {
        let at = self.at();
        let verb =
            self.ident("a statement (table, get, put, set, del, drop, index, show, explain)")?;
        match verb.as_str() {
            "table" => self.table_def(),
            "drop" => {
                self.expect_kw("table")?;
                Ok(Statement::DropTable { name: self.ident("a table name")? })
            }
            "put" => self.put(),
            "get" => self.get(),
            "set" => self.set(),
            "del" => self.del(),
            "index" => {
                let table = self.ident("a table name")?;
                self.expect(Token::Dot, "'.' as in index users.name")?;
                let column = self.ident("a column name")?;
                Ok(Statement::IndexDef { table, column })
            }
            "show" => {
                self.expect_kw("tables")?;
                Ok(Statement::ShowTables)
            }
            "explain" => Ok(Statement::Explain(Box::new(self.statement()?))),
            other => Err(ParseError::at(
                at,
                format!("'{other}' is not a statement (try table, get, put, set, del, drop, index, show, explain)"),
            )),
        }
    }

    fn table_def(&mut self) -> Result<Statement, ParseError> {
        let name = self.ident("a table name")?;
        self.expect(Token::LBrace, "'{' to open the column list")?;
        let mut columns = Vec::new();
        loop {
            if self.eat(&Token::RBrace) {
                break;
            }
            columns.push(self.column_def()?);
            // commas between columns are optional, newlines do the job in
            // multi-line definitions
            self.eat(&Token::Comma);
        }
        if columns.is_empty() {
            return Err(ParseError::at(
                self.at(),
                "a table needs at least one column",
            ));
        }
        Ok(Statement::TableDef(TableDef { name, columns }))
    }

    fn column_def(&mut self) -> Result<ColumnDef, ParseError> {
        let name = self.ident("a column name")?;
        self.expect(Token::Colon, "':' between column name and type")?;
        let ty_at = self.at();
        let ty = match self
            .ident("a type (int, float, text, bytes, bool)")?
            .as_str()
        {
            "int" => TypeName::Int,
            "float" => TypeName::Float,
            "text" => TypeName::Text,
            "bytes" => TypeName::Bytes,
            "bool" => TypeName::Bool,
            other => {
                return Err(ParseError::at(
                    ty_at,
                    format!("'{other}' is not a type (int, float, text, bytes, bool)"),
                ))
            }
        };
        // `?` suffix would be nice, but '?' is not a token; nullability is
        // spelled as the @null attribute to keep the lexer small
        let mut col = ColumnDef {
            name,
            ty,
            nullable: false,
            key: false,
            index: false,
            default: None,
        };
        loop {
            if self.eat(&Token::At) {
                let attr_at = self.at();
                match self.ident("an attribute (key, index, null)")?.as_str() {
                    "key" => col.key = true,
                    "index" => col.index = true,
                    "null" => col.nullable = true,
                    other => {
                        return Err(ParseError::at(
                            attr_at,
                            format!("'@{other}' is not an attribute (@key, @index, @null)"),
                        ))
                    }
                }
            } else if self.eat(&Token::Eq) {
                let at = self.at();
                col.default = Some(self.literal_value()?.ok_or_else(|| {
                    ParseError::at(at, "defaults must be literals, not expressions")
                })?);
            } else {
                break;
            }
        }
        Ok(col)
    }

    fn put(&mut self) -> Result<Statement, ParseError> {
        let table = self.ident("a table name")?;
        let mut rows = Vec::new();
        loop {
            self.expect(Token::LBrace, "'{' to open a row")?;
            let mut fields = Vec::new();
            loop {
                if self.eat(&Token::RBrace) {
                    break;
                }
                let col = self.ident("a column name")?;
                self.expect(Token::Colon, "':' between column and value")?;
                let expr = self.expr()?;
                fields.push((col, expr));
                self.eat(&Token::Comma);
            }
            if fields.is_empty() {
                return Err(ParseError::at(self.at(), "a row needs at least one column"));
            }
            rows.push(fields);
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        Ok(Statement::Put { table, rows })
    }

    fn get(&mut self) -> Result<Statement, ParseError> {
        let table = self.ident("a table name")?;
        let projection = if self.eat(&Token::LBrace) {
            let mut cols = Vec::new();
            loop {
                cols.push(self.ident("a column name")?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(Token::RBrace, "'}' to close the column list")?;
            Some(cols)
        } else {
            None
        };
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let order = if self.eat_kw("order") {
            self.expect_kw("by")?;
            let col = self.ident("a column name")?;
            let dir = if self.eat_kw("desc") {
                Direction::Desc
            } else {
                self.eat_kw("asc");
                Direction::Asc
            };
            Some((col, dir))
        } else {
            None
        };
        let limit = if self.eat_kw("limit") {
            let at = self.at();
            match self.bump() {
                Token::Int(n) if n >= 0 => Some(n as u64),
                _ => return Err(ParseError::at(at, "limit wants a non-negative integer")),
            }
        } else {
            None
        };
        Ok(Statement::Get(Get {
            table,
            projection,
            filter,
            order,
            limit,
        }))
    }

    fn set(&mut self) -> Result<Statement, ParseError> {
        let table = self.ident("a table name")?;
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        self.expect(Token::LBrace, "'{' to open the assignments")?;
        let mut assigns = Vec::new();
        loop {
            if self.eat(&Token::RBrace) {
                break;
            }
            let column = self.ident("a column name")?;
            let op_at = self.at();
            let op = self.bump();
            let rhs = self.expr()?;
            let expr = match op {
                Token::Eq => rhs,
                Token::PlusEq => desugar(&column, BinaryOp::Add, rhs),
                Token::MinusEq => desugar(&column, BinaryOp::Sub, rhs),
                Token::StarEq => desugar(&column, BinaryOp::Mul, rhs),
                Token::SlashEq => desugar(&column, BinaryOp::Div, rhs),
                other => {
                    return Err(ParseError::at(
                        op_at,
                        format!("expected =, +=, -=, *= or /=, found {}", show(&other)),
                    ))
                }
            };
            assigns.push(Assign { column, expr });
            self.eat(&Token::Comma);
        }
        if assigns.is_empty() {
            return Err(ParseError::at(
                self.at(),
                "set needs at least one assignment",
            ));
        }
        Ok(Statement::Set {
            table,
            filter,
            assigns,
        })
    }

    fn del(&mut self) -> Result<Statement, ParseError> {
        let table = self.ident("a table name")?;
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Statement::Del { table, filter })
    }

    // -----------------------------------------------------------------
    // expressions, standard precedence climbing
    // -----------------------------------------------------------------

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
        if self.eat_kw("not") {
            Ok(Expr::Unary(UnaryOp::Not, Box::new(self.not_expr()?)))
        } else {
            self.cmp_expr()
        }
    }

    fn cmp_expr(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.add_expr()?;
        let op = match self.peek() {
            Token::Eq => BinaryOp::Eq,
            Token::NotEq => BinaryOp::NotEq,
            Token::Lt => BinaryOp::Lt,
            Token::LtEq => BinaryOp::LtEq,
            Token::Gt => BinaryOp::Gt,
            Token::GtEq => BinaryOp::GtEq,
            _ => return Ok(lhs),
        };
        self.bump();
        let rhs = self.add_expr()?;
        Ok(Expr::Binary(Box::new(lhs), op, Box::new(rhs)))
    }

    fn add_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinaryOp::Add,
                Token::Minus => BinaryOp::Sub,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.mul_expr()?;
            lhs = Expr::Binary(Box::new(lhs), op, Box::new(rhs));
        }
    }

    fn mul_expr(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinaryOp::Mul,
                Token::Slash => BinaryOp::Div,
                Token::Percent => BinaryOp::Mod,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.unary_expr()?;
            lhs = Expr::Binary(Box::new(lhs), op, Box::new(rhs));
        }
    }

    fn unary_expr(&mut self) -> Result<Expr, ParseError> {
        if self.eat(&Token::Minus) {
            return Ok(Expr::Unary(UnaryOp::Neg, Box::new(self.unary_expr()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        if let Some(v) = self.literal_value()? {
            return Ok(Expr::Literal(v));
        }
        match self.peek().clone() {
            Token::Ident(w) => {
                self.bump();
                Ok(Expr::Column(w))
            }
            Token::LParen => {
                self.bump();
                let inner = self.expr()?;
                self.expect(Token::RParen, "')' to close the parenthesis")?;
                Ok(inner)
            }
            other => Err(ParseError::at(
                self.at(),
                format!("expected a value, column or '(', found {}", show(&other)),
            )),
        }
    }

    /// Literal if the next token is one; `true`, `false` and `null` are
    /// contextual words, not reserved.
    fn literal_value(&mut self) -> Result<Option<Value>, ParseError> {
        let v = match self.peek().clone() {
            Token::Int(i) => Value::Int(i),
            Token::Float(f) => Value::Float(f),
            Token::Str(s) => Value::Text(s),
            Token::Hex(b) => Value::Bytes(b),
            Token::Ident(w) if w == "true" => Value::Bool(true),
            Token::Ident(w) if w == "false" => Value::Bool(false),
            Token::Ident(w) if w == "null" => Value::Null,
            _ => return Ok(None),
        };
        self.bump();
        Ok(Some(v))
    }
}

fn desugar(column: &str, op: BinaryOp, rhs: Expr) -> Expr {
    Expr::Binary(
        Box::new(Expr::Column(column.to_string())),
        op,
        Box::new(rhs),
    )
}

fn show(token: &Token) -> String {
    match token {
        Token::Ident(w) => format!("'{w}'"),
        Token::Int(i) => format!("the number {i}"),
        Token::Float(f) => format!("the number {f}"),
        Token::Str(s) => format!("the string {s:?}"),
        Token::Hex(_) => "a bytes literal".to_string(),
        Token::Eof => "the end of the statement".to_string(),
        Token::LBrace => "'{'".to_string(),
        Token::RBrace => "'}'".to_string(),
        Token::LParen => "'('".to_string(),
        Token::RParen => "')'".to_string(),
        Token::Comma => "','".to_string(),
        Token::Colon => "':'".to_string(),
        Token::Dot => "'.'".to_string(),
        Token::At => "'@'".to_string(),
        Token::Eq => "'='".to_string(),
        Token::NotEq => "'!='".to_string(),
        Token::Lt => "'<'".to_string(),
        Token::LtEq => "'<='".to_string(),
        Token::Gt => "'>'".to_string(),
        Token::GtEq => "'>='".to_string(),
        Token::Plus => "'+'".to_string(),
        Token::Minus => "'-'".to_string(),
        Token::Star => "'*'".to_string(),
        Token::Slash => "'/'".to_string(),
        Token::Percent => "'%'".to_string(),
        Token::PlusEq => "'+='".to_string(),
        Token::MinusEq => "'-='".to_string(),
        Token::StarEq => "'*='".to_string(),
        Token::SlashEq => "'/='".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_architecture_examples() {
        // straight from docs/ARCHITECTURE.md
        parse("table users {\n  id:    int  @key\n  name:  text @index\n  score: int = 0\n}")
            .unwrap();
        parse("get users where score > 100 order by score desc limit 10").unwrap();
        parse("set users where id = 1 { score += 5 }").unwrap();
    }

    #[test]
    fn desugars_compound_assignment() {
        let Statement::Set { assigns, .. } = parse("set t { a += 2 }").unwrap() else {
            panic!("expected set")
        };
        assert_eq!(
            assigns[0].expr,
            Expr::Binary(
                Box::new(Expr::Column("a".into())),
                BinaryOp::Add,
                Box::new(Expr::Literal(Value::Int(2))),
            )
        );
    }

    #[test]
    fn precedence_is_sane() {
        let Statement::Get(get) = parse("get t where a = 1 or b = 2 and c < 3 + 4 * 5").unwrap()
        else {
            panic!("expected get")
        };
        // or(a=1, and(b=2, c < (3 + (4*5))))
        let Expr::Binary(_, BinaryOp::Or, rhs) = get.filter.unwrap() else {
            panic!("or must bind loosest")
        };
        let Expr::Binary(_, BinaryOp::And, _) = *rhs else {
            panic!("and under or")
        };
    }

    #[test]
    fn error_messages_point_at_the_problem() {
        let err = parse("get users where score >").unwrap_err();
        assert!(err.to_string().contains("expected a value"), "got: {err}");
        let err = parse("table t { a: intt @key }").unwrap_err();
        assert!(err.to_string().contains("not a type"), "got: {err}");
        let err = parse("get users limit -1").unwrap_err();
        assert!(err.to_string().contains("non-negative"), "got: {err}");
        let err = parse("get users trailing").unwrap_err();
        assert!(err.to_string().contains("there is more"), "got: {err}");
    }

    #[test]
    fn multiple_rows_in_one_put() {
        let Statement::Put { rows, .. } = parse(r#"put t { a: 1 }, { a: 2 }, { a: 3 }"#).unwrap()
        else {
            panic!("expected put")
        };
        assert_eq!(rows.len(), 3);
    }
}
