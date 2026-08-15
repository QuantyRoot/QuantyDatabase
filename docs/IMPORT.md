# Importing a SQLite database

```sh
quanty import app.sqlite app.qdb
quanty run app.qdb "get users { name } limit 10"
```

That is the whole thing when it works, which is the point. What follows is
what happens in between, what the import will tell you about, and what it
refuses to do.

There is no SQLite library underneath any of this. The file format is read
directly (ADR-005), so there is no C toolchain, no ffi, and no version of
libsqlite to match. The reader also cannot write: the type it reads through
has no write method, so the database being imported cannot be damaged by
us, no matter what goes wrong.

## Two passes

The first pass reads the whole source and decides what it becomes, writing
nothing. The second executes that decision. The split costs a second read
of the file and buys a report of every problem at once, a minute in,
instead of the first problem alone after ten minutes of writing (ADR-019).

`--dry-run` stops after the first pass and prints what it decided. It is
worth running once on anything unfamiliar.

```sh
quanty import app.sqlite app.qdb --dry-run
```

## What the types become

SQLite is dynamically typed and QuantyDB is not, so every column needs an
answer the source file does not give. The rule is that the declared type
proposes and the stored data decides, and neither alone is enough:

- reading the declaration alone gets it wrong, because `DATETIME` has
  numeric affinity and almost always holds text, and `NUMERIC(10,2)` holds
  reals
- reading the bytes alone gets it wrong too, because SQLite stores a whole
  numbered value in a real column as an integer to save space, so nearly
  every real column in the world looks mixed

So both are read: affinity is computed from the declaration by SQLite's own
rules, storage classes are counted over the rows, and the column type
follows from the two together.

A column that genuinely mixes types widens rather than stopping the import.
Integer and real together become `float`. Anything else becomes `text`, or
`bytes` once a blob is in the mix. A settings table whose `value` column
holds both numbers and strings is an ordinary thing to have, not a
corruption, and refusing it would mean refusing databases SQLite reads
perfectly well.

Widening has a cost and the report names it every time: as text, `10` sorts
before `9`. If you would rather fix the source than accept that, `--strict`
turns every widening back into a refusal.

## What the report tells you

Everything the import decided that you might disagree with, and nothing
else. Renamed tables and columns, widened types, columns typed from their
declaration because they hold no values at all, defaults that could not be
read, indexes that were skipped, and tables that were given a key they did
not have.

Two of those are worth expanding on.

**Added keys.** Every table needs a key. A rowid alias becomes one, a
composite primary key becomes a composite key. Where there is no usable
key, or the declared one holds NULL, or its values are longer than a
b-tree key may be, the rowid becomes a key column of its own and the
original column is kept as ordinary data. That adds a column your schema
did not have, so it is in the report.

**Constraints that do not come across.** A foreign key is not carried,
because the query language has nowhere to put one. This is not a `--strict`
matter: `--strict` refuses judgement calls, where a different answer was
available, and there is no other answer here. Refusing would leave you with
no import and no foreign key either, so the import happens and the report
says plainly what is no longer being enforced.

## What is skipped

Named in the report, each for a reason:

- views and triggers, which hold no rows
- SQLite's own `sqlite_` tables, which are another engine's bookkeeping
- virtual generated columns, which hold no data in the file
- indexes over more than one column or over an expression, which we cannot
  express yet
- indexes on columns holding values longer than a key may be

## What is refused

An import stops rather than guessing when:

- a value cannot survive the type its column was given, such as an integer
  past 2^53 in a column that has to become a float
- a table has no key available at all, which happens to a without rowid
  table whose own primary key cannot be used
- the source contradicts itself, in any of the ways the reader checks for

A refusal names the table, the column and the row.

## Databases the reader handles that you might expect it not to

- **Tables without rowids.** These are stored as index b-trees and their
  records reorder the columns, key columns first. The create statement is
  parsed to put them back.
- **Uncheckpointed write-ahead logs.** If a `-wal` file sits next to the
  database with committed frames in it, those pages are read on top of the
  main file, and frames from a transaction that was never committed are
  ignored. A database in wal mode is only refused when nothing accounts for
  its log, which is not the same as the header flag being set: a
  checkpointed database imports normally.
- **Text in utf-16**, in either byte order.
- **Columns added by `alter table add column`**, whose older rows have
  records that end early and take the column default.

## After the import

The database is a QuantyDB one, so everything else applies to it:

```sh
quanty tables app.qdb
quanty run app.qdb "get users { name, score } where score > 100"
quanty run app.qdb "select name from users where score > 100" --sql
quanty shell app.qdb < statements.txt
```

An import writes into a database that does not exist yet and refuses to
overwrite one that does. If it fails part way through, the target holds
part of the data and should be deleted; the planning pass exists so that
the failures worth knowing about are known before the first row is written.
