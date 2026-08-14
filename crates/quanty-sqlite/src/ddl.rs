//! Reading a `create table` statement.
//!
//! The file says which pages a table lives on, and nothing else about it.
//! Column names, their order, their declared types, which columns form the
//! primary key: all of that exists only as the text of the statement that
//! created the table, kept verbatim in the schema table. So importing
//! anything means parsing SQLite's dialect of `create table`, and this is
//! that parser.
//!
//! It is not the SQL front end from quanty-ql and should not become it.
//! That one parses our dialect, where we decide what is legal. This one
//! parses text another engine already accepted, including everything we do
//! not implement: check constraints, foreign key actions, collations,
//! generated columns, four ways of quoting an identifier, and type names
//! that are arbitrary words. Bending our own parser around a foreign
//! grammar would put that mess in the path of every query we run.
//!
//! Where it cannot understand something it stops rather than guesses.
//! A statement SQLite accepted and we cannot read means our parser has a
//! gap, and a gap that quietly produces a column list of the wrong shape
//! would be read as data of the wrong shape, which is the failure this
//! whole crate is built to prevent.

use crate::error::{Result, SqliteError};

/// A column as the create statement declared it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    /// The declared type, exactly as written, with its arguments: `INTEGER`,
    /// `NVARCHAR(160)`, `DOUBLE PRECISION`. `None` when the column was
    /// declared without one, which SQLite allows.
    pub declared_type: Option<String>,
    pub not_null: bool,
    /// The default, as written. Kept as text because a default can be an
    /// expression, and deciding what a given expression means is a question
    /// for whoever imports it.
    pub default: Option<String>,
    /// A generated column holds no data of its own in the file.
    pub generated: bool,
}

/// One column of a primary key, in key order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyColumn {
    pub name: String,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDef {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// Empty when the table has no primary key at all.
    pub primary_key: Vec<KeyColumn>,
    pub without_rowid: bool,
    pub strict: bool,
    /// Whether the primary key was written as a column constraint that
    /// spelled out `desc`. See `rowid_alias` for why that is worth keeping.
    inline_pk_desc: bool,
}

