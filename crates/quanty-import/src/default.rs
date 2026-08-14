//! Reading a column's default out of the create statement.
//!
//! A default matters more than it looks like it should. When
//! `alter table add column` adds a column, the rows that already existed
//! keep their shorter records, and the missing value is the default. So a
//! default we can read is the difference between a column that is nullable
//! because of a schema change and one that is genuinely full.
//!
//! SQLite allows an expression there, `current_timestamp` or `(1 + 2)`.
//! Evaluating those would mean implementing a second engine's expression
//! semantics to guess at values nobody stored, so this reads literals and
//! says plainly when it cannot.

use quanty_core::Value;

/// A literal default, if the text is one.
pub fn parse(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("null") {
        return Some(Value::Null);
    }
    if trimmed.eq_ignore_ascii_case("true") {
        return Some(Value::Bool(true));
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return Some(Value::Bool(false));
    }
    if let Some(rest) = strip_quotes(trimmed, '\'') {
        // sqlite doubles a quote to escape it
        return Some(Value::Text(rest.replace("''", "'")));
    }
    if let Some(hex) = hex_literal(trimmed) {
        return Some(Value::Bytes(hex));
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        return Some(Value::Int(n));
    }
    if let Ok(f) = trimmed.parse::<f64>() {
        // a default that is not a finite number is not one we can store
        return f.is_finite().then_some(Value::Float(f));
    }
    None
}

fn strip_quotes(text: &str, quote: char) -> Option<String> {
    let bytes: Vec<char> = text.chars().collect();
    if bytes.len() >= 2 && bytes[0] == quote && bytes[bytes.len() - 1] == quote {
        Some(bytes[1..bytes.len() - 1].iter().collect())
    } else {
        None
    }
}

/// `x'00ff'`, in either case.
fn hex_literal(text: &str) -> Option<Vec<u8>> {
    let rest = text.strip_prefix('x').or_else(|| text.strip_prefix('X'))?;
    let digits = strip_quotes(rest, '\'')?;
    if digits.len() % 2 != 0 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Vec<char> = digits.chars().collect();
    bytes
        .chunks(2)
        .map(|pair| u8::from_str_radix(&pair.iter().collect::<String>(), 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_come_through() {
        assert_eq!(parse("42"), Some(Value::Int(42)));
        assert_eq!(parse("-7"), Some(Value::Int(-7)));
        assert_eq!(parse("1.5"), Some(Value::Float(1.5)));
        assert_eq!(parse("'hi'"), Some(Value::Text("hi".into())));
        assert_eq!(parse("''"), Some(Value::Text(String::new())));
        assert_eq!(parse("'it''s'"), Some(Value::Text("it's".into())));
        assert_eq!(parse("x'00ff'"), Some(Value::Bytes(vec![0, 255])));
        assert_eq!(parse("X'0A'"), Some(Value::Bytes(vec![10])));
        assert_eq!(parse("NULL"), Some(Value::Null));
        assert_eq!(parse("  42  "), Some(Value::Int(42)));
    }

    #[test]
    fn expressions_are_not_guessed_at() {
        for text in [
            "current_timestamp",
            "CURRENT_DATE",
            "(1 + 2)",
            "(select 1)",
            "x'zz'",
            "'unterminated",
        ] {
            assert_eq!(parse(text), None, "{text} should not have parsed");
        }
    }

    #[test]
    fn a_default_that_is_not_finite_is_not_a_default() {
        assert_eq!(parse("1e400"), None);
        assert_eq!(parse("inf"), None);
    }
}
