//! The QQL lexer.
//!
//! Byte-position tracking on every token so parse errors can point at the
//! exact spot. Keywords are lowercase on purpose: one way to spell things
//! means diffs, logs and docs all look the same.

use crate::error::ParseError;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    Hex(Vec<u8>),

    // punctuation
    LBrace,
    RBrace,
    LParen,
    RParen,
    Comma,
    Colon,
    Dot,
    At,

    // operators
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,

    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    pub token: Token,
    pub at: usize,
}

pub fn lex(source: &str) -> Result<Vec<Spanned>, ParseError> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let at = i;
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'#' => {
                // comment to end of line
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => push(&mut out, Token::LBrace, at, &mut i),
            b'}' => push(&mut out, Token::RBrace, at, &mut i),
            b'(' => push(&mut out, Token::LParen, at, &mut i),
            b')' => push(&mut out, Token::RParen, at, &mut i),
            b',' => push(&mut out, Token::Comma, at, &mut i),
            b':' => push(&mut out, Token::Colon, at, &mut i),
            b'.' => push(&mut out, Token::Dot, at, &mut i),
            b'@' => push(&mut out, Token::At, at, &mut i),
            b'=' => push(&mut out, Token::Eq, at, &mut i),
            b'!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push(Spanned {
                        token: Token::NotEq,
                        at,
                    });
                    i += 2;
                } else {
                    return Err(ParseError::at(at, "expected != here"));
                }
            }
            b'<' => two(&mut out, bytes, &mut i, at, Token::Lt, Token::LtEq),
            b'>' => two(&mut out, bytes, &mut i, at, Token::Gt, Token::GtEq),
            b'+' => two(&mut out, bytes, &mut i, at, Token::Plus, Token::PlusEq),
            b'-' => two(&mut out, bytes, &mut i, at, Token::Minus, Token::MinusEq),
            b'*' => two(&mut out, bytes, &mut i, at, Token::Star, Token::StarEq),
            b'/' => two(&mut out, bytes, &mut i, at, Token::Slash, Token::SlashEq),
            b'%' => push(&mut out, Token::Percent, at, &mut i),
            b'"' => {
                let (s, next) = lex_string(source, i)?;
                out.push(Spanned {
                    token: Token::Str(s),
                    at,
                });
                i = next;
            }
            b'0'..=b'9' => {
                let (token, next) = lex_number(source, i)?;
                out.push(Spanned { token, at });
                i = next;
            }
            _ if b == b'_' || b.is_ascii_alphabetic() => {
                let start = i;
                while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                let word = &source[start..i];
                // x"..." is a hex bytes literal
                if word == "x" && bytes.get(i) == Some(&b'"') {
                    let (s, next) = lex_string(source, i)?;
                    let raw = decode_hex(&s).ok_or_else(|| {
                        ParseError::at(at, "bytes literal wants hex digits, like x\"deadbeef\"")
                    })?;
                    out.push(Spanned {
                        token: Token::Hex(raw),
                        at,
                    });
                    i = next;
                } else {
                    out.push(Spanned {
                        token: Token::Ident(word.to_string()),
                        at,
                    });
                }
            }
            _ => {
                return Err(ParseError::at(
                    at,
                    format!(
                        "unexpected character {:?}",
                        source[i..].chars().next().unwrap()
                    ),
                ))
            }
        }
    }
    out.push(Spanned {
        token: Token::Eof,
        at: bytes.len(),
    });
    Ok(out)
}

fn push(out: &mut Vec<Spanned>, token: Token, at: usize, i: &mut usize) {
    out.push(Spanned { token, at });
    *i += 1;
}

/// Single-char token, or the `=`-suffixed variant.
fn two(out: &mut Vec<Spanned>, bytes: &[u8], i: &mut usize, at: usize, one: Token, eq: Token) {
    if bytes.get(*i + 1) == Some(&b'=') {
        out.push(Spanned { token: eq, at });
        *i += 2;
    } else {
        out.push(Spanned { token: one, at });
        *i += 1;
    }
}

fn lex_string(source: &str, start: usize) -> Result<(String, usize), ParseError> {
    let bytes = source.as_bytes();
    let mut out = String::new();
    let mut i = start + 1; // past the opening quote
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Ok((out, i + 1)),
            b'\\' => {
                let esc = bytes.get(i + 1).copied();
                match esc {
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'n') => out.push('\n'),
                    Some(b't') => out.push('\t'),
                    Some(b'0') => out.push('\0'),
                    _ => return Err(ParseError::at(i, "unknown escape in string")),
                }
                i += 2;
            }
            _ => {
                let ch = source[i..].chars().next().expect("in bounds");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Err(ParseError::at(start, "string never closes"))
}

fn lex_number(source: &str, start: usize) -> Result<(Token, usize), ParseError> {
    let bytes = source.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    if i < bytes.len() && bytes[i] == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
        is_float = true;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        if j < bytes.len() && bytes[j].is_ascii_digit() {
            is_float = true;
            i = j;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    let text = &source[start..i];
    let token = if is_float {
        let f: f64 = text
            .parse()
            .map_err(|_| ParseError::at(start, "bad float literal"))?;
        if !f.is_finite() {
            return Err(ParseError::at(start, "float literal is out of range"));
        }
        Token::Float(f)
    } else {
        Token::Int(
            text.parse()
                .map_err(|_| ParseError::at(start, "integer too large for int"))?,
        )
    };
    Ok((token, i))
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_a_representative_statement() {
        let toks = lex(r#"get users where score >= 10 and name != "elchi" # tail"#).unwrap();
        let kinds: Vec<&Token> = toks.iter().map(|s| &s.token).collect();
        assert!(matches!(kinds[0], Token::Ident(w) if w == "get"));
        assert!(kinds.contains(&&Token::GtEq));
        assert!(kinds.contains(&&Token::NotEq));
        assert!(kinds
            .iter()
            .any(|t| matches!(t, Token::Str(s) if s == "elchi")));
        assert_eq!(kinds.last(), Some(&&Token::Eof));
    }

    #[test]
    fn numbers_and_bytes_literals() {
        let toks = lex(r#"42 3.5 1e3 x"c0ffee""#).unwrap();
        assert_eq!(toks[0].token, Token::Int(42));
        assert_eq!(toks[1].token, Token::Float(3.5));
        assert_eq!(toks[2].token, Token::Float(1000.0));
        assert_eq!(toks[3].token, Token::Hex(vec![0xC0, 0xFF, 0xEE]));
    }

    #[test]
    fn errors_carry_positions() {
        let err = lex("get users where a ~ 1").unwrap_err();
        assert_eq!(err.at, Some(18));
        assert!(lex("\"never ends").is_err());
        assert!(lex("x\"zz\"").is_err());
        assert!(lex("99999999999999999999999").is_err());
    }
}
