# QQL

The Quanty Query Language. Design goals: readable, typed, no surprises.
This document is normative for the parser in `crates/quanty-ql` and the
semantics in `crates/quanty-exec`; the golden tests under
`crates/quanty-exec/tests/golden/` are the executable version of it.

## Statements

```
table users {
  id:    int  @key
  name:  text @index
  score: int  = 0
  bio:   text @null
}

put users { id: 1, name: "elchi" }, { id: 2, name: "mira" }
get users { name, score } where score > 10 order by score desc limit 5
set users where id = 1 { score += 5 }
del users where score < 0
index users.score
drop table users
show tables
explain get users where name = "elchi"
```

One statement per string. `#` starts a comment that runs to the end of the
line. Keywords are lowercase, always. There are no reserved words; context
decides, so a column can be called `limit` if you insist.

## Tables

Every column is `name: type` with optional attributes and default:

- types: `int` (64 bit signed), `float` (64 bit IEEE), `text` (UTF-8),
  `bytes`, `bool`
- `@key` marks a primary key column; multiple `@key` columns form a
  composite key in declaration order; every table needs at least one
- `@index` creates a secondary index on the column
- `@null` allows null; key columns cannot be `@null`
- `= literal` sets a default used when `put` omits the column

A `put` that omits a column uses the default, then null if the column is
`@null`, and is an error otherwise. Inserting an existing primary key is an
error, not an upsert. All rows of a `put` land atomically or not at all;
that holds for every statement.

## Values and literals

Ints `42`, floats `1.5` / `2e3` (a float literal that overflows to
infinity is a parse error), strings `"..."` with `\" \\ \n \t \0` escapes,
bytes `x"c0ffee"`, `true`, `false`, `null`.

Int literals fit float columns and are widened on the way in. Nothing else
converts implicitly.

## Expressions

Comparisons `=`, `!=`, `<`, `<=`, `>`, `>=`; logic `and`, `or`, `not`;
arithmetic `+ - * / %` and unary `-`; parentheses. Precedence from loose to
tight: `or`, `and`, `not`, comparisons, `+ -`, `* / %`, unary minus.

The rules, spelled out because most of them are the usual SQL surprises
turned off:

- `=` and `!=` treat null as an ordinary value: `null = null` is true,
  `null = 5` is false, `v != null` reads as "v is not null"
- `<`, `<=`, `>`, `>=` involving null are always false
- arithmetic with null yields null; a null condition counts as false
- int and float compare and mix numerically; `2 = 2.0` is true
- every other type mix in a comparison or arithmetic is an error, not a
  silent coercion: `1 = "1"` fails loudly
- int arithmetic is checked: overflow, `/ 0` and `% 0` are errors
- `int / int` truncates: `7 / 2` is `3`, `1 / 2` is `0`; write `1.0 / 2`
  for `0.5`
- `+` concatenates text
- float comparison and sorting use IEEE total order, so even NaN sorts
  deterministically

`set` assignments (`=`, `+=`, `-=`, `*=`, `/=`) all evaluate against the
row as it was before the statement, so `set t { a = b, b = a }` swaps.
Assigning key columns is not supported yet (needs row moves, phase 3).

## Ordering and limits

`order by <column> [asc|desc]`, one column for now, nulls first ascending.
Without `order by`, rows come back in primary key order. `limit n` caps the
result after sorting.

## Reading plans

`explain` in front of `get`, `put`, `set` or `del` prints the plan instead
of running it:

```
explain get users where name = "elchi" and score > 3
Filter (score > 3)
  IndexScan users via name = "elchi"
```

Access paths in this version: `KeyLookup` when the filter pins the whole
primary key with equalities, `IndexScan` when an indexed column is pinned,
`SeqScan` otherwise. Whatever the access path did not consume shows up as
a `Filter` above it.

## Grammar

```
statement  = table_def | drop | put | get | set | del | index | show | explain
explain    = "explain" statement
table_def  = "table" ident "{" column+ "}"
column     = ident ":" type ("=" literal | "@" attr)*    (commas optional)
type       = "int" | "float" | "text" | "bytes" | "bool"
attr       = "key" | "index" | "null"
drop       = "drop" "table" ident
put        = "put" ident row ("," row)*
row        = "{" ident ":" expr ("," ident ":" expr)* "}"
get        = "get" ident ("{" ident ("," ident)* "}")?
             ("where" expr)? ("order" "by" ident ("asc"|"desc")?)?
             ("limit" int)?
set        = "set" ident ("where" expr)? "{" assign ("," assign)* "}"
assign     = ident ("=" | "+=" | "-=" | "*=" | "/=") expr
del        = "del" ident ("where" expr)?
index      = "index" ident "." ident
show       = "show" "tables"

expr       = or
or         = and ("or" and)*
and        = not ("and" not)*
not        = "not" not | cmp
cmp        = add (("=" | "!=" | "<" | "<=" | ">" | ">=") add)?
add        = mul (("+" | "-") mul)*
mul        = unary (("*" | "/" | "%") unary)*
unary      = "-" unary | primary
primary    = literal | ident | "(" expr ")"
literal    = int | float | string | bytes | "true" | "false" | "null"
```
