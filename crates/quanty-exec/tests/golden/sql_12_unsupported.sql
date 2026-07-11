# everything outside the subset fails with an error that names the missing
# piece; nothing parses into something subtly different. the acceptance
# criterion behind this file: unsupported sql returns a clear error.

> CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)
ok

> CREATE TABLE u (id INTEGER PRIMARY KEY)
ok

# join shapes outside inner and left
> SELECT * FROM t RIGHT JOIN u ON t.id = u.id
error: parse error at byte 16: not supported yet: right and full outer joins

> SELECT * FROM t NATURAL JOIN u
error: parse error at byte 16: not supported yet: cross and natural joins (write join ... on ...)

> SELECT * FROM t JOIN u USING (id)
error: parse error at byte 23: not supported yet: join ... using (write on ... = ...)

> SELECT * FROM t, u
error: parse error at byte 15: implicit comma joins are not supported; write join ... on ...

> SELECT * FROM t AS x
error: parse error at byte 16: not supported yet: table aliases

# select list shapes
> SELECT DISTINCT a FROM t
error: parse error at byte 7: not supported yet: select distinct

> SELECT a + 1 FROM t
error: parse error at byte 9: expected 'from' (select without from is not supported yet), found '+'

> SELECT a AS b FROM t
error: parse error at byte 9: not supported yet: column aliases

> SELECT count(a) FROM t
error: parse error at byte 12: not supported yet: functions and aggregates

> SELECT max(a) FROM t
error: parse error at byte 10: not supported yet: functions and aggregates

> SELECT 1
error: parse error at byte 7: the select list takes plain column names or * for now; expressions are not supported yet

# clauses
> SELECT * FROM t GROUP BY a
error: parse error at byte 16: not supported yet: group by and aggregates

> SELECT * FROM t ORDER BY a LIMIT 5 OFFSET 2
error: parse error at byte 35: not supported yet: offset

> SELECT * FROM t ORDER BY a, b
error: parse error at byte 26: not supported yet: more than one order by column

> SELECT * FROM t UNION SELECT * FROM u
error: parse error at byte 16: not supported yet: compound selects

# predicates
> SELECT * FROM t WHERE a BETWEEN 1 AND 2
error: parse error at byte 24: not supported yet: between (spell it with two comparisons)

> SELECT * FROM t WHERE a IN (1, 2)
error: parse error at byte 24: not supported yet: in (...)

> SELECT * FROM t WHERE b LIKE 'x%'
error: parse error at byte 24: not supported yet: like and pattern matching

> SELECT * FROM t WHERE b NOT LIKE 'x%'
error: parse error at byte 24: not supported yet: like and pattern matching

> SELECT * FROM t WHERE EXISTS (SELECT * FROM u)
error: parse error at byte 22: not supported yet: exists

> SELECT * FROM t WHERE a = (SELECT id FROM u)
error: parse error at byte 27: not supported yet: subqueries

> SELECT * FROM t WHERE CASE WHEN a = 1 THEN true ELSE false END
error: parse error at byte 22: not supported yet: case expressions

> SELECT * FROM t WHERE CAST(a AS TEXT) = '1'
error: parse error at byte 22: not supported yet: cast

# writes
> INSERT INTO t VALUES (1, 2, 'x')
error: parse error at byte 14: insert needs an explicit column list, like insert into t (a, b) values (1, 2); positional inserts are not supported yet

> INSERT INTO t DEFAULT VALUES
error: parse error at byte 14: not supported yet: insert ... default values

> INSERT INTO t (id) SELECT id FROM u
error: parse error at byte 19: not supported yet: insert from select

> INSERT OR REPLACE INTO t (id) VALUES (1)
error: parse error at byte 7: not supported yet: insert or (replace, ignore, ...)

> INSERT INTO t (id) VALUES (1) ON CONFLICT DO NOTHING
error: parse error at byte 30: not supported yet: on conflict / upsert

