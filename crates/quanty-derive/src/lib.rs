//! `#[derive(Row)]` for QuantyDB, written without `syn` and without
//! `quote` (ADR-020, ADR-031).
//!
//! The shape this accepts is deliberately narrow: a struct with named
//! fields, no generics, no lifetimes. Everything else gets a compile
//! error naming what it saw, because guessing what a tuple struct or an
//! enum should mean is how a surface grows things nobody asked for.

use proc_macro::{Delimiter, TokenStream, TokenTree};
use std::str::FromStr;

/// Derive [`quanty::Row`] for a struct of named fields.
#[proc_macro_derive(Row, attributes(quanty))]
pub fn derive_row(input: TokenStream) -> TokenStream {
    match parse(input) {
        Ok(model) => emit(&model),
        Err(message) => error(&message),
    }
}

/// A `compile_error!` in the caller's crate.
fn error(message: &str) -> TokenStream {
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    TokenStream::from_str(&format!("compile_error!(\"{escaped}\");"))
        .expect("compile_error! is valid Rust")
}

struct Model {
    name: String,
    table: String,
    fields: Vec<Field>,
}

struct Field {
    name: String,
    column: String,
}

/// Walk the derive input far enough to learn the struct name, the table
/// name and the fields. Types are never inspected: the generated code
/// asks the field's own type to convert itself, so this parser does not
/// need to understand `Option<Vec<u8>>`.
fn parse(input: TokenStream) -> Result<Model, String> {
    let tokens: Vec<TokenTree> = input.into_iter().collect();
    let mut i = 0;

    let mut table_override = None;
    while let Some(attr) = attribute(&tokens, &mut i)? {
        if let Some(value) = attr {
            table_override = Some(value);
        }
    }

    skip_visibility(&tokens, &mut i);

    match tokens.get(i) {
        Some(TokenTree::Ident(id)) if id.to_string() == "struct" => i += 1,
        Some(TokenTree::Ident(id)) => {
            return Err(format!(
                "Row can only be derived for a struct, found `{id}`"
            ))
        }
        _ => return Err("Row can only be derived for a struct".to_string()),
    }

    let name = match tokens.get(i) {
        Some(TokenTree::Ident(id)) => id.to_string(),
        _ => return Err("the struct has no name".to_string()),
    };
    i += 1;

    if let Some(TokenTree::Punct(p)) = tokens.get(i) {
        if p.as_char() == '<' {
            return Err(format!(
                "Row does not support generics or lifetimes, and `{name}` has them"
            ));
        }
    }

    let body = match tokens.get(i) {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => g.stream(),
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => {
            return Err(format!(
                "Row needs named fields, and `{name}` is a tuple struct"
            ))
        }
        _ => return Err(format!("Row needs named fields, and `{name}` has none")),
    };

    let fields = parse_fields(body, &name)?;
    if fields.is_empty() {
        return Err(format!("`{name}` has no fields to map"));
    }

    Ok(Model {
        table: table_override.unwrap_or_else(|| snake_case(&name)),
        name,
        fields,
    })
}

/// One `#[...]` if the cursor is on one. Returns the inner `quanty(...)`
/// string value when the attribute is ours, `None` when it is somebody
/// else's, and `None` for the outer option when there is no attribute.
#[allow(clippy::type_complexity)]
fn attribute(tokens: &[TokenTree], i: &mut usize) -> Result<Option<Option<String>>, String> {
    match tokens.get(*i) {
        Some(TokenTree::Punct(p)) if p.as_char() == '#' => {}
        _ => return Ok(None),
    }
    let group = match tokens.get(*i + 1) {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Bracket => g,
        _ => return Err("a `#` that is not an attribute".to_string()),
    };
    *i += 2;

    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    match inner.first() {
        Some(TokenTree::Ident(id)) if id.to_string() == "quanty" => {}
        _ => return Ok(Some(None)),
    }
    let args = match inner.get(1) {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => g.stream(),
        _ => return Err("`#[quanty]` needs a value, as in `#[quanty(table = \"x\")]`".to_string()),
    };
    named_string(args, &["table", "column"]).map(Some)
}

/// `key = "value"`, where `key` is one of `allowed`.
fn named_string(stream: TokenStream, allowed: &[&str]) -> Result<Option<String>, String> {
    let parts: Vec<TokenTree> = stream.into_iter().collect();
    let key = match parts.first() {
        Some(TokenTree::Ident(id)) => id.to_string(),
        _ => return Err("`#[quanty(...)]` starts with a name".to_string()),
    };
    if !allowed.contains(&key.as_str()) {
        return Err(format!(
            "`#[quanty({key})]` is not understood; expected one of {}",
            allowed.join(", ")
        ));
    }
    match parts.get(1) {
        Some(TokenTree::Punct(p)) if p.as_char() == '=' => {}
        _ => return Err(format!("`#[quanty({key})]` needs `= \"...\"`")),
    }
    match parts.get(2) {
        Some(TokenTree::Literal(l)) => unquote(&l.to_string())
            .map(Some)
            .ok_or_else(|| format!("`#[quanty({key})]` needs a plain string")),
        _ => Err(format!("`#[quanty({key})]` needs a string")),
    }
}

