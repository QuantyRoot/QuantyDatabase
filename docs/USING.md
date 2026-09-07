# Using QuantyDB

Three ways to use it, and they are the same database underneath:

- **a file**, driven from the command line
- **a library**, embedded in a Rust program
- **a server**, spoken to over a socket

Start with the file. Everything in this page was run, and the output
below each command is what came back.

Installing it is [INSTALL.md](INSTALL.md). The language in full is
[QQL.md](QQL.md), the SQL dialect is [SQL.md](SQL.md); this page is the
tour rather than the reference.

## A database is a file

```
$ quanty create shop.qdb
created shop.qdb
```

That is the whole of it. No server to start, no directory to lay out, no
configuration. The file holds the schema, the rows, the indexes and the
history.

## Tables

Columns are `name: type`, with attributes after them:

```
$ quanty run shop.qdb 'table products {
    id:    int  @key
    name:  text @index
    price: int
    tags:  text @text
  }'
ok
```

- `int`, `float`, `text`, `bytes`, `bool` are the types
- `@key` is the primary key; several make a composite one in order
- `@index` builds a secondary index on the column
- `@text` builds a full text index, which is what `match` searches
- `@null` allows nothing, `= 0` gives a default

## Writing

`put` takes one row or several:

```
$ quanty run shop.qdb 'put products
    { id: 1, name: "coffee grinder", price: 4900, tags: "kitchen burr manual" },
    { id: 2, name: "kettle",         price: 3200, tags: "kitchen electric" },
    { id: 3, name: "notebook",       price: 800,  tags: "office paper" }'
put 3
```

`set` changes rows in place, and arithmetic works on the column:

```
$ quanty run shop.qdb 'set products where id = 2 { price -= 200 }'
set 1
```

## Reading

`get` with no braces gives every column:

```
$ quanty run shop.qdb 'get products'
1|coffee grinder|4900|kitchen burr manual
2|kettle|3000|kitchen electric
3|notebook|800|office paper
```

Braces choose columns, and the rest reads as you would guess:

```
$ quanty run shop.qdb 'get products { name, price } where price > 1000 order by price desc'
coffee grinder|4900
kettle|3000
```

## Searching text

A `@text` column answers `match`. A bare word finds the word:

```
$ quanty run shop.qdb 'get products { name } where tags match "kitchen"'
kettle
coffee grinder
```

Several words in quotes are a phrase, and have to appear in that order:

```
$ quanty run shop.qdb 'get products { name } where tags match "burr manual"'
coffee grinder
```

A trailing star is a prefix:

```
$ quanty run shop.qdb 'get products { name } where tags match "elect*"'
kettle
```

Results come back best first, by BM25. There is no stemming, no stop word
list, and no wildcard inside a word.

## History, and asking about the past

Every write is a commit. Nothing is overwritten in place, so the old
version is still there:

```
$ quanty log shop.qdb
commit 3 parent 2
commit 2 parent 1
commit 1 parent 0
```

`as of` reads the database as it was at a commit:

```
$ quanty run shop.qdb 'get products { name, price } where id = 2'
kettle|3000

$ quanty run shop.qdb 'get products { name, price } as of 2 where id = 2'
kettle|3200
```

That is not a backup being restored. It is the same file answering a
question about a moment in it.

`quanty gc <db> <keep>` drops history beyond the last `keep` commits per
branch when you want the space back.

## Branches

A branch is a name for a commit, and writes land on the one you are on:

```
$ quanty branch shop.qdb experiment
ok
$ quanty switch shop.qdb experiment
switched to experiment
$ quanty run shop.qdb 'set products where id = 1 { price = 1 }'
set 1
$ quanty run shop.qdb 'get products { name, price } where id = 1'
coffee grinder|1
```

Main has not moved:

```
$ quanty switch shop.qdb main
switched to main
$ quanty run shop.qdb 'get products { name, price } where id = 1'
coffee grinder|4900

$ quanty branches shop.qdb
  experiment @6
* main @3
```