impl TableDef {
    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// The column that is an alias for the rowid, if the table has one.
    ///
    /// Such a column holds no value of its own in the record; SQLite stores
    /// NULL there and the value lives in the cell's rowid. Getting this
    /// wrong means reading the primary key of an entire table as NULL, or
    /// inventing one, and nothing about it fails loudly, so the rule is
    /// spelled out here and checked against a fixture built from these
    /// exact cases:
    ///
    /// | declaration                       | alias |
    /// |-----------------------------------|-------|
    /// | `x integer primary key`           | yes   |
    /// | `x integer primary key desc`      | no    |
    /// | `x integer, primary key (x)`      | yes   |
    /// | `x integer, primary key (x desc)` | yes   |
    /// | `x int primary key`               | no    |
    ///
    /// The two `desc` rows disagreeing is not a typo. Spelling out `desc`
    /// in a column constraint suppresses the alias, and spelling it out in
    /// a table constraint does not. SQLite documents that as a quirk kept
    /// for backwards compatibility, and every reader has to reproduce it.
    ///
    /// A without rowid table has no rowid to alias, so it never has one.
    pub fn rowid_alias(&self) -> Option<&ColumnDef> {
        if self.without_rowid || self.primary_key.len() != 1 || self.inline_pk_desc {
            return None;
        }
        let key = &self.primary_key[0];
        let column = self.column(&key.name)?;
        match &column.declared_type {
            // exactly "integer", in any mixture of case. "int" does not
            // count, and neither does "integer(8)".
            Some(declared) if declared.trim().eq_ignore_ascii_case("integer") => Some(column),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// tokens
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A bare word: a keyword, a type name, an unquoted identifier. SQLite
    /// does not reserve most of these, so which one it is depends on where
    /// it appears.
    Word(String),
    /// An identifier that was quoted, and therefore is never a keyword.
    Quoted(String),
    /// A string, number or blob literal, kept as written.
    Literal(String),
    Punct(char),
}

/// A token together with where it sits in the original statement, so that
/// type names and defaults can be handed back exactly as they were written
/// rather than reassembled from their parts.
#[derive(Debug, Clone)]
struct Tok {
    token: Token,
    start: usize,
    end: usize,
}

impl Token {
    /// The text of a word or quoted identifier, for keyword comparison.
    fn word(&self) -> Option<&str> {
        match self {
            Token::Word(w) => Some(w),
            _ => None,
        }
    }

    fn is_word(&self, keyword: &str) -> bool {
        matches!(self, Token::Word(w) if w.eq_ignore_ascii_case(keyword))
    }

    fn is_punct(&self, c: char) -> bool {
        matches!(self, Token::Punct(p) if *p == c)
    }

    /// The identifier this token names, quoted or not.
    fn identifier(&self) -> Option<String> {
        match self {
            Token::Word(w) => Some(w.clone()),
            Token::Quoted(q) => Some(q.clone()),
            _ => None,
        }
    }

    /// How this token was written, for reassembling type names and
    /// defaults.
    fn text(&self) -> String {
        match self {
            Token::Word(w) => w.clone(),
            Token::Quoted(q) => format!("\"{}\"", q.replace('"', "\"\"")),
            Token::Literal(l) => l.clone(),
            Token::Punct(p) => p.to_string(),
        }
    }
}

fn tokenize(sql: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = sql.chars().collect();
    // char index to byte offset, with a sentinel so that the end of the
    // last token is addressable
    let mut offsets: Vec<usize> = sql.char_indices().map(|(i, _)| i).collect();
    offsets.push(sql.len());

    let mut tokens: Vec<Tok> = Vec::new();
    let mut at = 0usize;

    while at < chars.len() {
        let c = chars[at];

        if c.is_whitespace() {
            at += 1;
            continue;
        }
        let began = at;

        // -- to end of line
        if c == '-' && chars.get(at + 1) == Some(&'-') {
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
            continue;
        }

        // /* ... */, which sqlite allows to run to end of input unclosed
        if c == '/' && chars.get(at + 1) == Some(&'*') {
            at += 2;
            while at < chars.len() && !(chars[at] == '*' && chars.get(at + 1) == Some(&'/')) {
                at += 1;
            }
            at = (at + 2).min(chars.len());
            continue;
        }

        // the three quoting styles for identifiers, plus strings
        if let Some((open, close)) = match c {
            '"' => Some(('"', '"')),
            '`' => Some(('`', '`')),
            '[' => Some(('[', ']')),
            _ => None,
        } {
            at += 1;
            let mut value = String::new();
            loop {
                let Some(&ch) = chars.get(at) else {
                    return Err(SqliteError::unsupported(format!(
                        "a create statement has an unterminated {open} quoted name"
                    )));
                };
                at += 1;
                if ch == close {
                    // brackets have no escape; the others double the quote
                    if close != ']' && chars.get(at) == Some(&close) {
                        value.push(close);
                        at += 1;
                        continue;
                    }
                    break;
                }
                value.push(ch);
            }
            tokens.push(Tok {
                token: Token::Quoted(value),
                start: offsets[began],
                end: offsets[at],
            });
            continue;
        }

        if c == '\'' {
            let mut text = String::from("'");
            at += 1;
            loop {
                let Some(&ch) = chars.get(at) else {
                    return Err(SqliteError::unsupported(
                        "a create statement has an unterminated string",
                    ));
                };
                at += 1;
                text.push(ch);
                if ch == '\'' {
                    if chars.get(at) == Some(&'\'') {
                        text.push('\'');
                        at += 1;
                        continue;
                    }
                    break;
                }
            }
            tokens.push(Tok {
                token: Token::Literal(text),
                start: offsets[began],
                end: offsets[at],
            });
            continue;
        }

        if c.is_ascii_digit() || (c == '.' && chars.get(at + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let start = at;
            while at < chars.len()
                && (chars[at].is_ascii_alphanumeric() || chars[at] == '.' || {
                    // an exponent's sign belongs to the number
                    (chars[at] == '+' || chars[at] == '-')
                        && matches!(chars[at - 1], 'e' | 'E')
                        && chars[start..at].iter().all(|c| *c != 'x' && *c != 'X')
                })
            {
                at += 1;
            }
            tokens.push(Tok {
                token: Token::Literal(chars[start..at].iter().collect()),
                start: offsets[began],
                end: offsets[at],
            });
            continue;
        }

        if c.is_alphabetic() || c == '_' || !c.is_ascii() {
            let start = at;
            while at < chars.len()
                && (chars[at].is_alphanumeric()
                    || chars[at] == '_'
                    || chars[at] == '$'
                    || !chars[at].is_ascii())
            {
                at += 1;
            }
            let word: String = chars[start..at].iter().collect();
            // x'00ff' is a blob literal, not the identifier x
            if (word.eq_ignore_ascii_case("x")) && chars.get(at) == Some(&'\'') {
                let mut text = word;
                text.push('\'');
                at += 1;
                while let Some(&ch) = chars.get(at) {
                    at += 1;
                    text.push(ch);
                    if ch == '\'' {
                        break;
                    }
                }
                tokens.push(Tok {
                    token: Token::Literal(text),
                    start: offsets[began],
                    end: offsets[at],
                });
            } else {
                tokens.push(Tok {
                    token: Token::Word(word),
                    start: offsets[began],
                    end: offsets[at],
                });
            }
            continue;
        }

        tokens.push(Tok {
            token: Token::Punct(c),
            start: offsets[began],
            end: offsets[at + 1],
        });
        at += 1;
    }

    Ok(tokens)
}

// ---------------------------------------------------------------------------
// parser
// ---------------------------------------------------------------------------

/// Words that end a column's type name and begin its constraints.
const CONSTRAINT_WORDS: &[&str] = &[
    "constraint",
    "primary",
    "not",
    "null",
    "unique",
    "check",
    "default",
    "collate",
    "references",
    "generated",
    "as",
];

struct Parser<'a> {
    sql: &'a str,
    tokens: Vec<Tok>,
    at: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|t| &t.token)
    }

