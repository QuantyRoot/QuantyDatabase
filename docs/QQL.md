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
get users join cities on users.city = cities.id { users.name, cities.name }
set users where id = 1 { score += 5 }
del users where score < 0
index users.score
drop table users
show tables
explain get users where name = "elchi"

get users where score > 10 as of 42          # read commit 42
get users as of time 1700000000000           # read by wall clock time
branch experiment                            # fork at the current head
branch fix at 42                             # fork at a specific commit
switch experiment                            # move new writes to a branch
merge experiment                             # fast-forward merge
drop branch experiment
show branches
log                                           # current branch history
gc keep 10                                    # retain 10 commits per branch
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

## Joins

A `get` can join more tables onto the base table, each with its own `on`
condition:

```
get users join cities on users.city = cities.id
get users left join cities on users.city = cities.id { users.name, cities.name }
```

`join` keeps only rows that match; `left join` keeps every base row and
fills the right columns with null when nothing matches. Joins chain left to
right, so a later `on` may reference any table named so far. A row of the
result is the base table's columns followed by each joined table's columns,
in join order.

Column references may be qualified with the table name, `cities.id`, or
left bare when the name occurs in only one table in the statement. A bare
name that two tables share is an error that lists the qualified spellings,
rather than a silent pick. Table aliases do not exist yet, so a table
cannot be joined to itself.

`where` runs after all joins, over the combined row, which matters for
`left join`: a condition in `on` decides whether the right side matches,
while the same condition in `where` filters the padded rows too. To find
base rows with no match, test a right key for null in `where`:

```
get users left join cities on users.city = cities.id where cities.id = null
```

`as of` reads every joined table from the same historical snapshot.

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

A join prints as a `NestedLoopJoin` or `IndexNestedLoopJoin` node with two
children: the plan for the left side and the access to the right side. The
right access is a `SeqScan` for a nested loop, or a `KeyProbe` or
`IndexProbe` when the `on` condition lets the join look the right row up by
key or index instead of scanning. A probe is only a shortcut: the full `on`
condition is still checked on every candidate, so the strategy never
changes which rows come out, only how fast.

## Branches and history

Commits form a history you can read back and fork, the same shape as git.

`branch <name>` creates a named branch at the current head, or at a given
commit with `branch <name> at <id>`. It does not switch to the new branch.
`switch <name>` points new writes at another branch. Each branch keeps its
own head, so writes on one are invisible on another until merged. `show
branches` lists them with their head commit ids, marking the current one with
`*`.

`merge <name>` fast-forwards the current branch to another branch's head. It
succeeds only when the current branch has not advanced since the fork; a
diverged merge is rejected rather than guessed at, because real three-way
merges need conflict handling that does not exist yet.

`get ... as of <id>` reads a table as it stood at a commit, schema included:
a table that did not exist at that commit is reported as unknown. `as of time
<ms>` resolves to the newest commit on the current branch at or before a unix
millisecond timestamp. Projections, filters, ordering and limits all apply to
a historical read exactly as to a live one. Commit ids are what `log` and
every commit acknowledgment print.

`log` lists the current branch's history, newest first, down to its retention
floor. `gc keep <n>` reclaims the space of commits older than the `n` newest
per branch; reads of a reclaimed commit fail with a clear message. Retaining
zero commits is refused, since a branch must keep at least its head.

## Grammar

```
statement  = table_def | drop | put | get | set | del | index | show
           | branch | switch | merge | log | gc | explain
explain    = "explain" statement
table_def  = "table" ident "{" column+ "}"
column     = ident ":" type ("=" literal | "@" attr)*    (commas optional)
type       = "int" | "float" | "text" | "bytes" | "bool"
attr       = "key" | "index" | "null"
drop       = "drop" ("table" | "branch") ident
put        = "put" ident row ("," row)*
row        = "{" ident ":" expr ("," ident ":" expr)* "}"
get        = "get" ident join* ("{" colref ("," colref)* "}")?
             as_of? ("where" expr)? ("order" "by" colref ("asc"|"desc")?)?
             ("limit" int)?
join       = ("left")? "join" ident "on" expr
colref     = ident ("." ident)?
as_of      = "as" "of" ("time")? int
set        = "set" ident ("where" expr)? "{" assign ("," assign)* "}"
assign     = ident ("=" | "+=" | "-=" | "*=" | "/=") expr
del        = "del" ident ("where" expr)?
index      = "index" ident "." ident
show       = "show" ("tables" | "branches")
branch     = "branch" ident ("at" int)?
switch     = "switch" ident
merge      = "merge" ident
log        = "log"
gc         = "gc" "keep" int

expr       = or
or         = and ("or" and)*
and        = not ("and" not)*
not        = "not" not | cmp
cmp        = add (("=" | "!=" | "<" | "<=" | ">" | ">=") add)?
add        = mul (("+" | "-") mul)*
mul        = unary (("*" | "/" | "%") unary)*
unary      = "-" unary | primary
primary    = literal | colref | "(" expr ")"
literal    = int | float | string | bytes | "true" | "false" | "null"
```
