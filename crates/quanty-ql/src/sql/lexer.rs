//! The SQL lexer.
//!
//! Same shape as the QQL lexer: byte position on every token, errors point
//! at the exact spot. The differences are all dialect. Keywords are matched
//! case-insensitively (the parser's job; the lexer preserves case), strings
//! are single quoted with '' as the escape and no backslash escapes,
//! identifiers can be quoted three ways ("x", [x], `x`), comments are
//! -- line and /* block */, and there are a few extra operators (<> and ==
//! as aliases, || for concatenation).

use crate::error::ParseError;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    /// A bare word: keyword or identifier, original case preserved.
    Word(String),
    /// A quoted identifier. Quoting bypasses keyword recognition.
    Quoted(String),
    Int(i64),
    Float(f64),
    Str(String),
    Blob(Vec<u8>),

    // punctuation
    LParen,
    RParen,
    Comma,
    Semi,
    Dot,

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
    Concat,

    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Spanned {
    pub token: Tok,
    pub at: usize,
}

pub(crate) fn lex(source: &str) -> Result<Vec<Spanned>, ParseError> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let at = i;
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                // line comment
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                // block comment, not nested (same as sqlite)
                let mut j = i + 2;
                loop {
                    match bytes.get(j) {
                        Some(b'*') if bytes.get(j + 1) == Some(&b'/') => {
                            i = j + 2;
                            break;
                        }
                        Some(_) => j += 1,
                        None => return Err(ParseError::at(at, "comment never closes")),
                    }
                }
            }
            b'(' => push(&mut out, Tok::LParen, at, &mut i),
            b')' => push(&mut out, Tok::RParen, at, &mut i),
            b',' => push(&mut out, Tok::Comma, at, &mut i),
            b';' => push(&mut out, Tok::Semi, at, &mut i),
            b'.' if !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                push(&mut out, Tok::Dot, at, &mut i)
            }
            b'=' => {
                // both = and == mean equality
                let len = if bytes.get(i + 1) == Some(&b'=') {
                    2
                } else {
                    1
                };
                out.push(Spanned { token: Tok::Eq, at });
                i += len;
            }
            b'!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push(Spanned {
                        token: Tok::NotEq,
                        at,
                    });
                    i += 2;
                } else {
                    return Err(ParseError::at(at, "expected != here"));
                }
            }
            b'<' => match bytes.get(i + 1) {
                Some(b'=') => {
                    out.push(Spanned {
                        token: Tok::LtEq,
                        at,
                    });
                    i += 2;
                }
                Some(b'>') => {
                    out.push(Spanned {
                        token: Tok::NotEq,
                        at,
                    });
                    i += 2;
                }
                Some(b'<') => {
                    return Err(ParseError::at(at, "bitwise operators are not supported"))
                }
                _ => push(&mut out, Tok::Lt, at, &mut i),
            },
            b'>' => match bytes.get(i + 1) {
                Some(b'=') => {
                    out.push(Spanned {
                        token: Tok::GtEq,
                        at,
                    });
                    i += 2;
                }
                Some(b'>') => {
                    return Err(ParseError::at(at, "bitwise operators are not supported"))
                }
                _ => push(&mut out, Tok::Gt, at, &mut i),
            },
            b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    out.push(Spanned {
                        token: Tok::Concat,
                        at,
                    });
                    i += 2;
                } else {
                    return Err(ParseError::at(at, "bitwise operators are not supported"));
                }
            }
            b'&' | b'~' => return Err(ParseError::at(at, "bitwise operators are not supported")),
            b'+' => push(&mut out, Tok::Plus, at, &mut i),
            b'-' => push(&mut out, Tok::Minus, at, &mut i),
            b'*' => push(&mut out, Tok::Star, at, &mut i),
            b'/' => push(&mut out, Tok::Slash, at, &mut i),
            b'%' => push(&mut out, Tok::Percent, at, &mut i),
            b'?' | b':' | b'@' | b'$' => {
                return Err(ParseError::at(
                    at,
                    "parameter placeholders are not supported; write the value inline",
                ))
            }
            b'\'' => {
                let (s, next) = lex_string(source, i)?;
                out.push(Spanned {
                    token: Tok::Str(s),
                    at,
                });
                i = next;
            }
            b'"' => {
                let (s, next) = lex_quoted(source, i, b'"', true)?;
                out.push(Spanned {
                    token: Tok::Quoted(s),
                    at,
                });
                i = next;
            }
            b'[' => {
                // bracket quoting has no escape; ] always closes
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != b']' {
                    j += 1;
                }
                if j == bytes.len() {
                    return Err(ParseError::at(at, "identifier never closes"));
                }
                out.push(Spanned {
                    token: Tok::Quoted(source[i + 1..j].to_string()),
                    at,
                });
                i = j + 1;
            }
            b'`' => {
                let (s, next) = lex_quoted(source, i, b'`', true)?;
                out.push(Spanned {
                    token: Tok::Quoted(s),
                    at,
                });
                i = next;
            }
            b'0' if matches!(bytes.get(i + 1), Some(b'x') | Some(b'X')) => {
                let start = i + 2;
                let mut j = start;
                while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
                    j += 1;
                }
                if j == start {
                    return Err(ParseError::at(at, "hex literal wants digits after 0x"));
                }
                // sqlite reads hex as the 64 bit two's complement pattern
                let value = u64::from_str_radix(&source[start..j], 16)
                    .map_err(|_| ParseError::at(at, "hex literal too large for int"))?;
                out.push(Spanned {
                    token: Tok::Int(value as i64),
                    at,
                });
                i = j;
            }
            b'0'..=b'9' | b'.' => {
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
                // x'...' is a blob literal
                if (word == "x" || word == "X") && bytes.get(i) == Some(&b'\'') {
                    let (s, next) = lex_string(source, i)?;
                    let raw = decode_hex(&s).ok_or_else(|| {
                        ParseError::at(at, "blob literal wants hex digits, like x'deadbeef'")
                    })?;
                    out.push(Spanned {
                        token: Tok::Blob(raw),
                        at,
                    });
                    i = next;
                } else {
                    out.push(Spanned {
                        token: Tok::Word(word.to_string()),
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
        token: Tok::Eof,
        at: bytes.len(),
    });
    Ok(out)
}

fn push(out: &mut Vec<Spanned>, token: Tok, at: usize, i: &mut usize) {
    out.push(Spanned { token, at });
    *i += 1;
}

/// A '...' string. The only escape is '' for a literal quote; backslashes
/// are ordinary characters, exactly like sqlite.
fn lex_string(source: &str, start: usize) -> Result<(String, usize), ParseError> {
    let bytes = source.as_bytes();
    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if bytes.get(i + 1) == Some(&b'\'') => {
                out.push('\'');
                i += 2;
            }
            b'\'' => return Ok((out, i + 1)),
            _ => {
                let ch = source[i..].chars().next().expect("in bounds");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Err(ParseError::at(start, "string never closes"))
}

/// A quoted identifier delimited by `quote`, doubled quote as the escape.
fn lex_quoted(
    source: &str,
    start: usize,
    quote: u8,
    doubled_escape: bool,
) -> Result<(String, usize), ParseError> {
    let bytes = source.as_bytes();
    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if doubled_escape && bytes.get(i + 1) == Some(&quote) {
                out.push(quote as char);
                i += 2;
                continue;
            }
            return Ok((out, i + 1));
        }
        let ch = source[i..].chars().next().expect("in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    Err(ParseError::at(start, "identifier never closes"))
}

/// Numbers, sqlite style: 42, 1.5, .5, 5., 2e3, 1.5e-3. Int literals that
/// do not fit an i64 and float literals that overflow to infinity are
/// errors, the same rules as QQL.
fn lex_number(source: &str, start: usize) -> Result<(Tok, usize), ParseError> {
    let bytes = source.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    if i < bytes.len() && bytes[i] == b'.' {
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
        Tok::Float(f)
    } else {
        Tok::Int(
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
    fn keywords_keep_their_case_strings_escape_with_doubled_quotes() {
        let toks = lex("SELECT Name FROM t WHERE a <> 'it''s' -- tail").unwrap();
        let kinds: Vec<&Tok> = toks.iter().map(|s| &s.token).collect();
        assert!(matches!(kinds[0], Tok::Word(w) if w == "SELECT"));
        assert!(matches!(kinds[1], Tok::Word(w) if w == "Name"));
        assert!(kinds.contains(&&Tok::NotEq));
        assert!(kinds
            .iter()
            .any(|t| matches!(t, Tok::Str(s) if s == "it's")));
        assert_eq!(kinds.last(), Some(&&Tok::Eof));
    }

    #[test]
    fn three_quoting_styles_and_blobs() {
        let toks = lex(r#""or""der" [we ird] `back``tick` x'C0FFEE'"#).unwrap();
        assert_eq!(toks[0].token, Tok::Quoted("or\"der".into()));
        assert_eq!(toks[1].token, Tok::Quoted("we ird".into()));
        assert_eq!(toks[2].token, Tok::Quoted("back`tick".into()));
        assert_eq!(toks[3].token, Tok::Blob(vec![0xC0, 0xFF, 0xEE]));
    }

    #[test]
    fn number_shapes() {
        let toks = lex("42 1.5 .5 5. 2e3 0x1f").unwrap();
        assert_eq!(toks[0].token, Tok::Int(42));
        assert_eq!(toks[1].token, Tok::Float(1.5));
        assert_eq!(toks[2].token, Tok::Float(0.5));
        assert_eq!(toks[3].token, Tok::Float(5.0));
        assert_eq!(toks[4].token, Tok::Float(2000.0));
        assert_eq!(toks[5].token, Tok::Int(31));
    }

    #[test]
    fn errors_carry_positions() {
        assert_eq!(lex("a ~ 1").unwrap_err().at, Some(2));
        assert!(lex("'never ends").is_err());
        assert!(lex("/* never ends").is_err());
        assert!(lex("x'zz'").is_err());
        assert!(lex("select ?").is_err());
        assert!(lex("99999999999999999999999").is_err());
        assert!(lex("1e999").is_err());
    }
}