> UPDATE t SET a = 1 FROM u
error: parse error at byte 19: not supported yet: update ... from

> DELETE FROM t ORDER BY id LIMIT 1
error: parse error at byte 14: not supported yet: order by / limit on delete

# transactions arrive later in this phase
> BEGIN
error: parse error at byte 0: not supported yet: transactions across statements

> COMMIT
error: parse error at byte 0: not supported yet: transactions across statements

> ROLLBACK
error: parse error at byte 0: not supported yet: transactions across statements

> SAVEPOINT sp1
error: parse error at byte 0: not supported yet: transactions across statements

# ddl
> CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)
error: parse error at byte 13: not supported yet: 'if not exists'

> DROP TABLE IF EXISTS t
error: parse error at byte 11: not supported yet: 'if exists'

> CREATE TABLE tmp (id INTEGER PRIMARY KEY AUTOINCREMENT)
error: parse error at byte 41: not supported yet: autoincrement

> CREATE TABLE tmp (id INTEGER PRIMARY KEY, a INTEGER UNIQUE)
error: parse error at byte 52: not supported yet: unique constraints

> CREATE TABLE tmp (id INTEGER PRIMARY KEY, a INTEGER CHECK (a > 0))
error: parse error at byte 52: not supported yet: check constraints

> CREATE TABLE tmp (id INTEGER PRIMARY KEY, a TEXT COLLATE NOCASE)
error: parse error at byte 49: not supported yet: collate

> CREATE TABLE tmp (a INTEGER, b INTEGER, PRIMARY KEY (b, a))
error: parse error at byte 56: composite primary keys must list columns in declaration order (other orders are not supported yet)

> CREATE TEMP TABLE tmp (id INTEGER PRIMARY KEY)
error: parse error at byte 7: not supported yet: temporary tables

> CREATE TABLE main.tmp (id INTEGER PRIMARY KEY)
error: parse error at byte 17: database-qualified names are not supported; there is one database per file

> CREATE UNIQUE INDEX i ON t (a)
error: parse error at byte 7: not supported yet: unique indexes

> CREATE INDEX i ON t (a, b)
error: parse error at byte 22: not supported yet: multi-column indexes

> CREATE INDEX i ON t (a DESC)
error: parse error at byte 23: not supported yet: descending indexes

> CREATE INDEX i ON t (a) WHERE a > 0
error: parse error at byte 24: not supported yet: partial indexes

> DROP INDEX i
error: parse error at byte 5: not supported yet: drop index (indexes live and die with their table for now)

> ALTER TABLE t ADD COLUMN c INTEGER
error: parse error at byte 0: not supported yet: alter table

> CREATE VIEW v AS SELECT * FROM t
error: parse error at byte 7: 'create view' is not supported

> PRAGMA journal_mode
error: parse error at byte 0: 'pragma' is not supported

> VACUUM
error: parse error at byte 0: 'vacuum' is not supported

> ATTACH 'other.db' AS other
error: parse error at byte 0: 'attach' is not supported

> WITH x AS (SELECT * FROM t) SELECT * FROM x
error: parse error at byte 0: not supported yet: with (common table expressions)

> REPLACE INTO t (id) VALUES (1)
error: parse error at byte 0: not supported yet: replace

# odds and ends
> SELECT * FROM t WHERE a = ?
error: parse error at byte 26: parameter placeholders are not supported; write the value inline

> SELECT * FROM t WHERE a & 1 = 1
error: parse error at byte 24: bitwise operators are not supported

> SELECT * FROM t ORDER BY a NULLS LAST
error: parse error at byte 27: not supported yet: nulls first / nulls last (nulls sort first ascending, always)

> SELECT "a b" FROM t
error: parse error at byte 7: the name "a b" does not fit the catalog; names are letters, digits and underscores, starting with a letter or underscore

> SELECT "" FROM t
error: parse error at byte 7: the name "" does not fit the catalog; names are letters, digits and underscores, starting with a letter or underscore