    fn at_word(&self, offset: usize, keyword: &str) -> bool {
        self.tokens
            .get(self.at + offset)
            .is_some_and(|t| t.token.is_word(keyword))
    }

    /// Where the token at `index` starts in the original statement.
    fn start_of(&self, index: usize) -> usize {
        match self.tokens.get(index) {
            Some(t) => t.start,
            None => self.sql.len(),
        }
    }

    /// Where the token before `index` ends.
    fn end_before(&self, index: usize) -> usize {
        match index.checked_sub(1).and_then(|i| self.tokens.get(i)) {
            Some(t) => t.end,
            None => 0,
        }
    }

    /// The original text spanned by the tokens from `from` up to the
    /// current position, exactly as it was written.
    fn text_since(&self, from: usize) -> String {
        let start = self.start_of(from);
        let end = self.end_before(self.at);
        if end <= start {
            String::new()
        } else {
            self.sql[start..end].trim().to_string()
        }
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).map(|t| t.token.clone());
        if token.is_some() {
            self.at += 1;
        }
        token
    }

    fn eat_word(&mut self, keyword: &str) -> bool {
        if self.peek().is_some_and(|t| t.is_word(keyword)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn eat_punct(&mut self, c: char) -> bool {
        if self.peek().is_some_and(|t| t.is_punct(c)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, keyword: &str) -> Result<()> {
        if self.eat_word(keyword) {
            Ok(())
        } else {
            Err(self.gap(&format!("expected {keyword}")))
        }
    }

    fn expect_punct(&mut self, c: char) -> Result<()> {
        if self.eat_punct(c) {
            Ok(())
        } else {
            Err(self.gap(&format!("expected {c}")))
        }
    }

    fn identifier(&mut self) -> Result<String> {
        match self.next().and_then(|t| t.identifier()) {
            Some(name) => Ok(name),
            None => Err(self.gap("expected a name")),
        }
    }

    /// A statement this parser cannot read. Not `Malformed`: the file is
    /// fine and SQLite accepted the statement, so this is our gap and the
    /// message says which token we tripped on.
    fn gap(&self, what: &str) -> SqliteError {
        let found = match self.tokens.get(self.at) {
            Some(t) => format!("{:?}", t.token.text()),
            None => "the end of the statement".to_string(),
        };
        SqliteError::unsupported(format!(
            "cannot read this create statement: {what}, found {found} at token {}",
            self.at
        ))
    }

    fn at_constraint_word(&self) -> bool {
        self.peek()
            .and_then(|t| t.word())
            .is_some_and(|w| CONSTRAINT_WORDS.iter().any(|k| w.eq_ignore_ascii_case(k)))
    }

    /// Step over a parenthesised group, returning it exactly as written.
    /// Assumes the opening paren is next.
    fn balanced(&mut self) -> Result<String> {
        let from = self.at;
        self.expect_punct('(')?;
        let mut depth = 1usize;
        while depth > 0 {
            let Some(token) = self.next() else {
                return Err(self.gap("expected a closing paren"));
            };
            if token.is_punct('(') {
                depth += 1;
            } else if token.is_punct(')') {
                depth -= 1;
            }
        }
        Ok(self.text_since(from))
    }

    /// Skip to the next comma or closing paren at the top level, which is
    /// how clauses we do not interpret are stepped over.
    fn skip_to_end_of_item(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                None => return Ok(()),
                Some(t) if t.is_punct(',') || t.is_punct(')') => return Ok(()),
                Some(t) if t.is_punct('(') => {
                    self.balanced()?;
                }
                _ => {
                    self.at += 1;
                }
            }
        }
    }

    /// `on conflict <action>`, which may follow several constraints.
    fn eat_conflict_clause(&mut self) {
        if self.at_word(0, "on") && self.at_word(1, "conflict") {
            self.at += 2;
            if self.peek().and_then(|t| t.word()).is_some() {
                self.at += 1;
            }
        }
    }
}