The star is the branch you are on, the number is the commit it names, and
the list is alphabetical.

`quanty merge shop.qdb experiment` answers
`merged experiment, head is now commit 6`, and
`quanty run shop.qdb "drop branch experiment"` removes the name. There is
no `--branch` flag on purpose: a write always lands on the branch you are
on, so running elsewhere would mean switching there and back.

## SQL, if that is what you have

The same database answers a SQL dialect:

```
$ quanty run shop.qdb --sql 'SELECT name, price FROM products WHERE price < 1000'
notebook|800
```

And an existing SQLite file can be read straight in:

```
quanty import old.sqlite new.qdb
```

`--dry-run` prints what it would do without writing, and `--strict`
refuses anything lossy instead of reporting it. What survives the trip and
what does not is [IMPORT.md](IMPORT.md).

## Many statements at once

`shell` reads statements from standard input, one per line, which is how
you load a file:

```
quanty shell shop.qdb < seed.qql
```

## Which index it wishes you had

The engine remembers what it had to scan:

```
quanty run shop.qdb "show suggestions"
```

It lists the columns a query narrowed on without an index, worst first by
rows walked, and prints nothing when it has no complaint.

## Embedded in Rust

Add the crate, then open a file or a page of memory. A transaction is a
borrow, so it cannot outlive the database and cannot be left open: the
closure returning is what decides commit or rollback.

```rust
use quanty::Database;

let mut db = Database::open("shop.qdb")?;

db.transaction(|tx| {
    tx.execute(r#"put products { id: 4, name: "scale", price: 2600, tags: "kitchen" }"#)
})?;

let rows = db.query("get products { name, price } where price > 1000")?;
for row in rows.rows() {
    println!("{:?}", row);
}
```

Rows come back typed if you ask them to:

```rust
use quanty::{Database, Row};

#[derive(Row, Debug, PartialEq)]
#[quanty(table = "users")]
struct User {
    id: i64,
    name: String,
    score: i32,
}

db.insert(&User { id: 1, name: "ada".into(), score: 7 })?;
let back: Vec<User> = db.query_as("get users { id, name, score }")?;
```

`Database::in_memory()` gives the same thing with no file, which is what
tests usually want.

Large payloads go in an `asset` column, which keeps the bytes in chunks
and the row keeps a descriptor. Only the embedded surface can write one:
the language cannot author a descriptor by hand, because one names its
content by hash and a hand written one would point at nothing.

## As a server

```
$ quanty setup
```

The wizard writes a database, a token file and optionally a systemd unit,
prints the token once, and prints the command to start it. By hand:

```
quanty serve shop.qdb --listen 127.0.0.1:7878 --tokens shop.tokens
quanty connect 127.0.0.1:7878 --token <token>
```

`connect` with a statement runs that one; without it, it reads statements
from standard input the way `shell` does.

**The wire is not encrypted.** The token crosses it in the clear on every
connection, so anyone who can watch the traffic can take one and use it.
TLS is written here rather than pulled in, and is not written yet. Keep
the server on loopback or a network you trust, or put wireguard, an ssh
tunnel or a TLS proxy in front of it. The server says so every time it
starts.

`quanty serve` runs on Linux. The tool, the library and both languages run
on Linux, macOS and Windows.

## When something is wrong

- `quanty tables <db>` lists what is defined
- `quanty stats <db>` gives page counts for the file as it stands
- `quanty log <db>` shows the commits, and `as of` reads any of them
- a killed process loses nothing committed: the file recovers on the next
  open, and that path is exercised by a few thousand kills on every push

## Where to go next

- [QQL.md](QQL.md) is the language, in full, with the grammar
- [SQL.md](SQL.md) is what the SQL front end accepts
- [ARCHITECTURE.md](ARCHITECTURE.md) is how it is put together
- [DECISIONS.md](DECISIONS.md) is why, with the costs
