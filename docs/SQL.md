# The SQL dialect

QuantyDB speaks two languages. QQL is the native one (docs/QQL.md). SQL is
the second front end: a pragmatic, sqlite-flavored subset that lowers onto
the exact same AST, so both languages run through one planner, one executor
and one set of semantics. `Session::execute_sql` is the entry point; the
transaction rule is the same as everywhere else, one statement is one
transaction.

Two principles shape everything below (decided in ADR-014):

1. Engine semantics, sql spelling. Where SQL tradition and the engine
   disagree, the engine wins and the divergence is documented here. Nothing
   silently behaves almost-but-not-quite like sqlite.
2. Outside the subset, fail loudly. Unsupported SQL is refused at parse
   time with an error that names the missing piece. It never parses into
   something subtly different.

## Statements

```
SELECT * | col [, col ...] FROM table
    [[INNER | LEFT [OUTER]] JOIN table ON expr ...]
    [WHERE expr]
    [ORDER BY col [ASC | DESC]]
    [LIMIT n]

INSERT INTO table (col [, col ...]) VALUES (expr, ...) [, (expr, ...) ...]

UPDATE table SET col = expr [, col = expr ...] [WHERE expr]

DELETE FROM table [WHERE expr]

CREATE TABLE name (column-def | table-constraint, ...) [WITHOUT ROWID] [STRICT]

CREATE INDEX name ON table (col)

DROP TABLE name

EXPLAIN [QUERY PLAN] statement

BEGIN [DEFERRED | IMMEDIATE | EXCLUSIVE] [TRANSACTION]
COMMIT [TRANSACTION] | END [TRANSACTION]
ROLLBACK [TRANSACTION]

SHOW TABLES
```

The column list on INSERT is required; a row must have as many values as
the list has columns. SHOW TABLES is a convenience borrowed from QQL, not
sqlite. EXPLAIN prints this engine's plan, not sqlite bytecode.

## Column definitions

```
name TYPE [PRIMARY KEY] [NOT NULL | NULL] [DEFAULT literal]
     [REFERENCES table [(col)] [actions...]] [CONSTRAINT name ...]
```

plus the table constraints `PRIMARY KEY (a [, b ...])` and
`FOREIGN KEY (...) REFERENCES ...`.

- Columns are nullable unless NOT NULL or PRIMARY KEY says otherwise. That
  is the SQL default; QQL has the opposite default (`@null` opts in). The
  parser resolves this, the catalog stores one truth.
- Exactly one primary key per table, required. A composite key is the
  table-level form and must list columns in declaration order; the key
  column order is the declaration order, and a reordered key cannot be
  expressed yet.
- Defaults are plain literals, no expressions, same rule as QQL.
- A `CREATE INDEX` name is required by the grammar and dropped by the
  engine: an index is identified by (table, column), as in QQL. There is
  no DROP INDEX yet; indexes live and die with their table.
- WITHOUT ROWID and STRICT parse and change nothing, because they describe
  properties this engine has anyway: every table clusters by its primary
  key, every column is strictly typed.
- Foreign keys (column-level REFERENCES and table-level FOREIGN KEY,
  including ON DELETE / ON UPDATE actions) parse and are not enforced.
  That is the behavior sqlite ships with by default. Nothing checks that
  the referenced table exists.

## Types

Type names map onto the five storage types:

| written as                                              | stored as |
| ------------------------------------------------------- | --------- |
| INT, INTEGER, BIGINT, SMALLINT, TINYINT, MEDIUMINT, INT2/4/8 | int   |
| REAL, FLOAT, DOUBLE [PRECISION], NUMERIC, DECIMAL        | float     |
| TEXT, VARCHAR, NVARCHAR, CHAR, NCHAR, CLOB, STRING, CHARACTER [VARYING] | text |
| DATE, DATETIME, TIMESTAMP                                | text      |
| BLOB                                                     | bytes     |
| BOOLEAN, BOOL                                            | bool      |

NUMERIC on float and the date family on text follow sqlite affinity in
spirit and are the lossy corners of the mapping; declare REAL or TEXT
directly when it matters. A length like `VARCHAR(160)` or `NUMERIC(10,2)`
parses and means nothing, the same treatment sqlite gives it. Unknown type
names are errors, not text.

Value behavior is the engine's: int literals widen into float columns,
`1 / 2` is integer division before any widening, arithmetic is checked
(overflow and division by zero are errors, never wraparound).

## Lexical shape

- Keywords match case-insensitively. `SELECT`, `select` and `SeLeCt` are
  the same word.
- Identifiers keep the case they were written in and match exactly.
  `Album` and `album` are different names. (sqlite matches names
  case-insensitively; this engine does not, because the catalog is
  case-sensitive and one exact spelling keeps diffs and docs honest.)