/// `"x"` to `x`, refusing raw, byte and escaped strings so that nothing
/// silently becomes a different table name than it looks like.
fn unquote(literal: &str) -> Option<String> {
    let inner = literal.strip_prefix('"')?.strip_suffix('"')?;
    if inner.contains('\\') {
        return None;
    }
    Some(inner.to_string())
}

fn skip_visibility(tokens: &[TokenTree], i: &mut usize) {
    if let Some(TokenTree::Ident(id)) = tokens.get(*i) {
        if id.to_string() == "pub" {
            *i += 1;
            if let Some(TokenTree::Group(g)) = tokens.get(*i) {
                if g.delimiter() == Delimiter::Parenthesis {
                    *i += 1;
                }
            }
        }
    }
}

/// `vis? name : type ,` repeated, with attributes on any of them.
fn parse_fields(stream: TokenStream, struct_name: &str) -> Result<Vec<Field>, String> {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut fields = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        let mut column_override = None;
        while let Some(attr) = attribute(&tokens, &mut i)? {
            if let Some(value) = attr {
                column_override = Some(value);
            }
        }
        if i >= tokens.len() {
            break;
        }

        skip_visibility(&tokens, &mut i);

        let name = match tokens.get(i) {
            Some(TokenTree::Ident(id)) => id.to_string(),
            _ => {
                return Err(format!(
                    "`{struct_name}` has a field this derive cannot read; \
                     named fields only"
                ))
            }
        };
        i += 1;

        match tokens.get(i) {
            Some(TokenTree::Punct(p)) if p.as_char() == ':' => i += 1,
            _ => return Err(format!("`{struct_name}.{name}` has no type")),
        }

        // Skip the type. Angle brackets arrive as separate punctuation, so
        // count them to find the comma that is really the field separator.
        let mut depth = 0i32;
        while i < tokens.len() {
            match &tokens[i] {
                TokenTree::Punct(p) if p.as_char() == '<' => depth += 1,
                TokenTree::Punct(p) if p.as_char() == '>' => depth -= 1,
                TokenTree::Punct(p) if p.as_char() == ',' && depth <= 0 => break,
                _ => {}
            }
            i += 1;
        }
        i += 1; // past the comma, or past the end

        fields.push(Field {
            column: column_override.unwrap_or_else(|| name.clone()),
            name,
        });
    }

    Ok(fields)
}

/// `UserAccount` to `user_account`.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The impl, as text. Building a `TokenStream` token by token would say
/// the same thing at four times the length, and this is the one place
/// where what is generated has to be read by a person.
fn emit(model: &Model) -> TokenStream {
    let name = &model.name;
    let table = &model.table;

    let columns: Vec<String> = model
        .fields
        .iter()
        .map(|f| format!("\"{}\"", f.column))
        .collect();

    let from: Vec<String> = model
        .fields
        .iter()
        .map(|f| {
            format!(
                "            {}: ::quanty::__private::field(\"{}\", \"{}\", it.next())?,",
                f.name, name, f.name
            )
        })
        .collect();

    let to: Vec<String> = model
        .fields
        .iter()
        .map(|f| {
            format!(
                "            (\"{}\", ::quanty::IntoValue::into_value(self.{}.clone())),",
                f.column, f.name
            )
        })
        .collect();

    let code = format!(
        "impl ::quanty::Row for {name} {{
    const TABLE: &'static str = \"{table}\";
    const COLUMNS: &'static [&'static str] = &[{columns}];

    fn from_values(values: ::std::vec::Vec<::quanty::Value>)
        -> ::quanty::Result<Self>
    {{
        let mut it = values.into_iter();
        ::quanty::Result::Ok({name} {{
{from}
        }})
    }}

    fn to_values(&self) -> ::std::vec::Vec<(&'static str, ::quanty::Value)> {{
        ::std::vec![
{to}
        ]
    }}
}}",
        columns = columns.join(", "),
        from = from.join("\n"),
        to = to.join("\n"),
    );

    TokenStream::from_str(&code).expect("the generated impl is valid Rust")
}

#[cfg(test)]
mod tests {
    use super::{snake_case, unquote};

    #[test]
    fn snake_case_splits_on_capitals_only() {
        assert_eq!(snake_case("User"), "user");
        assert_eq!(snake_case("UserAccount"), "user_account");
        assert_eq!(snake_case("HTTPHeader"), "h_t_t_p_header");
        assert_eq!(snake_case("already_snake"), "already_snake");
    }

    #[test]
    fn unquote_refuses_anything_that_is_not_a_plain_string() {
        assert_eq!(unquote("\"users\""), Some("users".to_string()));
        assert_eq!(unquote("r\"users\""), None);
        assert_eq!(unquote("b\"users\""), None);
        assert_eq!(unquote("42"), None);
        // An escape would make the name differ from what it looks like.
        assert_eq!(unquote("\"a\\\"b\""), None);
    }
}