/// Parse a `create table` statement into the shape of the table it makes.
pub fn parse_create_table(sql: &str) -> Result<TableDef> {
    let tokens = tokenize(sql)?;
    let mut parser = Parser { sql, tokens, at: 0 };

    parser.expect_word("create")?;
    // temp tables never appear in a database file's schema, but the
    // statement is legal and costs one line to accept
    let _ = parser.eat_word("temp") || parser.eat_word("temporary");
    parser.expect_word("table")?;
    if parser.eat_word("if") {
        parser.expect_word("not")?;
        parser.expect_word("exists")?;
    }

    let mut name = parser.identifier()?;
    if parser.eat_punct('.') {
        // schema qualified: the part after the dot is the table
        name = parser.identifier()?;
    }

    if parser.peek().is_some_and(|t| t.is_word("as")) {
        return Err(SqliteError::unsupported(format!(
            "{name} was created by `create table ... as select`, so the file does not record \
             its column types; this reader cannot import it"
        )));
    }

    parser.expect_punct('(')?;

    let mut columns: Vec<ColumnDef> = Vec::new();
    let mut primary_key: Vec<KeyColumn> = Vec::new();
    let mut inline_pk_desc = false;

    loop {
        if parser.peek().is_none() {
            return Err(parser.gap("the column list is not closed"));
        }
        if parser.eat_punct(')') {
            break;
        }

        // a table constraint, rather than another column
        let is_table_constraint = {
            // a table constraint may carry a name, in which case the
            // keyword that says which kind it is comes two tokens later
            let offset = if parser.at_word(0, "constraint") {
                2
            } else {
                0
            };
            parser.at_word(offset, "primary")
                || parser.at_word(offset, "unique")
                || parser.at_word(offset, "check")
                || parser.at_word(offset, "foreign")
        };

        if is_table_constraint {
            if parser.eat_word("constraint") {
                parser.identifier()?;
            }
            if parser.eat_word("primary") {
                parser.expect_word("key")?;
                if !primary_key.is_empty() {
                    return Err(SqliteError::unsupported(format!(
                        "{name} declares more than one primary key"
                    )));
                }
                parser.expect_punct('(')?;
                loop {
                    let column = parser.identifier()?;
                    if parser.eat_word("collate") {
                        parser.identifier()?;
                    }
                    let descending = if parser.eat_word("desc") {
                        true
                    } else {
                        parser.eat_word("asc");
                        false
                    };
                    primary_key.push(KeyColumn {
                        name: column,
                        descending,
                    });
                    if parser.eat_punct(',') {
                        continue;
                    }
                    parser.expect_punct(')')?;
                    break;
                }
                parser.eat_conflict_clause();
            } else {
                // unique, check and foreign key say nothing about the shape
                // of a row, so they are stepped over
                parser.skip_to_end_of_item()?;
            }
        } else {
            let column_name = parser.identifier()?;
            let type_from = parser.at;
            while !parser.at_constraint_word() {
                match parser.peek() {
                    None => break,
                    Some(t) if t.is_punct(',') || t.is_punct(')') => break,
                    Some(t) if t.is_punct('(') => {
                        parser.balanced()?;
                    }
                    Some(_) => {
                        parser.at += 1;
                    }
                }
            }
            let declared_type = parser.text_since(type_from);

            let mut column = ColumnDef {
                name: column_name,
                declared_type: if declared_type.is_empty() {
                    None
                } else {
                    Some(declared_type)
                },
                not_null: false,
                default: None,
                generated: false,
            };

            // column constraints, in any order and any number
            loop {
                if parser.eat_word("constraint") {
                    parser.identifier()?;
                    continue;
                }
                if parser.eat_word("primary") {
                    parser.expect_word("key")?;
                    if !primary_key.is_empty() {
                        return Err(SqliteError::unsupported(format!(
                            "{name} declares more than one primary key"
                        )));
                    }
                    let descending = if parser.eat_word("desc") {
                        true
                    } else {
                        parser.eat_word("asc");
                        false
                    };
                    inline_pk_desc = descending;
                    primary_key.push(KeyColumn {
                        name: column.name.clone(),
                        descending,
                    });
                    parser.eat_conflict_clause();
                    parser.eat_word("autoincrement");
                    continue;
                }
                if parser.eat_word("not") {
                    parser.expect_word("null")?;
                    column.not_null = true;
                    parser.eat_conflict_clause();
                    continue;
                }
                if parser.eat_word("null") {
                    parser.eat_conflict_clause();
                    continue;
                }
                if parser.eat_word("unique") {
                    parser.eat_conflict_clause();
                    continue;
                }
                if parser.eat_word("check") {
                    parser.balanced()?;
                    continue;
                }
                if parser.eat_word("default") {
                    let from = parser.at;
                    if parser.peek().is_some_and(|t| t.is_punct('(')) {
                        parser.balanced()?;
                    } else {
                        // a literal, a keyword like current_timestamp, or a
                        // signed number
                        let _ = parser.eat_punct('-') || parser.eat_punct('+');
                        if parser.next().is_none() {
                            return Err(parser.gap("expected a default value"));
                        }
                    }
                    column.default = Some(parser.text_since(from));
                    continue;
                }
                if parser.eat_word("collate") {
                    parser.identifier()?;
                    continue;
                }
                if parser.eat_word("references") {
                    parser.skip_to_end_of_item()?;
                    continue;
                }
                if parser.eat_word("generated") {
                    parser.expect_word("always")?;
                    parser.expect_word("as")?;
                    parser.balanced()?;
                    let _ = parser.eat_word("stored") || parser.eat_word("virtual");
                    column.generated = true;
                    continue;
                }
                if parser.peek().is_some_and(|t| t.is_word("as")) {
                    parser.at += 1;
                    parser.balanced()?;
                    let _ = parser.eat_word("stored") || parser.eat_word("virtual");
                    column.generated = true;
                    continue;
                }
                break;
            }

            columns.push(column);
        }

        if parser.eat_punct(',') {
            continue;
        }
        parser.expect_punct(')')?;
        break;
    }

    // table options: without rowid and strict, in either order
    let mut without_rowid = false;
    let mut strict = false;
    loop {
        if parser.eat_word("without") {
            parser.expect_word("rowid")?;
            without_rowid = true;
        } else if parser.eat_word("strict") {
            strict = true;
        } else {
            break;
        }
        if !parser.eat_punct(',') {
            break;
        }
    }
    let _ = parser.eat_punct(';');

    if columns.is_empty() {
        return Err(SqliteError::unsupported(format!(
            "{name} was parsed as having no columns"
        )));
    }
    for key in &primary_key {
        if !columns
            .iter()
            .any(|c| c.name.eq_ignore_ascii_case(&key.name))
        {
            return Err(SqliteError::unsupported(format!(
                "{name} has a primary key over {}, which is not one of its columns",
                key.name
            )));
        }
    }

    Ok(TableDef {
        name,
        columns,
        primary_key,
        without_rowid,
        strict,
        inline_pk_desc,
    })
}