- Quoting styles: `"name"`, `[name]`, `` `name` ``. Quoting bypasses
  reserved words. Quoted or not, a name must have identifier shape
  (letters, digits, underscores, starting with a letter or underscore),
  because every name in the catalog renders back into QQL, the canonical
  language. For the same reason the lowercase words `not`, `and` and `or`
  cannot be names even when quoted: they are operators in QQL (ADR-017).
  `"NOT"` is fine, since QQL keywords are lowercase.
- Strings are single quoted; the only escape is a doubled quote
  (`'it''s'`). Backslashes are ordinary bytes.
- Blobs are `x'c0ffee'` (either case). Numbers include `.5`, `5.`, `2e3`
  and hex ints like `0x1f`. Int literals must fit an i64, float literals
  must be finite; both are errors otherwise, same rules as QQL.
- Comments: `-- to end of line` and `/* block */`.
- `=` and `==` are the same operator, so are `!=` and `<>`. `||`
  concatenates text.

## Expressions and the null rules

Operator precedence, loosest to tightest: `OR`, `AND`, `NOT`, comparisons
and `IS [NOT]`, then `+ - ||`, then `* / %`, then unary `-`/`+`.

The engine's null rules apply (docs/QQL.md): `null = null` holds,
comparing null with a non-null value is false, null arithmetic yields
null, and a null WHERE condition keeps no rows. SQL three-valued logic
says a comparison written against the NULL literal never matches, which
would silently mean the opposite here. So:

- `expr = NULL`, `expr <> NULL` and ordered comparisons against the NULL
  literal are refused at parse time with a pointer to the right spelling.
- `IS NULL` / `IS NOT NULL` are the right spelling and lower onto the
  engine's null-safe `= null` / `!= null`, which do exactly what the SQL
  forms promise.
- `a IS b` and `a IS NOT b` work on any operands as null-safe
  comparisons, the same meaning sqlite gives them.

The one remaining divergence: a column-to-column comparison like
`WHERE a = b` where both are null matches here and would not match in
sqlite. Write `a IS b` when that is what you mean; it behaves identically
in both systems.

One more: `+` on text concatenates (engine rule). sqlite would coerce to
numbers instead. `||` is the portable spelling.

## Joins

`JOIN` and `INNER JOIN` are inner joins; `LEFT JOIN` and `LEFT OUTER JOIN`
are left outer joins. Both need an `ON` condition. `RIGHT` and `FULL`
joins, `CROSS` and `NATURAL` joins, `USING`, comma joins and table aliases
are refused with a message naming the piece; the semantics live in
docs/QQL.md and are the same whichever front end wrote the statement.

Columns may be qualified with the table name (`cities.id`) or left bare
when unambiguous. `ON` decides matching; `WHERE` runs after the whole join
and, on a `LEFT JOIN`, sees the null-padded rows too. Because the engine's
`= NULL` is a parse error, the way to keep only unmatched left rows is
`WHERE right.key IS NULL`, exactly as in sqlite.

`EXPLAIN` shows the join strategy the planner chose (nested loop, or a key
or index probe of the right table); see docs/QQL.md for how to read it. The
strategy is a speed choice only and never changes the result.

## Transactions

BEGIN, COMMIT (also spelled END) and ROLLBACK work across statements and
mean what they do in sqlite: the statements between BEGIN and COMMIT apply
as one unit or not at all, and reads inside the transaction see its own
pending writes. The locking words after BEGIN (DEFERRED, IMMEDIATE,
EXCLUSIVE) parse and change nothing; there is one writer per database
anyway. Savepoints are not supported. The semantics are shared with QQL,
see docs/QQL.md.

## Not in the subset (yet)

Everything here fails with an error naming the missing piece. Nothing from
this phase is outstanding. Not scheduled: expressions and aliases in the
select list, table aliases, RIGHT / FULL / CROSS / NATURAL joins, USING,
comma joins, savepoints, DISTINCT, functions and aggregates, GROUP BY /
HAVING, compound selects, OFFSET, subqueries, CASE, CAST, BETWEEN, IN,
LIKE, positional INSERT, INSERT ... SELECT, upserts, ALTER TABLE, views,
triggers, temporary tables, unique and check constraints, COLLATE,
multi-column / unique / partial / descending indexes, DROP INDEX,
IF [NOT] EXISTS, AUTOINCREMENT, PRAGMA, ATTACH, parameters, bitwise
operators.

Time travel and branching have no SQL spelling; they stay QQL-only
(`as of`, `branch`, `switch`, `merge`, `log`, `gc`).

## Errors

Parse errors carry the byte position and say what was expected or what is
unsupported. Errors from the planner and executor are shared with QQL and
use its vocabulary (a column that rejects null reads `not @null`, for
example); the message text is engine-owned, whichever language produced
the statement.
